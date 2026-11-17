//! VIRTBLK.EXE — a user-mode driver (D-127) for the virtio block device over
//! the modern (virtio 1.x) PCI transport: QEMU's `virtio-blk-pci`,
//! transitional (1AF4:1001) or modern-only (1AF4:1042). It is the kernel's
//! built-in virtio-blk driver moved out of the kernel as FREEHOLD's first
//! boot driver (D-254, D-280): carried inside the kernel file, started from
//! memory before `C:` is mounted, and started again if it ends.
//!
//! It finds the transport's structures in its device's capability list
//! (`DRIVER_CONFIG`), reaches them in the memory BARs they name
//! (`DRIVER_MAP`), keeps its virtqueue, request header and data buffer in
//! `DRIVER_DMA` memory, and serves the disk through the block bridge
//! (`DRIVER_BLOCK`): it registers the disk, then takes the kernel's
//! requests a piece at a time (64 KiB at most, the built-in's bounce
//! buffer) straight into its data buffer, which the device reads or writes,
//! and says when each is done. It needs no `unsafe`: every register access
//! and every byte of DMA goes through the checked wrappers in
//! `freehold_rt::driver`.
//!
//! As the built-in: one queue, one request at a time (header, data,
//! status: three descriptors), the block size and FLUSH when offered — and
//! now the read-only flag, which the disk is then registered with. With the
//! device's MSI-X interrupt (the queue on entry 0) it sleeps in
//! `DRIVER_WAIT` while the device works; without it, it polls the used ring
//! as the built-in did. Between requests it sleeps until the kernel has
//! one (`EVENT_WORK`).
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use core::sync::atomic::{fence, Ordering};

use freehold_rt::driver::{self, block, Bar0, Dma};
use freehold_rt::{println, thread, Psp};

freehold_rt::program!(main);

/// A vendor-specific capability: virtio's transport structures.
const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_DEVICE: u8 = 4;

// Common configuration offsets.
const DEVICE_FEATURE_SELECT: u32 = 0x00;
const DEVICE_FEATURE: u32 = 0x04;
const DRIVER_FEATURE_SELECT: u32 = 0x08;
const DRIVER_FEATURE: u32 = 0x0C;
const MSIX_CONFIG: u32 = 0x10;
const NUM_QUEUES: u32 = 0x12;
const DEVICE_STATUS: u32 = 0x14;
const QUEUE_SELECT: u32 = 0x16;
const QUEUE_SIZE: u32 = 0x18;
const QUEUE_MSIX_VECTOR: u32 = 0x1A;
const QUEUE_ENABLE: u32 = 0x1C;
const QUEUE_NOTIFY_OFF: u32 = 0x1E;
const QUEUE_DESC: u32 = 0x20;
const QUEUE_DRIVER: u32 = 0x28;
const QUEUE_DEVICE: u32 = 0x30;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;

const F_VERSION_1: u64 = 1 << 32;
const BLK_F_RO: u64 = 1 << 5;
const BLK_F_BLK_SIZE: u64 = 1 << 6;
const BLK_F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
/// The avail ring's flags: the device need not interrupt (it is polled).
const AVAIL_F_NO_INTERRUPT: u16 = 1;
/// The MSI-X table entry the queue is given: the one the kernel set up.
const VECTOR: u16 = 0;

/// Entries in the queue: one request (three descriptors) at a time.
const QUEUE_LEN: u16 = 8;
const DESC_OFFSET: usize = 0x000;
const AVAIL_OFFSET: usize = 0x800;
const USED_OFFSET: usize = 0xC00;
/// The request header and status byte, in a page of their own.
const HEADER_OFFSET: usize = 0x000;
const STATUS_OFFSET: usize = 0x010;
/// The data buffer: the built-in's 16 pages.
const DATA_BYTES: usize = 64 * 1024;
/// How long one request may take.
const REQUEST_MS: u64 = 5000;

/// A transport structure: where in which memory BAR.
#[derive(Clone, Copy)]
struct Region {
    bar: usize,
    offset: u32,
    length: u32,
}

/// The device's memory BARs the transport uses, each mapped once.
struct Bars {
    mapped: [Option<Bar0>; 6],
}

