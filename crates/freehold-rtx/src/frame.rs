//! Stage 5 groundwork — GSP message framing over the ring transport.
//!
//! [`crate::msgq`] gives a single-producer / single-consumer ring of
//! *fixed-size* slots. The GSP conversation, though, is made of *variable-size*
//! messages: a mode-set request, an allocation reply, a status event — each a
//! run of bytes that may be shorter than one slot or longer than several. This
//! module is the layer directly above the ring that turns the fixed slots into
//! those variable messages: it splits a logical message across as many
//! consecutive slots as it needs, stamps the first slot with a small header
//! (sequence number, how many slots the message spans, its byte length and an
//! integrity checksum), and reassembles it on the far side — publishing and
//! consuming each message as one atomic unit so the other end never sees half
//! of one.
//!
//! ## What a frame looks like on the ring
//!
//! A message of `len` payload bytes occupies `elem_count` consecutive slots.
//! The first slot carries the [`FrameLayout`] header in its leading
//! `header_len` bytes, then the first `slot_size - header_len` payload bytes;
//! each further slot carries up to `slot_size` more payload bytes. The last
//! slot's unused tail is left as-is — the receiver copies out exactly `len`
//! bytes, so slop never matters. Continuation slots carry no header of their
//! own: the first slot's `elem_count` is the single source of truth for how far
//! the message reaches.
//!
//! The write index is advanced by the whole `elem_count` in one step *after*
//! every slot has been written, and the read index likewise only after a whole
//! message has been copied out and its checksum has verified. So a reader that
//! watches [`crate::msgq::occupancy`] sees it jump from "message absent" to
//! "message fully present" with no partial state in between — the same
//! write-slot-then-publish-index discipline the ring itself uses, lifted to a
//! multi-slot message.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Splitting a byte run across fixed cells and putting it back together,
//! counting the cells it needs, refusing one that will not fit, and checking a
//! sum over the bytes are all *ordinary computer science* — correct or
//! incorrect on any machine, with no dependence on a real card. So the framing
//! is written and proven here against [`crate::mock::MockSharedMem`] with no
//! pending-hardware caveat, exactly as [`crate::msgq`] and [`crate::sha256`]
//! are.
//!
//! Two things are deliberately *not* fixed here, because they are the GSP
//! firmware's to dictate and cannot be confirmed without a card:
//!
//! * **The header's concrete byte offsets.** Where the sequence, count, length
//!   and checksum words sit inside the first slot is carried as data in a
//!   [`FrameLayout`], not baked into the code — the same treatment
//!   [`crate::msgq::QueueLayout`] gives the ring's offsets. The real values come
//!   from NVIDIA's published GSP message-queue element format and are recorded
//!   and checked on hardware (`validation/stage-5/`).
//! * **The checksum function.** The firmware checks each message with a
//!   particular algorithm, part of the same published interface. A [`Framer`]
//!   therefore holds the checksum as a supplied `fn(&[u8]) -> u32`, so FREEHOLD
//!   plugs in the firmware's own when it is confirmed; [`checksum32`] is a
//!   reference sum this crate ships so the transport has a working end-to-end
//!   integrity check between two FREEHOLD endpoints and the host tests have a
//!   concrete function to exercise.
//!
//! Clean-room: the splitting, reassembly and accounting are FREEHOLD's own
//! code; no driver source is copied or translated. The only NVIDIA-specific
//! facts this path will ever need — the header offsets and the checksum
//! choice — enter as data and a supplied function, never as lifted code.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: a [`Framer`] owns no
//! buffer, only a description of the ring and the header; every message is
//! carried in caller-provided slices and the [`SharedMem`] region, and every
//! access is bounded by layouts validated up front.

use crate::msgq::{free_slots, occupancy, QueueLayout, SharedMem};

