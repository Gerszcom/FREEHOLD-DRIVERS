//! Bring-up orchestration — the ordered sequence a FREEHOLD user-mode driver
//! runs once its device's BAR0 register aperture is mapped.
//!
//! Stages 1–5 each solve one piece of bringing an RTX card up: claim it
//! ([`crate::identify`]), read what silicon it is ([`crate::probe`]), wait for
//! the card's own boot firmware and read the VRAM size ([`crate::boot`]), and
//! work out which GSP firmware it needs and verify that file
//! ([`crate::firmware`]). This module is the piece that runs them *in order*,
//! as the real driver (`NVRTX.EXE`) will, and turns the result into one typed
//! outcome a driver and `DRIVERS` can report.
//!
//! It is written entirely over this crate's existing, already-tested pieces,
//! so it adds no new hardware knowledge — only the sequencing and the policy
//! that ties them together:
//!
//! * **Order.** These steps have a required order: a card must report its boot
//!   firmware complete *before* its VRAM size is valid (`DIRECTION.md` §4, and
//!   [`crate::boot`]), and the silicon must be known *before* the right
//!   firmware can be chosen. [`bring_up`] runs them in that order and stops at
//!   the first step that cannot proceed.
//! * **Graceful degradation.** FREEHOLD's rule is that a device whose firmware
//!   is missing "says so and does what it can without it" (`DIRECTION.md` §4,
//!   aim 7). Turing and Ampere do not need the GSP for their display path, so a
//!   missing or unverifiable firmware file leaves them *degraded*, not failed:
//!   the card is still claimed, probed and sized. Ada and Blackwell bring their
//!   display up through the GSP, so for them the same missing firmware is
//!   fatal. The documented difference is already carried by
//!   [`FirmwareRequest::display_requires_gsp`]; this module is where it decides
//!   the outcome.
//!
//! What this module does **not** do is touch hardware or the kernel: the
//! caller has already mapped BAR0 (the `Mmio` it is handed), and firmware bytes
//! are read through the [`FirmwareStore`] the caller supplies — on the host a
//! mock, on FREEHOLD a thin wrapper over the VFS. So the whole sequence,
//! including the degrade-or-fail decision, is exercised on the development
//! machine against [`crate::mock`] GPUs. It stays `no_std`,
//! `forbid(unsafe_code)` and allocation-free: every firmware string borrows
//! from the store.
//!
//! The one thing still left for later stages is what happens *after* a
//! successful bring-up with verified firmware — placing the image where the GSP
//! expects it and starting the processor (`ROADMAP.md`, Stage 5). This module
//! takes the card exactly to the point where that load can begin, and reports a
//! [`FirmwareStatus::Verified`] carrying the selection the loader will use.

use crate::boot::{self, BootError, VramInfo};
use crate::device::DeviceInfo;
use crate::firmware::{self, FirmwareSelection, SelectError, SidecarError, VerifyError};
use crate::mmio::Mmio;
use crate::probe::{self, GpuProbe, ProbeError};

/// The firmware files FREEHOLD keeps for one device: the image, and the
/// sidecar text beside it.
///
/// Both borrow from the [`FirmwareStore`] that produced them, so nothing is
/// copied. `sidecar_text` is the `Vendor`/`Version`/`Hash`/`Licence` file
/// ([`crate::firmware::FirmwareSidecar`]); `image` is the firmware itself,
/// whose SHA-256 is checked against the sidecar's digest.
#[derive(Clone, Copy, Debug)]
pub struct FirmwareFiles<'a> {
    /// The sidecar text, parsed by [`crate::firmware::FirmwareSidecar::parse`].
    pub sidecar_text: &'a str,
    /// The raw firmware image bytes, hashed and verified before loading.
    pub image: &'a [u8],
}

/// Where firmware files come from, abstracted so bring-up never reads a file
/// itself.
///
/// On FREEHOLD this is a thin wrapper over the VFS, reading `C:\FIRMWARE\` by
/// the device-named base name ([`FirmwareRequest::file_basename`]); on the host
/// it is a table in memory. Returning `None` means the file is simply not
/// present — a legitimate state FREEHOLD reports (aim 7), not an error. The
/// returned borrows live as long as the store is borrowed, so the selection
/// bring-up produces can keep pointing into them without allocating.
///
/// [`FirmwareRequest::file_basename`]: crate::firmware::FirmwareRequest::file_basename
pub trait FirmwareStore {
    /// Fetch the firmware image and sidecar for `basename`, or `None` if the
    /// device has no firmware file placed for it.
    fn load(&self, basename: &str) -> Option<FirmwareFiles<'_>>;
}

/// How bring-up tunes the wait for the card's boot firmware.
///
/// The actual delay between polls is still the caller's (the FREEHOLD runtime
/// owns the timer; see [`crate::boot::wait_gfw_boot`]); this only bounds how
/// many times bring-up looks before it calls the wait a timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BringupConfig {
    /// Maximum number of `GFW_BOOT_STATUS` polls before a live-but-not-complete
    /// card is reported as [`BootError::Timeout`]. `0` is treated as one look.
    pub max_boot_polls: u32,
}

