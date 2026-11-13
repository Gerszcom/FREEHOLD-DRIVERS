//! Stage 5 groundwork — segmented RPC calls in the client.
//!
//! [`crate::client`]'s [`RpcClient`](crate::client::RpcClient) is the host side
//! of the GSP conversation for the common case: a request and its reply each fit
//! in **one** ring frame. It hands out a request id, posts a typed call over the
//! single-frame [`RpcEndpoint`](crate::rpc::RpcEndpoint), and drains the status
//! ring matching each reply back. [`crate::xfer`]'s
//! [`SegmentedEndpoint`](crate::xfer::SegmentedEndpoint) lifts the one-frame size
//! bound: it carries an RPC envelope of any size as an ordered run of frames. The
//! two have not yet met — a caller that wants a *large* call (an allocation
//! descriptor, a capability table, a control block bigger than one ring message)
//! has had to drive [`SegmentedEndpoint`](crate::xfer::SegmentedEndpoint) by hand,
//! allocating and clearing request ids itself and reassembling replies itself.
//!
//! This module is that driver. A [`SegmentedClient`] is to
//! [`SegmentedEndpoint`](crate::xfer::SegmentedEndpoint) exactly what
//! [`RpcClient`](crate::client::RpcClient) is to
//! [`RpcEndpoint`](crate::rpc::RpcEndpoint): it owns the endpoint, the
//! fixed-capacity [`Outstanding`](crate::rpc::Outstanding) set of in-flight
//! request ids, and the counters that hand out a fresh request id and a fresh
//! segment message id. What it adds over the single-frame client is that **both
//! directions can span many frames**:
//!
//! * [`start_call`](SegmentedClient::start_call) issues a typed call under a
//!   fresh request id and posts as many segments of its (possibly large)
//!   encoded request as the command ring holds right now. If the whole request
//!   went out it returns [`Posting::Posted`] and the id is outstanding; if the
//!   ring filled first it returns [`Posting::Blocked`] carrying a
//!   [`SendCursor`], and the caller lets the GSP drain the command ring, then
//!   calls [`resume_send`](SegmentedClient::resume_send) with that cursor until
//!   the send completes. This is the *resumable send under flow control* — the
//!   id is reserved for the whole multi-frame post, cleared only if the caller
//!   [`abandon`](SegmentedClient::abandon_send)s it or a hard transport error
//!   aborts the send, so a call that never fully posts leaves nothing
//!   outstanding, exactly as [`RpcClient::call`](crate::client::RpcClient::call)
//!   does for the single-frame case.
//! * [`pump`](SegmentedClient::pump) advances the client over the status ring by
//!   **one frame**, reassembling a segmented reply or event across successive
//!   calls (a non-final frame reports [`SegPump::More`]) and, only when the final
//!   frame lands, classifying the whole message: a [`SegPump::Reply`] whose id was
//!   outstanding (now resolved), or a [`SegPump::Unsolicited`] event or stray.
//!
//! ## The synchronous convenience
//!
//! Most driver calls are one request answered by one reply, and the caller wants
//! the typed answer back, not a flow-control loop at every call site.
//! [`transact`](SegmentedClient::transact) packages the loop above for that case —
//! the segmented analogue of [`RpcClient::transact`](crate::client::RpcClient::transact):
//! it posts one typed call, letting a caller [`SegResponder`] *drain* the command
//! ring between send resumes so a large request streams out under flow control, then
//! pumps the status ring — reassembling the reply frame by frame, routing each
//! unsolicited event to the responder and asking it to *wait* for more status frames
//! when the ring is empty — until the reply lands ([`SegTransact::Completed`]) or a
//! wait budget is spent ([`SegTransact::TimedOut`]). The two waiting points are the
//! only richness over the single-frame transaction; the rest is pure composition of
//! the async primitives. The `start_call` / `resume_send` / `pump` loop stays for
//! genuinely concurrent calls.
//!
//! ## One reassembly, because the status ring is a FIFO
//!
//! The GSP posts each status message — reply or event — as a contiguous run of
//! frames: it is the single producer on the status ring, and the segment layer
//! reassembles strictly in order ([`Segmenter::push`](crate::segment::Segmenter::push)
//! rejects a frame that does not continue the message in progress). So the client
//! holds **one** [`Reassembly`](crate::segment::Reassembly): it stitches one
//! message at a time, resetting itself when the final frame lands, ready for the
//! next. Interleaving the segments of two *different* status messages would need
//! the firmware to interleave them on the one ring (which a single-producer FIFO
//! does not) and a reassembly per message id; that is left for if a real GSP ever
//! does so. A message partly reassembled when a bad frame arrives is abandoned
//! with [`reset_recv`](SegmentedClient::reset_recv), mirroring the
//! [`Reassembly::reset`](crate::segment::Reassembly::reset) contract of the layer
//! beneath.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! This layer is pure composition of two already host-verified layers — the
//! segmented envelope ([`crate::xfer`]) and the in-flight-id bookkeeping
//! ([`crate::rpc::Outstanding`], as the single-frame client uses it). Handing out
//! an unused id, driving a resumable send to completion as the ring drains,
//! stitching a reply back together in order and matching its echoed id are all
//! *ordinary computer science* — correct or incorrect on any machine — so the
//! driver is written and proven here against [`crate::mock::MockSharedMem`] with
//! no pending-hardware caveat, like every layer beneath it. What it does **not**
//! fix stays the firmware's data, exactly as for the layers it composes: the
//! envelope's concrete field offsets (an [`RpcLayout`](crate::rpc::RpcLayout)),
//! the transport's checksum and header offsets, and *which* functions the catalog
//! carries. Clean-room: the driver is FREEHOLD's own; no driver source is copied
//! or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the client holds only the
//! `Copy` endpoint, the fixed-capacity set, one reassembly accumulator and two
//! counters, and every call carries its bytes in caller-provided slices.

