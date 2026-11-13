//! Stage 3 groundwork — the FREEHOLD device description a user-mode driver
//! is handed, and the selection of the register aperture (BAR0) from it.
//!
//! When FREEHOLD starts a user-mode driver for a PCI device, the driver's
//! first system call, `DRIVER_DEVICE`, writes a fixed-size record describing
//! that one device: its PCI identity, its slot on the bus, and its six base
//! address registers (BARs). This module turns that documented byte layout
//! into typed values, so the rest of the driver works against [`DeviceInfo`]
//! and [`BarInfo`] rather than raw offsets, and it locates the memory BAR0
//! that holds the GPU's registers.
//!
//! The layout mirrored here is FREEHOLD's own ABI (`freehold_abi::driver`),
//! reproduced as the published interface this clean-room driver is written
//! against. No NVIDIA material is involved: that NVIDIA display GPUs place
//! their register aperture in memory BAR0 is a documented hardware fact, and
//! everything else here is FREEHOLD's own wire format.
//!
//! This is the driver-side half of Stage 3's "reach BAR0" work. It is pure,
//! allocation-free logic and is exercised on the host against byte records
//! built by hand, so `NVRTX.EXE` — the on-device user-mode driver — can be
//! built on top of a selection path that is already proven. The kernel call
//! that actually maps the chosen BAR (`DRIVER_MAP`) and the runtime's
//! `unsafe` BAR0 accessor live in FREEHOLD; this crate stays `no_std` and
//! `forbid(unsafe_code)`.

use crate::identify::{is_supported, model_name, GpuId};
use crate::mmio::Bar;

/// Byte layout of the `DRIVER_DEVICE` record (`freehold_abi::driver`).
///
/// Kept as private constants so the parser reads the documented offsets
/// directly; they are the single source of truth for the wire format and
/// match the kernel's `DriverDevice::describe`.
mod layout {
    /// Total record length: a fixed header, then six BARs.
    pub const DEVICE_BYTES: usize = BAR0 + 6 * BAR_STRIDE;

    // Header fields.
    pub const VENDOR: usize = 0x00; // u16
    pub const DEVICE: usize = 0x02; // u16
    pub const CLASS: usize = 0x04; // u8
    pub const SUBCLASS: usize = 0x05; // u8
    pub const PROG_IF: usize = 0x06; // u8
    pub const BUS: usize = 0x08; // u8
    pub const SLOT: usize = 0x09; // u8
    pub const FUNCTION: usize = 0x0A; // u8

    // The six BARs, each `BAR_STRIDE` bytes from `BAR0`.
    pub const BAR0: usize = 0x10;
    pub const BAR_STRIDE: usize = 16;
    /// Within one BAR: u64 base, u32 size, u32 flags.
    pub const BAR_BASE: usize = 0x00;
    pub const BAR_SIZE: usize = 0x08;
    pub const BAR_FLAGS: usize = 0x0C;

    // BAR flag values.
    pub const BAR_NONE: u32 = 0;
    pub const BAR_IO: u32 = 1;
    pub const BAR_MEM: u32 = 2;
}

/// How many BARs a PCI device header carries.
pub const BAR_COUNT: usize = 6;

/// The length in bytes of the `DRIVER_DEVICE` record this module parses.
pub const DEVICE_BYTES: usize = layout::DEVICE_BYTES;

/// What one base address register is, as FREEHOLD reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    /// Unused (size 0): the device does not implement this BAR.
    None,
    /// An I/O-port BAR, reached with `DRIVER_IN` / `DRIVER_OUT`.
    Io,
    /// A memory BAR, reached by mapping it (`DRIVER_MAP`). NVIDIA GPUs put
    /// their registers in one of these.
    Memory,
    /// A flag value this build does not recognise — reported, not rejected,
    /// so an unfamiliar record is still legible.
    Unknown(u32),
}

impl BarKind {
    fn from_flags(flags: u32) -> BarKind {
        match flags {
            layout::BAR_NONE => BarKind::None,
            layout::BAR_IO => BarKind::Io,
            layout::BAR_MEM => BarKind::Memory,
            other => BarKind::Unknown(other),
        }
    }
}

/// One base address register: where it is, how large, and of what kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarInfo {
    /// Physical base address the firmware assigned the region.
    pub base: u64,
    /// Size of the region in bytes.
    pub size: u64,
    /// Whether it is an I/O, memory, absent or unrecognised BAR.
    pub kind: BarKind,
}

