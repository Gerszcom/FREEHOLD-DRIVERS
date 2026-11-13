//! Stage 5 groundwork — the GSP RPC request/response layer.
//!
//! [`crate::frame`] turns a ring of fixed-size slots into atomic,
//! variable-length *messages*. But the bytes of one of those messages are still
//! opaque to the framer: it carries them, checksums them and hands them over,
//! without knowing what a message *means*. This module is the next layer up — it
//! gives those bytes a meaning. The GSP conversation is a remote-procedure call:
//! the host posts a **request** (a function selector plus its argument bytes) on
//! the command queue, the GSP runs it and posts a **reply** carrying a status
//! code back on the status queue, and the GSP may also post unsolicited
//! **events** that answer no request. This module is the envelope those three
//! kinds share, and the bookkeeping that matches a reply back to the request it
//! answers.
//!
//! ## The envelope
//!
//! Each framed message begins with a small fixed header — a *kind*
//! (request / reply / event), a *function* selector, a *request id* that ties a
//! reply to its request, and a *status* word the GSP fills on a reply — followed
//! by the call's argument or result bytes. Where those four words sit inside the
//! message, and how many leading bytes the header reserves before the arguments,
//! are carried as data in an [`RpcLayout`], exactly as the ring's offsets live
//! in a [`crate::msgq::QueueLayout`] and the frame header's in a
//! [`crate::frame::FrameLayout`]: the *structure* of an RPC is FREEHOLD's own
//! code, and the *concrete offsets and function numbers* the GSP firmware
//! dictates enter as data, to be confirmed on hardware (`validation/stage-5/`).
//!
//! The three kinds are encoded as `1`, `2`, `3` rather than from zero, so a
//! message whose header area happens to be zeroed never decodes as a valid kind:
//! a blank slot is rejected, not mistaken for a request.
//!
//! ## Correlating replies with requests
//!
//! A request and the reply that answers it travel on *different* rings (command
//! out, status back), so the framer's own per-ring sequence number cannot tie
//! them together — the host chooses a **request id** for each call, the GSP
//! echoes it in the reply, and the host matches the two. [`Outstanding`] is the
//! fixed-capacity set of request ids a host has sent and not yet seen answered:
//! [`send a request`](RpcEndpoint::send_request) remembers its id, a returning
//! reply [`resolves`](Outstanding::resolve) it, and anything on the status queue
//! whose id is *not* outstanding is an event (or a stray), told apart from a real
//! reply without a heap or a lock.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Writing a header in front of a byte run, reading it back, and matching an
//! echoed id against a small set of outstanding ones are all *ordinary computer
//! science* — correct or incorrect on any machine. So the envelope and the
//! correlation are written and proven here against
//! [`crate::mock::MockSharedMem`] with no pending-hardware caveat, like the ring
//! transport, the framing and the SHA-256 beneath them.
//!
//! What this module does **not** fix — because the GSP firmware dictates it and
//! it cannot be confirmed without a card — is the envelope's concrete field
//! offsets and the function numbers of the real RPCs; both enter as data (an
//! [`RpcLayout`], and the `function` values a caller passes), never as lifted
//! code. Clean-room: the envelope and the bookkeeping are FREEHOLD's own; no
//! driver source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: an [`RpcEndpoint`] owns no
//! buffer — only the two framers and the layout — and every call carries its
//! bytes in a caller-provided slice, so the same logic runs on the host and, one
//! day, over a real mapped region.

use crate::frame::{FrameError, FrameHeader, Framer};
use crate::msgq::SharedMem;

/// Which of the three kinds of message an RPC envelope carries.
///
/// Encoded on the wire as `1` / `2` / `3` (not from zero) so a zeroed header
/// decodes as [`RpcError::BadKind`] rather than a spurious request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcKind {
    /// Host → GSP: run the function with these argument bytes.
    Request,
    /// GSP → host: the result of a request, carrying its status word and echoing
    /// the request id it answers.
    Reply,
    /// GSP → host: an unsolicited notification, answering no request.
    Event,
}

impl RpcKind {
    /// The wire value for this kind.
    pub fn to_u32(self) -> u32 {
        match self {
            RpcKind::Request => 1,
            RpcKind::Reply => 2,
            RpcKind::Event => 3,
        }
    }

