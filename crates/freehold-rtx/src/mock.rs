//! A software stand-in for a GPU's BAR0 register aperture, so this crate's
//! logic is exercised on the host with no hardware.
//!
//! It is not test-only: it also documents the `PMC_BOOT_0` values real chips
//! report, and the later bring-up stages reuse it as their reference model.

use crate::channel::Doorbell;
use crate::mmio::Mmio;
use crate::msgq::SharedMem;

/// A tiny register file: a fixed set of (offset, value) cells. Any offset
/// never written reads back all-ones — the bus value of an unimplemented
/// register, and what a dead or absent card returns.
#[derive(Clone)]
pub struct MockGpu {
    cells: [(u32, u32); Self::SLOTS],
    used: usize,
}

impl MockGpu {
    const SLOTS: usize = 32;

    /// An empty aperture: every register reads back all-ones (a dead card).
    pub fn dead() -> Self {
        MockGpu {
            cells: [(0, 0); Self::SLOTS],
            used: 0,
        }
    }

    /// An aperture whose `PMC_BOOT_0` reports `boot0`.
    pub fn with_boot0(boot0: u32) -> Self {
        let mut m = MockGpu::dead();
        m.set(crate::regs::PMC_BOOT_0, boot0);
        m
    }

    /// A TU102 (GeForce RTX 2080 Ti class), `PMC_BOOT_0 = 0x162000A1`.
    pub fn turing_tu102() -> Self {
        MockGpu::with_boot0(0x1620_00A1)
    }

    /// A GA102 (GeForce RTX 3090 class), `PMC_BOOT_0 = 0x172000A1`.
    pub fn ampere_ga102() -> Self {
        MockGpu::with_boot0(0x1720_00A1)
    }

    /// An AD102 (GeForce RTX 4090 class), `PMC_BOOT_0 = 0x192000A1`.
    pub fn ada_ad102() -> Self {
        MockGpu::with_boot0(0x1920_00A1)
    }

    /// Set the GFW boot progress byte the firmware would report (Stage 4),
    /// leaving the rest of the secure-scratch word zero. `0x00` is "not
    /// started", [`crate::regs::GFW_BOOT_COMPLETED`] is "done".
    pub fn set_gfw_boot_progress(&mut self, progress: u8) {
        self.set(crate::regs::GFW_BOOT_STATUS, u32::from(progress));
    }

    /// Mark the card's boot firmware as finished: the shortcut for
    /// `set_gfw_boot_progress(GFW_BOOT_COMPLETED)`.
    pub fn set_gfw_boot_complete(&mut self) {
        self.set_gfw_boot_progress(crate::regs::GFW_BOOT_COMPLETED);
    }

    /// Set the local VRAM size the boot firmware would report (Stage 4), as
    /// the magnitude/scale/ECC fields of the framebuffer-range register. The
    /// raw size is `magnitude << (scale + 20)` bytes; e.g. `mag = 1`,
    /// `scale = 14` is 16 GiB, and `mag = 3`, `scale = 13` is 24 GiB.
    pub fn set_vram(&mut self, magnitude: u8, scale: u8, ecc: bool) {
        let range =
            (u32::from(magnitude & 0x3F) << 4) | u32::from(scale & 0x0F) | (u32::from(ecc) << 30);
        self.set(crate::regs::FB_LOCAL_MEMORY_RANGE, range);
    }

    /// Set the value a register offset reads back.
    pub fn set(&mut self, offset: u32, value: u32) {
        for cell in &mut self.cells[..self.used] {
            if cell.0 == offset {
                cell.1 = value;
                return;
            }
        }
        if self.used < Self::SLOTS {
            self.cells[self.used] = (offset, value);
            self.used += 1;
        }
    }
}

impl Mmio for MockGpu {
    fn read32(&self, offset: u32) -> u32 {
        for cell in &self.cells[..self.used] {
            if cell.0 == offset {
                return cell.1;
            }
        }
        0xFFFF_FFFF
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.set(offset, value);
    }
}

/// A fixed-size region of memory shared with the GSP, backing the message-queue
/// transport ([`crate::msgq`]) on the host.
///
/// It models exactly what a real mapped sysmem region is to this crate's
/// logic — a flat array of bytes that two parties read and write — so the ring
/// buffer code runs here unchanged. `N` is the region size in bytes; it is
/// stack-allocated and allocation-free, matching the crate's `no_std` rule.
/// Both sides of a queue (in a host test, the driver and the stand-in GSP)
/// share one instance, exactly as they share one physical region on the card.
/// Multi-byte words are little-endian, the byte order the host CPU and the GSP
/// both use.
#[derive(Clone)]
pub struct MockSharedMem<const N: usize> {
    bytes: [u8; N],
}

impl<const N: usize> Default for MockSharedMem<N> {
    fn default() -> Self {
        MockSharedMem { bytes: [0; N] }
    }
}

impl<const N: usize> MockSharedMem<N> {
    /// A zeroed shared region of `N` bytes.
    pub fn new() -> Self {
        Self::default()
    }

    /// The region size in bytes.
    pub fn len(&self) -> usize {
        N
    }

    /// Whether the region has zero length (never, for a real queue, but the
    /// clippy-blessed companion to [`len`](Self::len)).
    pub fn is_empty(&self) -> bool {
        N == 0
    }
}

impl<const N: usize> SharedMem for MockSharedMem<N> {
    fn read_u32(&self, off: usize) -> u32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.bytes[off..off + 4]);
        u32::from_le_bytes(b)
    }

    fn write_u32(&mut self, off: usize, value: u32) {
        self.bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn read_bytes(&self, off: usize, buf: &mut [u8]) {
        buf.copy_from_slice(&self.bytes[off..off + buf.len()]);
    }

    fn write_bytes(&mut self, off: usize, src: &[u8]) {
        self.bytes[off..off + src.len()].copy_from_slice(src);
    }
}

/// A host stand-in for the GSP's doorbell: it counts how many times it was
/// rung instead of writing a register.
///
/// A real host-side [`Doorbell`] writes the GSP's doorbell register through
/// FREEHOLD's BAR0 accessor to wake the firmware; its concrete offset and value
/// are hardware facts (`validation/stage-5/`). This mock lets a host test assert
/// a [`crate::channel::CommandChannel`] rang the bell exactly when it published
/// a command and never when a send was refused — the discipline settled
/// off-hardware.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MockDoorbell {
    signals: u32,
}

impl MockDoorbell {
    /// A fresh doorbell that has never been rung.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times [`Doorbell::signal`] has been called on this doorbell.
    pub fn signals(&self) -> u32 {
        self.signals
    }
}

impl Doorbell for MockDoorbell {
    fn signal(&mut self) {
        self.signals += 1;
    }
}
