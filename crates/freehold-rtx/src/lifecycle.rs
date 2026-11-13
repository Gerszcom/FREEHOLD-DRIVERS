//! Stage 5 groundwork — the whole device bring-up, pre-GSP and post-GSP joined
//! across the one hardware-gated step between them.
//!
//! Two orchestrators already each compose a run of ordered steps into one typed
//! outcome, and each is the host-verifiable core of one half of `NVRTX.EXE`:
//!
//! 1. [`bring_up`](crate::bringup::bring_up) runs Stages 1–5 *before* the GSP
//!    exists — claim the device, probe the silicon, wait for the card's own boot
//!    firmware, read the VRAM size, then choose and verify the GSP firmware — and
//!    hands back a [`Bringup`] whose [`FirmwareStatus`] is either a verified image
//!    ready to load or a recorded reason it is absent (on a card that does not
//!    need the GSP for its display path).
//! 2. [`bring_channel_online`](crate::online::bring_channel_online) runs the three
//!    steps *after* the GSP is booted — plan the shared region, establish a
//!    validated endpoint over the mapped region, and health-check the channel —
//!    and hands back a ready [`RpcClient`](crate::client::RpcClient).
//!
//! Between the two sits exactly one step neither performs and only hardware can:
//! taking a card with *verified* firmware, loading that image onto it, starting
//! the GSP processor and waiting for it to come up, and asking the kernel
//! (`DRIVER_MAP`) to allocate and map the shared command/status region. Until now
//! nothing joined the two orchestrators across that step: `NVRTX.EXE` would have
//! to call [`bring_up`](crate::bringup::bring_up), branch on its
//! [`FirmwareStatus`] by hand, do the firmware load / GSP boot / region map, then
//! call [`bring_channel_online`](crate::online::bring_channel_online) and fold two
//! more error enums into whatever it reports — exactly the hand-wiring
//! [`bring_channel_online`](crate::online::bring_channel_online) itself was built
//! to remove one layer down (its own module docs make that case for plan /
//! establish / health).
//!
//! This module is that join, one level up. [`bring_device_online`] is the top of
//! the bring-up tree: it runs the pre-GSP sequence, and then, *only* for a card
//! whose firmware verified, drives the one gated step through a caller-supplied
//! [`GspBoot`] seam and composes the post-GSP sequence over the region that step
//! returns, producing one typed [`DeviceOnline`] with one [`LifecycleError`].
//!
//! ## The two outcomes, and why they are exactly `bring_up`'s two firmware states
//!
//! [`bring_up`](crate::bringup::bring_up) already made the degrade-or-fail
//! decision: a card that needs the GSP for its display path (Ada, Blackwell) with
//! no verified firmware is a *failure*
//! ([`BringupError::FirmwareRequired`](crate::bringup::BringupError::FirmwareRequired)),
//! while a card that does not (Turing, Ampere) with no verified firmware is
//! *degraded but up* ([`FirmwareStatus::Absent`]). So a [`Bringup`] that returns
//! `Ok` has only two firmware states, and they map one-to-one onto this module's
//! two success outcomes:
//!
//! * [`FirmwareStatus::Verified`] → [`DeviceOnline::Full`]: the firmware is loaded,
//!   the GSP is booted, and the channel is online and health-proven.
//! * [`FirmwareStatus::Absent`] → [`DeviceOnline::Degraded`]: the card is claimed,
//!   probed and sized, running without the GSP. No channel is brought up, because
//!   there is no booted GSP to talk to — and that is sound precisely because
//!   [`bring_up`](crate::bringup::bring_up) only ever reports `Absent` for a card
//!   that does not need the GSP for display.
//!
//! The policy this module adds, then, is one line: *bring the GSP channel online
//! whenever — and only when — verified firmware is in hand.* A family that could
//! in principle be driven both ways (Turing/Ampere, which do not require the GSP
//! yet may still use it) is brought up *with* the GSP when its firmware is present
//! and verified; a future "prefer the non-GSP display path even when firmware is
//! present" refinement would live here, but booting the GSP you have verified
//! firmware for is the natural default and keeps the composition honest.
//!
//! ## Where the one gated step sits
//!
//! Each composed orchestrator already leaves its own single hardware hole as a
//! caller-supplied parameter: [`bring_up`](crate::bringup::bring_up) takes a
//! [`FirmwareStore`](crate::bringup::FirmwareStore) (the VFS on FREEHOLD, a table
//! on the host) and a `delay` closure, and
//! [`bring_channel_online`](crate::online::bring_channel_online) takes the mapped
//! region. This module adds the hole *between* them — the firmware load, the GSP
//! boot and the region mapping — as the [`GspBoot`] seam, in the same spirit: on
//! FREEHOLD `NVRTX.EXE` implements it by writing the firmware through BAR0,
//! starting the processor and making the `DRIVER_MAP` call; on the host a mock
//! boots a simulated GSP and hands back a [`MockSharedMem`](crate::mock::MockSharedMem).
//! So the whole device bring-up spans from a mapped BAR0 to a health-proven GSP
//! channel with exactly three caller-filled holes — firmware bytes, the boot &
//! map, and the shared region's far side — and every seam between them is this
//! crate's own host-verified logic.
//!
//! ## Why this is host-verifiable, and what is not
//!
//! Every layer it composes is itself host-verified with no hardware caveat, and
//! the seam is a parameter, so the sequencing, the branch onto the two outcomes,
//! and the folding of three phases' errors into one are ordinary logic, correct on
//! any machine. This is **fully host-verified with no hardware caveat**, like the
//! two orchestrators it joins: the tests drive the whole sequence against a
//! [`MockGpu`](crate::mock::MockGpu), an in-memory firmware store, a mock
//! [`GspBoot`] seam and a [`MockSharedMem`](crate::mock::MockSharedMem) with a
//! simulated GSP. What stays the firmware's data is unchanged from the layers it
//! composes; what only a real card settles is that the firmware loads and the GSP
//! boots as the seam's mock pretends, and that a real mapped region behaves as the
//! mock does (`validation/stage-5/`).
//!
//! Clean-room: sequencing FREEHOLD's own bring-up steps and reporting which phase
//! failed is FREEHOLD's own code; no driver source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free: the orchestrator holds no
//! memory of its own; the [`Bringup`] is a `Copy` description, the region is the
//! seam's, the client owns only its `Copy` endpoint, a fixed-capacity set and two
//! counters, and every byte buffer is the caller's.