impl BarInfo {
    /// Whether this is a memory BAR (the kind a GPU's registers live behind).
    pub fn is_memory(&self) -> bool {
        matches!(self.kind, BarKind::Memory)
    }

    /// Whether this is a non-empty memory BAR — one that can actually be
    /// mapped and read. An absent (size 0) or I/O BAR is not.
    pub fn is_mappable(&self) -> bool {
        self.is_memory() && self.size > 0
    }

    /// Whether a 32-bit access at `offset` stays inside this region — the
    /// bound every register access is checked against.
    pub fn holds_u32(&self, offset: u32) -> bool {
        (offset as u64)
            .checked_add(4)
            .is_some_and(|end| end <= self.size)
    }

    /// This BAR as an [`Mmio`](crate::mmio) [`Bar`] descriptor, for the
    /// bounds the register layer checks against. Memory BARs map to
    /// `io = false`; an I/O BAR to `io = true`. A non-memory kind keeps
    /// whatever base and size it reported.
    pub fn as_bar(&self) -> Bar {
        Bar {
            base: self.base,
            size: self.size,
            io: matches!(self.kind, BarKind::Io),
        }
    }
}

/// The PCI device a user-mode driver was given, parsed from the
/// `DRIVER_DEVICE` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The PCI identity, as [`is_supported`](crate::identify::is_supported)
    /// and [`model_name`](crate::identify::model_name) read it.
    pub id: GpuId,
    /// The device's programming interface byte.
    pub prog_if: u8,
    /// Its location on the PCI bus.
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    /// Its six BARs, in order.
    pub bars: [BarInfo; BAR_COUNT],
}

/// Why a `DRIVER_DEVICE` record could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The record is shorter than [`DEVICE_BYTES`]: not a whole description.
    TooShort,
}

impl DeviceInfo {
    /// Parse a `DRIVER_DEVICE` record. The slice must be at least
    /// [`DEVICE_BYTES`] long; any extra trailing bytes are ignored, so a
    /// caller may pass an oversized buffer.
    pub fn parse(record: &[u8]) -> Result<DeviceInfo, ParseError> {
        if record.len() < DEVICE_BYTES {
            return Err(ParseError::TooShort);
        }
        let id = GpuId {
            vendor: le_u16(record, layout::VENDOR),
            device: le_u16(record, layout::DEVICE),
            class: record[layout::CLASS],
            subclass: record[layout::SUBCLASS],
        };
        let mut bars = [BarInfo {
            base: 0,
            size: 0,
            kind: BarKind::None,
        }; BAR_COUNT];
        for (i, bar) in bars.iter_mut().enumerate() {
            let at = layout::BAR0 + i * layout::BAR_STRIDE;
            *bar = BarInfo {
                base: le_u64(record, at + layout::BAR_BASE),
                size: u64::from(le_u32(record, at + layout::BAR_SIZE)),
                kind: BarKind::from_flags(le_u32(record, at + layout::BAR_FLAGS)),
            };
        }
        Ok(DeviceInfo {
            id,
            prog_if: record[layout::PROG_IF],
            bus: record[layout::BUS],
            slot: record[layout::SLOT],
            function: record[layout::FUNCTION],
            bars,
        })
    }

    /// Whether this driver claims the device: an NVIDIA display controller.
    /// Delegates to [`is_supported`](crate::identify::is_supported) so the
    /// claim rule lives in one place.
    pub fn is_supported(&self) -> bool {
        is_supported(&self.id)
    }

    /// A catalogued model name for this device, if one is known.
    pub fn model_name(&self) -> Option<&'static str> {
        model_name(self.id.device)
    }

    /// The register aperture: BAR0, which on every NVIDIA GPU of these
    /// generations is the memory BAR holding the register file. Returns it
    /// only when it is a non-empty memory BAR — the precondition for mapping
    /// it and running [`probe`](crate::probe) against it. A card whose BAR0
    /// is absent or (wrongly) an I/O BAR yields `None`, which a driver
    /// treats as "cannot bring this device up", not a panic.
    pub fn register_aperture(&self) -> Option<&BarInfo> {
        let bar0 = &self.bars[0];
        bar0.is_mappable().then_some(bar0)
    }
}

/// Read a little-endian `u16` at `off`. Callers guarantee the slice is long
/// enough (checked once in [`DeviceInfo::parse`]).
fn le_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// Read a little-endian `u32` at `off`.
fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Read a little-endian `u64` at `off`.
fn le_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}
