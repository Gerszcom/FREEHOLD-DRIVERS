//! Stage 5 groundwork — typed RPC calls: the bridge from [`crate::codec`] into
//! [`crate::rpc`].
//!
//! Two layers already exist and do not yet meet. [`crate::rpc`] posts and
//! receives *envelopes* — a kind, a function selector, a request id and a status
//! word — in front of a run of argument bytes it never looks inside.
//! [`crate::codec`] reads and writes a run of *typed fields* — a handle, a
//! 64-bit address, a length, a flag — into and out of a byte buffer, but knows
//! nothing of rings or envelopes. This module is the thin layer that joins them:
//! it ties a **function number** to the **typed request** that function takes and
//! the **typed result** it returns, so a call is issued and answered by naming
//! its struct, not by hand-assembling argument bytes and remembering which
//! function selector goes with them.
//!
//! ## An [`RpcCall`] is one catalog entry
//!
//! The GSP exposes a fixed catalog of functions; each is a request shape, a
//! reply shape and the number that selects it. [`RpcCall`] is exactly that
//! triple — a `const FUNCTION`, a [`WireEncode`] request type and a
//! [`WireDecode`] response type — and nothing else. A future function catalog is
//! a set of zero-sized types each implementing it; because the function number
//! and the two struct shapes are bound together in one `impl`, a caller cannot
//! pair a request with the wrong selector or decode a reply as the wrong type.
//!
//! The methods here are generic over an [`RpcCall`], so they add no per-function
//! code: [`send_call`](RpcEndpoint::send_call) encodes a typed request and posts
//! it as a request envelope; [`send_call_reply`](RpcEndpoint::send_call_reply)
//! encodes a typed result and posts it as a reply; and
//! [`decode_args`](RpcEndpoint::decode_args) reads a typed value back out of a
//! received message's argument region. Correlating a reply to its request, and
//! telling a reply apart from an event, stay the caller's job through the
//! [`Outstanding`](crate::rpc::Outstanding) set and the
//! [`RpcHeader`] the [`crate::rpc`] layer already hands
//! back — this module adds the *typing*, not a second correlation scheme.
//!
//! ## Two scratch buffers
//!
//! Encoding a typed call takes two caller-provided buffers, bundled in a
//! [`CallScratch`]: `args` receives the request's or result's wire fields
//! ([`crate::codec`] writes them there), and `envelope` receives the whole
//! message — the header followed by a copy of those argument bytes
//! ([`crate::rpc`] assembles it there). They must be distinct slices, because
//! the envelope encoder copies the arguments out of one and into the other and
//! the borrow checker forbids the source and destination aliasing; holding them
//! as two separate `&mut` slices makes that a type-level guarantee. Both stay
//! the caller's memory, so this layer allocates nothing, and a strict
//! [`decode_args`](RpcEndpoint::decode_args) reuses the output buffer the
//! message was received into.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Encoding a struct's fields in order, wrapping them in an envelope, reading
//! them back and decoding the fields again are *ordinary computer science* —
//! correct or incorrect on any machine — so this module, like the codec, the
//! envelope, the framing and the ring beneath it, is written and proven here
//! against [`crate::mock::MockSharedMem`] with no pending-hardware caveat. What
//! it does **not** fix — because the GSP firmware dictates it — is *which*
//! functions exist, *what number* each carries and *which fields* its request
//! and result hold: those are the catalog's data, filled in against NVIDIA's
//! published GSP RPC interface and confirmed on hardware
//! (`validation/stage-5/`). Clean-room: the call machinery is FREEHOLD's own; no
//! driver source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free.

use crate::codec::{decode_exact, encode_into, WireDecode, WireEncode};
use crate::frame::FrameHeader;
use crate::msgq::SharedMem;
use crate::rpc::{RpcEndpoint, RpcError, RpcHeader, RpcInbound};

/// The two distinct buffers a typed call is assembled in.
///
/// `args` receives the request's or result's wire fields (the
/// [`crate::codec`] writes them there), and `envelope` receives the whole
/// message — the header followed by a copy of those argument bytes (the
/// [`crate::rpc`] layer assembles it there). They are held as two separate
/// `&mut` slices so the type itself guarantees they do not alias, which the
/// envelope encoder requires (it copies the arguments out of `args` and into
/// `envelope`). `args` must hold at least the encoded value's
/// [`encoded_len`](WireEncode::encoded_len) bytes, and `envelope` at least
/// `layout.header_len` more than that. Both stay the caller's memory; the call
/// layer allocates nothing.
pub struct CallScratch<'a> {
    /// Where the typed value's wire fields are encoded.
    pub args: &'a mut [u8],
    /// Where the full envelope (header + a copy of the argument bytes) is built.
    pub envelope: &'a mut [u8],
}

impl<'a> CallScratch<'a> {
    /// Bundle an argument buffer and an envelope buffer. They must be distinct
    /// slices; borrowing two separate `&mut [u8]`s already enforces that.
    pub fn new(args: &'a mut [u8], envelope: &'a mut [u8]) -> Self {
        CallScratch { args, envelope }
    }
}

