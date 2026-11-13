//! Stage 5 groundwork — segmented RPC envelopes: an RPC message larger than one
//! ring frame.
//!
//! Two layers already stand over the framer and until now did not meet.
//! [`crate::rpc`] wraps a call's bytes in an *envelope* — a kind, a function
//! selector, a request id and a status word, then the arguments — and posts the
//! whole envelope as **one** framed message. [`crate::segment`] carries a byte
//! run too large for one framed message as an **ordered run** of frames,
//! reassembled in order on the far side. Each is defined wholly above the
//! framer, so they are siblings, not a stack: an [`RpcEndpoint`] posts an
//! envelope as a single frame and fails with
//! [`FrameError::TooLarge`](crate::frame::FrameError::TooLarge) the moment the
//! envelope exceeds one message ([`Framer::capacity`](crate::frame::Framer::capacity)),
//! and nothing today splits it.
//!
//! A real GSP conversation does exceed that bound. Some requests and replies —
//! an allocation descriptor, a large control block, a capability table — are
//! bigger than any one ring message, and must travel as several. This module is
//! the thin layer that makes the two meet: it builds the whole RPC envelope into
//! one contiguous buffer (so the header rides *in front of* the arguments, as
//! ever), then hands that buffer to a [`Segmenter`] to carry across as many
//! frames as it needs, and on the far side reassembles the envelope and decodes
//! its header back. A message that already fits in one frame is simply a
//! one-segment message, so this is a strict generalisation of the single-frame
//! path, not a replacement for it.
//!
//! ## What rides where
//!
//! The segment header ([`crate::segment::SegHeader`], 16 bytes) is the
//! *outer* wrapper: it ties the frames of one logical message together and
//! orders them. The RPC envelope header ([`crate::rpc::RpcHeader`]) is *inside*
//! the reassembled payload, exactly where it sits in a single-frame message — so
//! the receiver reassembles first, then decodes the envelope out of the
//! completed buffer, and slices the arguments from
//! [`RpcInbound::args_range`](crate::rpc::RpcInbound::args_range) just as the
//! single-frame path does. The two framing layers never interleave their fields:
//! one is the frame's leading bytes, the other the reassembled payload's.
//!
//! ## Resumable, like the segmenter beneath it
//!
//! A message spanning more frames than the ring has free slots cannot be posted
//! all at once — segmenting exists precisely to exceed that bound — so the send
//! methods are *resumable* exactly as [`Segmenter::send`] is: each returns a
//! [`SendProgress`] naming the next segment still to post, and the caller loops,
//! letting the consumer drain the ring between calls, until
//! [`SendProgress::is_done`]. No state lives in a [`SegmentedEndpoint`]; the
//! resume cursor is a plain `u32` the caller carries, and the full envelope is
//! re-derived from the caller's unchanged `args` on each call, so the caller
//! passes the same arguments and envelope buffer each time it resumes. The
//! receive methods mirror the segmenter's [`Segmenter::push`]: a caller-owned
//! [`Reassembly`] accumulator is advanced one frame at a time and reports the
//! completed message when the final segment lands.
//!
//! Because segmentation assigns each frame its own transport sequence (the
//! segment index), these methods take no caller `seq` — the per-ring frame
//! sequence is a diagnostic tag the segmenter stamps, and the envelope's own
//! request id is what correlates a reply to its request, unchanged by how many
//! frames the message spans.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Assembling a header in front of an argument run, splitting the result across
//! a FIFO and stitching it back in order, then reading the header off the front
//! are all *ordinary computer science* — correct or incorrect on any machine —
//! so this module, like the envelope, the segmentation, the framing and the ring
//! beneath it, is written and proven here against
//! [`crate::mock::MockSharedMem`] with no pending-hardware caveat. What it does
//! **not** fix stays the firmware's data, exactly as for the layers it composes:
//! the envelope's concrete field offsets (an [`RpcLayout`](crate::rpc::RpcLayout)),
//! the transport's checksum and header offsets, and *which* functions the catalog
//! carries. The segment header is FREEHOLD's own and needs no hardware
//! confirmation. Clean-room: this composition is FREEHOLD's own; no driver source
//! is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: a [`SegmentedEndpoint`]
//! holds only the `Copy` [`RpcEndpoint`], and every buffer is the caller's.

