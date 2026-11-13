//! VIRTNET.EXE — a user-mode driver (D-127) for the virtio network device
//! over the modern (virtio 1.x) PCI transport: QEMU's `virtio-net-pci`,
//! transitional (1AF4:1000) or modern-only (1AF4:1041). It is the kernel's
//! built-in virtio-net driver moved out of the kernel (DIRECTION.md Stage 16,
//! part 2 of the drivers task, D-231): the same feature set, the same
//! queues, the same handshake, now an ordinary FREEHOLD program given this
//! one device.
//!
//! It finds the transport's structures (common configuration, notify,
//! device configuration) in its device's capability list, read with
//! `DRIVER_CONFIG`; reaches them in the memory BARs they name, mapped with
//! `DRIVER_MAP`; keeps its virtqueues and frame buffers in `DRIVER_DMA`
//! memory; and moves frames to and from the stack through the network
//! bridge (`DRIVER_NET`, `_RX`, `_TX`). It needs no `unsafe`: every register
//! access and every byte of DMA goes through the checked wrappers in
//! `freehold_rt::driver`.
//!
//! As the built-in: no mergeable buffers, no offloads, no control queue —
//! a frame is one descriptor, after the 12-byte virtio-net header; the MAC
//! and link status come from the device configuration when offered. Each
//! turn hands the stack every frame received and gives the device every
//! frame the stack queued that the transmit queue has room for. Between
//! turns it sleeps in `DRIVER_WAIT` (D-242): with the device's MSI-X
//! interrupt (both queues and configuration changes on entry 0) it sleeps
//! until the device interrupts or the stack queues a frame, a second at
//! most; a device without MSI-X is polled every millisecond, as the
//! built-in was (D-049).
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use core::sync::atomic::{fence, Ordering};

use freehold_rt::driver::{self, Bar0, Dma};
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
const NET_F_MAC: u64 = 1 << 5;
const NET_F_STATUS: u64 = 1 << 16;

const DESC_F_WRITE: u16 = 2;
/// The avail ring's flags: the device need not interrupt (it is polled).
const AVAIL_F_NO_INTERRUPT: u16 = 1;
/// The MSI-X table entry every interrupt source is given: the one the
/// kernel set up (`DRIVER_IRQ`).
const VECTOR: u16 = 0;
/// How long the driver sleeps with interrupts, at most: the link's state
/// is looked at then too (a configuration change also interrupts).
const IDLE_MS: u64 = 1000;

/// Entries per queue: 64 frames each way, as E1000.EXE has — a 64 KiB TCP
/// window arrives in bursts of ~45 frames between two polls (D-208).
const QUEUE_LEN: u16 = 64;
// One page per queue: descriptors (16 bytes each), the available ring
// (flags, idx, ring[QUEUE_LEN], used_event), the used ring (flags, idx,
// ring[QUEUE_LEN] of (id u32, len u32), avail_event).
const DESC_OFFSET: usize = 0x000;
const AVAIL_OFFSET: usize = 0x800;
const USED_OFFSET: usize = 0xC00;

const HEADER_LEN: usize = 12;
const BUF_SIZE: usize = 2048;
const MAX_FRAME: usize = 1514;
const RECEIVE_QUEUE: u16 = 0;
const TRANSMIT_QUEUE: u16 = 1;

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
        // The capability (16 bytes) and, for notify, its multiplier after it.
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

/// One split virtqueue in its own DMA page. The driver numbers the
/// descriptors: descriptor `i` always holds buffer `i`.
struct Queue {
    ring: Dma,
    index: u16,
    notify_at: u32,
    avail_idx: u16,
    last_used: u16,
    /// The device is polled: it need not interrupt for this queue.
    quiet: bool,
}

impl Queue {
    fn desc(&self, i: u16, addr: u64, len: u32, flags: u16) {
        let at = DESC_OFFSET + usize::from(i) * 16;
        self.ring.write_u32(at, addr as u32);
        self.ring.write_u32(at + 4, (addr >> 32) as u32);
        self.ring.write_u32(at + 8, len);
        // flags (u16), then next (u16): no chains.
        self.ring.write_u32(at + 12, u32::from(flags));
    }

