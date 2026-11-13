//! Stage 5 groundwork — large-message segmentation over the GSP framing.
//!
//! [`crate::frame`] turns a ring of fixed-size slots into atomic,
//! variable-length *messages*, but a single framed message is bounded: it can
//! be no larger than the whole ring can hold at once
//! ([`Framer::capacity`](crate::frame::Framer::capacity)) — a payload past that
//! is refused outright with [`FrameError::TooLarge`]. A real GSP conversation
//! does not stay inside that bound. Some requests and replies — an allocation
//! descriptor, a large control block — are bigger than any one message, and are
//! carried as an *ordered sequence* of messages the far side stitches back
//! together. This module is that layer: it splits a logical payload into chunks,
//! sends each chunk as one ordinary frame tagged with a small segment header,
//! and reassembles them in order on receive.
//!
//! ## The segment header
//!
//! Each frame the segmenter sends begins with a fixed [`SEG_HEADER_LEN`]-byte
//! header, little-endian, then that segment's slice of the payload:
//!
//! * a **message id** tying the segments of one logical message together, so a
//!   reassembler can tell one message's segments from the next's;
//! * the segment's **index** within the message (`0..count`);
//! * the total **count** of segments the message is split into; and
//! * the message's total **length** in bytes.
//!
//! The header is FREEHOLD's own and is encoded with the crate's own typed
//! [`codec`](crate::codec), so — unlike the ring, frame and RPC layouts, whose
//! concrete offsets the GSP firmware dictates — it needs no hardware
//! confirmation: this layer is defined wholly above the framer. The chunking and
//! the reassembly are supplied by FREEHOLD; the only hardware-dependent part is
//! the framer's geometry beneath, settled already.
//!
//! ## Back-pressure and resumption
//!
//! A message spanning more segments than the ring has free slots cannot be
//! pushed all at once — the point of segmenting is precisely to exceed the
//! ring's one-message bound. So [`Segmenter::send`] is *resumable*: it sends as
//! many segments as fit right now, starting at a caller-supplied index, and
//! returns the next index still to send. The caller drives it in a loop,
//! letting the consumer drain the ring between calls, until
//! [`SendProgress::is_done`]. No state lives in the segmenter — the resume
//! cursor is a plain `u32` the caller carries — matching the stateless framer
//! beneath it.
//!
//! Reassembly is the mirror: a [`Reassembly`] accumulator (a few counters,
//! owning no buffer) is advanced one frame at a time by [`Segmenter::push`],
//! which copies each segment into the caller's output buffer at its offset and
//! reports [`RecvStep::Complete`] when the final segment lands.
//!
//! ## Why this is host-verifiable
//!
//! Splitting a byte run into numbered chunks, carrying them over a FIFO, and
//! putting them back together in order is *ordinary computer science* — correct
//! or incorrect on any machine. So, like the ring transport, the framing, the
//! codec and the SHA-256 beneath it, this module is written and proven here
//! against [`crate::mock::MockSharedMem`] with no pending-hardware caveat.
//! Clean-room: the segment header and the chunking are FREEHOLD's own; no driver
//! source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the segmenter owns only a
//! [`Framer`], and every buffer is the caller's.

use crate::codec::{Reader, Writer};
use crate::frame::{FrameError, Framer};
use crate::msgq::SharedMem;

/// The fixed size, in bytes, of the per-segment header every frame carries:
/// four little-endian `u32` words (message id, index, count, total length).
pub const SEG_HEADER_LEN: usize = 16;

/// The header prefixed to each frame of a segmented message.
///
/// What [`Segmenter::send`] writes in front of a segment's payload and what
/// [`Segmenter::push`] reads back to place and order it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegHeader {
    /// Identifier shared by every segment of one logical message.
    pub msg_id: u32,
    /// This segment's position within the message, `0..count`.
    pub index: u32,
    /// How many segments the message is split into (at least one).
    pub count: u32,
    /// The whole message's length in bytes.
    pub total_len: u32,
}

