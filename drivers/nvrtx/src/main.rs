//! NVRTX.EXE — the clean-room user-mode driver (D-127, D-195) for NVIDIA
//! GeForce RTX 20–50 GPUs (Turing, Ampere, Ada, Blackwell). It runs as an
//! ordinary FREEHOLD program, started at boot for the display GPU the kernel
//! assigned it (catalog match by NVIDIA vendor + display class, D-196), and
//! brings the card up to the point its GSP firmware can be loaded.
//!
//! All the driver *logic* lives in the clean-room `freehold-rtx` crate
//! (vendored from `Gerszcom/FREEHOLD-RTX`): claim the device, probe the
//! silicon through BAR0, wait for the card's own boot firmware, read the VRAM
//! size, and choose and verify the GSP firmware. That crate is
//! `#![forbid(unsafe_code)]` and allocation-free; it reaches hardware only
//! through its `Mmio` trait. This program is the thin on-device glue that
//! gives it the three things it cannot supply itself:
//!
//! * an [`Mmio`] implementation over the runtime's checked [`Bar0`] accessor
//!   (`DRIVER_MAP`, D-194) — the one hardware seam, and still no `unsafe`
//!   here, exactly as the RTL8139 driver reaches its DMA rings only through
//!   the runtime's checked `Dma`;
//! * a [`FirmwareStore`] over FREEHOLD's VFS, reading `C:\FIRMWARE\`;
//! * the decision to report the outcome and hold the device.
//!
//! What is *not* here yet (later stages): booting the GSP, the command/status
//! message queues, and the display modeset. On a card that needs the GSP for
//! its display path (Ada, Blackwell) a missing firmware is fatal; on one that
//! does not (Turing, Ampere) the card still comes up, degraded. On a real RTX
//! card the positive path is confirmed under `FREEHOLD-RTX/validation/stage-3`.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use freehold_rt::driver::{self, Bar0};
use freehold_rt::{fs, println, thread, Psp};
use freehold_rtx::bringup::{
    bring_up, Bringup, BringupConfig, BringupError, FirmwareFiles, FirmwareStatus, FirmwareStore,
    FirmwareUnavailable,
};
use freehold_rtx::device::DeviceInfo;
use freehold_rtx::ids::Family;
use freehold_rtx::mmio::Mmio;

freehold_rt::program!(main);

/// Where FREEHOLD keeps device firmware (`DIRECTION.md` §4).
const FIRMWARE_DIR: &str = "C:\\FIRMWARE\\";
/// The per-family firmware base names the driver crate asks for
/// (`freehold_rtx::firmware::required_firmware`). The image is kept under the
/// base name itself; its sidecar is the base name plus `.FW`.
const FIRMWARE_BASENAMES: [&str; 4] = ["GSP-TURING", "GSP-AMPERE", "GSP-ADA", "GSP-BLACKWELL"];

/// The driver crate's [`Mmio`] over the runtime's bounds-checked [`Bar0`]
/// accessor. Each access delegates straight through; the `Bar0` already
/// refuses anything outside the mapped aperture, so there is no glue and no
/// `unsafe` here.
struct Bar0Mmio(Bar0);

impl Mmio for Bar0Mmio {
    fn read32(&self, offset: u32) -> u32 {
        self.0.read32(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.0.write32(offset, value);
    }
}

/// One firmware file set read from the VFS: the image bytes and the sidecar
/// text, owned so a borrow can be handed back for as long as the store lives.
struct FirmwareEntry {
    basename: &'static str,
    image: Vec<u8>,
    sidecar: String,
}

/// Firmware read from `C:\FIRMWARE\`, one entry per file actually present.
///
/// The files are read once, up front, because [`FirmwareStore::load`] returns
/// borrows that must live as long as the store — `bring_up` keeps the
/// selection it produces pointing into them. Reading on demand would need
/// interior mutability and could not hand back a `&self`-lived borrow without
/// `unsafe`, which this driver forbids. A card needs only its own family's
/// file, so at most one entry is normally present; on a machine with no
/// firmware placed (and under QEMU) the store is simply empty.
struct VfsFirmware {
    entries: Vec<FirmwareEntry>,
}

impl VfsFirmware {
    /// Read every firmware file present in `C:\FIRMWARE\` into owned buffers.
    fn load_present() -> VfsFirmware {
        let mut entries = Vec::new();
        for &basename in &FIRMWARE_BASENAMES {
            let image_path = format!("{FIRMWARE_DIR}{basename}");
            let sidecar_path = format!("{FIRMWARE_DIR}{basename}.FW");
            // Both the image and its sidecar must be present to be usable; a
            // lone image with no sidecar is treated as absent (the crate would
            // reject it anyway — it has no digest to verify against).
            let (Ok(image), Ok(sidecar_bytes)) =
                (fs::read_file(&image_path), fs::read_file(&sidecar_path))
            else {
                continue;
            };
            let Ok(sidecar) = String::from_utf8(sidecar_bytes) else {
                continue;
            };
            entries.push(FirmwareEntry {
                basename,
                image,
                sidecar,
            });
        }
        VfsFirmware { entries }
    }
}

impl FirmwareStore for VfsFirmware {
    fn load(&self, basename: &str) -> Option<FirmwareFiles<'_>> {
        self.entries
            .iter()
            .find(|e| e.basename == basename)
            .map(|e| FirmwareFiles {
                sidecar_text: e.sidecar.as_str(),
                image: e.image.as_slice(),
            })
    }
}