/// A reference 32-bit integrity checksum over a message's payload bytes.
///
/// It folds the bytes little-endian, four at a time, into a wrapping 32-bit
/// sum; a trailing group shorter than four bytes is zero-extended. The result
/// is order- and value-sensitive enough to catch the corruption a transport
/// cares about (a dropped, duplicated or flipped slot), while being trivial to
/// compute on either endpoint.
///
/// This is *FREEHOLD's* reference checksum, used by a [`Framer`] that is handed
/// no other. The GSP firmware verifies messages with its own documented
/// algorithm; when that is confirmed on hardware (`validation/stage-5/`), a
/// [`Framer`] is built with that function instead and this one stays the
/// default for FREEHOLD-to-FREEHOLD transport and for host tests. The framing
/// logic does not depend on *which* function is used — only that both ends use
/// the same one.
pub fn checksum32(bytes: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    let mut chunks = bytes.chunks_exact(4);
    for c in &mut chunks {
        sum = sum.wrapping_add(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut tail = [0u8; 4];
        tail[..rem.len()].copy_from_slice(rem);
        sum = sum.wrapping_add(u32::from_le_bytes(tail));
    }
    sum
}

/// Where a frame's header words sit within the first slot of a message.
///
/// All four are byte offsets of a little-endian `u32` inside the first slot,
/// and `header_len` is how many leading bytes of that slot the header reserves
/// (payload in the first slot begins at `header_len`). The four words must each
/// fit within `header_len` and not overlap one another; [`Framer::validate`]
/// checks this against the ring's `slot_size`.
///
/// Like [`QueueLayout`], these offsets are data: the concrete values for a real
/// GSP come from NVIDIA's published message-element format and are confirmed on
/// hardware (`validation/stage-5/`). This type names the fields so the framing
/// code below never bakes an offset in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLayout {
    /// Byte offset, within the first slot, of the sequence-number word.
    pub seq_off: usize,
    /// Byte offset, within the first slot, of the element-count word (how many
    /// slots the message spans, the first included).
    pub elem_count_off: usize,
    /// Byte offset, within the first slot, of the payload-length word (in
    /// bytes).
    pub length_off: usize,
    /// Byte offset, within the first slot, of the payload checksum word.
    pub checksum_off: usize,
    /// How many leading bytes of the first slot the header occupies; payload in
    /// the first slot begins here.
    pub header_len: usize,
}

/// The header carried by a frame: what [`Framer::send`] wrote, and what
/// [`Framer::receive`] read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// The sequence number the sender stamped on the message.
    pub seq: u32,
    /// The payload length in bytes.
    pub len: u32,
    /// How many slots the message spans (the first slot included).
    pub elem_count: u32,
    /// The checksum stored for the payload.
    pub checksum: u32,
}

/// Why a framing operation could not be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The [`QueueLayout`] or [`FrameLayout`] is not internally consistent: a
    /// bad ring layout, a header word that would not fit `header_len`, two
    /// header words that overlap, or a `header_len` larger than a slot. Caught
    /// by [`Framer::validate`] before any access.
    BadLayout,
    /// The payload is larger than the whole ring can carry in one message
    /// (`needed` slots against a ring of `capacity` slots). A hard limit of the
    /// queue's geometry, distinct from the transient [`WouldBlock`](Self::WouldBlock).
    TooLarge {
        /// How many slots the message needs.
        needed: u32,
        /// How many slots the ring has in total.
        capacity: u32,
    },
    /// The ring has room in principle but not enough free slots right now. The
    /// sender retries once the consumer has drained some (producer side), or
    /// the message has not fully arrived yet (consumer side). `have`/`need` say
    /// how many slots were free or present against how many the message spans.
    WouldBlock {
        /// Slots available (free, when sending; present, when receiving).
        have: u32,
        /// Slots the message needs.
        need: u32,
    },
    /// [`Framer::receive`] or [`Framer::drop_front`] was called on an empty
    /// queue.
    Empty,
    /// The header at the front of the queue is not self-consistent: a zero or
    /// out-of-range element count, a length larger than the ring can hold, or a
    /// count that does not match the length. Signals corruption or a wrong
    /// layout; the caller does not advance past it blindly.
    BadHeader,
    /// The caller's output buffer is smaller than the frame's payload. The
    /// frame is left in the queue so the caller can retry with a larger buffer.
    OutputTooSmall {
        /// Bytes the payload needs.
        needed: usize,
        /// Bytes the buffer has.
        got: usize,
    },
    /// The payload's checksum did not match the header's. The frame is left in
    /// the queue; a caller that wants to discard it calls
    /// [`Framer::drop_front`].
    ChecksumMismatch {
        /// The checksum the header carried.
        expected: u32,
        /// The checksum computed over the received payload.
        got: u32,
    },
}

