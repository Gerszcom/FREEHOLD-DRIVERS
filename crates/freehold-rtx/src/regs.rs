//! Named register offsets within the BAR0 aperture, and the decode of the
//! chip identification register.
//!
//! These are factual hardware constants, used the way nouveau and NVIDIA's
//! open kernel modules have long documented them. None of that source is
//! reproduced here — only the offsets and bit positions, which are facts
//! about the silicon, not anyone's code.

use crate::ids::Family;

/// `PMC_BOOT_0` — the master chip identification register, at the base of
/// BAR0. Every NVIDIA GPU answers here with its architecture, implementation
/// and revision; reading it is the first proof the register aperture is
/// live and the card is the one we think it is.
pub const PMC_BOOT_0: u32 = 0x0000_0000;

/// A `PMC_BOOT_0` value that means "no live device": all ones (a dead or
/// unmapped aperture) or a floating zero.
pub fn is_dead(boot0: u32) -> bool {
    boot0 == 0xFFFF_FFFF || boot0 == 0x0000_0000
}

/// The chipset number encoded in `PMC_BOOT_0`, e.g. `0x162` for TU102.
pub fn chipset(boot0: u32) -> u16 {
    ((boot0 >> 20) & 0x1FF) as u16
}

/// The architecture nibble of a chipset number, e.g. `0x160` for Turing.
pub fn architecture(boot0: u32) -> u16 {
    chipset(boot0) & 0x1F0
}

/// The implementation within a family (the die), e.g. `0x2` for the "102"
/// large die.
pub fn implementation(boot0: u32) -> u8 {
    (chipset(boot0) & 0x00F) as u8
}

/// The chip revision (mask/stepping), the low byte of `PMC_BOOT_0`.
pub fn revision(boot0: u32) -> u8 {
    (boot0 & 0xFF) as u8
}

/// Map a `PMC_BOOT_0` value to a [`Family`].
///
/// Turing, Ampere and Ada are confirmed. The Blackwell nibble is tentative
/// and must be confirmed on hardware (see `ROADMAP.md`); an unrecognised
/// value is reported as [`Family::Unknown`], which the probe treats as a
/// result, not a failure — so a card newer than this build is still brought
/// far enough to say what it is.
pub fn family(boot0: u32) -> Family {
    match architecture(boot0) {
        0x160 => Family::Turing,
        0x170 => Family::Ampere,
        0x190 => Family::Ada,
        0x1B0 => Family::Blackwell, // UNVERIFIED — confirm GB20x on silicon
        _ => Family::Unknown,
    }
}

// --- Stage 4 — devinit / GFW boot-complete and framebuffer size ---------
//
// From Turing on, the card runs its own boot firmware (NVIDIA calls it GFW,
// the "GPU FirmWare"; its init table work is the old "devinit"). It reads the
// board's VBIOS, trains memory, applies the straps, and sets the framebuffer
// size — all before a host driver should touch the rest of the chip. The
// firmware reports its progress in a secure scratch register, and the usable
// VRAM size it computed is left in a framebuffer-range register. Both offsets
// and the bit layouts below are documented hardware facts (the same ones
// nouveau and NVIDIA's open kernel modules have long published); none of that
// code is reproduced — only the register numbers and field positions, which
// describe the silicon, not anyone's driver.

/// `NV_PGC6_AON_SECURE_SCRATCH_GROUP_05` — the always-on secure scratch
/// register the GPU's own boot firmware (GFW / devinit) writes its progress
/// into. A host driver must wait for the firmware to report completion here
/// before it reads the framebuffer size or starts later bring-up, because
/// those values are not valid until the firmware has set them.
pub const GFW_BOOT_STATUS: u32 = 0x0011_8234;

/// The GFW boot *progress* occupies the low byte of [`GFW_BOOT_STATUS`]; the
/// rest of the register carries other scratch state this driver does not use.
pub const GFW_BOOT_PROGRESS_MASK: u32 = 0x0000_00FF;

/// The progress-byte value that means the firmware has finished its boot.
pub const GFW_BOOT_COMPLETED: u8 = 0xFF;

/// The GFW boot progress byte decoded from a [`GFW_BOOT_STATUS`] read.
pub fn gfw_boot_progress(status: u32) -> u8 {
    (status & GFW_BOOT_PROGRESS_MASK) as u8
}

/// Whether a [`GFW_BOOT_STATUS`] read reports the firmware's boot complete.
///
/// A fully-ones word (`0xFFFF_FFFF`) is a dead or unmapped aperture, not a
/// completed boot, so it is explicitly *not* treated as complete even though
/// its low byte is `0xFF`; the caller distinguishes that case (see
/// [`crate::boot`]).
pub fn gfw_boot_complete(status: u32) -> bool {
    status != 0xFFFF_FFFF && gfw_boot_progress(status) == GFW_BOOT_COMPLETED
}

/// `NV_PFB_PRI_MMU_LOCAL_MEMORY_RANGE` — the framebuffer-range register whose
/// low half encodes the size of the card's local VRAM, as a magnitude shifted
/// by a power-of-two scale (in megabytes), with a bit recording whether ECC
/// is enabled. The firmware fills this in during boot; it is only valid once
/// [`gfw_boot_complete`] holds.
pub const FB_LOCAL_MEMORY_RANGE: u32 = 0x0010_0CE0;

/// The scale (power-of-two exponent, in MiB) of the local memory size:
/// `LOWER_SCALE`, bits `3:0`.
pub fn fb_range_scale(range: u32) -> u32 {
    range & 0x0000_000F
}

/// The magnitude (mantissa) of the local memory size: `LOWER_MAG`, bits `9:4`.
pub fn fb_range_mag(range: u32) -> u32 {
    (range >> 4) & 0x0000_003F
}

/// Whether ECC is enabled for local memory: `ECC_MODE`, bit `30`. When it is,
/// the firmware reserves one sixteenth of VRAM for check bits, so the usable
/// size is fifteen sixteenths of the raw size.
pub fn fb_range_ecc_enabled(range: u32) -> bool {
    (range >> 30) & 1 == 1
}

/// The raw local VRAM size in bytes encoded by a [`FB_LOCAL_MEMORY_RANGE`]
/// read: `magnitude << (scale + 20)`. The `+ 20` turns the scale's megabyte
/// unit into bytes. With the documented 6-bit magnitude and 4-bit scale the
/// shift is at most `63`, so it cannot overflow a `u64`.
pub fn fb_range_bytes(range: u32) -> u64 {
    (fb_range_mag(range) as u64) << (fb_range_scale(range) + 20)
}
