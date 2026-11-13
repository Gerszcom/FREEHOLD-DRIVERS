//! PCI identity constants and the chip families this driver covers.

/// NVIDIA's PCI vendor ID.
pub const NVIDIA_VENDOR: u16 = 0x10DE;

/// PCI base class for a display controller.
pub const PCI_CLASS_DISPLAY: u8 = 0x03;

/// A GPU architecture family, decoded from the chip's `PMC_BOOT_0`
/// identification register and mirrored by the consumer GeForce series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Turing — GeForce RTX 20 series (and GTX 16).
    Turing,
    /// Ampere — GeForce RTX 30 series.
    Ampere,
    /// Ada Lovelace — GeForce RTX 40 series.
    Ada,
    /// Blackwell — GeForce RTX 50 series.
    Blackwell,
    /// A display GPU whose family this build does not yet recognise.
    Unknown,
}

impl Family {
    /// The consumer GeForce series this family ships as.
    pub fn rtx_series(self) -> &'static str {
        match self {
            Family::Turing => "GeForce RTX 20",
            Family::Ampere => "GeForce RTX 30",
            Family::Ada => "GeForce RTX 40",
            Family::Blackwell => "GeForce RTX 50",
            Family::Unknown => "unknown NVIDIA",
        }
    }

    /// Whether this crate's bring-up path is designed for this family.
    pub fn is_supported(self) -> bool {
        !matches!(self, Family::Unknown)
    }
}

/// A short, human-readable name for a few well-established device IDs.
///
/// This is a *starter* table, not an authoritative one: family detection
/// never depends on it — that comes from `PMC_BOOT_0` (see [`crate::probe`]).
/// It exists only to print something friendlier than a hex ID when the
/// device is a model already catalogued. Entries are added as they are
/// confirmed against NVIDIA's public device-ID list; an uncatalogued card
/// is still claimed and still probed, it just prints by its hex ID.
pub const KNOWN_MODELS: &[(u16, &str)] = &[
    // Turing — RTX 20
    (0x1E04, "GeForce RTX 2080 Ti"),
    (0x1E07, "GeForce RTX 2080 Ti"),
    (0x1E82, "GeForce RTX 2080"),
    (0x1F02, "GeForce RTX 2070"),
    (0x1F08, "GeForce RTX 2060"),
    // Ampere — RTX 30
    (0x2204, "GeForce RTX 3090"),
    (0x2206, "GeForce RTX 3080"),
    (0x2484, "GeForce RTX 3070"),
    (0x2487, "GeForce RTX 3060"),
    // Ada — RTX 40
    (0x2684, "GeForce RTX 4090"),
    (0x2704, "GeForce RTX 4080"),
    // Blackwell — RTX 50: added as confirmed.
];