impl Default for BringupConfig {
    /// A reasonable default poll budget. The real cadence is set by the delay
    /// the caller supplies; this is only the count.
    fn default() -> Self {
        BringupConfig {
            max_boot_polls: 1000,
        }
    }
}

/// Why a usable, verified firmware was not available for a card.
///
/// On a card that does not need the GSP for display (Turing, Ampere) this is
/// not fatal: it is carried inside [`FirmwareStatus::Absent`] and the card
/// still comes up. On a card that does (Ada, Blackwell) the same reason is
/// fatal and surfaces as [`BringupError::FirmwareRequired`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareUnavailable {
    /// The probe's family is not one this build knows, so it cannot say which
    /// firmware the card wants ([`crate::firmware::required_firmware`] returned
    /// `None`).
    UnknownFamily,
    /// No firmware file was placed for this device ([`FirmwareStore::load`]
    /// returned `None`).
    FileMissing,
    /// The sidecar beside the firmware did not parse.
    Sidecar(SidecarError),
    /// The sidecar parsed but was not acceptable for this card (wrong vendor,
    /// or no verifiable hash).
    Select(SelectError),
    /// The firmware bytes did not match the sidecar's digest — a corrupt or
    /// wrong-version file. Carries the digest actually computed.
    Verify(VerifyError),
}

/// The firmware outcome of a successful bring-up.
///
/// A bring-up that returns [`Ok`] has a card that is claimed, alive, booted and
/// sized. This says what became of its firmware: either a verified image ready
/// for the loader, or — only on a card that does not need the GSP for
/// display — a recorded reason it is absent, with the card running degraded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareStatus<'a> {
    /// The firmware file was found and its bytes verified against the sidecar's
    /// digest. The GSP firmware load (a later stage) begins from this
    /// selection.
    Verified(FirmwareSelection<'a>),
    /// No verified firmware, but this card does not need the GSP for its
    /// display path, so it comes up anyway. The reason is kept for `DRIVERS`.
    Absent(FirmwareUnavailable),
}

impl FirmwareStatus<'_> {
    /// Whether a verified firmware is in hand (the GSP load may begin).
    pub fn is_verified(&self) -> bool {
        matches!(self, FirmwareStatus::Verified(_))
    }
}

/// A card brought up to the point the GSP firmware load can begin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bringup<'a> {
    /// What the silicon reported ([`crate::probe`]).
    pub probe: GpuProbe,
    /// The usable VRAM size the boot firmware computed ([`crate::boot`]).
    pub vram: VramInfo,
    /// What became of this card's GSP firmware.
    pub firmware: FirmwareStatus<'a>,
}

/// Why bring-up could not complete.
///
/// Each variant is the first step that could not proceed; bring-up stops
/// there. A [`BringupError::FirmwareRequired`] is the one that depends on the
/// card: it is returned only when the card needs the GSP for display and no
/// verified firmware was available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BringupError {
    /// The device is not one this driver claims — not an NVIDIA display GPU
    /// ([`crate::identify::is_supported`]). A driver should not have been bound
    /// to it; bring-up refuses rather than probe a foreign device.
    NotClaimed,
    /// The device's BAR0 is not a mappable memory aperture, so there is nothing
    /// to probe ([`DeviceInfo::register_aperture`] returned `None`).
    NoRegisterAperture,
    /// The register aperture did not read back a live GPU ([`crate::probe`]).
    Probe(ProbeError),
    /// The card's boot firmware did not report complete ([`crate::boot`]).
    Boot(BootError),
    /// The card booted but the framebuffer-range register did not read back a
    /// valid VRAM size (dead or not-yet-written). The card is live but its
    /// reported size is unusable, so bring-up does not guess one.
    VramUnavailable,
    /// The card needs the GSP for its display path, but no verified firmware
    /// was available; the carried reason says why.
    FirmwareRequired(FirmwareUnavailable),
}