use crate::msgq::SharedMem;
use crate::rpc::{RpcEndpoint, RpcError, RpcHeader, RpcInbound, RpcKind};
use crate::segment::{Reassembly, RecvStep, SegError, Segmenter, SendProgress};

/// Why a segmented-envelope operation could not be carried out.
///
/// The two composed layers keep their own error types — the envelope codec
/// ([`RpcError`], from assembling or decoding the header) and the segmentation
/// ([`SegError`], from chunking, reassembly or the framing beneath it) — and this
/// enum carries whichever one failed, verbatim, so a caller can tell an envelope
/// fault (a bad [`RpcLayout`](crate::rpc::RpcLayout), a kind word that names no
/// kind) from a transport fault (back-pressure, a corrupt segment, a short
/// buffer) without losing the detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XferError {
    /// The envelope could not be assembled or decoded: a bad layout, a buffer too
    /// small for the header, a message shorter than the header, or a kind word
    /// that names no [`RpcKind`].
    Rpc(RpcError),
    /// The segmentation or the framing beneath it failed: a bad layout, an
    /// over-large payload, a corrupt or out-of-order segment, or a too-small
    /// output or scratch buffer.
    Seg(SegError),
}

impl From<RpcError> for XferError {
    fn from(e: RpcError) -> Self {
        XferError::Rpc(e)
    }
}

impl From<SegError> for XferError {
    fn from(e: SegError) -> Self {
        XferError::Seg(e)
    }
}

/// What advancing a [`SegmentedEndpoint`] over a ring by one frame turned up.
///
/// A segmented message completes only when its final frame lands; until then
/// each frame reports progress without yet yielding a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegRecv {
    /// Nothing was consumed: the ring is empty, or the next frame of the message
    /// in progress has not fully arrived yet. The caller waits for more to be
    /// posted and tries again.
    Blocked,
    /// A non-final frame of a message was consumed and placed; more are still
    /// expected. Carries the segment index just taken.
    More {
        /// The index of the segment just consumed.
        index: u32,
    },
    /// The final frame landed: the whole RPC envelope now sits in the caller's
    /// output buffer and its header has been decoded. The arguments are
    /// [`RpcInbound::args_range`](crate::rpc::RpcInbound::args_range) of that
    /// buffer, decodable with
    /// [`RpcEndpoint::decode_args`](crate::rpc::RpcEndpoint::decode_args).
    Message {
        /// The decoded envelope header and reassembled length.
        inbound: RpcInbound,
    },
}

/// An [`RpcEndpoint`] whose messages may be larger than one ring frame.
///
/// Wraps the endpoint's command and status framers in [`Segmenter`]s and reuses
/// its [`RpcLayout`](crate::rpc::RpcLayout) to assemble and decode the envelope.
/// Owns no memory — only the `Copy` endpoint — so it is `Copy`, and one instance
/// drives both halves of a host test over one shared region, exactly as the
/// endpoint and the framers beneath it do.
///
/// The host uses [`send_request`](Self::send_request) and
/// [`poll_response`](Self::poll_response); the GSP side (a host test's stand-in,
/// and conceptually a FREEHOLD GSP emulator) uses
/// [`recv_request`](Self::recv_request), [`send_reply`](Self::send_reply) and
/// [`send_event`](Self::send_event). All share the one envelope layout and the
/// one segment-header format.
#[derive(Clone, Copy)]
pub struct SegmentedEndpoint {
    /// The endpoint whose rings and envelope layout this carries segmented
    /// messages over.
    pub endpoint: RpcEndpoint,
}

impl SegmentedEndpoint {
    /// Build a segmented endpoint over `endpoint`.
    pub fn new(endpoint: RpcEndpoint) -> Self {
        SegmentedEndpoint { endpoint }
    }

    /// The segmenter over the command ring (host → GSP).
    pub fn command(&self) -> Segmenter {
        Segmenter::new(self.endpoint.command)
    }

    /// The segmenter over the status ring (GSP → host).
    pub fn status(&self) -> Segmenter {
        Segmenter::new(self.endpoint.status)
    }

