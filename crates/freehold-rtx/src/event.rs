//! Stage 5 groundwork — typed RPC events: the event-direction analogue of
//! [`crate::call`].
//!
//! The GSP conversation ([`crate::rpc`]) carries three kinds of message:
//! host→GSP **requests**, GSP→host **replies**, and GSP→host unsolicited
//! **events** ([`RpcKind`]). The typed call layer ([`crate::call`]) already
//! covers the first two: an [`RpcCall`](crate::call::RpcCall) ties a function
//! number to the typed request it carries and the typed result it returns, and
//! [`send_call`](crate::rpc::RpcEndpoint::send_call) /
//! [`send_call_reply`](crate::rpc::RpcEndpoint::send_call_reply) /
//! [`decode_args`](crate::rpc::RpcEndpoint::decode_args) post and read them by
//! naming a struct, never by hand-assembling argument bytes. The **event** kind
//! has had no such typing: the raw [`send_event`](crate::rpc::RpcEndpoint::send_event)
//! posts opaque bytes, and a host that receives one
//! ([`Pump::Unsolicited`](crate::client::Pump::Unsolicited)) must match the
//! envelope's function word by hand and call the untyped
//! [`decode_args`](crate::rpc::RpcEndpoint::decode_args) itself. This module
//! closes that asymmetry.
//!
//! ## An [`RpcEvent`] is one event-catalog entry
//!
//! [`RpcEvent`] is to an event what [`RpcCall`](crate::call::RpcCall) is to a
//! call: a `const FUNCTION` bound to the single [`WireEncode`]/[`WireDecode`]
//! **payload** type that event carries. An event answers no request, so it has
//! one typed body rather than a request/result pair, and the host both selects
//! it *and* decodes it by naming one struct. A future event catalog is a set of
//! (usually zero-sized) types each implementing this trait, exactly as the
//! function catalog ([`crate::catalog`]) is for calls.
//!
//! ## The two generic methods
//!
//! Like the call layer, the methods here are generic over an [`RpcEvent`], so
//! they add no per-event code:
//!
//! * [`emit_event`](RpcEndpoint::emit_event) is the GSP side (a host test's
//!   stand-in, and conceptually a FREEHOLD GSP emulator): it encodes a typed
//!   payload and posts it on the status ring as an event envelope for
//!   [`E::FUNCTION`](RpcEvent::FUNCTION), the typed analogue of the raw
//!   [`send_event`](crate::rpc::RpcEndpoint::send_event).
//! * [`decode_event`](RpcEndpoint::decode_event) is the host side: given a
//!   message taken off the status ring, it returns `Some(payload)` **only** when
//!   that message is an [`Event`](RpcKind::Event) whose function is
//!   [`E::FUNCTION`](RpcEvent::FUNCTION), and `None` when it is anything else — a
//!   reply, or an event of a different function. That `None` is what makes a
//!   *dispatch chain* clean: a caller tries `decode_event::<A>`, then
//!   `decode_event::<B>`, and the first `Some` is the match, with no manual
//!   inspection of the kind and function words. A message that *is* this event's
//!   function but whose payload does not fit `E::Payload`'s shape is a real error
//!   ([`RpcError::Codec`]), not a "not mine" — a corrupt event is caught, never
//!   silently skipped.
//!
//! Reusing [`CallScratch`] for [`emit_event`](RpcEndpoint::emit_event) keeps the
//! two-buffer discipline (the payload fields, then the whole envelope) identical
//! to a call's; the event simply has one body instead of a request and a result.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Tagging a byte run with a kind and a function, posting it, and reading the
//! tag back to pick a decode type are *ordinary computer science* — correct or
//! incorrect on any machine — so this module, like the call layer, the codec,
//! the envelope and the ring beneath it, is written and proven here against
//! [`crate::mock::MockSharedMem`] with no pending-hardware caveat. Like
//! [`crate::call`], it ships as **mechanism only**: *which* events a real GSP
//! posts, *what number* each carries and *which fields* its payload holds are
//! the firmware's data, read from NVIDIA's published GSP interface and confirmed
//! on a card (`validation/stage-5/`), and a concrete event entry joins the
//! catalog only then — exactly as the function catalog's vendor calls do.
//! Clean-room: the event typing is FREEHOLD's own; no driver source is copied or
//! translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free.

