//! Stage 5 groundwork — bringing a GSP channel online in one ordered step.
//!
//! Three modules already settle the three things a host must do, *in order*,
//! after a card's GSP firmware is loaded and the processor is booted, to make
//! the command/status channel usable:
//!
//! 1. [`crate::carveout::plan`] sizes the shared region and places the two
//!    queues disjointly within it — the plan the Stage-3 `DRIVER_MAP` kernel
//!    call is handed to allocate and map.
//! 2. [`crate::establish::establish`] turns that plan plus the firmware's wire
//!    geometry and the *mapped* region into a validated, initialised
//!    [`RpcEndpoint`](crate::rpc::RpcEndpoint) — every cross-layer invariant
//!    checked and both queues zeroed to empty.
//! 3. [`crate::health::check_channel`] drives FREEHOLD's own two diagnostics
//!    over that endpoint — a [`Keepalive`](crate::catalog::Keepalive) for
//!    *liveness* and an [`Echo`](crate::catalog::Echo) sweep for *integrity* —
//!    so the first functional call is never issued over a channel that has not
//!    proved it carries a faithful round trip.
//!
//! Each of those has its own entry point and its own error type, and until now
//! nothing joined them: a caller (`NVRTX.EXE`, the moment the kernel hands back
//! the mapped region) had to call all three by hand, thread the
//! [`Carveout`](crate::carveout::Carveout) from the first into the second and
//! the [`RpcEndpoint`](crate::rpc::RpcEndpoint) from the second into a client
//! for the third, and fold three distinct error enums into whatever it reports.
//! This module is that join. It is the post-boot analogue of
//! [`bring_up`](crate::bringup::bring_up): where `bring_up` composes Stages 1–5
//! (claim → probe → boot → VRAM → firmware) into one typed outcome and is the
//! host-verifiable core of the *first* half of `NVRTX.EXE`,
//! [`bring_channel_online`] composes plan → establish → health into one typed
//! outcome and is the host-verifiable core of the *second* half — everything
//! `NVRTX.EXE` does between "the GSP is booted" and "issue the first functional
//! call", save the one gated step in the middle (the allocation and mapping
//! itself).
//!
//! ## What it adds over calling the three by hand
//!
//! * **One ordered sequence with one error.** [`ChannelError`] attributes a
//!   failure to the phase that produced it — [`Plan`](ChannelError::Plan),
//!   [`Establish`](ChannelError::Establish) or [`Health`](ChannelError::Health)
//!   — so a caller matches one enum instead of three, and the ordering is a
//!   guarantee of the type: a [`Health`](ChannelError::Health) can only be
//!   returned once planning and establishment have *already* succeeded, so the
//!   health verdict is never reached over an unestablished channel.
//! * **The ready client, not just the endpoint.** [`establish`](crate::establish::establish)
//!   returns a bare [`RpcEndpoint`](crate::rpc::RpcEndpoint); a caller still has
//!   to wrap it in an [`RpcClient`](crate::client::RpcClient) before it can issue
//!   or drain calls. [`bring_channel_online`] does that wrapping, runs the
//!   health check *through* that same client, and hands the client back in
//!   [`ChannelOnline`] — idle, validated, and already proven to carry a
//!   conversation — so the caller's next step is its first functional call, not
//!   more wiring.
//!
//! ## Where the gated step sits
//!
//! The one thing between planning and establishing — asking the kernel to
//! allocate and map `carveout.total_len` bytes (the Stage-3 `DRIVER_MAP` call) —
//! is not performed here, exactly as [`establish`](crate::establish::establish)
//! does not perform it: the caller passes the mapped region (`mem`) and its size
//! (`region_len`), and on the host that region is a
//! [`MockSharedMem`](crate::mock::MockSharedMem). So this orchestrator spans the
//! whole post-boot sequence with a single hole where the kernel mapping goes,
//! and that hole is the caller's to fill with the real `DRIVER_MAP` result on
//! FREEHOLD or with a mock region on the host.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Every layer it composes is itself host-verified with no hardware caveat — the
//! arithmetic of the carveout, the cross-layer checks of establishment, and the
//! liveness/integrity verdict of the health check, which depends on *no*
//! firmware data (a reply arriving and a cookie returned unchanged, never what a
//! status word means). Composing three host-verified steps and folding their
//! errors adds no new dependence on a card, so this is **fully host-verified
//! with no hardware caveat**, like [`bring_up`](crate::bringup::bring_up) and the
//! layers beneath it: the tests drive the whole sequence against a
//! [`MockSharedMem`](crate::mock::MockSharedMem) and a simulated GSP. What stays
//! the firmware's data is unchanged from the layers it composes — each queue's
//! internal geometry and alignment (a [`QueueRequest`](crate::carveout::QueueRequest)),
//! the frame/envelope offsets and transport checksum (a
//! [`ChannelGeometry`](crate::establish::ChannelGeometry)) — and what only a card
//! settles is that a real mapped region behaves as the mock does and that the
//! real GSP answers the diagnostics (`validation/stage-5/`).
//!
//! Clean-room: sequencing three FREEHOLD-own steps and reporting which one
//! failed is FREEHOLD's own code; no driver source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the orchestrator holds no
//! memory of its own; the plan and the endpoint are `Copy` descriptions, the
//! client owns only the `Copy` endpoint, a fixed-capacity set and two counters,
//! and every byte buffer is the caller's.

