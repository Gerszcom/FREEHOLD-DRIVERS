//! E1000.EXE — a user-mode driver (D-127) for Intel's PRO/1000 Ethernet
//! controllers that share the 8254x legacy descriptor format: the 82540EM
//! QEMU calls `e1000`, the 82544GC and 82545EM, the 82541, and the 82574L
//! (QEMU's `e1000e`). It is the kernel's built-in e1000 driver moved out of
//! the kernel (DIRECTION.md Stage 16, part 2 of the drivers task): the same
//! rings, the same register sequence, now an ordinary FREEHOLD program given
//! this one device. It reaches the card's registers through its memory BAR
//! mapped with `DRIVER_MAP`, its rings and buffers through `DRIVER_DMA`
//! memory, and the stack through the network bridge (`DRIVER_NET`, `_RX`,
//! `_TX`). It needs no `unsafe`: every register access and every byte of DMA
//! goes through the checked wrappers in `freehold_rt::driver`.
//!
//! Each turn of the loop hands the stack every frame received and gives the
//! card every frame the stack queued that the transmit ring has room for.
//! Between turns it sleeps in `DRIVER_WAIT` (D-242). A card with MSI-X (the
//! 82574L) or MSI is given its interrupt (D-250): on MSI-X every cause —
//! receive queue 0, transmit queue 0 and "other" (the link) — is aimed at
//! entry 0 through IVAR; the driver sleeps until the card interrupts or the
//! stack queues a frame, a second at most, and clears the causes it woke
//! for (ICR, written back) before it looks at the rings. A card with
//! neither (the 8254x QEMU offers) is polled every millisecond, as the
//! built-in polled it (D-049), its interrupts masked.
//!
//! The 82574L's EEPROM read register (EERD) has its address at bit 2 and
//! its done bit at bit 1, where the 8254x's are at bit 8 and bit 4; both
//! are tried, the card's own first.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use freehold_rt::driver::{self, Bar0, Dma};
use freehold_rt::{println, thread, Psp};

freehold_rt::program!(main);

// Registers, as byte offsets into BAR0.
const CTRL: u32 = 0x0000;
const STATUS: u32 = 0x0008;
const EERD: u32 = 0x0014;
const CTRL_EXT: u32 = 0x0018;
const ICR: u32 = 0x00C0;
const IMS: u32 = 0x00D0;
const IMC: u32 = 0x00D8;
/// The 82574L's interrupt vector allocation: which MSI-X entry each cause
/// is sent to.
const IVAR: u32 = 0x00E4;
const RCTL: u32 = 0x0100;
const TCTL: u32 = 0x0400;
const TIPG: u32 = 0x0410;
const RDBAL: u32 = 0x2800;
const RDBAH: u32 = 0x2804;
const RDLEN: u32 = 0x2808;
const RDH: u32 = 0x2810;
const RDT: u32 = 0x2818;
const TDBAL: u32 = 0x3800;
const TDBAH: u32 = 0x3804;
const TDLEN: u32 = 0x3808;
const TDH: u32 = 0x3810;
const TDT: u32 = 0x3818;
const MTA: u32 = 0x5200;
const RAL0: u32 = 0x5400;
const RAH0: u32 = 0x5404;

const CTRL_ASDE: u32 = 1 << 5;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const STATUS_LU: u32 = 1 << 1;
const RAH_AV: u32 = 1 << 31;
/// CTRL_EXT: the MSI-X pending-bit array is in use (82574L, with MSI-X).
const CTRL_EXT_PBA_SUPPORT: u32 = 1 << 31;

// Interrupt causes (ICR, IMS). Without MSI-X: transmit descriptor written
// back, link status change, receive minimum threshold, receiver overrun,
// receive timer (a frame written back).
const ICR_TXDW: u32 = 1 << 0;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXDMT0: u32 = 1 << 4;
const ICR_RXO: u32 = 1 << 6;
const ICR_RXT0: u32 = 1 << 7;
// With MSI-X (82574L): receive queue 0, transmit queue 0, and "other"
// (the link change, overrun and the rest).
const ICR_RXQ0: u32 = 1 << 20;
const ICR_TXQ0: u32 = 1 << 22;
const ICR_OTHER: u32 = 1 << 24;
/// IVAR: receive queue 0 (bits 3:0), transmit queue 0 (11:8) and other
/// (19:16) each to MSI-X entry 0, valid (bit 3 of each field).
const IVAR_ALL_TO_ENTRY_0: u32 = 0x8 | (0x8 << 8) | (0x8 << 16);
/// How long the driver sleeps with interrupts, at most: the link's state
/// is looked at then too (a link change also interrupts).
const IDLE_MS: u64 = 1000;