    /// The kind a wire value names, or `None` if it names none.
    pub fn from_u32(value: u32) -> Option<RpcKind> {
        match value {
            1 => Some(RpcKind::Request),
            2 => Some(RpcKind::Reply),
            3 => Some(RpcKind::Event),
            _ => None,
        }
    }
}

/// Where an RPC envelope's header words sit within a framed message, and how
/// many leading bytes the header reserves before the arguments.
///
/// All four are byte offsets of a little-endian `u32` at the start of the
/// message payload (the bytes [`crate::frame::Framer`] carries), and
/// `header_len` is where the argument bytes begin. The four words must each fit
/// within `header_len` and not overlap one another; [`RpcLayout::validate`]
/// checks this.
///
/// Like [`crate::frame::FrameLayout`], these offsets are data: the concrete
/// values for a real GSP come from NVIDIA's published RPC message format and are
/// confirmed on hardware (`validation/stage-5/`). This type names the fields so
/// the codec below never bakes an offset in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcLayout {
    /// Byte offset of the kind word (see [`RpcKind`]).
    pub kind_off: usize,
    /// Byte offset of the function-selector word.
    pub function_off: usize,
    /// Byte offset of the request-id word (ties a reply to its request).
    pub request_id_off: usize,
    /// Byte offset of the status word (the GSP's result code on a reply).
    pub status_off: usize,
    /// How many leading bytes of the message the header occupies; arguments
    /// begin here.
    pub header_len: usize,
}

impl RpcLayout {
    /// Check that the four header words each lie wholly within `header_len` and
    /// none overlaps another.
    ///
    /// Every other method assumes a validated layout, so a caller validates once
    /// when it builds the channel and then reuses it freely.
    pub fn validate(&self) -> Result<(), RpcError> {
        let offs = [
            self.kind_off,
            self.function_off,
            self.request_id_off,
            self.status_off,
        ];
        for &a in &offs {
            match a.checked_add(4) {
                Some(end) if end <= self.header_len => {}
                _ => return Err(RpcError::BadLayout),
            }
        }
        // No two words may overlap. Every offset is now `<= header_len - 4`, so
        // `+ 4` cannot overflow.
        for (i, &a) in offs.iter().enumerate() {
            for &b in &offs[i + 1..] {
                if a < b + 4 && b < a + 4 {
                    return Err(RpcError::BadLayout);
                }
            }
        }
        Ok(())
    }

    /// Write an envelope into `buf`: the header for `hdr`, then `args`.
    ///
    /// The header region (`buf[..header_len]`) is zeroed first so bytes between
    /// the four words are deterministic, then each word is written at its offset
    /// and the arguments are copied after the header. Returns the total number
    /// of bytes written (`header_len + args.len()`), which is what a caller
    /// hands [`crate::frame::Framer::send`]. Fails with
    /// [`RpcError::BufferTooSmall`] if `buf` cannot hold the whole envelope.
    pub fn encode(&self, buf: &mut [u8], hdr: &RpcHeader, args: &[u8]) -> Result<usize, RpcError> {
        self.validate()?;
        let total = self.header_len + args.len();
        if buf.len() < total {
            return Err(RpcError::BufferTooSmall {
                needed: total,
                got: buf.len(),
            });
        }
        for b in &mut buf[..self.header_len] {
            *b = 0;
        }
        write_u32_le(buf, self.kind_off, hdr.kind.to_u32());
        write_u32_le(buf, self.function_off, hdr.function);
        write_u32_le(buf, self.request_id_off, hdr.request_id);
        write_u32_le(buf, self.status_off, hdr.status);
        buf[self.header_len..total].copy_from_slice(args);
        Ok(total)
    }