/// The message-framing layer over one ring: a [`QueueLayout`], the
/// [`FrameLayout`] for the per-message header, and the checksum both ends use.
///
/// A `Framer` owns no memory — only this description — so it is `Copy` and the
/// same instance drives both a host test's "driver" and "GSP" halves over one
/// shared region, exactly as a real driver and the firmware share one ring with
/// one agreed layout.
#[derive(Clone, Copy)]
pub struct Framer {
    /// The ring this framer carries messages over.
    pub queue: QueueLayout,
    /// Where the per-message header sits in a message's first slot.
    pub frame: FrameLayout,
    /// The checksum both ends compute over a payload. See [`checksum32`].
    pub checksum: fn(&[u8]) -> u32,
}

impl Framer {
    /// Build a framer over `queue` and `frame` using the [`checksum32`]
    /// reference checksum.
    pub fn new(queue: QueueLayout, frame: FrameLayout) -> Self {
        Framer {
            queue,
            frame,
            checksum: checksum32,
        }
    }

    /// Build a framer with a caller-supplied checksum — the one to use once the
    /// GSP firmware's own algorithm is confirmed (`validation/stage-5/`).
    pub fn with_checksum(
        queue: QueueLayout,
        frame: FrameLayout,
        checksum: fn(&[u8]) -> u32,
    ) -> Self {
        Framer {
            queue,
            frame,
            checksum,
        }
    }

    /// Check that the ring and header layouts are usable together: the ring
    /// validates ([`QueueLayout::validate`]), the header fits inside one slot,
    /// and the four header words each fit `header_len` without overlapping.
    ///
    /// Every other method assumes a validated framer, so a caller validates
    /// once when it builds the channel and then reuses it freely.
    pub fn validate(&self) -> Result<(), FrameError> {
        self.queue.validate().map_err(|_| FrameError::BadLayout)?;
        let slot = self.queue.slot_size;
        if self.frame.header_len > slot {
            return Err(FrameError::BadLayout);
        }
        let offs = [
            self.frame.seq_off,
            self.frame.elem_count_off,
            self.frame.length_off,
            self.frame.checksum_off,
        ];
        // Each word must lie wholly within the reserved header.
        for &a in &offs {
            match a.checked_add(4) {
                Some(end) if end <= self.frame.header_len => {}
                _ => return Err(FrameError::BadLayout),
            }
        }
        // No two words may overlap: writing one would corrupt another. Every
        // offset is now known to be `<= header_len - 4`, so `+ 4` cannot
        // overflow.
        for (i, &a) in offs.iter().enumerate() {
            for &b in &offs[i + 1..] {
                if a < b + 4 && b < a + 4 {
                    return Err(FrameError::BadLayout);
                }
            }
        }
        Ok(())
    }

    /// How many payload bytes the first slot of a message carries (after its
    /// header): `slot_size - header_len`.
    pub fn first_slot_payload(&self) -> usize {
        self.queue.slot_size - self.frame.header_len
    }

    /// The most payload bytes a single message can carry: the first slot's
    /// payload plus a full slot for every other slot in the ring.
    pub fn capacity(&self) -> usize {
        self.first_slot_payload() + (self.queue.slot_count as usize - 1) * self.queue.slot_size
    }

    /// How many slots a payload of `len` bytes needs: one for the header and its
    /// first chunk, then one more per further `slot_size` bytes.
    pub fn slots_for(&self, len: usize) -> u32 {
        let first = self.first_slot_payload();
        if len <= first {
            return 1;
        }
        let rest = len - first;
        // ceil(rest / slot) further slots; `slot` is non-zero (validated).
        1 + rest.div_ceil(self.queue.slot_size) as u32
    }