impl SegHeader {
    /// Encode the header into the first [`SEG_HEADER_LEN`] bytes of `out`.
    fn encode(&self, out: &mut [u8]) -> Result<(), SegError> {
        let mut w = Writer::new(out);
        w.put_u32(self.msg_id)
            .map_err(|_| SegError::ScratchTooSmall {
                needed: SEG_HEADER_LEN,
                got: 0,
            })?;
        w.put_u32(self.index)
            .map_err(|_| SegError::ScratchTooSmall {
                needed: SEG_HEADER_LEN,
                got: 0,
            })?;
        w.put_u32(self.count)
            .map_err(|_| SegError::ScratchTooSmall {
                needed: SEG_HEADER_LEN,
                got: 0,
            })?;
        w.put_u32(self.total_len)
            .map_err(|_| SegError::ScratchTooSmall {
                needed: SEG_HEADER_LEN,
                got: 0,
            })?;
        Ok(())
    }

    /// Decode a header from the leading bytes of a received frame payload.
    fn decode(buf: &[u8]) -> Result<SegHeader, SegError> {
        let mut r = Reader::new(buf);
        let msg_id = r.get_u32().map_err(|_| SegError::BadSegment)?;
        let index = r.get_u32().map_err(|_| SegError::BadSegment)?;
        let count = r.get_u32().map_err(|_| SegError::BadSegment)?;
        let total_len = r.get_u32().map_err(|_| SegError::BadSegment)?;
        Ok(SegHeader {
            msg_id,
            index,
            count,
            total_len,
        })
    }
}

/// Why a segmentation operation could not be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegError {
    /// The underlying [`Framer`] is unusable, or its capacity leaves no room for
    /// payload after the segment header (`capacity <= SEG_HEADER_LEN`).
    BadLayout,
    /// The payload is larger than a segmented message can describe: its length
    /// does not fit the header's 32-bit total-length field.
    TooLarge {
        /// The payload length that overflowed.
        len: usize,
    },
    /// An underlying framing error that is not back-pressure — a bad header, a
    /// failed checksum, a corrupt frame. Carried through unchanged.
    Frame(FrameError),
    /// A received segment is not internally consistent, or does not belong to
    /// the message being reassembled: a zero count, an index past the count, a
    /// count that disagrees with the total length, a chunk of the wrong size, or
    /// a segment whose id, count, length or position does not continue the
    /// message in progress.
    BadSegment,
    /// The caller's reassembly buffer is smaller than the message's total
    /// length. The segment has been consumed and the accumulator is left
    /// untouched; the message cannot be reassembled into a buffer this small, so
    /// the caller sizes `out` to the largest message it expects.
    OutputTooSmall {
        /// Bytes the message needs.
        needed: usize,
        /// Bytes the buffer has.
        got: usize,
    },
    /// The caller's scratch buffer is smaller than one frame's worth of payload
    /// ([`Segmenter::scratch_len`]). Nothing is sent or consumed.
    ScratchTooSmall {
        /// Bytes the scratch needs.
        needed: usize,
        /// Bytes the scratch has.
        got: usize,
    },
}

/// How far [`Segmenter::send`] got: the next segment index still to send and the
/// total number of segments the message spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendProgress {
    /// The index of the next segment still to send. Equal to `count` once the
    /// whole message has been posted.
    pub next_index: u32,
    /// How many segments the message is split into.
    pub count: u32,
}

impl SendProgress {
    /// Whether every segment of the message has been posted.
    pub fn is_done(&self) -> bool {
        self.next_index >= self.count
    }
}

/// One step of reassembly from [`Segmenter::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvStep {
    /// Nothing was consumed: the status ring is empty, or the next frame has not
    /// fully arrived yet. The caller retries once more has been posted.
    Blocked,
    /// A non-final segment was consumed and copied into the output buffer; more
    /// are still expected.
    More {
        /// The index of the segment just consumed.
        index: u32,
    },
    /// The final segment landed: the whole message now sits in `out[..len]` and
    /// the accumulator has reset, ready for the next message.
    Complete {
        /// The reassembled message's length in bytes.
        len: usize,
    },
}

/// The accumulator a reader advances across the frames of one segmented message.
///
/// Holds only counters — no buffer — so it is `Copy` and the reassembly storage
/// stays the caller's. A fresh one ([`Reassembly::new`]) expects segment `0` of
/// some message; each [`Segmenter::push`] advances it, and it resets itself when
/// the final segment lands so the same accumulator serves the next message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Reassembly {
    /// Whether a message is in progress (segment `0` seen, final not yet).
    started: bool,
    /// The id of the message in progress.
    msg_id: u32,
    /// The index the next segment must carry.
    next_index: u32,
    /// The segment count the message in progress declared.
    count: u32,
    /// The total length the message in progress declared.
    total_len: u32,
}