use crate::call::RpcCall;
use crate::client::Pending;
use crate::codec::{encode_into, WireDecode};
use crate::msgq::SharedMem;
use crate::rpc::{Outstanding, RpcError, RpcInbound, RpcKind};
use crate::segment::{Reassembly, SendProgress};
use crate::xfer::{SegRecv, SegmentedEndpoint, XferError};

/// A segmented call that has not finished posting: the bookkeeping
/// [`resume_send`](SegmentedClient::resume_send) needs to continue it.
///
/// Carries the request id reserved for the call (held until the send completes
/// or is abandoned), the function it names, the segment message id its frames
/// share, how many bytes its encoded arguments occupy, and how far the resumable
/// send has got. The caller holds it by value between
/// [`start_call`](SegmentedClient::start_call) and
/// [`resume_send`](SegmentedClient::resume_send), passing the **same** `args` and
/// `envelope` buffers each time, exactly as
/// [`SegmentedEndpoint::send_request`](crate::xfer::SegmentedEndpoint::send_request)
/// requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendCursor {
    request_id: u32,
    function: u32,
    msg_id: u32,
    args_len: usize,
    progress: SendProgress,
}

impl SendCursor {
    /// The request id reserved for this call (never zero).
    pub fn request_id(&self) -> u32 {
        self.request_id
    }

    /// The function selector the call names.
    pub fn function(&self) -> u32 {
        self.function
    }

    /// The index of the next segment still to post.
    pub fn next_index(&self) -> u32 {
        self.progress.next_index
    }

    /// How many segments the whole request spans.
    pub fn segment_count(&self) -> u32 {
        self.progress.count
    }
}

/// How far [`start_call`](SegmentedClient::start_call) or
/// [`resume_send`](SegmentedClient::resume_send) got posting a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Posting {
    /// Every segment of the request has been posted. The request id is now
    /// outstanding; the caller awaits the reply through
    /// [`pump`](SegmentedClient::pump), matching it against this [`Pending`].
    Posted(Pending),
    /// The command ring filled before the whole request was posted. The caller
    /// lets the GSP drain the command ring, then calls
    /// [`resume_send`](SegmentedClient::resume_send) with this cursor (the same
    /// `args`/`envelope` buffers). The request id stays reserved meanwhile.
    Blocked(SendCursor),
}

impl Posting {
    /// Whether the whole request has been posted.
    pub fn is_posted(&self) -> bool {
        matches!(self, Posting::Posted(_))
    }
}

