//! Stage 5 groundwork — the RPC client transaction driver.
//!
//! [`crate::rpc`] gives the host two primitives over the command/status ring
//! pair — [`send a request`](RpcEndpoint::send_request) and
//! [`poll a response`](RpcEndpoint::poll_response) — and an
//! [`Outstanding`](crate::rpc::Outstanding) set to match a reply back to the
//! request it answers. [`crate::call`] adds the *typing* on top, so a call is
//! issued and read back by naming its struct. What neither settles is the
//! *bookkeeping of actually using the endpoint as a client*: choosing a fresh
//! request id for each call that is not already in flight, remembering it,
//! stamping a transport sequence, and — once a call is out — draining the status
//! ring and deciding, for each message that arrives, whether it answers an
//! outstanding call or is an unsolicited event. This module is that driver.
//!
//! ## One object, the host side of the conversation
//!
//! An [`RpcClient`] owns the [`RpcEndpoint`], the
//! [`Outstanding`](crate::rpc::Outstanding) set of in-flight request ids, and two
//! monotonic counters — the next request id to hand out and the next transport
//! sequence to stamp. `N` is the most calls that may be outstanding at once; it
//! is the endpoint's correlation capacity, chosen by the caller to match how many
//! concurrent calls the driver keeps in flight.
//!
//! * [`call`](RpcClient::call) issues a typed call: it allocates a request id not
//!   currently outstanding (never zero — zero is reserved for events), stamps the
//!   next sequence, posts the request, and only on a successful post records the
//!   id as outstanding. It returns a [`Pending`] naming the id the reply will
//!   echo and the function it calls. If the post is refused (a full command ring,
//!   an over-large payload, a bad layout), nothing is left outstanding and the id
//!   is simply skipped.
//! * [`pump`](RpcClient::pump) advances the client over the status ring by one
//!   message. It decodes the next reply or event into the caller's buffer and
//!   classifies it: a [`Reply`](Pump::Reply) whose id was outstanding (now
//!   resolved) answers a call the caller issued; anything else —
//!   an event, or a reply whose id the client is not waiting on — is
//!   [`Unsolicited`](Pump::Unsolicited); an empty ring is [`Idle`](Pump::Idle).
//!   The caller matches a [`Reply`](Pump::Reply) to the [`Pending`] it holds and
//!   decodes the typed result with [`decode_reply`](RpcClient::decode_reply).
//!
//! A driver loop therefore reads: `call` each request, then `pump` whenever the
//! GSP signals status (on hardware, a status interrupt; in a host test, right
//! after the GSP side posts), handling events and collecting the replies to the
//! calls it is waiting on, until every [`Pending`] is answered.
//!
//! ## The synchronous convenience
//!
//! Most driver calls are a single request answered by a single reply, and the
//! caller wants the typed answer back, not a poll loop. [`transact`](RpcClient::transact)
//! packages the loop above for that case: it posts one typed call and pumps the
//! status ring — routing each unsolicited event to the caller's [`Responder`] and
//! asking it for the next status signal when the ring is empty — until the reply
//! arrives (returned as the decoded typed result) or a caller-set wait budget is
//! spent (the call is forgotten and [`Transact::TimedOut`] is returned). It is
//! the blocking primitive the on-device paths of `NVRTX.EXE` issue their display
//! and memory calls through; the asynchronous `call` / `pump` pair stays for
//! genuinely concurrent calls.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Handing out a request id no other in-flight call is using, matching an echoed
//! id back against the set of ids in flight, and telling a reply apart from an
//! event are all *ordinary computer science* — correct or incorrect on any
//! machine, with no dependence on a real card. So the driver is written and
//! proven here against [`crate::mock::MockSharedMem`] with no pending-hardware
//! caveat, exactly as the envelope, the framing and the ring beneath it are. What
//! it does **not** fix — because the GSP firmware dictates it — stays the
//! firmware's data: the envelope's concrete field offsets (an
//! [`RpcLayout`](crate::rpc::RpcLayout)), the checksum and header offsets of the
//! transport beneath it, and *which* functions the catalog carries. Clean-room:
//! the driver is FREEHOLD's own; no driver source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the client holds only the
//! `Copy` endpoint, the fixed-capacity set and two counters, and every call
//! carries its bytes in caller-provided slices.