    /// Read the envelope header from the front `len` bytes of a message.
    ///
    /// `len` is the payload length [`crate::frame::Framer::receive`] reported; it
    /// must be at least `header_len`, and `buf` must hold at least that many
    /// bytes. The caller reads the arguments from `buf[header_len..len]`. Fails
    /// with [`RpcError::ShortMessage`] if the message is shorter than a header,
    /// or [`RpcError::BadKind`] if the kind word names no kind.
    pub fn decode(&self, buf: &[u8], len: usize) -> Result<RpcHeader, RpcError> {
        self.validate()?;
        if len < self.header_len || buf.len() < self.header_len {
            return Err(RpcError::ShortMessage {
                len,
                header_len: self.header_len,
            });
        }
        let kind_raw = read_u32_le(buf, self.kind_off);
        let kind = RpcKind::from_u32(kind_raw).ok_or(RpcError::BadKind(kind_raw))?;
        Ok(RpcHeader {
            kind,
            function: read_u32_le(buf, self.function_off),
            request_id: read_u32_le(buf, self.request_id_off),
            status: read_u32_le(buf, self.status_off),
        })
    }
}

/// The decoded header of one RPC message: what [`RpcLayout::encode`] wrote, and
/// what [`RpcLayout::decode`] read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcHeader {
    /// Request, reply or event.
    pub kind: RpcKind,
    /// The function selector.
    pub function: u32,
    /// The request id (set by the host on a request, echoed by the GSP on its
    /// reply; unused — zero by convention — on an event).
    pub request_id: u32,
    /// The status/result word (meaningful on a reply; zero on a request or
    /// event by convention).
    pub status: u32,
}

/// A message read off a ring: its decoded [`RpcHeader`] and the payload length,
/// so the caller can slice the arguments out of the buffer it supplied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcInbound {
    /// The decoded envelope header.
    pub header: RpcHeader,
    /// Total payload length in bytes; the arguments are the output buffer's
    /// `[header_len..len]`.
    pub len: usize,
}

impl RpcInbound {
    /// The byte range of the arguments within the output buffer: `header_len`
    /// up to the payload length.
    pub fn args_range(&self, layout: &RpcLayout) -> core::ops::Range<usize> {
        layout.header_len..self.len
    }
}

/// Why an RPC operation could not be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcError {
    /// The [`RpcLayout`] is not internally consistent: a header word that would
    /// not fit `header_len`, or two words that overlap. Caught by
    /// [`RpcLayout::validate`] before any access.
    BadLayout,
    /// The underlying framing or ring transport could not carry the message.
    /// Carries the [`FrameError`] verbatim (`TooLarge`, `WouldBlock`,
    /// `ChecksumMismatch`, …).
    Frame(FrameError),
    /// The buffer handed to [`RpcLayout::encode`] is smaller than the whole
    /// envelope (`header_len + args.len()`).
    BufferTooSmall {
        /// Bytes the envelope needs.
        needed: usize,
        /// Bytes the buffer has.
        got: usize,
    },
    /// A received message is shorter than the envelope header, so it cannot be
    /// an RPC at all. Signals corruption or a wrong layout.
    ShortMessage {
        /// The payload length received.
        len: usize,
        /// The header length the layout expects.
        header_len: usize,
    },
    /// The kind word of a received message names no [`RpcKind`].
    BadKind(u32),
    /// [`Outstanding::remember`] was called but the tracker is already at
    /// capacity. The host must consume some replies before issuing more
    /// concurrent requests.
    TrackerFull,
    /// [`Outstanding::remember`] was given a request id already outstanding. A
    /// caller bug — request ids in flight must be distinct.
    AlreadyPending(u32),
    /// A typed call's arguments could not be encoded into, or decoded out of,
    /// their byte run: a request that does not fit its buffer, a reply shorter
    /// than its declared shape, or trailing bytes a strict decoder rejects.
    /// Carries the [`crate::codec::CodecError`] verbatim. Raised by the typed
    /// call layer ([`crate::call`]); the raw envelope path never produces it.
    Codec(crate::codec::CodecError),
}

impl From<FrameError> for RpcError {
    fn from(e: FrameError) -> Self {
        RpcError::Frame(e)
    }
}

impl From<crate::codec::CodecError> for RpcError {
    fn from(e: crate::codec::CodecError) -> Self {
        RpcError::Codec(e)
    }
}