use crate::bringup::{
    bring_up, Bringup, BringupConfig, BringupError, FirmwareStatus, FirmwareStore,
};
use crate::call::CallScratch;
use crate::carveout::plan;
use crate::client::Responder;
use crate::firmware::FirmwareSelection;
use crate::mmio::Mmio;
use crate::msgq::SharedMem;
use crate::online::{bring_channel_online, ChannelConfig, ChannelError, ChannelOnline};

/// The one hardware-gated step between the pre-GSP and post-GSP halves of device
/// bring-up: load the verified firmware, boot the GSP, and map the shared region.
///
/// [`bring_device_online`] reaches this seam only once a card's firmware has been
/// selected and verified ([`FirmwareStatus::Verified`]). The implementor's job is
/// the three things this crate cannot do on the host:
///
/// 1. place the firmware image where the GSP expects it and start the processor,
/// 2. wait for the GSP to report itself booted, and
/// 3. allocate and map `region_len` bytes of shared command/status memory — the
///    [`Carveout::total_len`](crate::carveout::Carveout::total_len) the planner
///    computed — returning that mapped region.
///
/// `firmware` names the file to load and the digest its bytes were verified
/// against ([`FirmwareSelection`]); the implementor obtains the bytes however it
/// must (re-reading `C:\FIRMWARE\` by
/// [`firmware.request.file_basename`](crate::firmware::FirmwareRequest::file_basename)
/// through the VFS on FREEHOLD; held in the mock on the host). The returned region
/// must be at least `region_len` bytes; it is handed straight to
/// [`bring_channel_online`](crate::online::bring_channel_online).
///
/// On the host this is a mock that boots a simulated GSP and returns a
/// [`MockSharedMem`](crate::mock::MockSharedMem); on FREEHOLD it is `NVRTX.EXE`'s
/// own firmware-load / processor-boot / `DRIVER_MAP` code. Keeping it a trait is
/// what lets the whole device bring-up run on the development machine.
pub trait GspBoot {
    /// The shared command/status region this seam maps, which the post-boot
    /// channel bring-up then drives (a real mapped region on FREEHOLD, a
    /// [`MockSharedMem`](crate::mock::MockSharedMem) on the host).
    type Region: SharedMem;
    /// Why the firmware load, GSP boot or region mapping could not complete. It is
    /// the implementor's own type, carried verbatim by
    /// [`LifecycleError::GspBoot`].
    type Error;

    /// Load `firmware` onto the card, boot the GSP, then allocate and map
    /// `region_len` bytes of shared memory and return the mapped region.
    fn boot_and_map(
        &mut self,
        firmware: &FirmwareSelection<'_>,
        region_len: usize,
    ) -> Result<Self::Region, Self::Error>;
}

