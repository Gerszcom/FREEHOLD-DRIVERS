//! Stage 4 — devinit / GFW boot-complete, and the framebuffer size the
//! firmware reports.
//!
//! On Turing and newer, the card runs its own boot firmware (GFW / devinit)
//! before a host driver should do anything beyond identifying the chip
//! ([`crate::probe`]). That firmware reads the board's VBIOS, trains the
//! memory, applies the straps and computes the usable VRAM size. A driver's
//! job at this stage is only to *wait* for the firmware to finish and then to
//! read back the VRAM size it left — not to drive any of that work itself.
//!
//! Both steps are pure register logic: poll a progress byte in a secure
//! scratch register until it reads "completed", then decode a size out of the
//! framebuffer-range register. They go through the [`Mmio`] trait like every
//! other hardware access here, so the polling loop and the size decode are
//! exercised on the host against a [`crate::mock`] GPU — including a mock
//! whose status flips from in-progress to complete across successive reads.
//! See `ROADMAP.md` (Stage 4).
//!
//! Clean-room: the register offsets and field layouts live in [`crate::regs`]
//! as documented hardware facts; this module is FREEHOLD's own logic over
//! them. It is `no_std` and `forbid(unsafe_code)`; the real delay between
//! polls is the caller's to supply (the FREEHOLD runtime has the timer), so
//! this crate stays free of any notion of time.

use crate::mmio::Mmio;
use crate::regs;

/// Why waiting for the GFW boot to complete did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootError {
    /// The scratch register read back all-ones every time: the aperture is
    /// not a live GPU (wrong BAR, powered off, or nothing mapped). This is
    /// kept distinct from a timeout because the two call for different
    /// responses — a dead aperture is a setup error, a timeout is a card
    /// that is alive but whose firmware never reported done.
    NotResponding,
    /// The firmware was still live and progressing (or idle) but had not
    /// reported completion within the allotted number of polls. The last
    /// progress byte seen is carried for diagnostics.
    Timeout { last_progress: u8 },
}

/// Read the GFW boot status register once and report where the firmware is.
///
/// `None` means the aperture read back all-ones — not a live device. `Some`
/// carries the raw status word; [`regs::gfw_boot_complete`] and
/// [`regs::gfw_boot_progress`] decode it.
pub fn gfw_boot_status<M: Mmio>(bar0: &M) -> Option<u32> {
    let status = bar0.read32(regs::GFW_BOOT_STATUS);
    (status != 0xFFFF_FFFF).then_some(status)
}

/// Wait for the card's own boot firmware (GFW / devinit) to report complete.
///
/// Polls [`regs::GFW_BOOT_STATUS`] up to `max_polls` times, calling `delay`
/// between reads so the caller can space the polls with a real timer. Returns
/// as soon as the firmware reports completion.
///
/// * [`BootError::NotResponding`] — every read was all-ones (dead aperture).
/// * [`BootError::Timeout`] — the aperture was live but never reported
///   complete within `max_polls`; the last progress byte is returned.
///
/// `max_polls` of 0 is treated as "one look": the status is always read at
/// least once, so a card that is already done succeeds without any delay.
pub fn wait_gfw_boot<M: Mmio>(
    bar0: &M,
    max_polls: u32,
    mut delay: impl FnMut(),
) -> Result<(), BootError> {
    let attempts = max_polls.max(1);
    let mut saw_live = false;
    let mut last_progress = 0u8;
    for attempt in 0..attempts {
        if let Some(status) = gfw_boot_status(bar0) {
            saw_live = true;
            if regs::gfw_boot_complete(status) {
                return Ok(());
            }
            last_progress = regs::gfw_boot_progress(status);
        }
        // Space out the polls, but never pay the delay after the final look.
        if attempt + 1 < attempts {
            delay();
        }
    }
    if saw_live {
        Err(BootError::Timeout { last_progress })
    } else {
        Err(BootError::NotResponding)
    }
}

/// The card's local VRAM, as the boot firmware reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VramInfo {
    /// The raw local memory size in bytes, before any ECC reservation.
    pub total_bytes: u64,
    /// The size a driver may actually allocate from: the same as
    /// `total_bytes` without ECC, or fifteen sixteenths of it with ECC on.
    pub usable_bytes: u64,
    /// Whether ECC is enabled, which is why `usable_bytes` may be smaller.
    pub ecc_enabled: bool,
}

/// Read and decode the usable VRAM size from the framebuffer-range register.
///
/// Only valid once [`wait_gfw_boot`] has succeeded; before the firmware fills
/// it in, the value is meaningless. Returns `None` for an all-ones (dead
/// aperture) or all-zero (not yet written) read, so a caller never reports a
/// bogus size for a card whose firmware has not finished. With ECC enabled,
/// one sixteenth of VRAM is reserved for check bits, so `usable_bytes` is
/// fifteen sixteenths of the raw size.
pub fn vram_size<M: Mmio>(bar0: &M) -> Option<VramInfo> {
    let range = bar0.read32(regs::FB_LOCAL_MEMORY_RANGE);
    if range == 0xFFFF_FFFF || range == 0 {
        return None;
    }
    let total_bytes = regs::fb_range_bytes(range);
    let ecc_enabled = regs::fb_range_ecc_enabled(range);
    let usable_bytes = if ecc_enabled {
        total_bytes - total_bytes / 16
    } else {
        total_bytes
    };
    Some(VramInfo {
        total_bytes,
        usable_bytes,
        ecc_enabled,
    })
}