/// Read a little-endian `u32` at byte offset `off` in `buf` (bounds already
/// established by the layout).
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Write `value` as a little-endian `u32` at byte offset `off` in `buf`.
fn write_u32_le(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

/// A fixed-capacity set of request ids a host has sent and not yet seen
/// answered.
///
/// `N` is the most requests that may be in flight at once. Allocation-free: the
/// ids live in a stack array, and membership is a linear scan — `N` is small (a
/// GSP driver keeps only a handful of calls outstanding), so a set is the right
/// shape, not a map. A host registers each request it sends, resolves the one a
/// reply answers, and asks whether an arriving id is one it is waiting on; an id
/// that is not is an event or a stray.
#[derive(Clone, Debug)]
pub struct Outstanding<const N: usize> {
    slots: [Option<u32>; N],
    len: usize,
}

impl<const N: usize> Default for Outstanding<N> {
    fn default() -> Self {
        Outstanding {
            slots: [None; N],
            len: 0,
        }
    }
}

impl<const N: usize> Outstanding<N> {
    /// An empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// The most ids that can be outstanding at once.
    pub fn capacity(&self) -> usize {
        N
    }

    /// How many ids are outstanding now.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no id is outstanding.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the tracker is at capacity.
    pub fn is_full(&self) -> bool {
        self.len == N
    }

    /// Whether `id` is currently outstanding.
    pub fn contains(&self, id: u32) -> bool {
        self.slots.contains(&Some(id))
    }

    /// Record `id` as outstanding.
    ///
    /// Fails with [`RpcError::AlreadyPending`] if it already is, or
    /// [`RpcError::TrackerFull`] if the tracker is at capacity.
    pub fn remember(&mut self, id: u32) -> Result<(), RpcError> {
        if self.contains(id) {
            return Err(RpcError::AlreadyPending(id));
        }
        for slot in &mut self.slots {
            if slot.is_none() {
                *slot = Some(id);
                self.len += 1;
                return Ok(());
            }
        }
        Err(RpcError::TrackerFull)
    }

    /// Clear `id` if it was outstanding, returning whether it was.
    ///
    /// A host calls this with the request id a reply echoes; `false` means the
    /// reply answers no request this tracker knows — a duplicate, a stray, or a
    /// reply to a call already resolved.
    pub fn resolve(&mut self, id: u32) -> bool {
        for slot in &mut self.slots {
            if *slot == Some(id) {
                *slot = None;
                self.len -= 1;
                return true;
            }
        }
        false
    }
}

/// An RPC endpoint over one command/status ring pair.
///
/// `command` is the ring the host posts requests on and the GSP drains;
/// `status` is the ring the GSP posts replies and events on and the host drains.
/// The endpoint owns no memory — only the two framers and the layout — so it is
/// `Copy`, and one instance drives both halves of a host test over one shared
/// region, as a real driver and the firmware share one pair of rings with one
/// agreed layout.
///
/// The host uses [`send_request`](Self::send_request) and
/// [`poll_response`](Self::poll_response); the GSP side (a host test's stand-in,
/// and conceptually a FREEHOLD GSP emulator) uses
/// [`recv_request`](Self::recv_request), [`send_reply`](Self::send_reply) and
/// [`send_event`](Self::send_event). All share the one envelope codec.
#[derive(Clone, Copy)]
pub struct RpcEndpoint {
    /// The ring host requests travel on (host → GSP).
    pub command: Framer,
    /// The ring GSP replies and events travel on (GSP → host).
    pub status: Framer,
    /// The envelope layout both ends agree on.
    pub layout: RpcLayout,
}

impl RpcEndpoint {
    /// Build an endpoint over the command and status framers and the envelope
    /// layout.
    pub fn new(command: Framer, status: Framer, layout: RpcLayout) -> Self {
        RpcEndpoint {
            command,
            status,
            layout,
        }
    }

    /// Check that both framers and the envelope layout are usable.
    pub fn validate(&self) -> Result<(), RpcError> {
        self.command.validate()?;
        self.status.validate()?;
        self.layout.validate()
    }

    /// Post a request on the command ring: run `function` with `args`, tagged
    /// with `request_id` for the reply to echo.
    ///
    /// `scratch` is a caller-provided buffer the envelope is assembled in; it
    /// must hold `header_len + args.len()` bytes. `seq` is the transport
    /// sequence number stamped on the frame (the framer's own per-ring tag,
    /// independent of `request_id`). Returns the [`FrameHeader`] the frame was
    /// sent with. The caller normally also [`remember`](Outstanding::remember)s
    /// `request_id` in its [`Outstanding`] set so the reply can be matched.
    pub fn send_request<M: SharedMem>(
        &self,
        mem: &mut M,
        scratch: &mut [u8],
        seq: u32,
        request_id: u32,
        function: u32,
        args: &[u8],
    ) -> Result<FrameHeader, RpcError> {
        let hdr = RpcHeader {
            kind: RpcKind::Request,
            function,
            request_id,
            status: 0,
        };
        let total = self.layout.encode(scratch, &hdr, args)?;
        Ok(self.command.send(mem, seq, &scratch[..total])?)
    }

    /// Post a reply on the status ring (the GSP side), answering `request`.
    ///
    /// The reply echoes the request's function and request id — so the host
    /// matches it with [`Outstanding::resolve`] — and carries the GSP's result
    /// code `status` and result bytes `args`. `request` is the header a
    /// [`recv_request`](Self::recv_request) decoded.
    pub fn send_reply<M: SharedMem>(
        &self,
        mem: &mut M,
        scratch: &mut [u8],
        seq: u32,
        request: &RpcHeader,
        status: u32,
        args: &[u8],
    ) -> Result<FrameHeader, RpcError> {
        let hdr = RpcHeader {
            kind: RpcKind::Reply,
            function: request.function,
            request_id: request.request_id,
            status,
        };
        let total = self.layout.encode(scratch, &hdr, args)?;
        Ok(self.status.send(mem, seq, &scratch[..total])?)
    }

    /// Post an unsolicited event on the status ring (the GSP side): a
    /// notification of `function` with `args`, answering no request (its request
    /// id is zero).
    pub fn send_event<M: SharedMem>(
        &self,
        mem: &mut M,
        scratch: &mut [u8],
        seq: u32,
        function: u32,
        args: &[u8],
    ) -> Result<FrameHeader, RpcError> {
        let hdr = RpcHeader {
            kind: RpcKind::Event,
            function,
            request_id: 0,
            status: 0,
        };
        let total = self.layout.encode(scratch, &hdr, args)?;
        Ok(self.status.send(mem, seq, &scratch[..total])?)
    }

    /// Take the next request off the command ring (the GSP side), decoding its
    /// envelope into `out`.
    ///
    /// Returns `Ok(None)` when the ring is empty, `Ok(Some(_))` with the decoded
    /// header and length (arguments in `out[header_len..len]`) otherwise. A frame
    /// error — a half-arrived message, a too-small `out`, a checksum
    /// mismatch — surfaces as [`RpcError::Frame`], leaving the message in place.
    pub fn recv_request<M: SharedMem>(
        &self,
        mem: &mut M,
        out: &mut [u8],
    ) -> Result<Option<RpcInbound>, RpcError> {
        poll(&self.command, &self.layout, mem, out)
    }

    /// Take the next reply or event off the status ring (the host side),
    /// decoding its envelope into `out`.
    ///
    /// Returns `Ok(None)` when the ring is empty. On a message, inspect the
    /// returned [`RpcHeader::kind`]: a [`Reply`](RpcKind::Reply) whose
    /// `request_id` is [`Outstanding::resolve`]d answers a pending request; an
    /// [`Event`](RpcKind::Event), or a reply whose id is not outstanding, is
    /// unsolicited.
    pub fn poll_response<M: SharedMem>(
        &self,
        mem: &mut M,
        out: &mut [u8],
    ) -> Result<Option<RpcInbound>, RpcError> {
        poll(&self.status, &self.layout, mem, out)
    }
}

/// Receive one framed message off `framer`, decoding its RPC envelope.
///
/// `Ok(None)` on an empty ring; the message's bytes land in `out` and the
/// decoded header and length come back in the [`RpcInbound`]. Any frame-level
/// failure other than "empty" is returned as [`RpcError::Frame`] with the
/// message left in the ring.
fn poll<M: SharedMem>(
    framer: &Framer,
    layout: &RpcLayout,
    mem: &mut M,
    out: &mut [u8],
) -> Result<Option<RpcInbound>, RpcError> {
    match framer.receive(mem, out) {
        Ok(fh) => {
            let header = layout.decode(out, fh.len as usize)?;
            Ok(Some(RpcInbound {
                header,
                len: fh.len as usize,
            }))
        }
        Err(FrameError::Empty) => Ok(None),
        Err(e) => Err(RpcError::Frame(e)),
    }
}