/// A card brought as far online as it can go, in one of the two states a
/// successful [`bring_up`](crate::bringup::bring_up) leaves it in.
///
/// `N` is the online channel's in-flight correlation capacity (see
/// [`ChannelOnline`]); `M` is the shared region the [`GspBoot`] seam mapped. The
/// lifetime `'s` is the firmware store's: a verified [`FirmwareStatus`] inside the
/// carried [`Bringup`] borrows the sidecar text from it.
///
/// The two variants differ in size — [`Full`](Self::Full) also carries the region
/// and the channel — but this crate is allocation-free (`no_std`, no `Box`), so the
/// larger variant is held inline by design rather than boxed; a caller matches it
/// out at once. On FREEHOLD the region type `M` is a thin mapped-handle, not the
/// bytes.
#[allow(clippy::large_enum_variant)]
pub enum DeviceOnline<'s, const N: usize, M> {
    /// The card had verified GSP firmware: it was loaded, the GSP booted and the
    /// command/status channel brought online and health-proven. Carries the
    /// pre-GSP [`Bringup`] (probe, VRAM, the verified firmware selection), the
    /// mapped shared region the seam returned (kept so the caller can keep driving
    /// the client), and the online channel.
    Full {
        /// The pre-GSP bring-up result; its `firmware` is a
        /// [`FirmwareStatus::Verified`].
        bringup: Bringup<'s>,
        /// The shared region the [`GspBoot`] seam mapped and the channel was
        /// established over. The returned channel's client reads and writes it on
        /// every call, so the caller keeps it alive here.
        region: M,
        /// The online, health-proven channel: the carveout, the health verdict and
        /// the idle [`RpcClient`](crate::client::RpcClient).
        channel: ChannelOnline<N>,
    },
    /// The card did not need the GSP for its display path (Turing, Ampere) and no
    /// verified firmware was available, so it is online but *degraded*: claimed,
    /// probed and sized, driven without the GSP. No channel is brought up — there
    /// is no booted GSP to talk to.
    Degraded {
        /// The pre-GSP bring-up result; its `firmware` is a
        /// [`FirmwareStatus::Absent`] recording why no GSP firmware was loaded.
        bringup: Bringup<'s>,
    },
}

impl<const N: usize, M> DeviceOnline<'_, N, M> {
    /// Whether the GSP channel is online (a [`DeviceOnline::Full`]). `false` for a
    /// [`DeviceOnline::Degraded`] card running without the GSP.
    pub fn is_full(&self) -> bool {
        matches!(self, DeviceOnline::Full { .. })
    }

    /// The pre-GSP [`Bringup`] either outcome carries.
    pub fn bringup(&self) -> &Bringup<'_> {
        match self {
            DeviceOnline::Full { bringup, .. } | DeviceOnline::Degraded { bringup } => bringup,
        }
    }
}

/// Why a device could not be brought online, tagged with the phase that failed.
///
/// The three variants are the three phases, strictly in order, so a later variant
/// implies the earlier phases succeeded: a [`GspBoot`](Self::GspBoot) means the
/// pre-GSP bring-up already succeeded with verified firmware, and a
/// [`Channel`](Self::Channel) means the firmware also loaded, the GSP booted and
/// the region mapped. `E` is the [`GspBoot`] seam's own error type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleError<E> {
    /// The pre-GSP bring-up failed: the device was not claimed, did not probe
    /// live, did not report boot-complete, had no usable VRAM size, or needed the
    /// GSP for display with no verified firmware
    /// ([`BringupError::FirmwareRequired`](crate::bringup::BringupError::FirmwareRequired)).
    /// The [`GspBoot`] seam is never reached. Carries the underlying
    /// [`BringupError`].
    Bringup(BringupError),
    /// The firmware had verified, but the [`GspBoot`] seam failed to load it, boot
    /// the GSP or map the region. Carries the seam's own error; the channel
    /// bring-up never runs.
    GspBoot(E),
    /// The GSP booted and the region mapped, but the channel could not be brought
    /// online: the carveout would not plan, the plan did not fit the mapped region
    /// or its wire geometry was inconsistent, or the booted GSP failed its health
    /// check. Carries the underlying [`ChannelError`].
    Channel(ChannelError),
}

impl<E> From<ChannelError> for LifecycleError<E> {
    fn from(err: ChannelError) -> Self {
        LifecycleError::Channel(err)
    }
}