    /// Byte offset of the slot `k` places after raw index `base`, wrapping
    /// within the ring. Uses `usize` so the sum never overflows regardless of
    /// the index space.
    fn slot_byte(&self, base: u32, k: usize) -> usize {
        let slot = (base as usize + k) % self.queue.slot_count as usize;
        self.queue.ring_off + slot * self.queue.slot_size
    }

    /// Advance a raw ring index by `n`, wrapping within `0..2*slot_count`.
    fn advance(&self, index: u32, n: u32) -> u32 {
        let modulus = self.queue.index_modulus() as u64;
        ((index as u64 + n as u64) % modulus) as u32
    }

    /// Post one message of `payload` bytes with sequence number `seq`.
    ///
    /// The message occupies [`slots_for`](Self::slots_for)`(payload.len())`
    /// consecutive slots. Every slot is written first, then the write index is
    /// advanced by the whole count, so a reader never observes a partial frame.
    /// Returns [`FrameError::TooLarge`] if the payload cannot fit the ring at
    /// all, [`FrameError::WouldBlock`] if it does not fit right now (the queue
    /// is left untouched in both cases).
    pub fn send<M: SharedMem>(
        &self,
        mem: &mut M,
        seq: u32,
        payload: &[u8],
    ) -> Result<FrameHeader, FrameError> {
        self.validate()?;
        let need = self.slots_for(payload.len());
        if need > self.queue.slot_count {
            return Err(FrameError::TooLarge {
                needed: need,
                capacity: self.queue.slot_count,
            });
        }
        let free = free_slots(&self.queue, mem);
        if free < need {
            return Err(FrameError::WouldBlock { have: free, need });
        }

        let checksum = (self.checksum)(payload);
        let write = mem.read_u32(self.queue.write_index_off);

        // First slot: header, then the first chunk of payload.
        let first_off = self.slot_byte(write, 0);
        mem.write_u32(first_off + self.frame.seq_off, seq);
        mem.write_u32(first_off + self.frame.elem_count_off, need);
        mem.write_u32(first_off + self.frame.length_off, payload.len() as u32);
        mem.write_u32(first_off + self.frame.checksum_off, checksum);

        let first_chunk = self.first_slot_payload().min(payload.len());
        if first_chunk > 0 {
            mem.write_bytes(first_off + self.frame.header_len, &payload[..first_chunk]);
        }

        // Continuation slots: pure payload, a slot at a time.
        let mut cursor = first_chunk;
        let mut k = 1usize;
        while cursor < payload.len() {
            let take = self.queue.slot_size.min(payload.len() - cursor);
            let off = self.slot_byte(write, k);
            mem.write_bytes(off, &payload[cursor..cursor + take]);
            cursor += take;
            k += 1;
        }

        // Publish: reveal every slot at once.
        mem.write_u32(self.queue.write_index_off, self.advance(write, need));

        Ok(FrameHeader {
            seq,
            len: payload.len() as u32,
            elem_count: need,
            checksum,
        })
    }

    /// Read the header of the message at the front of the queue without
    /// consuming it, validating that it is self-consistent.
    ///
    /// Returns [`FrameError::Empty`] if nothing is queued and
    /// [`FrameError::BadHeader`] if the front header is not internally
    /// consistent (bad count, over-long length, or a count that disagrees with
    /// the length).
    pub fn peek_header<M: SharedMem>(&self, mem: &M) -> Result<FrameHeader, FrameError> {
        self.validate()?;
        if occupancy(&self.queue, mem) == 0 {
            return Err(FrameError::Empty);
        }
        let read = mem.read_u32(self.queue.read_index_off);
        let off = self.slot_byte(read, 0);
        let header = FrameHeader {
            seq: mem.read_u32(off + self.frame.seq_off),
            elem_count: mem.read_u32(off + self.frame.elem_count_off),
            len: mem.read_u32(off + self.frame.length_off),
            checksum: mem.read_u32(off + self.frame.checksum_off),
        };
        self.check_header(&header)?;
        Ok(header)
    }