    /// Offer descriptor `head` to the device (without notifying it).
    fn offer(&mut self, head: u16) {
        let slot = usize::from(self.avail_idx % QUEUE_LEN);
        self.ring.write(AVAIL_OFFSET + 4 + slot * 2, &head.to_le_bytes());
        fence(Ordering::SeqCst);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // flags and idx in one 32-bit store, so the device never sees half
        // an index.
        let flags = if self.quiet { AVAIL_F_NO_INTERRUPT } else { 0 };
        self.ring.write_u32(AVAIL_OFFSET, u32::from(flags) | (u32::from(self.avail_idx) << 16));
        fence(Ordering::SeqCst);
    }

    fn kick(&self, bars: &Bars, t: &Transport) {
        bars.w16(&t.notify, self.notify_at, self.index);
    }

    /// The next used element: (descriptor, bytes the device wrote).
    fn used(&mut self) -> Option<(u16, u32)> {
        fence(Ordering::SeqCst);
        if self.ring.read_u16(USED_OFFSET + 2) == self.last_used {
            return None;
        }
        let at = USED_OFFSET + 4 + usize::from(self.last_used % QUEUE_LEN) * 8;
        let id = self.ring.read_u32(at);
        let len = self.ring.read_u32(at + 4);
        self.last_used = self.last_used.wrapping_add(1);
        Some(((id % u32::from(QUEUE_LEN)) as u16, len))
    }
}

struct Nic {
    bars: Bars,
    t: Transport,
    rx: Queue,
    tx: Queue,
    rx_bufs: Dma,
    tx_bufs: Dma,
    /// Transmit descriptors the device has finished with.
    tx_free: [bool; QUEUE_LEN as usize],
    status: bool,
}

impl Nic {
    /// Reset the device and negotiate: the MAC and link status if offered,
    /// nothing else. Leaves FEATURES_OK set; the features accepted.
    fn negotiate(&self) -> Result<u64, &'static str> {
        let (b, c) = (&self.bars, &self.t.common);
        b.w8(c, DEVICE_STATUS, 0);
        let t0 = thread::monotonic_ns();
        while b.r8(c, DEVICE_STATUS) != 0 {
            if thread::monotonic_ns().wrapping_sub(t0) > 1_000_000_000 {
                return Err("the device did not reset");
            }
        }
        b.w8(c, DEVICE_STATUS, STATUS_ACKNOWLEDGE);
        b.w8(c, DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        b.w32(c, DEVICE_FEATURE_SELECT, 0);
        let lo = u64::from(b.r32(c, DEVICE_FEATURE));
        b.w32(c, DEVICE_FEATURE_SELECT, 1);
        let hi = u64::from(b.r32(c, DEVICE_FEATURE));
        let offered = lo | (hi << 32);
        if offered & F_VERSION_1 == 0 {
            b.w8(c, DEVICE_STATUS, STATUS_FAILED);
            return Err("a legacy-only device (no virtio 1.x)");
        }
        let wanted = (offered & (NET_F_MAC | NET_F_STATUS)) | F_VERSION_1;
        b.w32(c, DRIVER_FEATURE_SELECT, 0);
        b.w32(c, DRIVER_FEATURE, wanted as u32);
        b.w32(c, DRIVER_FEATURE_SELECT, 1);
        b.w32(c, DRIVER_FEATURE, (wanted >> 32) as u32);
        b.w8(c, DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        if b.r8(c, DEVICE_STATUS) & STATUS_FEATURES_OK == 0 {
            b.w8(c, DEVICE_STATUS, STATUS_FAILED);
            return Err("the device refused the features");
        }
        Ok(wanted)
    }

    /// Set queue `index` up in `ring` with `QUEUE_LEN` entries, its
    /// interrupt on MSI-X entry `vector` if given. The queue, and whether
    /// the device took the vector.
    fn setup_queue(
        bars: &Bars,
        t: &Transport,
        index: u16,
        ring: Dma,
        vector: Option<u16>,
    ) -> Result<(Queue, bool), &'static str> {
        let c = &t.common;
        bars.w16(c, QUEUE_SELECT, index);
        if bars.r16(c, QUEUE_SIZE) < QUEUE_LEN {
            return Err("a queue is too small");
        }
        bars.w16(c, QUEUE_SIZE, QUEUE_LEN);
        bars.w64(c, QUEUE_DESC, ring.phys() + DESC_OFFSET as u64);
        bars.w64(c, QUEUE_DRIVER, ring.phys() + AVAIL_OFFSET as u64);
        bars.w64(c, QUEUE_DEVICE, ring.phys() + USED_OFFSET as u64);
        let notify_at = u32::from(bars.r16(c, QUEUE_NOTIFY_OFF)).wrapping_mul(t.notify_multiplier);
        if u64::from(notify_at) + 2 > u64::from(t.notify.length) {
            return Err("a queue's notify address is outside the notify structure");
        }
        // Before the queue is enabled (virtio 1.x, 4.1.4.3.2), read back:
        // NO_VECTOR if the device could not take it.
        let took = vector.is_some_and(|v| {
            bars.w16(c, QUEUE_MSIX_VECTOR, v);
            bars.r16(c, QUEUE_MSIX_VECTOR) == v
        });
        bars.w16(c, QUEUE_ENABLE, 1);
        Ok((Queue { ring, index, notify_at, avail_idx: 0, last_used: 0, quiet: true }, took))
    }