use crate::call::{CallScratch, RpcCall};
use crate::codec::WireDecode;
use crate::event::RpcEvent;
use crate::msgq::SharedMem;
use crate::rpc::{Outstanding, RpcEndpoint, RpcError, RpcInbound, RpcKind};

/// A call the host has posted and not yet seen answered.
///
/// [`request_id`](Self::request_id) is the id the GSP echoes in its reply, which
/// [`RpcClient::pump`] resolves and reports; [`function`](Self::function) is the
/// call's function selector ([`RpcCall::FUNCTION`]), carried so the caller can
/// tell *which* call a resolved id belongs to when several of different types are
/// in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The request id the reply will echo (never zero).
    pub request_id: u32,
    /// The function selector the call was posted with.
    pub function: u32,
}

/// What advancing an [`RpcClient`] over the status ring by one message turned up.
///
/// The message's bytes, when there is one, land in the output buffer the caller
/// passed [`pump`](RpcClient::pump); the [`RpcInbound`] carried here describes it
/// so the caller can decode the typed payload with
/// [`decode_reply`](RpcClient::decode_reply) (or
/// [`RpcEndpoint::decode_args`](crate::rpc::RpcEndpoint::decode_args)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pump {
    /// The status ring was empty: no reply or event is waiting. Any call still
    /// outstanding has not been answered yet; the caller should wait for the
    /// GSP's status signal and pump again.
    Idle,
    /// A reply arrived that answers a call that was outstanding. Its id has been
    /// resolved out of the in-flight set, so the caller matches `request_id`
    /// against the [`Pending`] it holds and decodes the typed result from the
    /// output buffer. `inbound.header.status` is the GSP's result word.
    Reply {
        /// The resolved request id (matches a [`Pending::request_id`]).
        request_id: u32,
        /// The decoded message, for slicing and decoding the result.
        inbound: RpcInbound,
    },
    /// A message came off the ring that answers no outstanding call: an
    /// unsolicited event, or a reply whose id the client was not waiting on (a
    /// late duplicate, a reply to an already-resolved or forgotten call, or a
    /// stray). The caller inspects `inbound.header` and handles it, then pumps
    /// again.
    Unsolicited {
        /// The decoded message.
        inbound: RpcInbound,
    },
}

/// How a synchronous [`transact`](RpcClient::transact) ended.
///
/// A transaction posts one typed call and drives the status ring until that
/// call's reply arrives or the wait budget runs out. It is the blocking
/// convenience over the `call` / `pump` loop: the on-device driver paths
/// (`NVRTX.EXE`'s display and memory calls) want "post this request, give me its
/// typed answer", not a hand-rolled poll loop each time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transact<R> {
    /// The awaited reply arrived. `status` is the GSP's result word
    /// ([`RpcHeader::status`](crate::rpc::RpcHeader::status)) and `response` the
    /// decoded typed result ([`RpcCall::Response`]).
    Completed {
        /// The GSP's result status word, carried out of the reply envelope.
        status: u32,
        /// The decoded typed result.
        response: R,
    },
    /// The wait budget was exhausted before the reply arrived. The call has been
    /// [`forget`](RpcClient::forget)ten, so its slot is freed and a reply that
    /// later arrives for it reads as [`Pump::Unsolicited`].
    TimedOut,
}

/// The caller's side of a synchronous [`transact`](RpcClient::transact): what to
/// do when the status ring is empty, and what to do with any message that is not
/// the awaited reply.
///
/// `transact` owns the `&mut M` shared region while it pumps, so it hands that
/// region to [`wait`](Responder::wait) rather than letting the responder keep its
/// own alias. On real hardware [`wait`](Responder::wait) blocks on the GSP's
/// status interrupt and ignores `mem`; in a host test it is where the GSP side
/// posts its reply into `mem` before returning. [`on_event`](Responder::on_event)
/// receives every message the transaction is not waiting on — an unsolicited GSP
/// event, or a stray or late reply — so the caller can handle it without aborting
/// the call in progress.
pub trait Responder<M: SharedMem> {
    /// Called once each time [`transact`](RpcClient::transact) finds the status
    /// ring empty, up to its wait budget. Return `true` to wait again and let the
    /// transaction pump once more; return `false` to give up now (the call is then
    /// forgotten and the transaction returns [`Transact::TimedOut`]). `mem` is the
    /// shared region, offered so a host-side responder can post the GSP's reply;
    /// a hardware responder blocks on the status interrupt and ignores it.
    fn wait(&mut self, mem: &mut M) -> bool;