use crate::call::CallScratch;
use crate::codec::{encode_into, WireDecode, WireEncode};
use crate::frame::FrameHeader;
use crate::msgq::SharedMem;
use crate::rpc::{RpcEndpoint, RpcError, RpcInbound, RpcKind};

/// One entry in the GSP RPC event catalog: a function selector bound to the
/// single typed payload the event carries.
///
/// A concrete event is a (usually zero-sized) type implementing this trait, for
/// example:
///
/// ```ignore
/// struct DisplayHotplug;
/// impl RpcEvent for DisplayHotplug {
///     const FUNCTION: u32 = /* the GSP's event number, from its interface */;
///     type Payload = HotplugInfo; // : WireEncode + WireDecode
/// }
/// ```
///
/// An event answers no request, so — unlike an [`RpcCall`](crate::call::RpcCall),
/// which has a request type *and* a result type — it has exactly one body type,
/// used both to post the event (the GSP side encodes it) and to decode it (the
/// host decodes it). The concrete number and field layout are the firmware's to
/// dictate and are not fixed here (see the module docs); this trait is only the
/// shape every event entry has.
pub trait RpcEvent {
    /// The function selector the GSP tags this event with, written into the
    /// envelope's function word.
    const FUNCTION: u32;
    /// The typed payload the event carries. The GSP side encodes it into the
    /// event's argument bytes and the host decodes it back, so it is both a
    /// [`WireEncode`] and a [`WireDecode`] — one struct defining both directions
    /// of the wire shape in one place.
    type Payload: WireEncode + WireDecode;
}

impl RpcEndpoint {
    /// Post the typed event `E` on the status ring (the GSP side): encode
    /// `payload` into `scratch.args`, wrap it in an event envelope for
    /// [`E::FUNCTION`](RpcEvent::FUNCTION) (request id and status zero, by the
    /// event convention), and send it with transport sequence `seq`.
    ///
    /// The typed analogue of [`send_event`](Self::send_event): it only encodes
    /// the typed payload into `scratch.args` first, then posts it exactly as the
    /// raw path does. `scratch` carries the two buffers the event is built in
    /// (see [`CallScratch`]). Returns the [`FrameHeader`] the frame was sent
    /// with.
    ///
    /// Errors: [`RpcError::Codec`] if `payload` does not fit `scratch.args`,
    /// otherwise the same envelope and transport errors as
    /// [`send_event`](Self::send_event).
    pub fn emit_event<E: RpcEvent, M: SharedMem>(
        &self,
        mem: &mut M,
        scratch: &mut CallScratch<'_>,
        seq: u32,
        payload: &E::Payload,
    ) -> Result<FrameHeader, RpcError> {
        let encoded = encode_into(payload, scratch.args)?;
        self.send_event(mem, scratch.envelope, seq, E::FUNCTION, encoded)
    }

    /// Decode a received message as the typed event `E`, if it is one.
    ///
    /// `inbound` is what [`poll_response`](Self::poll_response) returned (or the
    /// [`RpcInbound`] a [`Pump::Unsolicited`](crate::client::Pump::Unsolicited)
    /// carried), and `out` is the buffer that message was received into.
    ///
    /// Returns:
    /// * `Ok(Some(payload))` when `inbound` is an [`Event`](RpcKind::Event)
    ///   whose function is [`E::FUNCTION`](RpcEvent::FUNCTION) and whose argument
    ///   region decodes exactly as `E::Payload`;
    /// * `Ok(None)` when `inbound` is **not** this event — a reply, or an event
    ///   of some other function — so a caller can try the next event type in a
    ///   dispatch chain without inspecting the header by hand;
    /// * `Err(RpcError::Codec)` when `inbound` *is* this event's function but its
    ///   payload does not match `E::Payload`'s shape (a field under/overrun or
    ///   trailing bytes) — a corrupt event of the right kind is an error, not a
    ///   miss.
    ///
    /// The decode is strict and exact (via
    /// [`decode_args`](Self::decode_args)): every argument byte must be consumed,
    /// so an event whose region is longer or shorter than the payload's shape is
    /// rejected rather than read as garbage.
    pub fn decode_event<E: RpcEvent>(
        &self,
        inbound: &RpcInbound,
        out: &[u8],
    ) -> Result<Option<E::Payload>, RpcError> {
        if inbound.header.kind != RpcKind::Event || inbound.header.function != E::FUNCTION {
            return Ok(None);
        }
        let payload = self.decode_args::<E::Payload>(inbound, out)?;
        Ok(Some(payload))
    }
}