    fn driver_ok(&self) {
        self.bars.w8(
            &self.t.common,
            DEVICE_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
    }

    fn mac(&self, wanted: u64) -> [u8; 6] {
        match &self.t.device {
            Some(d) if wanted & NET_F_MAC != 0 => {
                let mut m = [0u8; 6];
                for (i, b) in m.iter_mut().enumerate() {
                    *b = self.bars.r8(d, i as u32);
                }
                m
            }
            // The built-in's fallback when the device names none.
            _ => [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
        }
    }

    fn link_up(&self) -> bool {
        // Without VIRTIO_NET_F_STATUS the link is always up.
        match (&self.t.device, self.status) {
            (Some(d), true) => self.bars.r16(d, 6) & 1 != 0,
            _ => true,
        }
    }

    fn post_receive(&mut self, i: u16) {
        let addr = self.rx_bufs.phys() + u64::from(i) * BUF_SIZE as u64;
        self.rx.desc(i, addr, BUF_SIZE as u32, DESC_F_WRITE);
        self.rx.offer(i);
    }

    /// Hand the stack every frame the device has received (a queue's worth
    /// at most per turn), each buffer given straight back.
    fn drain_receive(&mut self, scratch: &mut [u8; BUF_SIZE]) {
        let mut posted = false;
        for _ in 0..QUEUE_LEN {
            let Some((id, written)) = self.rx.used() else {
                break;
            };
            let written = written as usize;
            if written > HEADER_LEN && written - HEADER_LEN <= MAX_FRAME {
                let len = written - HEADER_LEN;
                self.rx_bufs
                    .read(usize::from(id) * BUF_SIZE + HEADER_LEN, &mut scratch[..len]);
                driver::rx(&scratch[..len]);
            }
            self.post_receive(id);
            posted = true;
        }
        if posted {
            self.rx.kick(&self.bars, &self.t);
        }
    }

    /// Give the device every frame the stack has queued, while a transmit
    /// descriptor is free. A frame is taken from the stack only once a slot
    /// is free for it, so none is lost to a full queue.
    fn send_queued(&mut self, scratch: &mut [u8; BUF_SIZE]) {
        while let Some((id, _)) = self.tx.used() {
            self.tx_free[usize::from(id)] = true;
        }
        let mut sent = false;
        while let Some(i) = self.tx_free.iter().position(|f| *f) {
            let len = driver::tx(&mut scratch[HEADER_LEN..HEADER_LEN + MAX_FRAME]);
            if len == 0 {
                break;
            }
            self.tx_free[i] = false;
            scratch[..HEADER_LEN].fill(0);
            let start = i * BUF_SIZE;
            self.tx_bufs.write(start, &scratch[..HEADER_LEN + len]);
            let addr = self.tx_bufs.phys() + start as u64;
            self.tx.desc(i as u16, addr, (HEADER_LEN + len) as u32, 0);
            self.tx.offer(i as u16);
            sent = true;
        }
        if sent {
            self.tx.kick(&self.bars, &self.t);
        }
    }
}

fn main(_psp: &Psp) -> u8 {
    let Some(dev) = driver::device() else {
        println!("VIRTNET: run by the driver system, not by hand.");
        return 1;
    };
    let Some(t) = find_transport() else {
        println!(
            "VIRTNET: {:04x}:{:04x} at {:02x}:{:02x}.{} has no modern (virtio 1.x) transport.",
            dev.vendor, dev.device, dev.bus, dev.slot, dev.function
        );
        return 1;
    };
    driver::enable();
    let mut bars = Bars { mapped: [None, None, None, None, None, None] };
    let regions = [Some(t.common), Some(t.notify), t.device];
    if !regions.iter().flatten().all(|r| bars.map(r)) {
        println!("VIRTNET: cannot map the transport's memory BARs.");
        return 1;
    }
    let (Some(rx_ring), Some(tx_ring)) = (Dma::alloc(4096), Dma::alloc(4096)) else {
        println!("VIRTNET: no memory for the queues.");
        return 1;
    };
    let bytes = usize::from(QUEUE_LEN) * BUF_SIZE;
    let (Some(rx_bufs), Some(tx_bufs)) = (Dma::alloc(bytes), Dma::alloc(bytes)) else {
        println!("VIRTNET: no memory for the frame buffers.");
        return 1;
    };
    // Negotiate first (it resets the device), then the queues.
    let probe = Nic {
        bars,
        t,
        rx: Queue { ring: rx_ring, index: RECEIVE_QUEUE, notify_at: 0, avail_idx: 0, last_used: 0, quiet: true },
        tx: Queue { ring: tx_ring, index: TRANSMIT_QUEUE, notify_at: 0, avail_idx: 0, last_used: 0, quiet: true },
        rx_bufs,
        tx_bufs,
        tx_free: [true; QUEUE_LEN as usize],
        status: false,
    };
    let wanted = match probe.negotiate() {
        Ok(w) => w,
        Err(e) => {
            println!("VIRTNET: {e}.");
            return 1;
        }
    };
    if probe.bars.r16(&probe.t.common, NUM_QUEUES) < 2 {
        println!("VIRTNET: the device has no receive and transmit queue pair.");
        return 1;
    }
    let Nic { bars, t, rx, tx, rx_bufs, tx_bufs, tx_free, .. } = probe;
    // The device's interrupt (D-242): virtio's is MSI-X, entry 0 for the
    // configuration changes and both queues. Without it, polled.
    let msix = driver::interrupts() == Some(driver::Interrupt::MsiX) && {
        bars.w16(&t.common, MSIX_CONFIG, VECTOR);
        bars.r16(&t.common, MSIX_CONFIG) == VECTOR
    };
    let vector = msix.then_some(VECTOR);
    let (rx, tx, queues_took) = match (
        Nic::setup_queue(&bars, &t, RECEIVE_QUEUE, rx.ring, vector),
        Nic::setup_queue(&bars, &t, TRANSMIT_QUEUE, tx.ring, vector),
    ) {
        (Ok((rx, a)), Ok((tx, b))) => (rx, tx, a && b),
        (Err(e), _) | (_, Err(e)) => {
            println!("VIRTNET: {e}.");
            return 1;
        }
    };
    let interrupts = msix && queues_took;
    let mut nic = Nic { bars, t, rx, tx, rx_bufs, tx_bufs, tx_free, status: wanted & NET_F_STATUS != 0 };
    nic.rx.quiet = !interrupts;
    nic.tx.quiet = !interrupts;
    for i in 0..QUEUE_LEN {
        nic.post_receive(i);
    }
    nic.driver_ok();
    nic.rx.kick(&nic.bars, &nic.t);
    let mac = nic.mac(wanted);
    let mut link = nic.link_up();
    let how = if interrupts { "interrupts (MSI-X)" } else { "polled" };
    let say = |link: bool| {
        let mut line = alloc::string::String::from(if link { "link up, " } else { "link down, " });
        line.push_str(how);
        driver::status(&line);
    };
    driver::net(mac, link);
    say(link);
    println!(
        "VIRTNET: {:04x}:{:04x} at {:02x}:{:02x}.{} up, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}, {}",
        dev.vendor, dev.device, dev.bus, dev.slot, dev.function,
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        if link { "up" } else { "down" },
        how
    );

    let mut scratch = [0u8; BUF_SIZE];
    let mut turns: u32 = 0;
    let mut events = driver::Events::default();
    loop {
        nic.drain_receive(&mut scratch);
        nic.send_queued(&mut scratch);
        // The link's state: with interrupts, when the device interrupted
        // (a configuration change does) or a second passed; polled, about
        // four times a second.
        turns = turns.wrapping_add(1);
        let look = if interrupts { !events.work || events.interrupt } else { turns.is_multiple_of(256) };
        if look {
            let now = nic.link_up();
            if now != link {
                link = now;
                driver::net(mac, link);
                say(link);
            }
        }
        // Sleep until the device interrupts or the stack queues a frame
        // (D-242); polled, a millisecond (and a queued frame ends it early).
        events = driver::wait(if interrupts { IDLE_MS } else { 1 });
    }
}
