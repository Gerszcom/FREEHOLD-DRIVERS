//! Stage 2 — probe. Through the BAR0 register aperture, confirm the card
//! responds and decode which silicon it is.

use crate::ids::Family;
use crate::mmio::Mmio;
use crate::regs;

/// Why a probe did not find a live GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeError {
    /// `PMC_BOOT_0` read back all-ones or zero: the aperture is not a live
    /// GPU (wrong BAR, device powered off, or nothing mapped).
    NotResponding,
}

/// What a successful probe learned about the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuProbe {
    /// The raw `PMC_BOOT_0` value, kept for logging and later stages.
    pub boot0: u32,
    /// The chipset number, e.g. `0x162` for TU102.
    pub chipset: u16,
    /// The architecture family decoded from the chipset.
    pub family: Family,
    /// The implementation within the family (die), e.g. `0x2` for "102".
    pub implementation: u8,
    /// The chip revision (stepping).
    pub revision: u8,
}

impl GpuProbe {
    /// Whether the decoded family is one this driver's bring-up path covers.
    pub fn is_supported(&self) -> bool {
        self.family.is_supported()
    }
}

/// Read and decode the GPU's identification register through its BAR0
/// aperture.
///
/// Succeeds for any live NVIDIA GPU. `family` is [`Family::Unknown`] for
/// silicon newer than this build knows — a reported result, not an error,
/// so a future card is still identified by chipset and revision.
pub fn probe<M: Mmio>(bar0: &M) -> Result<GpuProbe, ProbeError> {
    let boot0 = bar0.read32(regs::PMC_BOOT_0);
    if regs::is_dead(boot0) {
        return Err(ProbeError::NotResponding);
    }
    Ok(GpuProbe {
        boot0,
        chipset: regs::chipset(boot0),
        family: regs::family(boot0),
        implementation: regs::implementation(boot0),
        revision: regs::revision(boot0),
    })
}
