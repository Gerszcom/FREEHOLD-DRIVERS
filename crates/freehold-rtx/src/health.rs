//! Stage 5 groundwork — the GSP channel health-check.
//!
//! Every layer below this one builds the *mechanism* of the host↔GSP
//! conversation: the ring ([`crate::msgq`]), the framing ([`crate::frame`]),
//! the envelope ([`crate::rpc`]), the typed codec and calls ([`crate::codec`],
//! [`crate::call`]), the catalog's first diagnostics ([`crate::catalog`]), and
//! the client that issues calls and drains replies ([`crate::client`]). The
//! catalog ships two FREEHOLD-own diagnostics — [`Keepalive`] (a null request
//! answered by a null reply) and [`Echo`] (a 32-bit cookie returned unchanged) —
//! but until now *nothing calls them*: they are exercised only by their own
//! round-trip tests. This module is their first real caller.
//!
//! ## What it is for
//!
//! When `NVRTX.EXE` has loaded the GSP firmware, booted the processor and
//! brought up the command/status queues, it must not start issuing functional
//! calls (allocate memory, set a display mode) until it has established two
//! things, in order:
//!
//! 1. **Liveness** — the GSP is actually draining the command ring and posting
//!    on the status ring. A [`Keepalive`] whose reply comes back, matched to its
//!    request id, is the proof: a reply *at all* means the far side is running
//!    the conversation. [`ping`] is that one call.
//! 2. **Integrity** — the transport carries payload bytes faithfully, with no
//!    stuck bits, truncation or reordering between the host's buffer and the
//!    GSP's. An [`Echo`] cookie that comes back bit-for-bit equal to the one
//!    sent is the proof; a sweep of several cookies with complementary bit
//!    patterns ([`PROBE_COOKIES`]) catches a line stuck high or low.
//!    [`check_channel`] runs the liveness call and then that sweep.
//!
//! Only once both hold is the channel trustworthy enough to carry the
//! firmware-dictated functional calls that come later.
//!
//! ## Why this is host-verifiable, and firmware-independent
//!
//! The verdict this module reaches depends on **no** firmware data. Liveness is
//! "did a correlated reply arrive", which the [`client`](crate::client) layer
//! already settles with no dependence on a card; integrity is "did the cookie
//! come back unchanged", which is a property of the bytes alone. Neither asks
//! what the GSP's status word *means* — that meaning is the firmware's, and this
//! module deliberately never judges on it: the keepalive's status word is
//! carried out of [`ping`] and [`Health`] purely as information, and the health
//! verdict is the reply's arrival and the cookie's fidelity, not any status
//! constant. Both diagnostics are FREEHOLD's own protocol-level calls, so, like
//! every layer they compose, this is **fully host-verified with no hardware
//! caveat**: the tests drive a simulated GSP (a [`Responder`](crate::client::Responder)
//! that answers whichever diagnostic is on the command ring) against a
//! [`MockSharedMem`](crate::mock::MockSharedMem).
//!
//! What stays the firmware's data is unchanged from the layers beneath: the
//! envelope offsets, the transport's checksum and header offsets, and *which*
//! functional calls the catalog carries. This check uses only the two
//! layout-independent diagnostics, so it is complete as it stands.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the caller supplies the
//! client, the shared region, the scratch buffers and the cookie slice; this
//! module adds only the sequencing.

use crate::call::CallScratch;
use crate::catalog::{Echo, Keepalive, NoArgs};
use crate::client::{Responder, RpcClient, Transact};
use crate::msgq::SharedMem;
use crate::rpc::RpcError;

/// A default sweep of loopback cookies with complementary bit patterns.
///
/// All-zeros and all-ones catch a payload line stuck low or high; the two
/// alternating patterns (`0xA5A5_A5A5` and its complement `0x5A5A_5A5A`) catch
/// adjacent-bit shorts and byte-order faults. Passing these to
/// [`check_channel`] exercises every bit of a 32-bit word in both states. A
/// caller is free to supply its own cookies (a counter, a nonce) instead.
pub const PROBE_COOKIES: [u32; 4] = [0x0000_0000, 0xFFFF_FFFF, 0xA5A5_A5A5, 0x5A5A_5A5A];

/// The outcome of a successful [`check_channel`]: the channel answered the
/// liveness probe and every integrity cookie came back intact.
///
/// `keepalive_status` is the GSP's status word from the keepalive reply, carried
/// out purely as information — the liveness verdict is that the reply *arrived*,
/// not this value, since what a status word means is the firmware's to define.
/// `echoes_verified` is how many cookies round-tripped bit-for-bit (the whole
/// slice, when `check_channel` returns `Ok`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Health {
    /// The GSP's status word from the keepalive reply (informational only).
    pub keepalive_status: u32,
    /// How many integrity cookies were confirmed to round-trip unchanged.
    pub echoes_verified: usize,
}

