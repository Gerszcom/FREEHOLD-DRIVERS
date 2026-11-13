//! Stage 5 groundwork — the GSP message-queue transport.
//!
//! Once a card's GSP firmware is verified ([`crate::firmware`]) and started,
//! the host driver and the GSP talk over a pair of ring buffers in memory both
//! of them can see: a **command queue** the host fills and the GSP drains, and
//! a **status queue** the GSP fills and the host drains. Each queue is a
//! single-producer / single-consumer ring of fixed-size message slots, with a
//! write index owned by the producer and a read index owned by the consumer,
//! both living in the shared region so each side can see how far the other has
//! got. This module is that transport — the index arithmetic, the full/empty
//! accounting and the slot copy — with nothing above it (no message encoding,
//! no RPC, no doorbell) and nothing below it (no real shared memory yet).
//!
//! ## Why this is host-verifiable, and what is not
//!
//! A single-producer / single-consumer ring buffer is ordinary, well-known
//! computer science: an index that advances and wraps, a copy into a slot, and
//! the arithmetic that tells full from empty. All of that is *correct or
//! incorrect on any machine*, so it is written and proven here against an
//! in-memory [`crate::mock::MockSharedMem`] with no hardware caveat — exactly
//! as [`crate::sha256`] is.
//!
//! What this module does **not** fix is the *wire layout* the GSP firmware
//! expects: at which byte offsets within the shared region the write index, the
//! read index and the first slot actually sit, and the header fields
//! (version/size/slot-size/slot-count/flags) NVIDIA's published GSP message
//! queue carries around them. That layout is dictated by the firmware, not by
//! FREEHOLD, and cannot be confirmed without a real card. So this transport is
//! deliberately *parameterised* by a [`QueueLayout`] the caller supplies: the
//! ring logic is settled here, and the concrete offsets are filled in — and
//! validated on hardware — when the on-device loader (`ROADMAP.md`, Stage 5)
//! brings a real GSP up. See `validation/stage-5/`.
//!
//! Clean-room: a ring buffer is FREEHOLD's own code; no driver source is
//! copied or translated. The only NVIDIA-specific facts this will eventually
//! need — the exact header offsets — are a documented interface, recorded as
//! data in a [`QueueLayout`], not lifted as code.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the queue holds no buffer
//! of its own, only a description of where one lives, and every access goes
//! through the [`SharedMem`] trait so the same logic runs on the host and, one
//! day, over a real mapped region.

/// Byte-addressable access to a region of memory shared with the GSP.
///
/// On FREEHOLD this is a thin wrapper over a mapped, uncached sysmem region the
/// GSP also sees; on the host it is [`crate::mock::MockSharedMem`]. The two
/// 32-bit accessors read and write the queue indices (little-endian, the byte
/// order both the host CPU and the GSP use); the byte accessors copy message
/// payloads into and out of slots. Implementations must treat out-of-range
/// offsets as a programming error — this module never issues one, because every
/// access is bounded by a [`QueueLayout`] that was validated first.
pub trait SharedMem {
    /// Read the little-endian 32-bit word at byte offset `off`.
    fn read_u32(&self, off: usize) -> u32;

    /// Write `value` as a little-endian 32-bit word at byte offset `off`.
    fn write_u32(&mut self, off: usize, value: u32);

    /// Copy `buf.len()` bytes starting at byte offset `off` into `buf`.
    fn read_bytes(&self, off: usize, buf: &mut [u8]);

    /// Copy `src` to `src.len()` bytes starting at byte offset `off`.
    fn write_bytes(&mut self, off: usize, src: &[u8]);
}

/// Why a queue operation could not be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// A [`QueueLayout`] is not internally consistent: a zero slot size or slot
    /// count, or a slot count large enough that the doubled index space would
    /// not fit a `u32`. Caught by [`QueueLayout::validate`] before any access.
    BadLayout,
    /// [`enqueue`] was called on a full queue, or [`dequeue`] on an empty one.
    /// The caller retries once the other side has made room (producer) or
    /// posted a message (consumer).
    WouldBlock,
    /// The message buffer handed to [`enqueue`] or [`dequeue`] is not exactly
    /// one slot (`slot_size`) long. Slots are fixed-size; a short or long
    /// buffer is a caller bug, reported rather than truncated.
    BadMessageLen {
        /// The slot size the queue uses.
        expected: usize,
        /// The length of the buffer the caller supplied.
        got: usize,
    },
}

/// Where a queue's parts sit in the shared region, and how big its slots are.
///
/// Both ends of a queue agree this layout out of band (the host sets it up when
/// it builds the queue for the GSP). The two index words are each owned by one
/// side — the producer writes `write_index_off` and only reads
/// `read_index_off`, the consumer the reverse — so no lock is needed. The ring
/// is `slot_count` contiguous slots of `slot_size` bytes beginning at
/// `ring_off`.
///
/// The concrete values for a real GSP come from NVIDIA's published message
/// queue layout and are confirmed on hardware (`validation/stage-5/`); this
/// type carries them as data so the transport logic below never bakes them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLayout {
    /// Byte offset of the producer-owned write index word.
    pub write_index_off: usize,
    /// Byte offset of the consumer-owned read index word.
    pub read_index_off: usize,
    /// Byte offset of the first message slot.
    pub ring_off: usize,
    /// Size of one message slot, in bytes.
    pub slot_size: usize,
    /// Number of slots in the ring.
    pub slot_count: u32,
}