impl Bars {
    fn map(&mut self, r: &Region) -> bool {
        if self.mapped[r.bar].is_none() {
            self.mapped[r.bar] = Bar0::map(r.bar as u8);
        }
        self.mapped[r.bar]
            .as_ref()
            .is_some_and(|b| u64::from(r.offset) + u64::from(r.length) <= b.len() as u64)
    }
    fn bar(&self, r: &Region) -> &Bar0 {
        // `map` succeeded for every region before any access.
        self.mapped[r.bar].as_ref().expect("mapped")
    }
    fn r8(&self, r: &Region, at: u32) -> u8 {
        self.bar(r).read8(r.offset + at)
    }
    fn r16(&self, r: &Region, at: u32) -> u16 {
        self.bar(r).read16(r.offset + at)
    }
    fn r32(&self, r: &Region, at: u32) -> u32 {
        self.bar(r).read32(r.offset + at)
    }
    fn w8(&self, r: &Region, at: u32, v: u8) {
        self.bar(r).write8(r.offset + at, v)
    }
    fn w16(&self, r: &Region, at: u32, v: u16) {
        self.bar(r).write16(r.offset + at, v)
    }
    fn w32(&self, r: &Region, at: u32, v: u32) {
        self.bar(r).write32(r.offset + at, v)
    }
    /// A 64-bit field, as its two 32-bit halves (virtio 1.x, 4.1.3.1).
    fn w64(&self, r: &Region, at: u32, v: u64) {
        self.w32(r, at, v as u32);
        self.w32(r, at + 4, (v >> 32) as u32);
    }
}

/// The transport: its three structures and the notify multiplier.
struct Transport {
    common: Region,
    notify: Region,
    notify_multiplier: u32,
    device: Option<Region>,
}

/// Find the modern transport's structures in the capability list.
fn find_transport() -> Option<Transport> {
    let (mut common, mut notify, mut device) = (None, None, None);
    for (id, at) in driver::capabilities()? {
        if id != CAP_VENDOR {
            continue;
        }
        let mut cap = [0u8; 20];
        let n = (256 - usize::from(at)).min(cap.len());
        if n < 16 || !driver::config(at, &mut cap[..n]) {
            continue;
        }
        let u32_at = |o: usize| u32::from_le_bytes([cap[o], cap[o + 1], cap[o + 2], cap[o + 3]]);
        let (cfg_type, bar) = (cap[3], usize::from(cap[4]));
        if bar >= 6 {
            continue;
        }
        let region = Region { bar, offset: u32_at(8), length: u32_at(12) };
        match cfg_type {
            CFG_COMMON => common = Some(region),
            CFG_NOTIFY if n == 20 => notify = Some((region, u32_at(16))),
            CFG_DEVICE => device = Some(region),
            _ => {}
        }
    }
    let (notify, notify_multiplier) = notify?;
    Some(Transport { common: common?, notify, notify_multiplier, device })
}

struct Disk {
    bars: Bars,
    t: Transport,
    ring: Dma,
    meta: Dma,
    data: Dma,
    notify_at: u32,
    avail_idx: u16,
    last_used: u16,
    sector_size: u32,
    sectors: u64,
    flush: bool,
    interrupts: bool,
}

impl Disk {
    /// Reset the device and negotiate the block size, FLUSH and the
    /// read-only flag when offered. Leaves FEATURES_OK set.
    fn negotiate(bars: &Bars, c: &Region) -> Result<u64, &'static str> {
        bars.w8(c, DEVICE_STATUS, 0);
        let t0 = thread::monotonic_ns();
        while bars.r8(c, DEVICE_STATUS) != 0 {
            if thread::monotonic_ns().wrapping_sub(t0) > 1_000_000_000 {
                return Err("the device did not reset");
            }
        }
        bars.w8(c, DEVICE_STATUS, STATUS_ACKNOWLEDGE);
        bars.w8(c, DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        bars.w32(c, DEVICE_FEATURE_SELECT, 0);
        let lo = u64::from(bars.r32(c, DEVICE_FEATURE));
        bars.w32(c, DEVICE_FEATURE_SELECT, 1);
        let hi = u64::from(bars.r32(c, DEVICE_FEATURE));
        let offered = lo | (hi << 32);
        if offered & F_VERSION_1 == 0 {
            bars.w8(c, DEVICE_STATUS, STATUS_FAILED);
            return Err("a legacy-only device (no virtio 1.x)");
        }
        let wanted = (offered & (BLK_F_RO | BLK_F_BLK_SIZE | BLK_F_FLUSH)) | F_VERSION_1;
        bars.w32(c, DRIVER_FEATURE_SELECT, 0);
        bars.w32(c, DRIVER_FEATURE, wanted as u32);
        bars.w32(c, DRIVER_FEATURE_SELECT, 1);
        bars.w32(c, DRIVER_FEATURE, (wanted >> 32) as u32);
        bars.w8(c, DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        if bars.r8(c, DEVICE_STATUS) & STATUS_FEATURES_OK == 0 {
            bars.w8(c, DEVICE_STATUS, STATUS_FAILED);
            return Err("the device refused the features");
        }
        Ok(wanted)
    }

    fn desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let at = DESC_OFFSET + usize::from(i) * 16;
        self.ring.write_u32(at, addr as u32);
        self.ring.write_u32(at + 4, (addr >> 32) as u32);
        self.ring.write_u32(at + 8, len);
        self.ring.write_u32(at + 12, u32::from(flags) | (u32::from(next) << 16));
    }