fn main(_psp: &Psp) -> u8 {
    // The driver subsystem hands a driver exactly one device. Run by hand
    // there is no device record — nothing to do.
    let Some(record) = driver::device_record() else {
        println!("NVRTX: run by the driver subsystem, not by hand (no device assigned).");
        return 1;
    };

    let device = match DeviceInfo::parse(&record) {
        Ok(d) => d,
        Err(_) => {
            println!("NVRTX: the device record did not parse.");
            return 1;
        }
    };

    let vendor = device.id.vendor;
    let dev_id = device.id.device;
    let model = device.model_name().unwrap_or("NVIDIA display GPU");

    // The crate re-checks this inside bring_up, but a clear line up front is
    // worth it for the console.
    if !device.is_supported() {
        println!(
            "NVRTX: {vendor:04x}:{dev_id:04x} is not an NVIDIA display GPU this driver claims."
        );
        return 2;
    }
    if device.register_aperture().is_none() {
        println!(
            "NVRTX: {model} ({vendor:04x}:{dev_id:04x}) has no mappable BAR0 memory aperture."
        );
        return 3;
    }

    // Enable the device (I/O, memory space and bus mastering) so BAR0 MMIO
    // responds, then map BAR0 (index 0 — the register aperture on every RTX
    // card) uncached through DRIVER_MAP (D-193).
    driver::enable();
    let Some(bar0) = Bar0::map(0) else {
        println!("NVRTX: {model} ({vendor:04x}:{dev_id:04x}): the kernel refused to map BAR0.");
        return 3;
    };
    let mmio = Bar0Mmio(bar0);

    // Firmware the owner placed in C:\FIRMWARE\, read through the VFS.
    let store = VfsFirmware::load_present();

    // The ordered bring-up. The boot poll is spaced with the runtime timer;
    // the crate's default bounds how many times it looks.
    let config = BringupConfig::default();
    match bring_up(&device, &mmio, &store, config, || thread::sleep_ms(1)) {
        Ok(up) => {
            report_up(model, vendor, dev_id, &up);
            println!(
                "NVRTX: holding the device. (GSP boot, message queues and display are later stages.)"
            );
            // Stay alive as the device's owner. Exiting would run the kernel's
            // driver teardown (bus mastering off), undoing the bring-up, so a
            // brought-up driver idles rather than returns — as the RTL8139
            // driver never leaves its poll loop. Nothing to poll yet, so this
            // just yields.
            loop {
                thread::sleep_ms(1000);
            }
        }
        Err(err) => {
            report_err(model, vendor, dev_id, &err);
            bringup_error_code(&err)
        }
    }
}

/// A readable name for a silicon family.
fn family_name(family: Family) -> &'static str {
    match family {
        Family::Turing => "Turing (GeForce RTX 20 / GTX 16)",
        Family::Ampere => "Ampere (GeForce RTX 30)",
        Family::Ada => "Ada Lovelace (GeForce RTX 40)",
        Family::Blackwell => "Blackwell (GeForce RTX 50)",
        Family::Unknown => "unrecognised NVIDIA silicon",
    }
}

/// Report a successful (or degraded) bring-up to the console.
fn report_up(model: &str, vendor: u16, dev_id: u16, up: &Bringup) {
    let usable_mib = up.vram.usable_bytes / (1024 * 1024);
    println!(
        "NVRTX: {model} ({vendor:04x}:{dev_id:04x}) up — {}, impl {:#04x} rev {:#04x}, {usable_mib} MiB VRAM.",
        family_name(up.probe.family),
        up.probe.implementation,
        up.probe.revision,
    );
    match &up.firmware {
        FirmwareStatus::Verified(selection) => {
            println!(
                "NVRTX: GSP firmware verified ({}).",
                selection.request.file_basename
            );
        }
        FirmwareStatus::Absent(reason) => {
            println!(
                "NVRTX: no GSP firmware — running degraded: {}.",
                unavailable_reason(reason)
            );
        }
    }
}

/// Report a failed bring-up to the console.
fn report_err(model: &str, vendor: u16, dev_id: u16, err: &BringupError) {
    let head = format!("NVRTX: {model} ({vendor:04x}:{dev_id:04x})");
    match err {
        BringupError::NotClaimed => {
            println!("{head}: not an NVIDIA display GPU this driver claims.")
        }
        BringupError::NoRegisterAperture => {
            println!("{head}: BAR0 is not a mappable memory aperture.")
        }
        BringupError::Probe(_) => {
            println!("{head}: the register aperture did not read back a live GPU.")
        }
        BringupError::Boot(_) => {
            println!("{head}: the card's boot firmware did not report complete.")
        }
        BringupError::VramUnavailable => {
            println!("{head}: booted, but the VRAM size did not read back valid.")
        }
        BringupError::FirmwareRequired(reason) => println!(
            "{head}: GSP firmware is required for this card's display, but {}.",
            unavailable_reason(reason)
        ),
    }
}

/// Why no verified firmware was available, in words.
fn unavailable_reason(reason: &FirmwareUnavailable) -> &'static str {
    match reason {
        FirmwareUnavailable::UnknownFamily => {
            "the silicon family is not recognised, so no firmware is named"
        }
        FirmwareUnavailable::FileMissing => "no firmware file is placed in C:\\FIRMWARE\\",
        FirmwareUnavailable::Sidecar(_) => "the firmware sidecar did not parse",
        FirmwareUnavailable::Select(_) => "the sidecar is not acceptable for this card",
        FirmwareUnavailable::Verify(_) => "the firmware bytes did not match the sidecar's digest",
    }
}

/// A distinct exit code per failure, so `DRIVERS` shows which step stopped
/// the driver.
fn bringup_error_code(err: &BringupError) -> u8 {
    match err {
        BringupError::NotClaimed => 2,
        BringupError::NoRegisterAperture => 3,
        BringupError::Probe(_) => 4,
        BringupError::Boot(_) => 5,
        BringupError::VramUnavailable => 6,
        BringupError::FirmwareRequired(_) => 7,
    }
}
