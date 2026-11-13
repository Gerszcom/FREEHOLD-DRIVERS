//! Stage 5 groundwork — the GSP RPC function catalog.
//!
//! Every layer beneath this one is *shape without meaning*: the ring
//! ([`crate::msgq`]) moves fixed-size slots, the framing ([`crate::frame`])
//! turns them into variable-length messages, the envelope ([`crate::rpc`])
//! stamps each with a kind / function / request id / status, the codec
//! ([`crate::codec`]) reads and writes typed fields, and the call layer
//! ([`crate::call`]) ties a function number to the request and result *types* it
//! carries through an [`RpcCall`] impl. What none of them fix is **which
//! functions actually exist** — the concrete selectors the GSP answers to and
//! the field layout of each one's request and result. This module is where that
//! catalog lives: one [`RpcCall`] impl per function, added as each is nailed
//! down against the published interface and confirmed on hardware.
//!
//! ## What is settled here, and what is not
//!
//! A catalog entry has two independent parts, and they are settled at different
//! times:
//!
//! * The **machinery** — that an entry is a `const FUNCTION` plus a
//!   [`WireEncode`]/[`WireDecode`] request type and result type, and that
//!   [`RpcEndpoint::send_call`](crate::rpc::RpcEndpoint) / the client's
//!   [`call`](crate::client::RpcClient::call) post, answer and decode it — is
//!   *ordinary computer science*, correct on any machine, and is already proven
//!   host-side (`call.rs`, `client.rs`, `tests/call.rs`).
//! * The **data** — the exact function *number* each GSP call carries and the
//!   *order and width* of the fields in its request and result — is the
//!   firmware's to dictate. It comes from NVIDIA's *published* GSP RPC interface
//!   (an interface fact, like a register offset — no driver source is copied or
//!   translated), and each layout is only *confirmed* on a real card. So a
//!   functional entry (allocate memory, set a display mode, …) is added here
//!   only once its layout is read from that interface and queued for hardware
//!   confirmation under `validation/stage-5/`.
//!
//! ## The first entries are FREEHOLD's own diagnostics
//!
//! The two entries this module ships are deliberately the ones that depend on
//! *no* NVIDIA-proprietary field layout, so the catalog pattern is established
//! and host-proven before any firmware-dictated structure enters it:
//!
//! * [`Keepalive`] — a null request answered by a null reply ([`NoArgs`] both
//!   ways). The liveness probe: a reply at all (matched to its request id)
//!   proves the GSP is draining the command ring and posting on the status ring.
//! * [`Echo`] — a 32-bit cookie the far side returns unchanged. The loopback
//!   self-test of the whole typed-call path: the cookie that comes back must be
//!   the one that went out, so a transport that corrupts, reorders or drops
//!   payload bytes is caught end to end.
//!
//! Both are FREEHOLD's own protocol-level diagnostics, not reconstructions of a
//! vendor message, so their request and result shapes are FREEHOLD's to define
//! and are fully host-verifiable. Their *function numbers* are FREEHOLD working
//! selectors (see [`Keepalive::FUNCTION`] and [`Echo::FUNCTION`]); if a FREEHOLD
//! GSP build ever maps them onto the firmware's own keepalive/version selectors,
//! that mapping is an interface fact confirmed on hardware, and the loopback
//! tests below exercise the full stack whatever the number ends up being.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free.

use crate::call::RpcCall;
use crate::codec::{CodecError, Reader, WireDecode, WireEncode, Writer};

/// An empty RPC payload — a request or result that carries no argument bytes.
///
/// Many calls take no arguments (a keepalive, a reset) or return only the
/// envelope's status word with no result body; this is the [`WireEncode`] /
/// [`WireDecode`] type for that zero-field shape. It encodes to zero bytes and
/// decodes from zero bytes, so a message whose argument region is empty is the
/// valid encoding of `NoArgs` and a message carrying stray argument bytes is
/// rejected by the strict [`decode_exact`](crate::codec::decode_exact) /
/// [`decode_args`](crate::rpc::RpcEndpoint::decode_args) the call layer uses
/// (those call [`Reader::finish`], which fails on the trailing bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoArgs;

impl WireEncode for NoArgs {
    fn encode(&self, _w: &mut Writer<'_>) -> Result<(), CodecError> {
        Ok(())
    }
    fn encoded_len(&self) -> usize {
        0
    }
}

impl WireDecode for NoArgs {
    fn decode(_r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(NoArgs)
    }
}

/// Liveness probe — a null request answered by a null reply.
///
/// The host posts a [`Keepalive`] with no arguments and waits for the reply
/// matched to its request id; the reply arriving at all is the proof that the
/// GSP conversation is live (the firmware is draining the command ring and
/// posting on the status ring). This is the simplest possible catalog entry —
/// [`NoArgs`] in both directions — and exercises the envelope, framing, ring and
/// correlation with an empty payload.
///
/// [`FUNCTION`](RpcCall::FUNCTION) is a FREEHOLD working selector; see the module
/// docs for how it relates to the firmware's own keepalive number.
pub struct Keepalive;

impl Keepalive {
    /// FREEHOLD working selector for the keepalive/null call.
    ///
    /// The number is referenced through [`RpcCall::FUNCTION`]; it is named here
    /// only for documentation and tests. If a FREEHOLD GSP build maps this onto
    /// the firmware's own keepalive message, that number is read from the
    /// published GSP interface and confirmed on hardware
    /// (`validation/stage-5/`).
    pub const FUNCTION: u32 = 0x0000_0001;
}

impl RpcCall for Keepalive {
    const FUNCTION: u32 = Keepalive::FUNCTION;
    type Request = NoArgs;
    type Response = NoArgs;
}

/// Loopback self-test — a 32-bit cookie the far side returns unchanged.
///
/// The host picks a cookie (any 32-bit value — a counter, a nonce), posts it as
/// an [`Echo`] request, and the GSP returns the identical value in the reply.
/// Because request-id correlation already matches a reply to its request, the
/// cookie proves something correlation cannot: that the *payload bytes*
/// survived the round trip intact. A transport that truncated, reordered or
/// corrupted the argument region would return a different cookie (or fail the
/// strict decode), so this is the end-to-end integrity check of the whole typed
/// stack. The request and result are the same [`u32`] shape the codec already
/// round-trips.
///
/// [`FUNCTION`](RpcCall::FUNCTION) is a FREEHOLD working selector; see the module
/// docs.
pub struct Echo;

impl Echo {
    /// FREEHOLD working selector for the echo/loopback call.
    ///
    /// See [`Keepalive::FUNCTION`] for how a working selector relates to the
    /// firmware's own numbering.
    pub const FUNCTION: u32 = 0x0000_0002;
}

impl RpcCall for Echo {
    const FUNCTION: u32 = Echo::FUNCTION;
    type Request = u32;
    type Response = u32;
}