    /// Offer the chain at descriptor 0 and notify the device.
    fn submit(&mut self) {
        let slot = usize::from(self.avail_idx % QUEUE_LEN);
        self.ring.write(AVAIL_OFFSET + 4 + slot * 2, &0u16.to_le_bytes());
        fence(Ordering::SeqCst);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        let flags = if self.interrupts { 0 } else { AVAIL_F_NO_INTERRUPT };
        self.ring.write_u32(AVAIL_OFFSET, u32::from(flags) | (u32::from(self.avail_idx) << 16));
        fence(Ordering::SeqCst);
        self.bars.w16(&self.t.notify, self.notify_at, 0);
    }

    /// Has the device returned the request?
    fn used(&mut self) -> bool {
        fence(Ordering::SeqCst);
        if self.ring.read_u16(USED_OFFSET + 2) == self.last_used {
            return false;
        }
        self.last_used = self.last_used.wrapping_add(1);
        true
    }

    /// One request: header, `bytes` of the data buffer (none for a
    /// flush), status; wait for the device.
    fn request(&mut self, kind: u32, sector: u64, bytes: usize) -> bool {
        self.meta.write_u32(HEADER_OFFSET, kind);
        self.meta.write_u32(HEADER_OFFSET + 4, 0);
        self.meta.write_u32(HEADER_OFFSET + 8, sector as u32);
        self.meta.write_u32(HEADER_OFFSET + 12, (sector >> 32) as u32);
        self.meta.write(STATUS_OFFSET, &[0xFF]);
        let header = self.meta.phys() + HEADER_OFFSET as u64;
        let status = self.meta.phys() + STATUS_OFFSET as u64;
        if bytes > 0 {
            let device_writes = if kind == T_IN { DESC_F_WRITE } else { 0 };
            self.desc(0, header, 16, DESC_F_NEXT, 1);
            self.desc(1, self.data.phys(), bytes as u32, DESC_F_NEXT | device_writes, 2);
            self.desc(2, status, 1, DESC_F_WRITE, 0);
        } else {
            self.desc(0, header, 16, DESC_F_NEXT, 1);
            self.desc(1, status, 1, DESC_F_WRITE, 0);
        }
        self.submit();
        let t0 = thread::monotonic_ns();
        while !self.used() {
            let waited = thread::monotonic_ns().wrapping_sub(t0) / 1_000_000;
            if waited > REQUEST_MS {
                println!("VIRTBLK: a request took over {REQUEST_MS} ms; failed.");
                return false;
            }
            if self.interrupts {
                driver::wait(REQUEST_MS - waited);
            }
        }
        self.meta.read_u8(STATUS_OFFSET) == 0
    }

    /// Do one piece the kernel gave: its data is in (or goes to) `data`.
    fn serve(&mut self, p: block::Piece) -> bool {
        // virtio-blk sectors are always 512 bytes.
        let scale = u64::from(self.sector_size / 512);
        let bytes = (p.sectors * u64::from(self.sector_size)) as usize;
        match p.kind {
            block::Kind::Read => self.request(T_IN, p.lba * scale, bytes),
            block::Kind::Write => self.request(T_OUT, p.lba * scale, bytes),
            block::Kind::Flush => !self.flush || self.request(T_FLUSH, 0, 0),
        }
    }
}