    /// Check both segmenters and the envelope layout are usable.
    ///
    /// Each framer must be valid and leave room for a segment header, and the
    /// envelope layout's four words must fit a header that does not overlap — so
    /// a caller validates once when it builds the endpoint and then reuses it
    /// freely.
    pub fn validate(&self) -> Result<(), XferError> {
        self.command().validate()?;
        self.status().validate()?;
        self.endpoint.layout.validate()?;
        Ok(())
    }

    /// The scratch buffer size the command-ring send/receive methods require: one
    /// full command frame's payload (segment header plus one maximum chunk).
    pub fn command_scratch_len(&self) -> usize {
        self.command().scratch_len()
    }

    /// The scratch buffer size the status-ring send/receive methods require: one
    /// full status frame's payload.
    pub fn status_scratch_len(&self) -> usize {
        self.status().scratch_len()
    }

    /// How many segments a request/reply/event of `args_len` argument bytes spans
    /// on the command ring — the whole envelope being `header_len + args_len`.
    pub fn command_segment_count(&self, args_len: usize) -> u32 {
        self.command()
            .segment_count(self.endpoint.layout.header_len + args_len)
    }

    /// How many segments a reply/event of `args_len` argument bytes spans on the
    /// status ring.
    pub fn status_segment_count(&self, args_len: usize) -> u32 {
        self.status()
            .segment_count(self.endpoint.layout.header_len + args_len)
    }

    /// Assemble `hdr` + `args` into `envelope` and post it, segmented, over
    /// `seg`, resuming at `from_index`.
    #[allow(clippy::too_many_arguments)]
    fn send_envelope<M: SharedMem>(
        &self,
        seg: &Segmenter,
        mem: &mut M,
        msg_id: u32,
        hdr: &RpcHeader,
        args: &[u8],
        envelope: &mut [u8],
        from_index: u32,
        scratch: &mut [u8],
    ) -> Result<SendProgress, XferError> {
        let total = self.endpoint.layout.encode(envelope, hdr, args)?;
        Ok(seg.send(mem, msg_id, &envelope[..total], from_index, scratch)?)
    }

    /// Post a request on the command ring, segmented across as many frames as it
    /// needs: run `function` with `args`, tagged with `request_id` for the reply
    /// to echo, under segment message id `msg_id`.
    ///
    /// Resumable: posts as many segments as the command ring holds right now,
    /// starting at `from_index`, and returns the [`SendProgress`] to continue
    /// from (the message is done when [`SendProgress::is_done`]). The caller loops
    /// — draining the GSP side between calls — passing the same `args` and
    /// `envelope` each time. `envelope` must hold the whole envelope
    /// (`header_len + args.len()`); `scratch` must be at least
    /// [`command_scratch_len`](Self::command_scratch_len) bytes. The caller
    /// normally also [`remember`](crate::rpc::Outstanding::remember)s `request_id`
    /// so the reply can be matched, exactly as on the single-frame path.
    #[allow(clippy::too_many_arguments)]
    pub fn send_request<M: SharedMem>(
        &self,
        mem: &mut M,
        msg_id: u32,
        request_id: u32,
        function: u32,
        args: &[u8],
        envelope: &mut [u8],
        from_index: u32,
        scratch: &mut [u8],
    ) -> Result<SendProgress, XferError> {
        let hdr = RpcHeader {
            kind: RpcKind::Request,
            function,
            request_id,
            status: 0,
        };
        self.send_envelope(
            &self.command(),
            mem,
            msg_id,
            &hdr,
            args,
            envelope,
            from_index,
            scratch,
        )
    }