/// What advancing a [`SegmentedClient`] over the status ring by one frame turned
/// up.
///
/// A segmented message completes only when its final frame lands; until then each
/// frame reports [`More`](SegPump::More) without yet yielding a message. Once the
/// final frame lands the whole envelope sits in the caller's output buffer and is
/// classified like the single-frame [`Pump`](crate::client::Pump).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegPump {
    /// The status ring was empty, or the next frame of the message in progress
    /// has not fully arrived: nothing was consumed. The caller waits for the
    /// GSP's status signal and pumps again.
    Idle,
    /// A non-final frame of a message was consumed and placed; more are still
    /// expected. Carries the segment index just taken.
    More {
        /// The index of the segment just consumed.
        index: u32,
    },
    /// The final frame of a reply landed and its id was outstanding (now
    /// resolved): the whole reply sits in the output buffer. The caller decodes
    /// its typed result with [`decode_reply`](SegmentedClient::decode_reply).
    Reply {
        /// The resolved request id (matches a [`Pending::request_id`]).
        request_id: u32,
        /// The decoded message, for slicing and decoding the result.
        inbound: RpcInbound,
    },
    /// The final frame of a message that answers no outstanding call landed: an
    /// unsolicited event, or a reply whose id the client was not waiting on (a
    /// late duplicate, a reply to a forgotten or already-resolved call, or a
    /// stray). The caller inspects `inbound.header` and handles it.
    Unsolicited {
        /// The decoded message.
        inbound: RpcInbound,
    },
}

/// How a synchronous [`transact`](SegmentedClient::transact) ended.
///
/// A segmented transaction posts one typed call — whose encoded request may span
/// many ring frames — and drives the conversation to its (possibly multi-frame)
/// reply or to a give-up. It is the blocking convenience over the
/// `start_call` / `resume_send` / `pump` loop, the segmented analogue of the
/// single-frame [`Transact`](crate::client::Transact): the on-device driver paths
/// want "post this request, give me its typed answer", not a hand-rolled
/// flow-control loop at every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegTransact<R> {
    /// The awaited reply arrived and was reassembled in full. `status` is the
    /// GSP's result word ([`RpcHeader::status`](crate::rpc::RpcHeader::status)) and
    /// `response` the decoded typed result ([`RpcCall::Response`]).
    Completed {
        /// The GSP's result status word, carried out of the reply envelope.
        status: u32,
        /// The decoded typed result.
        response: R,
    },
    /// The transaction gave up before the reply arrived — the wait budget was
    /// spent, or the responder declined to continue. A call still mid-send is
    /// [`abandon`](SegmentedClient::abandon_send)ed (its reserved id freed); a
    /// fully-posted call is [`forget`](SegmentedClient::forget)ten, so a reply
    /// that later arrives for it reads as [`SegPump::Unsolicited`].
    TimedOut,
}

/// The caller's side of a synchronous [`transact`](SegmentedClient::transact):
/// how to make room on the command ring while a large request is still being
/// posted, what to do when the status ring is empty while the reply is awaited,
/// and what to do with any message that is not the awaited reply.
///
/// A segmented transaction is richer than the single-frame
/// [`Responder`](crate::client::Responder) because **both** directions can span
/// many frames. The request may be larger than the command ring holds at once, so
/// between send resumes the GSP must *drain* the command ring
/// ([`drain_command`](Self::drain_command)); the reply may be larger than the
/// status ring holds at once, so between pumps the host must *wait* for the next
/// status frames ([`wait`](Self::wait)). [`on_event`](Self::on_event) receives
/// every completed message the transaction is not waiting on, so the caller can
/// handle an unsolicited event without aborting the call in progress.
///
/// `transact` owns the `&mut M` shared region while it runs, so it hands that
/// region to each method rather than letting the responder keep its own alias. On
/// real hardware the two waiting methods block on the GSP (its command-queue
/// consumption and its status interrupt) and ignore `mem`; in a host test they are
/// where the GSP side reassembles the request and posts the reply into `mem`.
pub trait SegResponder<M: SharedMem> {
    /// Called when [`transact`](SegmentedClient::transact) has posted every
    /// segment the command ring could hold but the request is not yet fully out
    /// ([`Posting::Blocked`]). Let the GSP drain its side of the command ring so
    /// the next [`resume_send`](SegmentedClient::resume_send) has room, and return
    /// `true` to post more; return `false` to give up now — the partly-posted call
    /// is [`abandon`](SegmentedClient::abandon_send)ed and the transaction returns
    /// [`SegTransact::TimedOut`]. A drain that frees no room is a stall and is
    /// charged against the wait budget, so a responder that can make no progress
    /// should return `false` rather than spin.
    fn drain_command(&mut self, mem: &mut M) -> bool;