const RCTL_EN: u32 = 1 << 1;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0x10 << 4;
const TCTL_COLD: u32 = 0x40 << 12;

const DESC_DD: u8 = 1;
const TX_EOP: u8 = 1 << 0;
const TX_IFCS: u8 = 1 << 1;
const TX_RS: u8 = 1 << 3;

/// 64 frames each way, as the built-in had: a 64 KiB TCP window arrives in
/// bursts of ~45 frames between two polls (D-208).
const RING_LEN: usize = 64;
const BUF_SIZE: usize = 2048;
/// The ring page: receive descriptors, then transmit descriptors.
const RX_DESC: usize = 0x000;
const TX_DESC: usize = 0x800;
const MAX_FRAME: usize = 1514;

struct Nic {
    regs: Bar0,
    rings: Dma,
    /// The receive buffers, then the transmit buffers, 2 KiB each.
    buffers: Dma,
    rx_head: usize,
    tx_tail: usize,
}

impl Nic {
    fn rx_buf(&self, i: usize) -> u64 {
        self.buffers.phys() + (i * BUF_SIZE) as u64
    }

    fn tx_buf(&self, i: usize) -> u64 {
        self.buffers.phys() + ((RING_LEN + i) * BUF_SIZE) as u64
    }

    /// A 16-byte descriptor at `at` in the ring page: buffer address, then
    /// the second quadword (`hi`, low dword first).
    fn put_desc(&self, at: usize, addr: u64, lo: u32, hi: u32) {
        self.rings.write_u32(at, addr as u32);
        self.rings.write_u32(at + 4, (addr >> 32) as u32);
        self.rings.write_u32(at + 8, lo);
        self.rings.write_u32(at + 12, hi);
    }

    /// EEPROM word `word`: through EERD laid out as the 82574L's (address
    /// at bit 2, done at bit 1) when `extended`, else as the 8254x's
    /// (address at bit 8, done at bit 4); start is bit 0 in both.
    fn eeprom_read_as(&self, word: u8, extended: bool) -> Option<u16> {
        let (shift, done) = if extended { (2, 1 << 1) } else { (8, 1 << 4) };
        self.regs.write32(EERD, (u32::from(word) << shift) | 1);
        for _ in 0..100_000 {
            let v = self.regs.read32(EERD);
            if v & done != 0 {
                return Some((v >> 16) as u16);
            }
        }
        None
    }

    /// EEPROM word `word`, the card's own layout first (`extended`: an
    /// 82574L), then the other.
    fn eeprom_read(&self, word: u8, extended: bool) -> Option<u16> {
        self.eeprom_read_as(word, extended).or_else(|| self.eeprom_read_as(word, !extended))
    }

    /// The MAC the firmware loaded into receive address 0, or else the
    /// EEPROM's first three words.
    fn read_mac(&self, extended: bool) -> [u8; 6] {
        let ral = self.regs.read32(RAL0);
        let rah = self.regs.read32(RAH0);
        if rah & RAH_AV != 0 && (ral != 0 || rah & 0xFFFF != 0) {
            let l = ral.to_le_bytes();
            return [l[0], l[1], l[2], l[3], rah as u8, (rah >> 8) as u8];
        }
        let mut mac = [0u8; 6];
        for i in 0..3 {
            let w = self.eeprom_read(i as u8, extended).unwrap_or(0);
            mac[2 * i] = w as u8;
            mac[2 * i + 1] = (w >> 8) as u8;
        }
        mac
    }

    fn link_up(&self) -> bool {
        self.regs.read32(STATUS) & STATUS_LU != 0
    }

    /// Unmask the causes the driver wakes for, on the interrupt the kernel
    /// gave it (D-242): on MSI-X each aimed at entry 0, on MSI the classic
    /// causes. Whatever was pending is cleared.
    fn enable_interrupts(&self, kind: driver::Interrupt) {
        let r = &self.regs;
        let causes = match kind {
            driver::Interrupt::MsiX => {
                r.write32(CTRL_EXT, r.read32(CTRL_EXT) | CTRL_EXT_PBA_SUPPORT);
                r.write32(IVAR, IVAR_ALL_TO_ENTRY_0);
                ICR_RXQ0 | ICR_TXQ0 | ICR_OTHER | ICR_LSC
            }
            driver::Interrupt::Msi => ICR_RXT0 | ICR_TXDW | ICR_LSC | ICR_RXDMT0 | ICR_RXO,
        };
        self.acknowledge();
        r.write32(IMS, causes);
    }