use crate::call::CallScratch;
use crate::carveout::{plan, Carveout, CarveoutError, QueueRequest};
use crate::client::{Responder, RpcClient};
use crate::establish::{establish, ChannelGeometry, EstablishError};
use crate::health::{check_channel, Health, HealthError};
use crate::msgq::SharedMem;

/// The firmware- and policy-supplied inputs a channel bring-up needs, gathered
/// in one place.
///
/// Everything here is *data* a caller supplies, not logic: the two queues'
/// internal shapes and alignments, the region alignment, the wire geometry, and
/// the health-check policy (which loopback cookies to sweep and how long to wait
/// for each reply). The mapped region itself and its size are passed to
/// [`bring_channel_online`] separately, because the region is the one input that
/// comes from the (gated) kernel mapping rather than from configuration.
#[derive(Clone, Copy)]
pub struct ChannelConfig<'c> {
    /// The command queue's internal layout and base alignment (host → GSP).
    pub command: QueueRequest,
    /// The status queue's internal layout and base alignment (GSP → host).
    pub status: QueueRequest,
    /// The alignment of the whole carveout's base, typically a page. The planned
    /// [`Carveout::total_len`](crate::carveout::Carveout::total_len) is a whole
    /// multiple of it. See [`crate::carveout::plan`].
    pub region_align: usize,
    /// The firmware's frame and envelope offsets and the transport checksum. See
    /// [`ChannelGeometry`].
    pub geometry: ChannelGeometry,
    /// The loopback cookies the integrity sweep sends, each of which must come
    /// back bit-for-bit. An empty slice makes the health check liveness-only.
    /// [`PROBE_COOKIES`](crate::health::PROBE_COOKIES) is a ready sweep of
    /// complementary bit patterns.
    pub cookies: &'c [u32],
    /// The per-call wait budget passed to
    /// [`check_channel`](crate::health::check_channel): how many times an empty
    /// status ring may be waited on before a diagnostic is declared timed out.
    pub max_waits: u32,
}

/// A GSP channel brought fully online: the plan it was built on, the health it
/// proved, and the ready client to drive it.
///
/// Returned by [`bring_channel_online`] once planning, establishment and the
/// health check have all succeeded. The `client` is idle (the health check's
/// calls have all resolved), its endpoint's two rings are exactly the planner's
/// absolute layouts, and the channel has carried a real keepalive/echo
/// conversation — so the caller's next step is its first functional call.
///
/// `N` is the client's in-flight correlation capacity, chosen by the caller for
/// the concurrent functional calls it will issue later; the health check itself
/// never keeps more than one call outstanding, so any `N >= 1` suffices to reach
/// this point.
pub struct ChannelOnline<const N: usize> {
    /// The carveout [`crate::carveout::plan`] produced: the two queues' absolute
    /// layouts and the region's total size (what `DRIVER_MAP` was asked to map).
    pub carveout: Carveout,
    /// The health verdict: the keepalive's status word (informational) and the
    /// number of integrity cookies confirmed.
    pub health: Health,
    /// The ready, validated, idle client over the established endpoint — the
    /// handle the caller issues its functional calls through.
    pub client: RpcClient<N>,
}