    /// Called once each time [`transact`](SegmentedClient::transact) finds the
    /// status ring empty while awaiting the reply. Return `true` to wait and pump
    /// again; return `false` to give up (the call is
    /// [`forget`](SegmentedClient::forget)ten and the transaction returns
    /// [`SegTransact::TimedOut`]). `mem` is the shared region, offered so a
    /// host-side responder can post the GSP's reply; a hardware responder blocks on
    /// the status interrupt and ignores it.
    fn wait(&mut self, mem: &mut M) -> bool;

    /// Called for every completed message the transaction pumps that is **not**
    /// the awaited reply: an unsolicited event, or a stray or late reply to some
    /// other id. `bytes` is the reassembled output buffer; the carried
    /// [`RpcInbound`] slices and decodes its payload (via
    /// [`decode_args`](SegmentedClient::decode_args)). The default drops it.
    /// Routed events never end the transaction and never consume the wait budget.
    fn on_event(&mut self, inbound: &RpcInbound, bytes: &[u8]) {
        let _ = (inbound, bytes);
    }
}

/// The host side of one GSP RPC conversation whose messages may each span many
/// ring frames.
///
/// Owns the [`SegmentedEndpoint`](crate::xfer::SegmentedEndpoint), the
/// fixed-capacity set of in-flight request ids, one status-ring reassembly
/// accumulator, and the next request id and segment message id to hand out. `N`
/// bounds how many calls may be outstanding at once. See the module docs for the
/// `start_call` / `resume_send` / `pump` loop this drives.
#[derive(Clone)]
pub struct SegmentedClient<const N: usize> {
    endpoint: SegmentedEndpoint,
    outstanding: Outstanding<N>,
    recv: Reassembly,
    next_request_id: u32,
    next_msg_id: u32,
}

impl<const N: usize> SegmentedClient<N> {
    /// A client over `endpoint`, handing out request ids from `1` and segment
    /// message ids from `0`.
    pub fn new(endpoint: SegmentedEndpoint) -> Self {
        Self::with_start(endpoint, 1, 0)
    }

    /// A client whose first request id is `first_request_id` and first segment
    /// message id is `first_msg_id`.
    ///
    /// Request ids wrap and skip zero (reserved for events) and any id currently
    /// outstanding, so `first_request_id` is only where allocation begins. Useful
    /// for exercising the wrap behaviour deterministically.
    pub fn with_start(
        endpoint: SegmentedEndpoint,
        first_request_id: u32,
        first_msg_id: u32,
    ) -> Self {
        SegmentedClient {
            endpoint,
            outstanding: Outstanding::new(),
            recv: Reassembly::new(),
            next_request_id: first_request_id,
            next_msg_id: first_msg_id,
        }
    }

    /// The segmented endpoint this client drives.
    pub fn endpoint(&self) -> &SegmentedEndpoint {
        &self.endpoint
    }

    /// How many calls are outstanding (fully posted and not yet answered or
    /// forgotten, plus any mid-send call whose id is reserved).
    pub fn pending_count(&self) -> usize {
        self.outstanding.len()
    }

    /// Whether no call is outstanding or mid-send.
    pub fn is_idle(&self) -> bool {
        self.outstanding.is_empty()
    }

    /// The most calls that may be outstanding at once (`N`).
    pub fn capacity(&self) -> usize {
        N
    }

    /// Whether `request_id` is currently outstanding or reserved for a mid-send
    /// call.
    pub fn is_outstanding(&self, request_id: u32) -> bool {
        self.outstanding.contains(request_id)
    }