    /// Called for every message the transaction pumps that is **not** the awaited
    /// reply: an unsolicited event (`bytes` is its payload region, decodable with
    /// [`decode_args`](RpcClient::decode_args) via the carried
    /// [`RpcInbound`]), or a stray/late reply to some other id. The default
    /// drops it. Events never consume the wait budget.
    fn on_event(&mut self, inbound: &RpcInbound, bytes: &[u8]) {
        let _ = (inbound, bytes);
    }
}

/// The host side of one GSP RPC conversation over a command/status ring pair.
///
/// Owns the [`RpcEndpoint`], the fixed-capacity set of in-flight request ids, and
/// the next request id and transport sequence to hand out. `N` bounds how many
/// calls may be outstanding at once. See the module docs for the `call` / `pump`
/// loop this drives.
#[derive(Clone)]
pub struct RpcClient<const N: usize> {
    endpoint: RpcEndpoint,
    outstanding: Outstanding<N>,
    next_request_id: u32,
    next_seq: u32,
}

impl<const N: usize> RpcClient<N> {
    /// A client over `endpoint`, handing out request ids from `1` and transport
    /// sequences from `0`.
    pub fn new(endpoint: RpcEndpoint) -> Self {
        Self::with_start(endpoint, 1, 0)
    }

    /// A client whose first request id is `first_request_id` and first transport
    /// sequence is `first_seq`.
    ///
    /// Request ids wrap and skip zero (reserved for events) and any id currently
    /// outstanding, so `first_request_id` is only where allocation begins; a
    /// `first_request_id` of zero simply means the first id handed out is one.
    /// Useful for exercising the wrap behaviour deterministically.
    pub fn with_start(endpoint: RpcEndpoint, first_request_id: u32, first_seq: u32) -> Self {
        RpcClient {
            endpoint,
            outstanding: Outstanding::new(),
            next_request_id: first_request_id,
            next_seq: first_seq,
        }
    }

    /// The endpoint this client drives.
    pub fn endpoint(&self) -> &RpcEndpoint {
        &self.endpoint
    }

    /// How many calls are outstanding (posted and not yet answered or forgotten).
    pub fn pending_count(&self) -> usize {
        self.outstanding.len()
    }

    /// Whether no call is outstanding.
    pub fn is_idle(&self) -> bool {
        self.outstanding.is_empty()
    }

    /// The most calls that may be outstanding at once (`N`).
    pub fn capacity(&self) -> usize {
        N
    }

    /// Whether `request_id` is currently outstanding.
    pub fn is_outstanding(&self, request_id: u32) -> bool {
        self.outstanding.contains(request_id)
    }

    /// Validate the underlying endpoint (both framers and the envelope layout).
    pub fn validate(&self) -> Result<(), RpcError> {
        self.endpoint.validate()
    }

    /// Hand out the next request id that is non-zero and not already outstanding,
    /// advancing the counter past it.
    ///
    /// Returns `None` only when the in-flight set is full. Otherwise at most `N`
    /// ids are outstanding and zero is the one further forbidden value, so a free
    /// id is found within `N + 2` consecutive candidates by the pigeonhole
    /// argument.
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