/// Bring one device online end to end: the pre-GSP sequence, then — only for a
/// card with verified firmware — the gated firmware load / GSP boot / region map,
/// then the post-GSP channel bring-up.
///
/// This is the top of `NVRTX.EXE`'s bring-up tree. `device` is the parsed
/// `DRIVER_DEVICE` record and `bar0` its already-mapped register aperture (the
/// same inputs [`bring_up`](crate::bringup::bring_up) takes); `store`,
/// `bringup_config` and `delay` are [`bring_up`](crate::bringup::bring_up)'s
/// firmware source, boot-poll budget and poll timer; `channel_config`, `scratch`,
/// `out` and `responder` are
/// [`bring_channel_online`](crate::online::bring_channel_online)'s channel
/// geometry, call-assembly buffers, decode buffer and the caller's side of the
/// status wait; and `gsp` is the one new piece — the [`GspBoot`] seam that loads
/// the firmware, boots the GSP and maps the shared region.
///
/// The steps, in order, so the first failure is the one reported:
///
/// 1. **Pre-GSP bring-up.** [`bring_up`](crate::bringup::bring_up) claims, probes,
///    waits for boot, reads VRAM, and selects and verifies the firmware
///    ([`LifecycleError::Bringup`]).
/// 2. **Branch.** A [`FirmwareStatus::Absent`] card is returned at once as
///    [`DeviceOnline::Degraded`] — it does not need the GSP, so there is nothing
///    to boot. A [`FirmwareStatus::Verified`] card continues:
/// 3. **Plan.** The carveout is planned to size the shared region the seam must
///    map. A plan error short-circuits here, *before* the GSP is booted, so a
///    channel that cannot even be laid out never costs a firmware load
///    ([`LifecycleError::Channel`] with a
///    [`ChannelError::Plan`](crate::online::ChannelError::Plan)).
/// 4. **Boot & map (gated).** The [`GspBoot`] seam loads the verified firmware,
///    boots the GSP and maps `total_len` bytes ([`LifecycleError::GspBoot`]).
/// 5. **Channel.** [`bring_channel_online`](crate::online::bring_channel_online)
///    establishes and health-checks the channel over the mapped region
///    ([`LifecycleError::Channel`]).
///
/// The carveout is planned once here, to size the region for the seam;
/// [`bring_channel_online`](crate::online::bring_channel_online) re-derives the
/// identical plan internally (planning is pure, cheap arithmetic), which keeps
/// that function self-contained rather than threading the plan through the seam.
///
/// On a [`DeviceOnline::Full`] the firmware is loaded, the GSP booted and the
/// channel idle and health-proven, so the caller's next step is its first
/// functional call through `channel.client` (reading and writing the returned
/// `region`).
///
/// # Errors
///
/// Returns the first [`LifecycleError`] above whose phase fails. A
/// [`LifecycleError::Bringup`] or a plan-phase [`LifecycleError::Channel`] is
/// reported before the [`GspBoot`] seam is ever invoked, so neither loads
/// firmware or boots a GSP.
#[allow(clippy::too_many_arguments)]
pub fn bring_device_online<'s, const N: usize, B, S, G, R>(
    device: &crate::device::DeviceInfo,
    bar0: &B,
    store: &'s S,
    bringup_config: BringupConfig,
    delay: impl FnMut(),
    channel_config: &ChannelConfig<'_>,
    gsp: &mut G,
    scratch: &mut CallScratch<'_>,
    out: &mut [u8],
    responder: &mut R,
) -> Result<DeviceOnline<'s, N, G::Region>, LifecycleError<G::Error>>
where
    B: Mmio,
    S: FirmwareStore,
    G: GspBoot,
    R: Responder<G::Region>,
{
    // 1. Pre-GSP bring-up: claim, probe, boot, VRAM, firmware select + verify.
    let bringup =
        bring_up(device, bar0, store, bringup_config, delay).map_err(LifecycleError::Bringup)?;

    // 2. Branch on the firmware outcome. An Absent firmware is only ever reported
    //    for a card that does not need the GSP for display, so there is nothing to
    //    boot: the card is online, degraded.
    let selection = match bringup.firmware {
        FirmwareStatus::Verified(selection) => selection,
        FirmwareStatus::Absent(_) => {
            return Ok(DeviceOnline::Degraded { bringup });
        }
    };

    // 3. Plan the carveout to learn the region size the seam must map. A plan
    //    failure short-circuits before the GSP is booted — no firmware load is
    //    spent on a channel that cannot be laid out.
    let carveout = plan(
        &channel_config.command,
        &channel_config.status,
        channel_config.region_align,
    )
    .map_err(ChannelError::from)?;

    // 4. The gated seam: load the verified firmware, boot the GSP, map the region.
    let mut region = gsp
        .boot_and_map(&selection, carveout.total_len)
        .map_err(LifecycleError::GspBoot)?;

    // 5. Post-GSP channel bring-up over the mapped region.
    let channel = bring_channel_online::<N, _, _>(
        channel_config,
        &mut region,
        carveout.total_len,
        scratch,
        out,
        responder,
    )?;

    Ok(DeviceOnline::Full {
        bringup,
        region,
        channel,
    })
}