    /// Whether a status message is partly reassembled (between its first and final
    /// frames).
    pub fn recv_in_progress(&self) -> bool {
        self.recv.in_progress()
    }

    /// Validate the underlying endpoint (both segmenters and the envelope layout).
    pub fn validate(&self) -> Result<(), XferError> {
        self.endpoint.validate()
    }

    /// Hand out the next request id that is non-zero and not already outstanding,
    /// advancing the counter past it.
    ///
    /// Returns `None` only when the in-flight set is full. Otherwise at most `N`
    /// ids are outstanding and zero is the one further forbidden value, so a free
    /// id is found within `N + 2` consecutive candidates by the pigeonhole
    /// argument — the same allocation
    /// [`RpcClient`](crate::client::RpcClient) uses.
    fn alloc_request_id(&mut self) -> Option<u32> {
        if self.outstanding.is_full() {
            return None;
        }
        for _ in 0..N + 2 {
            let id = self.next_request_id;
            self.next_request_id = self.next_request_id.wrapping_add(1);
            if id != 0 && !self.outstanding.contains(id) {
                return Some(id);
            }
        }
        // Unreachable while the set is not full: among N + 2 consecutive values
        // at most N + 1 are forbidden (the <= N outstanding ids plus zero).
        None
    }

    /// Issue the typed call `C` with `request`, posting as many segments of its
    /// encoded request as the command ring holds right now.
    ///
    /// Allocates a fresh request id, reserves it, encodes `request` into `args`,
    /// and drives [`SegmentedEndpoint::send_request`](crate::xfer::SegmentedEndpoint::send_request)
    /// from segment zero. `args` must hold the request's
    /// [`encoded_len`](crate::codec::WireEncode::encoded_len) bytes, `envelope`
    /// the whole envelope (`header_len + encoded_len`), and `scratch` at least
    /// [`command_scratch_len`](crate::xfer::SegmentedEndpoint::command_scratch_len)
    /// bytes.
    ///
    /// On [`Posting::Posted`] the whole request is out and the id is outstanding.
    /// On [`Posting::Blocked`] the ring filled first; the caller drains the GSP
    /// side and calls [`resume_send`](Self::resume_send) with the carried cursor,
    /// passing the **same** `args` and `envelope`.
    ///
    /// Errors leave the client unchanged with nothing added to the in-flight set:
    /// [`XferError::Rpc`] with [`RpcError::TrackerFull`] if `N` calls are already
    /// outstanding, [`XferError::Rpc`] with [`RpcError::Codec`] if `request` does
    /// not fit `args`, or — if a non-back-pressure transport fault aborts the
    /// send — an [`XferError::Seg`], with the reserved id cleared.
    pub fn start_call<C: RpcCall, M: SharedMem>(
        &mut self,
        mem: &mut M,
        request: &C::Request,
        args: &mut [u8],
        envelope: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<Posting, XferError> {
        let args_len = encode_into(request, args)
            .map_err(|e| XferError::Rpc(RpcError::Codec(e)))?
            .len();
        let request_id = self
            .alloc_request_id()
            .ok_or(XferError::Rpc(RpcError::TrackerFull))?;
        let msg_id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        // Reserve the id for the whole multi-frame post, so a concurrent
        // allocation cannot reuse it mid-send. Cleared on a hard send error or an
        // explicit abandon; a call that never fully posts thus leaves nothing
        // outstanding, as the single-frame client's `call` does.
        self.outstanding
            .remember(request_id)
            .map_err(XferError::Rpc)?;
        self.drive_send::<M>(
            mem,
            SendCursor {
                request_id,
                function: C::FUNCTION,
                msg_id,
                args_len,
                progress: SendProgress {
                    next_index: 0,
                    count: 0,
                },
            },
            &args[..args_len],
            envelope,
            scratch,
            true,
        )
    }

    /// Continue posting a request that [`start_call`](Self::start_call) or an
    /// earlier [`resume_send`](Self::resume_send) left [`Blocked`](Posting::Blocked).
    ///
    /// Pass the cursor the previous step returned and the **same** `args` and
    /// `envelope` buffers (unchanged since the start), and `scratch` at least
    /// [`command_scratch_len`](crate::xfer::SegmentedEndpoint::command_scratch_len)
    /// bytes. Posts as many further segments as the command ring now holds,
    /// returning [`Posting::Posted`] once the request is complete or
    /// [`Posting::Blocked`] to continue again after another drain.
    ///
    /// A hard transport fault clears the reserved id and surfaces as
    /// [`XferError::Seg`]; an `args` shorter than the cursor's encoded length is
    /// rejected with [`RpcError::BufferTooSmall`].
    pub fn resume_send<M: SharedMem>(
        &mut self,
        mem: &mut M,
        cursor: SendCursor,
        args: &[u8],
        envelope: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<Posting, XferError> {
        if args.len() < cursor.args_len {
            return Err(XferError::Rpc(RpcError::BufferTooSmall {
                needed: cursor.args_len,
                got: args.len(),
            }));
        }
        let args = &args[..cursor.args_len];
        self.drive_send::<M>(mem, cursor, args, envelope, scratch, false)
    }

    /// The shared body of [`start_call`](Self::start_call) and
    /// [`resume_send`](Self::resume_send): post from `cursor.next_index` (or zero
    /// on the first step) and classify the resulting progress.
    fn drive_send<M: SharedMem>(
        &mut self,
        mem: &mut M,
        cursor: SendCursor,
        args: &[u8],
        envelope: &mut [u8],
        scratch: &mut [u8],
        first: bool,
    ) -> Result<Posting, XferError> {
        let from = if first { 0 } else { cursor.progress.next_index };
        match self.endpoint.send_request(
            mem,
            cursor.msg_id,
            cursor.request_id,
            cursor.function,
            args,
            envelope,
            from,
            scratch,
        ) {
            Ok(progress) => {
                if progress.is_done() {
                    Ok(Posting::Posted(Pending {
                        request_id: cursor.request_id,
                        function: cursor.function,
                    }))
                } else {
                    Ok(Posting::Blocked(SendCursor { progress, ..cursor }))
                }
            }
            Err(e) => {
                // A hard transport fault aborts the call: clear the reserved id so
                // nothing is left outstanding, matching the single-frame client.
                self.outstanding.resolve(cursor.request_id);
                Err(e)
            }
        }
    }

    /// Stop posting a call that is still [`Blocked`](Posting::Blocked), freeing its
    /// reserved request id. Returns whether the id was reserved.
    ///
    /// The give-up path for a send that never completed (for example, the command
    /// ring stayed full). Use it before any segment of the message has gone out —
    /// abandoning a partly-posted message leaves its already-sent segments on the
    /// ring for the GSP to reject, so a caller that has posted segments should
    /// drive the send to completion rather than abandon it.
    pub fn abandon_send(&mut self, cursor: &SendCursor) -> bool {
        self.outstanding.resolve(cursor.request_id)
    }

    /// Advance the client over the status ring by one frame, reassembling a
    /// segmented reply or event into `out` and classifying it once complete (see
    /// [`SegPump`]).
    ///
    /// `out` must hold the largest whole message expected and `scratch` at least
    /// [`status_scratch_len`](crate::xfer::SegmentedEndpoint::status_scratch_len)
    /// bytes. A non-final frame returns [`SegPump::More`]; the final frame of a
    /// reply whose id was outstanding returns [`SegPump::Reply`] (its id resolved
    /// out of the in-flight set); any other completed message is
    /// [`SegPump::Unsolicited`]; an empty or not-yet-complete ring is
    /// [`SegPump::Idle`].
    ///
    /// On a [`SegError::BadSegment`](crate::segment::SegError::BadSegment) the
    /// offending frame has been consumed and a partly-reassembled message is now
    /// invalid; the caller calls [`reset_recv`](Self::reset_recv) before pumping
    /// again, mirroring the layer beneath.
    pub fn pump<M: SharedMem>(
        &mut self,
        mem: &mut M,
        out: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<SegPump, XferError> {
        match self
            .endpoint
            .poll_response(&mut self.recv, mem, out, scratch)?
        {
            SegRecv::Blocked => Ok(SegPump::Idle),
            SegRecv::More { index } => Ok(SegPump::More { index }),
            SegRecv::Message { inbound } => {
                let header = inbound.header;
                if header.kind == RpcKind::Reply && self.outstanding.resolve(header.request_id) {
                    Ok(SegPump::Reply {
                        request_id: header.request_id,
                        inbound,
                    })
                } else {
                    Ok(SegPump::Unsolicited { inbound })
                }
            }
        }
    }

    /// Abandon a status message that was partly reassembled, returning the
    /// receive side to its initial state.
    ///
    /// The escape hatch after a
    /// [`SegError::BadSegment`](crate::segment::SegError::BadSegment): the client
    /// drops what it had and waits for a fresh segment zero.
    pub fn reset_recv(&mut self) {
        self.recv.reset();
    }

    /// Decode the typed result `C::Response` from a reply's argument region.
    ///
    /// `inbound` is the one a [`SegPump::Reply`] carried and `out` the buffer it
    /// was reassembled into. A strict exact-length decode: a reply whose argument
    /// region is longer or shorter than the result's shape is rejected.
    pub fn decode_reply<C: RpcCall>(
        &self,
        inbound: &RpcInbound,
        out: &[u8],
    ) -> Result<C::Response, XferError> {
        self.endpoint
            .endpoint
            .decode_args::<C::Response>(inbound, out)
            .map_err(XferError::Rpc)
    }

    /// Decode an arbitrary typed value `T` from a message's argument region — the
    /// escape hatch for an event's payload (a [`SegPump::Unsolicited`]), which has
    /// no [`RpcCall`] result type.
    pub fn decode_args<T: WireDecode>(
        &self,
        inbound: &RpcInbound,
        out: &[u8],
    ) -> Result<T, XferError> {
        self.endpoint
            .endpoint
            .decode_args::<T>(inbound, out)
            .map_err(XferError::Rpc)
    }

    /// Stop waiting on an outstanding call, returning whether it was in flight.
    ///
    /// The timeout / give-up path for a fully-posted call: after this, a reply
    /// that later arrives for `pending`'s id is reported as
    /// [`SegPump::Unsolicited`], and the slot it held is freed. `false` means the
    /// call was not outstanding.
    pub fn forget(&mut self, pending: &Pending) -> bool {
        self.outstanding.resolve(pending.request_id)
    }

    /// Issue the typed call `C` and drive the whole conversation to its reply in
    /// one blocking step: post `request` — segmented across as many command frames
    /// as it needs, letting `responder.drain_command` free the ring between send
    /// resumes — then pump the status ring, reassembling the reply frame by frame,
    /// handing every message that is not this call's reply to `responder.on_event`
    /// and asking `responder.wait` for the next status frames whenever the ring is
    /// empty, until the reply lands or the transaction gives up.
    ///
    /// This is the synchronous convenience over the [`start_call`](Self::start_call)
    /// / [`resume_send`](Self::resume_send) / [`pump`](Self::pump) loop — the
    /// segmented analogue of [`RpcClient::transact`](crate::client::RpcClient::transact),
    /// richer only in that the request send is itself flow-controlled. On success it
    /// returns [`SegTransact::Completed`] carrying the GSP's status word and the
    /// decoded typed result.
    ///
    /// ## Buffers
    ///
    /// `args` holds the request's
    /// [`encoded_len`](crate::codec::WireEncode::encoded_len) bytes and `envelope`
    /// the whole request envelope (`header_len + encoded_len`), exactly as
    /// [`start_call`](Self::start_call) requires — both carried unchanged across the
    /// internal resumes. `send_scratch` is at least
    /// [`command_scratch_len`](crate::xfer::SegmentedEndpoint::command_scratch_len)
    /// bytes; `out` holds the largest whole reply or event expected and
    /// `recv_scratch` at least
    /// [`status_scratch_len`](crate::xfer::SegmentedEndpoint::status_scratch_len)
    /// bytes, as [`pump`](Self::pump) requires.
    ///
    /// ## The wait budget
    ///
    /// `max_waits` bounds the number of **unproductive stalls** across the *whole*
    /// transaction — a `drain_command` that freed no command-ring room, or an empty
    /// status ring while awaiting the reply. Forward progress is always free, however
    /// many steps a large message takes: posting another request segment,
    /// reassembling another reply frame, and routing an unsolicited event never
    /// consume the budget, so neither a large request streamed under flow control nor
    /// a burst of events can time a pending reply out early. Once the budget is spent
    /// (or the responder returns `false`), the call is abandoned (if still mid-send)
    /// or [`forget`](Self::forget)ten (if fully posted) and [`SegTransact::TimedOut`]
    /// is returned; a reply that arrives afterward reads as
    /// [`SegPump::Unsolicited`]. A `max_waits` of `0` still drives a productive send
    /// and a reply already waiting to completion, but gives up at the first genuine
    /// stall.
    ///
    /// Errors are those of [`start_call`](Self::start_call) /
    /// [`resume_send`](Self::resume_send) (notably [`RpcError::TrackerFull`] and the
    /// encode/transport faults, the latter already clearing the reserved id) and
    /// [`pump`](Self::pump) (the transport and envelope faults), plus an
    /// [`RpcError::Codec`] if the matched reply's argument region does not fit
    /// `C::Response`, each wrapped in an [`XferError`].
    ///
    /// Intended to drive one call at a time: call it when this client is
    /// [`idle`](Self::is_idle). If other calls are outstanding, a reply to one of
    /// them is resolved out of the in-flight set and delivered to
    /// `responder.on_event` like any other non-awaited message; drive genuinely
    /// concurrent calls with the async primitives instead.
    #[allow(clippy::too_many_arguments)]
    pub fn transact<C, M, R>(
        &mut self,
        mem: &mut M,
        request: &C::Request,
        args: &mut [u8],
        envelope: &mut [u8],
        send_scratch: &mut [u8],
        out: &mut [u8],
        recv_scratch: &mut [u8],
        max_waits: u32,
        responder: &mut R,
    ) -> Result<SegTransact<C::Response>, XferError>
    where
        C: RpcCall,
        M: SharedMem,
        R: SegResponder<M>,
    {
        let mut waits: u32 = 0;

        // --- Send phase: drive the (possibly multi-frame) request fully out. ---
        let mut posting = self.start_call::<C, M>(mem, request, args, envelope, send_scratch)?;
        let pending = loop {
            let cursor = match posting {
                Posting::Posted(p) => break p,
                Posting::Blocked(c) => c,
            };
            // The command ring filled before the whole request was posted. Ask the
            // caller to let the GSP drain it; a refusal abandons the send, leaving
            // nothing outstanding.
            if !responder.drain_command(mem) {
                self.abandon_send(&cursor);
                return Ok(SegTransact::TimedOut);
            }
            let prev_index = cursor.next_index();
            posting = self.resume_send(mem, cursor, args, envelope, send_scratch)?;
            // Posting further segments is forward progress and is free, however many
            // resumes a large message takes; a drain that freed no room leaves the
            // send where it was — a genuine stall, charged against the budget.
            let progressed = match &posting {
                Posting::Posted(_) => true,
                Posting::Blocked(c) => c.next_index() > prev_index,
            };
            if !progressed {
                if waits >= max_waits {
                    if let Posting::Blocked(c) = &posting {
                        self.abandon_send(c);
                    }
                    return Ok(SegTransact::TimedOut);
                }
                waits = waits.wrapping_add(1);
            }
        };

        // --- Reply phase: pump the status ring until this call's reply lands. ---
        loop {
            match self.pump(mem, out, recv_scratch)? {
                SegPump::Reply {
                    request_id,
                    inbound,
                } if request_id == pending.request_id => {
                    let status = inbound.header.status;
                    let response = self.decode_reply::<C>(&inbound, out)?;
                    return Ok(SegTransact::Completed { status, response });
                }
                SegPump::Reply { inbound, .. } | SegPump::Unsolicited { inbound } => {
                    responder.on_event(&inbound, out);
                }
                SegPump::More { .. } => {
                    // A non-final frame was reassembled: forward progress, free.
                }
                SegPump::Idle => {
                    if waits >= max_waits || !responder.wait(mem) {
                        self.forget(&pending);
                        return Ok(SegTransact::TimedOut);
                    }
                    waits = waits.wrapping_add(1);
                }
            }
        }
    }
}