    /// Post a reply on the status ring (the GSP side), segmented, answering
    /// `request` with result code `status` and result bytes `args`.
    ///
    /// The reply echoes `request`'s function and request id — so the host matches
    /// it with [`Outstanding::resolve`](crate::rpc::Outstanding::resolve) — exactly
    /// as the single-frame [`send_reply`](crate::rpc::RpcEndpoint::send_reply)
    /// does. Resumable like [`send_request`](Self::send_request); `scratch` must be
    /// at least [`status_scratch_len`](Self::status_scratch_len) bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn send_reply<M: SharedMem>(
        &self,
        mem: &mut M,
        msg_id: u32,
        request: &RpcHeader,
        status: u32,
        args: &[u8],
        envelope: &mut [u8],
        from_index: u32,
        scratch: &mut [u8],
    ) -> Result<SendProgress, XferError> {
        let hdr = RpcHeader {
            kind: RpcKind::Reply,
            function: request.function,
            request_id: request.request_id,
            status,
        };
        self.send_envelope(
            &self.status(),
            mem,
            msg_id,
            &hdr,
            args,
            envelope,
            from_index,
            scratch,
        )
    }

    /// Post an unsolicited event on the status ring (the GSP side), segmented: a
    /// notification of `function` with `args`, answering no request (its request
    /// id is zero).
    ///
    /// Resumable like [`send_request`](Self::send_request); `scratch` must be at
    /// least [`status_scratch_len`](Self::status_scratch_len) bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn send_event<M: SharedMem>(
        &self,
        mem: &mut M,
        msg_id: u32,
        function: u32,
        args: &[u8],
        envelope: &mut [u8],
        from_index: u32,
        scratch: &mut [u8],
    ) -> Result<SendProgress, XferError> {
        let hdr = RpcHeader {
            kind: RpcKind::Event,
            function,
            request_id: 0,
            status: 0,
        };
        self.send_envelope(
            &self.status(),
            mem,
            msg_id,
            &hdr,
            args,
            envelope,
            from_index,
            scratch,
        )
    }

    /// Advance one frame of an incoming segmented message over `seg`, decoding
    /// the envelope once the message completes.
    fn recv<M: SharedMem>(
        &self,
        seg: &Segmenter,
        re: &mut Reassembly,
        mem: &mut M,
        out: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<SegRecv, XferError> {
        match seg.push(re, mem, out, scratch)? {
            RecvStep::Blocked => Ok(SegRecv::Blocked),
            RecvStep::More { index } => Ok(SegRecv::More { index }),
            RecvStep::Complete { len } => {
                let header = self.endpoint.layout.decode(out, len)?;
                Ok(SegRecv::Message {
                    inbound: RpcInbound { header, len },
                })
            }
        }
    }

    /// Take the next frame of an incoming request off the command ring (the GSP
    /// side), advancing `re` and, on the final frame, decoding its envelope into
    /// `out`.
    ///
    /// On [`SegRecv::Message`] the whole envelope sits in `out[..inbound.len]` and
    /// the arguments are
    /// [`inbound.args_range(&layout)`](crate::rpc::RpcInbound::args_range). An
    /// empty or not-yet-complete ring returns [`SegRecv::Blocked`] without
    /// consuming. `out` must hold the largest message expected and `scratch` at
    /// least [`command_scratch_len`](Self::command_scratch_len) bytes. A segment
    /// that does not continue the message in progress surfaces as
    /// [`XferError::Seg`] with [`SegError::BadSegment`]; the frame has been
    /// consumed and the caller [`reset`](crate::segment::Reassembly::reset)s `re`.
    pub fn recv_request<M: SharedMem>(
        &self,
        re: &mut Reassembly,
        mem: &mut M,
        out: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<SegRecv, XferError> {
        self.recv(&self.command(), re, mem, out, scratch)
    }

    /// Take the next frame of an incoming reply or event off the status ring (the
    /// host side), advancing `re` and, on the final frame, decoding its envelope
    /// into `out`.
    ///
    /// On [`SegRecv::Message`], inspect [`RpcHeader::kind`](crate::rpc::RpcHeader):
    /// a [`Reply`](RpcKind::Reply) whose `request_id` is outstanding answers a
    /// pending request; an [`Event`](RpcKind::Event), or a reply whose id is not
    /// outstanding, is unsolicited — the same classification the single-frame
    /// [`poll_response`](crate::rpc::RpcEndpoint::poll_response) leaves to the
    /// caller. `scratch` must be at least
    /// [`status_scratch_len`](Self::status_scratch_len) bytes.
    pub fn poll_response<M: SharedMem>(
        &self,
        re: &mut Reassembly,
        mem: &mut M,
        out: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<SegRecv, XferError> {
        self.recv(&self.status(), re, mem, out, scratch)
    }
}