    /// Clear the causes the card raised, so the next one interrupts again:
    /// read ICR and write back what it held (on MSI-X reading alone does
    /// not clear it; without, the read did and the write changes nothing).
    fn acknowledge(&self) {
        let icr = self.regs.read32(ICR);
        if icr != 0 {
            self.regs.write32(ICR, icr);
        }
    }

    /// Reset the card and bring it up: the built-in's sequence.
    fn setup(&mut self, extended: bool) -> Result<[u8; 6], &'static str> {
        let r = &self.regs;
        r.write32(CTRL, r.read32(CTRL) | CTRL_RST);
        let t0 = thread::monotonic_ns();
        while r.read32(CTRL) & CTRL_RST != 0 {
            if thread::monotonic_ns().wrapping_sub(t0) > 100_000_000 {
                return Err("the card did not come out of reset");
            }
        }
        // Interrupts off; clear whatever is pending.
        r.write32(IMC, 0xFFFF_FFFF);
        let _ = r.read32(ICR);
        // Link up, auto speed detection; no PHY reset, no VLAN mode.
        let ctrl = r.read32(CTRL);
        r.write32(CTRL, (ctrl | CTRL_SLU | CTRL_ASDE) & !(1 << 3) & !(1 << 7) & !(1 << 31));
        let mac = self.read_mac(extended);
        // Receive address 0 = our MAC, valid; no multicast table entries.
        r.write32(RAL0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        r.write32(RAH0, u32::from(mac[4]) | (u32::from(mac[5]) << 8) | RAH_AV);
        for i in 0..128 {
            r.write32(MTA + i * 4, 0);
        }
        // Receive ring: every descriptor owns a buffer, all handed to the
        // card (tail = last).
        for i in 0..RING_LEN {
            self.put_desc(RX_DESC + i * 16, self.rx_buf(i), 0, 0);
        }
        let rx = self.rings.phys() + RX_DESC as u64;
        r.write32(RDBAL, rx as u32);
        r.write32(RDBAH, (rx >> 32) as u32);
        r.write32(RDLEN, (RING_LEN * 16) as u32);
        r.write32(RDH, 0);
        r.write32(RDT, (RING_LEN - 1) as u32);
        self.rx_head = 0;
        // Transmit ring: empty, every slot's status DD (free).
        for i in 0..RING_LEN {
            self.put_desc(TX_DESC + i * 16, self.tx_buf(i), 0, u32::from(DESC_DD));
        }
        let tx = self.rings.phys() + TX_DESC as u64;
        r.write32(TDBAL, tx as u32);
        r.write32(TDBAH, (tx >> 32) as u32);
        r.write32(TDLEN, (RING_LEN * 16) as u32);
        r.write32(TDH, 0);
        r.write32(TDT, 0);
        self.tx_tail = 0;
        r.write32(TIPG, 10 | (8 << 10) | (6 << 20));
        r.write32(TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);
        // Receive: broadcast, all multicast, 2 KiB buffers, CRC stripped.
        r.write32(RCTL, RCTL_EN | RCTL_BAM | RCTL_MPE | RCTL_SECRC);
        Ok(mac)
    }

    /// Hand the stack every frame the card has received (a ring's worth at
    /// most per turn).
    fn drain_receive(&mut self, scratch: &mut [u8; BUF_SIZE]) {
        for _ in 0..RING_LEN {
            let at = RX_DESC + self.rx_head * 16;
            // Second quadword: length (16), checksum (16), status (8),
            // errors (8), special (16).
            let w2 = self.rings.read_u32(at + 8);
            let w3 = self.rings.read_u32(at + 12);
            if (w3 as u8) & DESC_DD == 0 {
                return;
            }
            let len = (w2 & 0xFFFF) as usize;
            let errors = (w3 >> 8) as u8;
            if errors == 0 && (14..=MAX_FRAME).contains(&len) {
                self.buffers.read(self.rx_head * BUF_SIZE, &mut scratch[..len]);
                driver::rx(&scratch[..len]);
            }
            // Give the descriptor back and advance.
            self.rings.write_u32(at + 8, 0);
            self.rings.write_u32(at + 12, 0);
            self.regs.write32(RDT, self.rx_head as u32);
            self.rx_head = (self.rx_head + 1) % RING_LEN;
        }
    }