impl Reassembly {
    /// A fresh accumulator, ready for segment `0` of a new message.
    pub fn new() -> Self {
        Reassembly::default()
    }

    /// Whether a message is partly reassembled (between its first and final
    /// segments).
    pub fn in_progress(&self) -> bool {
        self.started
    }

    /// Abandon a message in progress and return to the initial state. The escape
    /// hatch after a [`SegError::BadSegment`]: the caller drops what it had and
    /// waits for a fresh segment `0`.
    pub fn reset(&mut self) {
        *self = Reassembly::default();
    }
}

/// The segmentation layer over one [`Framer`]: splits a large payload into an
/// ordered run of frames and reassembles them.
///
/// Owns no memory — only the framer's description — so it is `Copy` and the one
/// instance drives both ends over a shared region in a host test, exactly as the
/// framer beneath it does.
#[derive(Clone, Copy)]
pub struct Segmenter {
    /// The framing layer segments are carried over.
    pub framer: Framer,
}

impl Segmenter {
    /// Build a segmenter over `framer`.
    pub fn new(framer: Framer) -> Self {
        Segmenter { framer }
    }

    /// Check the framer is usable and leaves room for payload after a segment
    /// header. Every other method assumes a validated segmenter.
    pub fn validate(&self) -> Result<(), SegError> {
        self.framer.validate().map_err(SegError::Frame)?;
        if self.framer.capacity() <= SEG_HEADER_LEN {
            return Err(SegError::BadLayout);
        }
        Ok(())
    }

    /// The most payload bytes one segment can carry: a frame's capacity less the
    /// segment header.
    pub fn max_chunk(&self) -> usize {
        self.framer.capacity() - SEG_HEADER_LEN
    }

    /// The scratch buffer size [`send`](Self::send) and [`push`](Self::push)
    /// require: a full frame's payload (segment header plus one maximum chunk).
    pub fn scratch_len(&self) -> usize {
        self.framer.capacity()
    }

    /// How many segments a message of `total_len` bytes is split into. A
    /// zero-length message is one (empty) segment.
    pub fn segment_count(&self, total_len: usize) -> u32 {
        if total_len == 0 {
            return 1;
        }
        total_len.div_ceil(self.max_chunk()) as u32
    }

    /// The payload length of segment `index` of a `total_len`-byte, `count`-part
    /// message: a full chunk for every segment but the last, the remainder for
    /// the last.
    fn chunk_len(&self, total_len: usize, index: u32, count: u32) -> usize {
        let max = self.max_chunk();
        if index + 1 < count {
            max
        } else {
            total_len - (count as usize - 1) * max
        }
    }

    /// Post the segments of `payload` under message id `msg_id`, starting at
    /// `from_index`, stopping when the ring fills.
    ///
    /// Returns the [`SendProgress`] to resume from: `next_index` is the first
    /// segment *not* yet posted (the message is done when
    /// [`SendProgress::is_done`]). A full ring stops the run without error — the
    /// caller lets the consumer drain and calls again with the returned index.
    /// Any framing error other than back-pressure aborts with
    /// [`SegError::Frame`], having posted the segments before it (the ring is a
    /// FIFO, so a partial message is simply continued on the next call). `scratch`
    /// must be at least [`scratch_len`](Self::scratch_len) bytes.
    pub fn send<M: SharedMem>(
        &self,
        mem: &mut M,
        msg_id: u32,
        payload: &[u8],
        from_index: u32,
        scratch: &mut [u8],
    ) -> Result<SendProgress, SegError> {
        self.validate()?;
        if scratch.len() < self.scratch_len() {
            return Err(SegError::ScratchTooSmall {
                needed: self.scratch_len(),
                got: scratch.len(),
            });
        }
        if payload.len() > u32::MAX as usize {
            return Err(SegError::TooLarge { len: payload.len() });
        }
        let total_len = payload.len();
        let count = self.segment_count(total_len);
        let max = self.max_chunk();

        let mut index = from_index;
        while index < count {
            let start = index as usize * max;
            let clen = self.chunk_len(total_len, index, count);
            let header = SegHeader {
                msg_id,
                index,
                count,
                total_len: total_len as u32,
            };
            header.encode(scratch)?;
            scratch[SEG_HEADER_LEN..SEG_HEADER_LEN + clen]
                .copy_from_slice(&payload[start..start + clen]);
            match self
                .framer
                .send(mem, index, &scratch[..SEG_HEADER_LEN + clen])
            {
                Ok(_) => index += 1,
                Err(FrameError::WouldBlock { .. }) => break,
                Err(e) => return Err(SegError::Frame(e)),
            }
        }
        Ok(SendProgress {
            next_index: index,
            count,
        })
    }