impl QueueLayout {
    /// Check that the layout is usable: non-zero slot size and count, and a
    /// slot count small enough that the doubled index space (see
    /// [`index_modulus`](Self::index_modulus)) still fits a `u32`.
    ///
    /// Every other method assumes a validated layout, so a caller validates
    /// once when it builds the queue and then reuses it freely.
    pub fn validate(&self) -> Result<(), QueueError> {
        if self.slot_size == 0 || self.slot_count == 0 || self.slot_count > u32::MAX / 2 {
            return Err(QueueError::BadLayout);
        }
        Ok(())
    }

    /// The modulus the indices run in: twice the slot count.
    ///
    /// Indices range over `0..2*slot_count` rather than `0..slot_count` so that
    /// a full queue (write index one whole lap ahead of read) is distinct from
    /// an empty one (indices equal) without a separate flag. The slot an index
    /// refers to is the index modulo `slot_count`.
    pub fn index_modulus(&self) -> u32 {
        self.slot_count * 2
    }

    /// The total number of bytes the ring's slots occupy (`slot_size *
    /// slot_count`). The shared region must hold at least this many bytes
    /// beyond [`ring_off`](Self::ring_off), plus the two index words.
    pub fn ring_len(&self) -> usize {
        self.slot_size * self.slot_count as usize
    }

    /// Byte offset of slot `slot` (a value in `0..slot_count`).
    fn slot_off(&self, slot: u32) -> usize {
        self.ring_off + slot as usize * self.slot_size
    }
}

/// Advance an index by one within `0..modulus`, wrapping at the top.
fn next_index(index: u32, modulus: u32) -> u32 {
    let n = index + 1;
    if n == modulus {
        0
    } else {
        n
    }
}

/// How many messages are waiting in the queue: the gap from the read index up
/// to the write index, measured in the doubled index space.
///
/// Returns a value in `0..=slot_count`. `0` is empty, `slot_count` is full.
pub fn occupancy<M: SharedMem>(layout: &QueueLayout, mem: &M) -> u32 {
    let write = mem.read_u32(layout.write_index_off);
    let read = mem.read_u32(layout.read_index_off);
    let modulus = layout.index_modulus();
    (write + modulus - read) % modulus
}

/// How many free slots the queue has: [`slot_count`](QueueLayout::slot_count)
/// less the current [`occupancy`].
pub fn free_slots<M: SharedMem>(layout: &QueueLayout, mem: &M) -> u32 {
    layout.slot_count - occupancy(layout, mem)
}

/// Whether the queue holds no messages.
pub fn is_empty<M: SharedMem>(layout: &QueueLayout, mem: &M) -> bool {
    occupancy(layout, mem) == 0
}

/// Whether the queue has no room for another message.
pub fn is_full<M: SharedMem>(layout: &QueueLayout, mem: &M) -> bool {
    occupancy(layout, mem) == layout.slot_count
}

/// Reset a queue to empty by zeroing both index words.
///
/// The producer does this when it first builds the queue, before either side
/// starts, so the slots' contents do not matter — only that both indices begin
/// at `0`.
pub fn init<M: SharedMem>(layout: &QueueLayout, mem: &mut M) -> Result<(), QueueError> {
    layout.validate()?;
    mem.write_u32(layout.write_index_off, 0);
    mem.write_u32(layout.read_index_off, 0);
    Ok(())
}

/// Append one message to the queue (the producer's operation).
///
/// `msg` must be exactly [`slot_size`](QueueLayout::slot_size) bytes. The bytes
/// are copied into the slot the write index points at, and only then is the
/// write index advanced — so the consumer never sees a slot announced before it
/// is filled. Returns [`QueueError::WouldBlock`] if the queue is full, leaving
/// it untouched.
pub fn enqueue<M: SharedMem>(
    layout: &QueueLayout,
    mem: &mut M,
    msg: &[u8],
) -> Result<(), QueueError> {
    if msg.len() != layout.slot_size {
        return Err(QueueError::BadMessageLen {
            expected: layout.slot_size,
            got: msg.len(),
        });
    }
    if is_full(layout, mem) {
        return Err(QueueError::WouldBlock);
    }
    let write = mem.read_u32(layout.write_index_off);
    let slot = write % layout.slot_count;
    mem.write_bytes(layout.slot_off(slot), msg);
    mem.write_u32(
        layout.write_index_off,
        next_index(write, layout.index_modulus()),
    );
    Ok(())
}

/// Take the oldest message from the queue into `out` (the consumer's
/// operation).
///
/// `out` must be exactly [`slot_size`](QueueLayout::slot_size) bytes. The slot
/// the read index points at is copied out, and only then is the read index
/// advanced — so the producer does not reclaim the slot until it has been read.
/// Returns [`QueueError::WouldBlock`] if the queue is empty, leaving it
/// untouched.
pub fn dequeue<M: SharedMem>(
    layout: &QueueLayout,
    mem: &mut M,
    out: &mut [u8],
) -> Result<(), QueueError> {
    if out.len() != layout.slot_size {
        return Err(QueueError::BadMessageLen {
            expected: layout.slot_size,
            got: out.len(),
        });
    }
    if is_empty(layout, mem) {
        return Err(QueueError::WouldBlock);
    }
    let read = mem.read_u32(layout.read_index_off);
    let slot = read % layout.slot_count;
    mem.read_bytes(layout.slot_off(slot), out);
    mem.write_u32(
        layout.read_index_off,
        next_index(read, layout.index_modulus()),
    );
    Ok(())
}
