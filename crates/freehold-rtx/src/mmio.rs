//! The hardware-access boundary: a trait for 32-bit reads and writes into a
//! GPU's BAR0 register aperture, plus the BAR descriptor the kernel hands a
//! user-mode driver.

/// One PCI base address register, as FREEHOLD's `DRIVER_DEVICE` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    /// Physical base address of the region.
    pub base: u64,
    /// Size of the region in bytes.
    pub size: u64,
    /// True for an I/O-port BAR, false for a memory BAR. NVIDIA GPUs expose
    /// their registers through a *memory* BAR (BAR0), never I/O ports — so a
    /// FREEHOLD driver for one needs memory-BAR mapping, not the port I/O the
    /// current `DRIVER_*` calls give (see `ROADMAP.md`, kernel enablement).
    pub io: bool,
}

impl Bar {
    /// Whether a 32-bit access at `offset` stays inside this region — the
    /// bound every register access is checked against.
    pub fn holds_u32(&self, offset: u32) -> bool {
        (offset as u64)
            .checked_add(4)
            .is_some_and(|end| end <= self.size)
    }
}

/// 32-bit access to a GPU's BAR0 register aperture.
///
/// On hardware the implementation is a volatile read or write at
/// `base + offset`, BAR0 having been mapped uncached into the driver
/// process. The trait lets all of this crate's logic run unchanged against a
/// [`crate::mock`] GPU on the host.
pub trait Mmio {
    /// Read the 32-bit register at `offset` within BAR0.
    fn read32(&self, offset: u32) -> u32;

    /// Write `value` to the 32-bit register at `offset` within BAR0.
    fn write32(&mut self, offset: u32, value: u32);

    /// Read-modify-write: clear the bits of `mask`, then set those of
    /// `value` that `mask` selects.
    fn modify32(&mut self, offset: u32, mask: u32, value: u32) {
        let cur = self.read32(offset);
        self.write32(offset, (cur & !mask) | (value & mask));
    }
}