    /// Issue the typed call `C` with `request`: allocate a fresh request id,
    /// stamp the next transport sequence, post it on the command ring, and record
    /// the id as outstanding.
    ///
    /// `scratch` carries the two buffers the call is assembled in (see
    /// [`CallScratch`]). On success returns the [`Pending`] naming the id the
    /// reply will echo and the function selector, which the caller keeps to match
    /// against a later [`Pump::Reply`].
    ///
    /// Errors leave the client unchanged with nothing added to the in-flight set:
    /// [`RpcError::TrackerFull`] if `N` calls are already outstanding (drain some
    /// with [`pump`](Self::pump) first), or — if the post itself is refused — the
    /// same encode and transport errors as
    /// [`send_call`](crate::rpc::RpcEndpoint::send_call) (a [`RpcError::Codec`]
    /// for a request too large for `scratch.args`, or a
    /// [`RpcError::Frame`] such as `WouldBlock` on a full command ring).
    pub fn call<C: RpcCall, M: SharedMem>(
        &mut self,
        mem: &mut M,
        scratch: &mut CallScratch<'_>,
        request: &C::Request,
    ) -> Result<Pending, RpcError> {
        let request_id = self.alloc_request_id().ok_or(RpcError::TrackerFull)?;
        let seq = self.next_seq;
        // Post first; only a successful post is remembered, so a refused send
        // leaves nothing outstanding. The allocated id is simply skipped.
        self.endpoint
            .send_call::<C, M>(mem, scratch, seq, request_id, request)?;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.outstanding.remember(request_id)?;
        Ok(Pending {
            request_id,
            function: C::FUNCTION,
        })
    }

    /// Advance the client over the status ring by one message, decoding it into
    /// `out` and classifying it (see [`Pump`]).
    ///
    /// A [`Pump::Reply`] has already resolved its id out of the in-flight set;
    /// the caller decodes its typed result from `out`. A [`Pump::Unsolicited`]
    /// leaves the set unchanged. A [`Pump::Idle`] means the ring was empty.
    ///
    /// Errors are the transport and envelope errors
    /// [`poll_response`](crate::rpc::RpcEndpoint::poll_response) raises (a
    /// [`RpcError::Frame`], [`RpcError::ShortMessage`] or [`RpcError::BadKind`]);
    /// an empty ring is reported as [`Pump::Idle`], not an error.
    pub fn pump<M: SharedMem>(&mut self, mem: &mut M, out: &mut [u8]) -> Result<Pump, RpcError> {
        match self.endpoint.poll_response(mem, out)? {
            None => Ok(Pump::Idle),
            Some(inbound) => {
                let header = inbound.header;
                if header.kind == RpcKind::Reply && self.outstanding.resolve(header.request_id) {
                    Ok(Pump::Reply {
                        request_id: header.request_id,
                        inbound,
                    })
                } else {
                    Ok(Pump::Unsolicited { inbound })
                }
            }
        }
    }

    /// Decode the typed result `C::Response` from a reply's argument region.
    ///
    /// `inbound` is the one a [`Pump::Reply`] carried and `out` the buffer it was
    /// decoded into. A strict exact-length decode: a reply whose argument region
    /// is longer or shorter than the result's shape is rejected
    /// ([`RpcError::Codec`]), not read as garbage. Equivalent to
    /// [`RpcEndpoint::decode_args`](crate::rpc::RpcEndpoint::decode_args) for
    /// `C::Response`.
    pub fn decode_reply<C: RpcCall>(
        &self,
        inbound: &RpcInbound,
        out: &[u8],
    ) -> Result<C::Response, RpcError> {
        self.endpoint.decode_args::<C::Response>(inbound, out)
    }

    /// Decode an arbitrary typed value `T` from a message's argument region —
    /// the escape hatch for an event's payload (a [`Pump::Unsolicited`]), which
    /// has no [`RpcCall`] result type.
    pub fn decode_args<T: WireDecode>(
        &self,
        inbound: &RpcInbound,
        out: &[u8],
    ) -> Result<T, RpcError> {
        self.endpoint.decode_args::<T>(inbound, out)
    }

    /// Decode a pumped message as the typed event `E`, if it is one — the typed
    /// handler for a [`Pump::Unsolicited`].
    ///
    /// `inbound` is the one a [`Pump::Unsolicited`] carried and `out` the buffer
    /// it was decoded into. Returns `Some(payload)` when the message is an event
    /// of [`E::FUNCTION`](crate::event::RpcEvent::FUNCTION), `None` when it is not
    /// this event (a reply, or an event of another function — so a caller can try
    /// the next event type), and [`RpcError::Codec`] when it is this event's
    /// function but its payload does not match. Equivalent to
    /// [`RpcEndpoint::decode_event`](crate::rpc::RpcEndpoint::decode_event) for
    /// `E`.
    pub fn decode_event<E: RpcEvent>(
        &self,
        inbound: &RpcInbound,
        out: &[u8],
    ) -> Result<Option<E::Payload>, RpcError> {
        self.endpoint.decode_event::<E>(inbound, out)
    }