/// Why a channel could not be brought online, tagged with the phase that failed.
///
/// The three variants mirror the three composed steps and occur strictly in
/// order: a [`Plan`](Self::Plan) means the geometry could not be laid out at all;
/// an [`Establish`](Self::Establish) means a valid plan did not fit the mapped
/// region or the wire geometry was inconsistent; a [`Health`](Self::Health) means
/// the channel was established but the GSP did not answer, or answered corrupt.
/// Because the phases run in order, a later variant implies the earlier phases
/// succeeded — a [`Health`](Self::Health) error is proof the carveout planned and
/// the endpoint established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// The carveout could not be planned from the two queue requests and the
    /// region alignment. Carries the underlying [`CarveoutError`].
    Plan(CarveoutError),
    /// The plan could not be established over the mapped region: it did not fit,
    /// the wire geometry was inconsistent, or the rings overlapped. Carries the
    /// underlying [`EstablishError`].
    Establish(EstablishError),
    /// The channel was established but failed its health check: the GSP did not
    /// answer the keepalive or an echo, or an echo came back changed. Carries the
    /// underlying [`HealthError`].
    Health(HealthError),
}

impl From<CarveoutError> for ChannelError {
    fn from(err: CarveoutError) -> Self {
        ChannelError::Plan(err)
    }
}

impl From<EstablishError> for ChannelError {
    fn from(err: EstablishError) -> Self {
        ChannelError::Establish(err)
    }
}

impl From<HealthError> for ChannelError {
    fn from(err: HealthError) -> Self {
        ChannelError::Health(err)
    }
}

/// Bring a GSP channel online in one ordered step: plan the carveout, establish
/// the endpoint over the mapped region, and verify the channel's health.
///
/// This is the post-boot core of `NVRTX.EXE`. On FREEHOLD the caller has, by
/// this point, booted the GSP firmware and asked the kernel (`DRIVER_MAP`) to
/// allocate and map `config`-sized shared memory; it passes that region as `mem`
/// and its size as `region_len`. On the host `mem` is a
/// [`MockSharedMem`](crate::mock::MockSharedMem) and `region_len` is its length.
///
/// The steps, in order, so the first failure is the one reported:
///
/// 1. [`crate::carveout::plan`] lays the two queues out in one region
///    ([`ChannelError::Plan`]);
/// 2. [`crate::establish::establish`] validates the plan against the mapped
///    region and the wire geometry and initialises both queues
///    ([`ChannelError::Establish`]);
/// 3. the established endpoint is wrapped in an [`RpcClient<N>`](crate::client::RpcClient)
///    and [`crate::health::check_channel`] drives the liveness and integrity
///    diagnostics through it ([`ChannelError::Health`]).
///
/// On success the queues are initialised, the channel has carried a real
/// conversation, and the returned [`ChannelOnline`] carries the plan, the health
/// verdict and the idle client for the caller's first functional call.
///
/// `scratch` carries the two assembly buffers a call is built in (large enough
/// for either diagnostic — a [`NoArgs`](crate::catalog::NoArgs) or a `u32`
/// cookie — and its envelope); `out` is the buffer each reply is decoded into;
/// `responder` is the caller's side of the wait (on hardware it blocks on the
/// GSP's status interrupt; in a host test it is where the simulated GSP posts its
/// reply). All three are as [`check_channel`](crate::health::check_channel)
/// requires them.
///
/// # Errors
///
/// Returns the first [`ChannelError`] above whose phase fails. A
/// [`ChannelError::Plan`] leaves `mem` untouched; a [`ChannelError::Establish`]
/// leaves `mem` untouched (establishment initialises the queues only after every
/// check passes); a [`ChannelError::Health`] leaves the queues established and
/// drained (the failed call is forgotten), so a caller may retry the health check
/// over the same established channel without re-establishing it.
pub fn bring_channel_online<const N: usize, M, R>(
    config: &ChannelConfig<'_>,
    mem: &mut M,
    region_len: usize,
    scratch: &mut CallScratch<'_>,
    out: &mut [u8],
    responder: &mut R,
) -> Result<ChannelOnline<N>, ChannelError>
where
    M: SharedMem,
    R: Responder<M>,
{
    // 1. Lay out the two queues in one region.
    let carveout = plan(&config.command, &config.status, config.region_align)?;

    // 2. Validate the plan against the mapped region and the wire geometry, and
    //    bring both queues to their empty start state.
    let endpoint = establish(mem, &carveout, region_len, &config.geometry)?;

    // 3. Drive the liveness and integrity diagnostics over the established
    //    endpoint through a fresh, idle client.
    let mut client = RpcClient::<N>::new(endpoint);
    let health = check_channel(
        &mut client,
        mem,
        scratch,
        out,
        config.cookies,
        config.max_waits,
        responder,
    )?;

    Ok(ChannelOnline {
        carveout,
        health,
        client,
    })
}