    /// Give the card every frame the stack has queued, while the transmit
    /// ring has a free slot. A frame is taken from the stack only once a
    /// slot is free for it, so none is lost to a full ring.
    fn send_queued(&mut self, scratch: &mut [u8; BUF_SIZE]) {
        let mut sent = false;
        for _ in 0..RING_LEN {
            let at = TX_DESC + self.tx_tail * 16;
            if (self.rings.read_u32(at + 12) as u8) & DESC_DD == 0 {
                break; // the card has not finished with this slot: ring full
            }
            let len = driver::tx(&mut scratch[..MAX_FRAME]);
            if len == 0 {
                break;
            }
            self.buffers.write((RING_LEN + self.tx_tail) * BUF_SIZE, &scratch[..len]);
            // Length, CSO 0, command EOP|IFCS|RS; status 0 (not done).
            let cmd = TX_EOP | TX_IFCS | TX_RS;
            self.put_desc(at, self.tx_buf(self.tx_tail), (len as u32) | (u32::from(cmd) << 24), 0);
            self.tx_tail = (self.tx_tail + 1) % RING_LEN;
            sent = true;
        }
        if sent {
            self.regs.write32(TDT, self.tx_tail as u32);
        }
    }
}

fn main(_psp: &Psp) -> u8 {
    let Some(dev) = driver::device() else {
        println!("E1000: run by the driver system, not by hand.");
        return 1;
    };
    // The registers are BAR0, a memory BAR.
    if !dev.bars[0].is_some_and(|b| !b.io) {
        println!("E1000: BAR0 is not a memory BAR.");
        return 1;
    }
    driver::enable();
    let Some(regs) = Bar0::map(0) else {
        println!("E1000: cannot map the card's registers (BAR0).");
        return 1;
    };
    if regs.len() < 0x6000 {
        println!("E1000: BAR0 is too small ({} bytes).", regs.len());
        return 1;
    }
    let Some(rings) = Dma::alloc(4096) else {
        println!("E1000: no memory for the descriptor rings.");
        return 1;
    };
    let Some(buffers) = Dma::alloc(2 * RING_LEN * BUF_SIZE) else {
        println!("E1000: no memory for the frame buffers.");
        return 1;
    };
    let mut nic = Nic { regs, rings, buffers, rx_head: 0, tx_tail: 0 };
    // The 82574L (8086:10D3) has the newer EEPROM read layout.
    let extended = dev.vendor == 0x8086 && dev.device == 0x10D3;
    let mac = match nic.setup(extended) {
        Ok(mac) => mac,
        Err(e) => {
            println!("E1000: {e}.");
            return 1;
        }
    };
    // The card's interrupt (D-242, D-250): MSI-X on the 82574L, MSI where
    // a card has only that; neither on the 8254x QEMU offers: polled.
    let irq = driver::interrupts();
    if let Some(kind) = irq {
        nic.enable_interrupts(kind);
    }
    let how = match irq {
        Some(driver::Interrupt::MsiX) => "interrupts (MSI-X)",
        Some(driver::Interrupt::Msi) => "interrupts (MSI)",
        None => "polled",
    };
    let say = |link: bool| {
        let mut line = alloc::string::String::from(if link { "link up, " } else { "link down, " });
        line.push_str(how);
        driver::status(&line);
    };
    let mut link = nic.link_up();
    driver::net(mac, link);
    say(link);
    println!(
        "E1000: {:04x}:{:04x} at {:02x}:{:02x}.{} up, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}, {}",
        dev.vendor, dev.device, dev.bus, dev.slot, dev.function,
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        if link { "up" } else { "down" },
        how
    );

    let mut scratch = [0u8; BUF_SIZE];
    let mut turns: u32 = 0;
    let mut events = driver::Events::default();
    loop {
        if irq.is_some() && (events.interrupt || events.timed_out()) {
            // Clear first: a frame written back after this interrupts anew
            // (and, once a second, whatever slipped past an interrupt).
            nic.acknowledge();
        }
        nic.drain_receive(&mut scratch);
        nic.send_queued(&mut scratch);
        // The link's state: with interrupts, when the card interrupted (a
        // link change does) or a second passed; polled, about four times a
        // second.
        turns = turns.wrapping_add(1);
        let look = if irq.is_some() { !events.work || events.interrupt } else { turns.is_multiple_of(256) };
        if look {
            let now = nic.link_up();
            if now != link {
                link = now;
                driver::net(mac, link);
                say(link);
            }
        }
        // Sleep until the card interrupts or the stack queues a frame
        // (D-242); polled, a millisecond (a queued frame ends it early).
        events = driver::wait(if irq.is_some() { IDLE_MS } else { 1 });
    }
}