/// Why a [`check_channel`] (or the liveness step of one) did not pass.
///
/// A timeout means the wait budget was spent before the awaited reply arrived —
/// on a real card, the GSP never answered within the time allowed; the call has
/// been forgotten, so a reply that arrives afterward reads as an unsolicited
/// message. A mismatch means the loopback came back *different*, so the
/// transport is corrupting payload. [`Rpc`](HealthError::Rpc) is a lower-layer
/// transport or envelope fault surfaced unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthError {
    /// The keepalive reply never arrived within the wait budget: the GSP is not
    /// draining the command ring or not posting status. The channel is not live.
    KeepaliveTimedOut,
    /// An echo reply never arrived within the wait budget. `cookie` is the value
    /// that was sent and left unanswered.
    EchoTimedOut {
        /// The cookie whose reply never came.
        cookie: u32,
    },
    /// An echo reply arrived but carried a different value than was sent: the
    /// transport corrupted, truncated or reordered the payload bytes.
    EchoMismatch {
        /// The cookie the host sent.
        sent: u32,
        /// The value the GSP returned instead.
        got: u32,
    },
    /// A transport or envelope error from the RPC layers beneath
    /// ([`RpcError`]): a refused post, a malformed message, a full in-flight set.
    Rpc(RpcError),
}

impl From<RpcError> for HealthError {
    fn from(err: RpcError) -> Self {
        HealthError::Rpc(err)
    }
}

/// Liveness probe — post one [`Keepalive`] and wait for its reply.
///
/// Returns `Ok(Some(status))` if a reply came back (the channel is live;
/// `status` is the GSP's result word, informational — see [`Health`]),
/// `Ok(None)` if the wait budget was spent first (no reply), or a transport
/// [`RpcError`] from the layers beneath. This is the single call a driver sends
/// to confirm an idle channel is still alive, and the first step of
/// [`check_channel`].
///
/// `max_waits` is the wait budget passed to
/// [`transact`](RpcClient::transact): how many times an empty status ring may be
/// waited on before giving up. `responder` is the caller's side of the wait — on
/// a real card it blocks on the GSP's status interrupt; in a host test it is
/// where the simulated GSP posts the reply. `scratch` must hold a [`NoArgs`]
/// request and its envelope, and `out` the reply.
///
/// Call this when the client is [`idle`](RpcClient::is_idle): it drives one call
/// to completion.
pub fn ping<const N: usize, M, R>(
    client: &mut RpcClient<N>,
    mem: &mut M,
    scratch: &mut CallScratch<'_>,
    out: &mut [u8],
    max_waits: u32,
    responder: &mut R,
) -> Result<Option<u32>, RpcError>
where
    M: SharedMem,
    R: Responder<M>,
{
    match client.transact::<Keepalive, M, R>(mem, scratch, &NoArgs, out, max_waits, responder)? {
        Transact::Completed { status, .. } => Ok(Some(status)),
        Transact::TimedOut => Ok(None),
    }
}

/// Full channel health-check — liveness, then an integrity sweep.
///
/// Runs one [`ping`] (liveness) and then, if it answered, one [`Echo`]
/// transaction per cookie in `cookies` (integrity). On success returns a
/// [`Health`] carrying the keepalive's status word and the number of cookies
/// confirmed (the whole slice). It fails at the first problem:
/// [`HealthError::KeepaliveTimedOut`] if the channel is not live,
/// [`HealthError::EchoTimedOut`] if a cookie's reply never comes,
/// [`HealthError::EchoMismatch`] if a cookie comes back changed, or
/// [`HealthError::Rpc`] for a lower-layer transport fault.
///
/// An empty `cookies` slice makes this a liveness-only check — the keepalive
/// still runs, and `echoes_verified` is `0`. [`PROBE_COOKIES`] is a ready sweep
/// of complementary bit patterns.
///
/// `max_waits`, `responder`, `scratch` and `out` are as for [`ping`]; `scratch`
/// must also hold an [`Echo`] request (a `u32`). Each call is driven to
/// completion in turn, so the client is [`idle`](RpcClient::is_idle) between
/// them; call this when it is idle to begin with.
pub fn check_channel<const N: usize, M, R>(
    client: &mut RpcClient<N>,
    mem: &mut M,
    scratch: &mut CallScratch<'_>,
    out: &mut [u8],
    cookies: &[u32],
    max_waits: u32,
    responder: &mut R,
) -> Result<Health, HealthError>
where
    M: SharedMem,
    R: Responder<M>,
{
    let keepalive_status = match ping(client, mem, scratch, out, max_waits, responder)? {
        Some(status) => status,
        None => return Err(HealthError::KeepaliveTimedOut),
    };

    for &cookie in cookies {
        match client.transact::<Echo, M, R>(mem, scratch, &cookie, out, max_waits, responder)? {
            Transact::Completed { response, .. } => {
                if response != cookie {
                    return Err(HealthError::EchoMismatch {
                        sent: cookie,
                        got: response,
                    });
                }
            }
            Transact::TimedOut => return Err(HealthError::EchoTimedOut { cookie }),
        }
    }

    Ok(Health {
        keepalive_status,
        echoes_verified: cookies.len(),
    })
}