/// Bring one card up, in order, from a mapped BAR0 to the point its GSP
/// firmware can be loaded.
///
/// `device` is the parsed `DRIVER_DEVICE` record ([`DeviceInfo`]); `bar0` is
/// its register aperture, already mapped by the caller (`DRIVER_MAP` on
/// FREEHOLD, a [`crate::mock`] on the host). `store` supplies firmware files,
/// `config` bounds the boot wait, and `delay` spaces the boot polls with the
/// caller's timer.
///
/// The steps, each stopping bring-up on failure:
///
/// 1. **Claim.** Refuse a device this driver does not own
///    ([`BringupError::NotClaimed`]).
/// 2. **Aperture.** Confirm BAR0 is a mappable memory aperture
///    ([`BringupError::NoRegisterAperture`]). The caller has already mapped it;
///    this guards against being handed a device whose BAR0 is absent or an I/O
///    BAR.
/// 3. **Probe.** Read `PMC_BOOT_0` and decode the silicon
///    ([`BringupError::Probe`]).
/// 4. **Boot.** Wait for the card's own boot firmware to report complete
///    ([`BringupError::Boot`]).
/// 5. **VRAM.** Read the usable VRAM size the firmware left
///    ([`BringupError::VramUnavailable`]).
/// 6. **Firmware.** Choose, parse, select and verify the GSP firmware. A
///    verified image yields [`FirmwareStatus::Verified`]; an unavailable one
///    yields [`FirmwareStatus::Absent`] on a card that does not need the GSP
///    for display, or [`BringupError::FirmwareRequired`] on one that does.
///
/// On success the card is claimed, alive, booted, sized, and either has
/// verified firmware ready to load or is running degraded without it.
pub fn bring_up<'s, M, S>(
    device: &DeviceInfo,
    bar0: &M,
    store: &'s S,
    config: BringupConfig,
    delay: impl FnMut(),
) -> Result<Bringup<'s>, BringupError>
where
    M: Mmio,
    S: FirmwareStore,
{
    // 1. Claim: only an NVIDIA display GPU is ours to bring up.
    if !device.is_supported() {
        return Err(BringupError::NotClaimed);
    }

    // 2. Aperture: BAR0 must be a mappable memory aperture. The caller mapped
    //    it to hand us `bar0`, but a device with no usable BAR0 should never
    //    have got this far — refuse it explicitly rather than probe blindly.
    if device.register_aperture().is_none() {
        return Err(BringupError::NoRegisterAperture);
    }

    // 3. Probe: read and decode the silicon.
    let probe = probe::probe(bar0).map_err(BringupError::Probe)?;

    // 4. Boot: wait for the card's own boot firmware to finish. Its VRAM size
    //    is not valid until it has.
    boot::wait_gfw_boot(bar0, config.max_boot_polls, delay).map_err(BringupError::Boot)?;

    // 5. VRAM: read the usable size the firmware computed.
    let vram = boot::vram_size(bar0).ok_or(BringupError::VramUnavailable)?;

    // 6. Firmware: choose, verify, and decide degrade-or-fail by whether this
    //    card needs the GSP for its display path.
    let firmware = resolve_firmware(&probe, store)?;

    Ok(Bringup {
        probe,
        vram,
        firmware,
    })
}

/// Resolve the GSP firmware for a probed card into a [`FirmwareStatus`], or a
/// fatal [`BringupError::FirmwareRequired`].
///
/// Walks the firmware pipeline — which file the card needs, read it, parse the
/// sidecar, select it for this card, verify the bytes — and on any failure
/// decides from [`FirmwareRequest::display_requires_gsp`] whether the card can
/// still come up without it.
///
/// [`FirmwareRequest::display_requires_gsp`]: crate::firmware::FirmwareRequest::display_requires_gsp
fn resolve_firmware<'s, S: FirmwareStore>(
    probe: &GpuProbe,
    store: &'s S,
) -> Result<FirmwareStatus<'s>, BringupError> {
    // Which firmware does this card want? An unknown family has no answer; the
    // card is still identified and alive, so this degrades rather than fails
    // (and an unknown family cannot be asserted to need the GSP).
    let request = match firmware::required_firmware(probe) {
        Some(request) => request,
        None => {
            return Ok(FirmwareStatus::Absent(FirmwareUnavailable::UnknownFamily));
        }
    };

    // Try to produce a verified selection; a failure is classified by reason.
    let outcome = load_and_verify(probe, request.file_basename, store);

    match outcome {
        Ok(selection) => Ok(FirmwareStatus::Verified(selection)),
        Err(reason) => {
            if request.display_requires_gsp {
                // Ada / Blackwell: the display path runs through the GSP, so
                // no verified firmware means the card cannot come up.
                Err(BringupError::FirmwareRequired(reason))
            } else {
                // Turing / Ampere: degrade — claimed, probed, sized, but no GSP.
                Ok(FirmwareStatus::Absent(reason))
            }
        }
    }
}

/// Read, select and verify the firmware for `basename`, returning the verified
/// selection or the first reason it was unavailable.
fn load_and_verify<'s, S: FirmwareStore>(
    probe: &GpuProbe,
    basename: &str,
    store: &'s S,
) -> Result<FirmwareSelection<'s>, FirmwareUnavailable> {
    let files = store
        .load(basename)
        .ok_or(FirmwareUnavailable::FileMissing)?;
    let sidecar = firmware::FirmwareSidecar::parse(files.sidecar_text)
        .map_err(FirmwareUnavailable::Sidecar)?;
    let selection = firmware::select(probe, sidecar).map_err(FirmwareUnavailable::Select)?;
    selection
        .verify(files.image)
        .map_err(FirmwareUnavailable::Verify)?;
    Ok(selection)
}