    /// Consume the next frame of a segmented message, advancing `re` and copying
    /// the segment into `out`.
    ///
    /// On the final segment the whole message sits in `out[..len]` and `re` has
    /// reset. An empty or not-yet-complete ring returns [`RecvStep::Blocked`]
    /// without consuming. A segment that does not continue the message — wrong
    /// id, count, length, order or chunk size — returns [`SegError::BadSegment`];
    /// the frame has been consumed, and the caller [`reset`](Reassembly::reset)s
    /// `re` to start over. `out` must be at least the message's total length (a
    /// shorter one yields [`SegError::OutputTooSmall`] with the segment consumed
    /// and `re` untouched) and `scratch` at least
    /// [`scratch_len`](Self::scratch_len) bytes.
    pub fn push<M: SharedMem>(
        &self,
        re: &mut Reassembly,
        mem: &mut M,
        out: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<RecvStep, SegError> {
        self.validate()?;
        if scratch.len() < self.scratch_len() {
            return Err(SegError::ScratchTooSmall {
                needed: self.scratch_len(),
                got: scratch.len(),
            });
        }

        // Take the next frame into scratch, mapping the "nothing here yet" errors
        // to Blocked and leaving the frame in place on every error.
        let fh = match self.framer.receive(mem, scratch) {
            Ok(h) => h,
            Err(FrameError::Empty) | Err(FrameError::WouldBlock { .. }) => {
                return Ok(RecvStep::Blocked)
            }
            Err(FrameError::OutputTooSmall { needed, got }) => {
                return Err(SegError::ScratchTooSmall { needed, got })
            }
            Err(e) => return Err(SegError::Frame(e)),
        };

        let flen = fh.len as usize;
        if flen < SEG_HEADER_LEN {
            return Err(SegError::BadSegment);
        }
        let hdr = SegHeader::decode(&scratch[..SEG_HEADER_LEN])?;
        let total_len = hdr.total_len as usize;

        // The segment must be self-consistent before it is matched to the run.
        if hdr.count == 0 || hdr.index >= hdr.count {
            return Err(SegError::BadSegment);
        }
        if self.segment_count(total_len) != hdr.count {
            return Err(SegError::BadSegment);
        }
        let want_chunk = self.chunk_len(total_len, hdr.index, hdr.count);
        if flen != SEG_HEADER_LEN + want_chunk {
            return Err(SegError::BadSegment);
        }

        // The buffer must hold the whole message. Checked before `re` is
        // touched, so a refusal leaves the accumulator exactly as it was.
        if out.len() < total_len {
            return Err(SegError::OutputTooSmall {
                needed: total_len,
                got: out.len(),
            });
        }

        // Match the segment to the message in progress (or start a new one).
        if re.started {
            if hdr.msg_id != re.msg_id
                || hdr.count != re.count
                || hdr.total_len != re.total_len
                || hdr.index != re.next_index
            {
                return Err(SegError::BadSegment);
            }
        } else {
            if hdr.index != 0 {
                return Err(SegError::BadSegment);
            }
            re.started = true;
            re.msg_id = hdr.msg_id;
            re.count = hdr.count;
            re.total_len = hdr.total_len;
            re.next_index = 0;
        }

        let start = hdr.index as usize * self.max_chunk();
        out[start..start + want_chunk].copy_from_slice(&scratch[SEG_HEADER_LEN..flen]);
        re.next_index += 1;

        if re.next_index == re.count {
            re.reset();
            Ok(RecvStep::Complete { len: total_len })
        } else {
            Ok(RecvStep::More { index: hdr.index })
        }
    }
}