    /// Stop waiting on an outstanding call, returning whether it was in flight.
    ///
    /// The timeout / give-up path: after this, a reply that later arrives for
    /// `pending`'s id is reported as [`Pump::Unsolicited`] (its id is no longer
    /// outstanding), and the slot it held is freed for a new call. `false` means
    /// the call was not outstanding — already answered, already forgotten, or
    /// never issued by this client.
    pub fn forget(&mut self, pending: &Pending) -> bool {
        self.outstanding.resolve(pending.request_id)
    }

    /// Issue the typed call `C` and drive the conversation to its reply in one
    /// blocking step: post `request`, then pump the status ring — handing every
    /// message that is not this call's reply to `responder.on_event` and asking
    /// `responder.wait` for the next status signal whenever the ring is empty —
    /// until the reply arrives or the wait budget runs out.
    ///
    /// This is the synchronous convenience over the [`call`](Self::call) /
    /// [`pump`](Self::pump) loop. On success it returns
    /// [`Transact::Completed`] carrying the GSP's status word and the decoded
    /// typed result. If the status ring is found empty more than `max_waits`
    /// times (or `responder.wait` returns `false`), the call is
    /// [`forget`](Self::forget)ten and [`Transact::TimedOut`] is returned; a reply
    /// that arrives afterward reads as [`Pump::Unsolicited`]. A `max_waits` of `0`
    /// makes this a single non-blocking attempt: pump once, and time out unless
    /// the reply is already waiting.
    ///
    /// `out` is the buffer each pumped message is decoded into (large enough for
    /// the biggest reply or event, as [`pump`](Self::pump) requires); `scratch`
    /// carries the two assembly buffers [`call`](Self::call) needs.
    ///
    /// Unsolicited events do **not** consume the wait budget — only an empty ring
    /// does — so a burst of events cannot time a pending reply out early. Errors
    /// are those of [`call`](Self::call) (notably [`RpcError::TrackerFull`] and the
    /// post's encode/transport errors) and [`pump`](Self::pump) (the transport and
    /// envelope errors), plus a [`RpcError::Codec`] if the matched reply's argument
    /// region does not fit `C::Response`.
    ///
    /// Intended to drive one call at a time: call it when this client is
    /// [`idle`](Self::is_idle). If other calls are outstanding, a reply to one of
    /// them is resolved out of the in-flight set and delivered to
    /// `responder.on_event` like any other non-awaited message; drive genuinely
    /// concurrent calls with [`call`](Self::call) / [`pump`](Self::pump) instead.
    pub fn transact<C, M, R>(
        &mut self,
        mem: &mut M,
        scratch: &mut CallScratch<'_>,
        request: &C::Request,
        out: &mut [u8],
        max_waits: u32,
        responder: &mut R,
    ) -> Result<Transact<C::Response>, RpcError>
    where
        C: RpcCall,
        M: SharedMem,
        R: Responder<M>,
    {
        let pending = self.call::<C, M>(mem, scratch, request)?;
        let mut waits: u32 = 0;
        loop {
            match self.pump(mem, out)? {
                Pump::Reply {
                    request_id,
                    inbound,
                } if request_id == pending.request_id => {
                    let status = inbound.header.status;
                    let response = self.decode_reply::<C>(&inbound, out)?;
                    return Ok(Transact::Completed { status, response });
                }
                Pump::Reply { inbound, .. } | Pump::Unsolicited { inbound } => {
                    responder.on_event(&inbound, out);
                }
                Pump::Idle => {
                    if waits >= max_waits || !responder.wait(mem) {
                        self.forget(&pending);
                        return Ok(Transact::TimedOut);
                    }
                    waits = waits.wrapping_add(1);
                }
            }
        }
    }
}