    /// Validate a header against the ring's geometry: non-zero, in-range count;
    /// length within capacity; and the count the length implies.
    fn check_header(&self, header: &FrameHeader) -> Result<(), FrameError> {
        if header.elem_count == 0 || header.elem_count > self.queue.slot_count {
            return Err(FrameError::BadHeader);
        }
        if header.len as usize > self.capacity() {
            return Err(FrameError::BadHeader);
        }
        if self.slots_for(header.len as usize) != header.elem_count {
            return Err(FrameError::BadHeader);
        }
        Ok(())
    }

    /// Take the message at the front of the queue into `out`.
    ///
    /// On success the payload is copied into `out[..len]`, its checksum is
    /// verified, the read index is advanced past the whole message, and the
    /// header is returned. On *any* error the queue is left exactly as it was,
    /// so the operation is safe to retry (with a larger buffer after
    /// [`OutputTooSmall`](FrameError::OutputTooSmall), once more slots arrive
    /// after [`WouldBlock`](FrameError::WouldBlock)). A frame whose checksum
    /// fails is likewise left in place; a caller that means to discard it calls
    /// [`drop_front`](Self::drop_front).
    pub fn receive<M: SharedMem>(
        &self,
        mem: &mut M,
        out: &mut [u8],
    ) -> Result<FrameHeader, FrameError> {
        let header = self.peek_header(mem)?;
        let present = occupancy(&self.queue, mem);
        if present < header.elem_count {
            return Err(FrameError::WouldBlock {
                have: present,
                need: header.elem_count,
            });
        }
        let len = header.len as usize;
        if out.len() < len {
            return Err(FrameError::OutputTooSmall {
                needed: len,
                got: out.len(),
            });
        }

        self.reassemble(mem, len, &mut out[..len]);

        let got = (self.checksum)(&out[..len]);
        if got != header.checksum {
            return Err(FrameError::ChecksumMismatch {
                expected: header.checksum,
                got,
            });
        }

        // Consume: release every slot at once.
        let read = mem.read_u32(self.queue.read_index_off);
        mem.write_u32(
            self.queue.read_index_off,
            self.advance(read, header.elem_count),
        );
        Ok(header)
    }

    /// Copy `len` payload bytes of the front message into `out` (which must be
    /// exactly `len` long), spanning the first slot's payload area and any
    /// continuation slots. Does not touch the read index.
    fn reassemble<M: SharedMem>(&self, mem: &M, len: usize, out: &mut [u8]) {
        let read = mem.read_u32(self.queue.read_index_off);
        let first = self.first_slot_payload().min(len);
        if first > 0 {
            let off = self.slot_byte(read, 0) + self.frame.header_len;
            mem.read_bytes(off, &mut out[..first]);
        }
        let mut cursor = first;
        let mut k = 1usize;
        while cursor < len {
            let take = self.queue.slot_size.min(len - cursor);
            let off = self.slot_byte(read, k);
            mem.read_bytes(off, &mut out[cursor..cursor + take]);
            cursor += take;
            k += 1;
        }
    }

    /// Discard the message at the front of the queue without reading its
    /// payload, advancing the read index past its slots.
    ///
    /// The escape hatch for a frame [`receive`](Self::receive) refused on its
    /// checksum, or one a caller otherwise wants to drop. The header must still
    /// parse ([`BadHeader`](FrameError::BadHeader) otherwise) and the whole
    /// message must be present ([`WouldBlock`](FrameError::WouldBlock)
    /// otherwise), so a half-arrived frame is never skipped.
    pub fn drop_front<M: SharedMem>(&self, mem: &mut M) -> Result<FrameHeader, FrameError> {
        let header = self.peek_header(mem)?;
        let present = occupancy(&self.queue, mem);
        if present < header.elem_count {
            return Err(FrameError::WouldBlock {
                have: present,
                need: header.elem_count,
            });
        }
        let read = mem.read_u32(self.queue.read_index_off);
        mem.write_u32(
            self.queue.read_index_off,
            self.advance(read, header.elem_count),
        );
        Ok(header)
    }
}