/// One entry in the GSP RPC function catalog: a function selector bound to the
/// typed request it takes and the typed result it returns.
///
/// A concrete call is a (usually zero-sized) type implementing this trait, for
/// example:
///
/// ```ignore
/// struct GetMemoryInfo;
/// impl RpcCall for GetMemoryInfo {
///     const FUNCTION: u32 = /* the GSP's function number, from its interface */;
///     type Request = MemoryInfoRequest;   // : WireEncode + WireDecode
///     type Response = MemoryInfoResult;    // : WireEncode + WireDecode
/// }
/// ```
///
/// Binding the three together is what lets [`RpcEndpoint::send_call`] and
/// friends take and return the right types without the caller ever naming the
/// function number or the argument bytes directly. The concrete numbers and
/// field layouts are the firmware's to dictate and are not fixed here (see the
/// module docs); this trait is only the shape every catalog entry has.
pub trait RpcCall {
    /// The function selector the GSP uses to name this call, written into the
    /// envelope's function word.
    const FUNCTION: u32;
    /// The typed arguments the host sends. The host encodes it into the
    /// request's argument bytes; the GSP side (a host test's stand-in, and
    /// conceptually a FREEHOLD GSP emulator) decodes it back — so it is both a
    /// [`WireEncode`] and a [`WireDecode`], and the one struct defines both
    /// directions of the wire shape in one place.
    type Request: WireEncode + WireDecode;
    /// The typed result the GSP returns. The GSP side encodes it into the
    /// reply's argument bytes and the host decodes it back, so it too is both a
    /// [`WireEncode`] and a [`WireDecode`].
    type Response: WireEncode + WireDecode;
}

impl RpcEndpoint {
    /// Post call `C` on the command ring: encode `request` into `scratch.args`,
    /// wrap it in a request envelope for [`C::FUNCTION`](RpcCall::FUNCTION)
    /// tagged with `request_id`, and send it with transport sequence `seq`.
    ///
    /// `scratch` carries the two buffers the call is built in (see
    /// [`CallScratch`]). Returns the [`FrameHeader`] the frame was sent with.
    /// The caller normally also
    /// [`remember`](crate::rpc::Outstanding::remember)s `request_id` so the
    /// reply can be matched, exactly as with
    /// [`send_request`](RpcEndpoint::send_request).
    ///
    /// Errors: [`RpcError::Codec`] if `request` does not fit `scratch.args`,
    /// otherwise the same envelope and transport errors as
    /// [`send_request`](RpcEndpoint::send_request).
    pub fn send_call<C: RpcCall, M: SharedMem>(
        &self,
        mem: &mut M,
        scratch: &mut CallScratch<'_>,
        seq: u32,
        request_id: u32,
        request: &C::Request,
    ) -> Result<FrameHeader, RpcError> {
        let encoded = encode_into(request, scratch.args)?;
        self.send_request(mem, scratch.envelope, seq, request_id, C::FUNCTION, encoded)
    }

    /// Post the reply to call `C` on the status ring (the GSP side): encode
    /// `response` into `args`, wrap it in a reply envelope answering `request`
    /// with result code `status`, and send it with transport sequence `seq`.
    ///
    /// The reply echoes `request`'s function and request id — so the host
    /// matches it with [`Outstanding::resolve`](crate::rpc::Outstanding::resolve)
    /// — exactly as [`send_reply`](RpcEndpoint::send_reply) does; this wrapper
    /// only encodes the typed result into `scratch.args` first. `scratch`
    /// carries the two buffers the reply is built in (see [`CallScratch`]).
    /// `request` is the header a [`recv_request`](RpcEndpoint::recv_request)
    /// decoded.
    pub fn send_call_reply<C: RpcCall, M: SharedMem>(
        &self,
        mem: &mut M,
        scratch: &mut CallScratch<'_>,
        seq: u32,
        request: &RpcHeader,
        status: u32,
        response: &C::Response,
    ) -> Result<FrameHeader, RpcError> {
        let encoded = encode_into(response, scratch.args)?;
        self.send_reply(mem, scratch.envelope, seq, request, status, encoded)
    }

    /// Decode the argument region of a received message as a typed value.
    ///
    /// `inbound` is what [`recv_request`](RpcEndpoint::recv_request) or
    /// [`poll_response`](RpcEndpoint::poll_response) returned, and `out` is the
    /// same buffer the message was received into. The arguments are
    /// `out[header_len..len]` (see [`RpcInbound::args_range`]), and this reads a
    /// `T` from exactly that slice, requiring every argument byte to be consumed
    /// — so a message whose argument region is longer or shorter than `T`'s
    /// shape is rejected rather than silently truncated.
    ///
    /// `T` is typically `C::Request` on the GSP side (after dispatching on
    /// [`RpcHeader::function`](crate::rpc::RpcHeader)) or `C::Response` on the
    /// host side (after matching the request id); an event's payload decodes the
    /// same way. Fails with [`RpcError::Codec`] on any field under/overrun or on
    /// trailing bytes.
    pub fn decode_args<T: WireDecode>(
        &self,
        inbound: &RpcInbound,
        out: &[u8],
    ) -> Result<T, RpcError> {
        let args = &out[inbound.args_range(&self.layout)];
        Ok(decode_exact::<T>(args)?)
    }
}