fn main(_psp: &Psp) -> u8 {
    let Some(dev) = driver::device() else {
        println!("VIRTBLK: run by the driver system, not by hand.");
        return 1;
    };
    let at = alloc::format!("{:04x}:{:04x} at {:02x}:{:02x}.{}", dev.vendor, dev.device, dev.bus, dev.slot, dev.function);
    let Some(t) = find_transport() else {
        println!("VIRTBLK: {at} has no modern (virtio 1.x) transport.");
        return 1;
    };
    driver::enable();
    let mut bars = Bars { mapped: [None, None, None, None, None, None] };
    let regions = [Some(t.common), Some(t.notify), t.device];
    if !regions.iter().flatten().all(|r| bars.map(r)) {
        println!("VIRTBLK: cannot map the transport's memory BARs.");
        return 1;
    }
    let (Some(ring), Some(meta), Some(data)) = (Dma::alloc(4096), Dma::alloc(4096), Dma::alloc(DATA_BYTES)) else {
        println!("VIRTBLK: no memory for the queue and buffers.");
        return 1;
    };
    let c = t.common;
    let wanted = match Disk::negotiate(&bars, &c) {
        Ok(w) => w,
        Err(e) => {
            println!("VIRTBLK: {at}: {e}.");
            return 1;
        }
    };
    if bars.r16(&c, NUM_QUEUES) == 0 {
        println!("VIRTBLK: {at} has no queues.");
        return 1;
    }
    let Some(device) = t.device else {
        println!("VIRTBLK: {at} has no device configuration.");
        return 1;
    };
    // The device's interrupt (D-242): MSI-X entry 0 for the queue.
    let msix = driver::interrupts() == Some(driver::Interrupt::MsiX) && {
        bars.w16(&c, MSIX_CONFIG, VECTOR);
        bars.r16(&c, MSIX_CONFIG) == VECTOR
    };
    bars.w16(&c, QUEUE_SELECT, 0);
    if bars.r16(&c, QUEUE_SIZE) < QUEUE_LEN {
        println!("VIRTBLK: {at}: the queue is too small.");
        return 1;
    }
    bars.w16(&c, QUEUE_SIZE, QUEUE_LEN);
    bars.w64(&c, QUEUE_DESC, ring.phys() + DESC_OFFSET as u64);
    bars.w64(&c, QUEUE_DRIVER, ring.phys() + AVAIL_OFFSET as u64);
    bars.w64(&c, QUEUE_DEVICE, ring.phys() + USED_OFFSET as u64);
    let notify_at = u32::from(bars.r16(&c, QUEUE_NOTIFY_OFF)).wrapping_mul(t.notify_multiplier);
    if u64::from(notify_at) + 2 > u64::from(t.notify.length) {
        println!("VIRTBLK: {at}: the queue's notify address is outside the notify structure.");
        return 1;
    }
    let interrupts = msix && {
        bars.w16(&c, QUEUE_MSIX_VECTOR, VECTOR);
        bars.r16(&c, QUEUE_MSIX_VECTOR) == VECTOR
    };
    bars.w16(&c, QUEUE_ENABLE, 1);
    bars.w8(
        &c,
        DEVICE_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );
    // Capacity in 512-byte sectors at 0; the block size at 20 when offered.
    let sectors512 = u64::from(bars.r32(&device, 0)) | (u64::from(bars.r32(&device, 4)) << 32);
    let bs = if wanted & BLK_F_BLK_SIZE != 0 { bars.r32(&device, 20) } else { 512 };
    let sector_size = if [512, 1024, 2048, 4096].contains(&bs) { bs } else { 512 };
    let read_only = wanted & BLK_F_RO != 0;
    let mut disk = Disk {
        bars,
        t,
        ring,
        meta,
        data,
        notify_at,
        avail_idx: 0,
        last_used: 0,
        sector_size,
        sectors: sectors512 / u64::from(sector_size / 512),
        flush: wanted & BLK_F_FLUSH != 0,
        interrupts,
    };
    if let Err(e) = block::register(disk.sector_size, disk.sectors, read_only) {
        println!("VIRTBLK: {at}: the kernel refused the disk (error {e}).");
        return 1;
    }
    let how = if interrupts { "interrupts (MSI-X)" } else { "polled" };
    driver::status(&alloc::format!(
        "{} sectors of {} bytes ({} MiB){}, flush={}, {}",
        disk.sectors,
        disk.sector_size,
        (disk.sectors * u64::from(disk.sector_size)) >> 20,
        if read_only { ", read-only" } else { "" },
        disk.flush,
        how
    ));
    loop {
        while let Some(piece) = block::take(&disk.data) {
            let ok = disk.serve(piece);
            block::done(ok);
        }
        // Until the kernel has a request (EVENT_WORK); an interrupt that
        // came late for the last one wakes it too, harmlessly.
        driver::wait(60_000);
    }
}
