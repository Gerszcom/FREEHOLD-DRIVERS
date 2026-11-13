//! Stage 1 — identify. Decide from PCI identity whether a device is an
//! NVIDIA display GPU this driver claims, and name it when the model is
//! already catalogued.

use crate::ids::{KNOWN_MODELS, NVIDIA_VENDOR, PCI_CLASS_DISPLAY};

/// The PCI identity of a candidate device, as FREEHOLD's `DRIVER_DEVICE`
/// reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuId {
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
}

/// Whether this driver claims the device: an NVIDIA display controller.
///
/// Family is deliberately *not* decided here. PCI device IDs do not map
/// cleanly onto architectures, and the authoritative source is the chip's
/// own `PMC_BOOT_0` register, read during [`crate::probe`]. Stage 1 answers
/// only "is this one of ours", which is all that is needed to claim the
/// device and hand it to the probe.
pub fn is_supported(id: &GpuId) -> bool {
    id.vendor == NVIDIA_VENDOR && id.class == PCI_CLASS_DISPLAY
}

/// A catalogued model name for a device ID, if one is known.
pub fn model_name(device: u16) -> Option<&'static str> {
    KNOWN_MODELS
        .iter()
        .find(|(id, _)| *id == device)
        .map(|(_, name)| *name)
}
