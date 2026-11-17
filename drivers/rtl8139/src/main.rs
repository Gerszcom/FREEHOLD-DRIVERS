//! RTL8139.EXE — a user-mode driver (D-127) for the Realtek RTL8139
//! Ethernet controller (PCI 10EC:8139), the NIC QEMU offers that FREEHOLD
//! has no built-in driver for. It runs as an ordinary FREEHOLD program,
//! started at boot for the card the kernel assigned it, and talks to it
//! only through the driver system calls: port I/O on the card's I/O BAR,
//! two DMA buffers (a receive ring and a transmit buffer), and the network
//! bridge that moves frames to and from FREEHOLD's stack. It needs no
//! `unsafe`: every register poke and every byte of DMA goes through the
//! checked wrappers in `freehold_rt::driver`.
//!
//! Each turn of the loop clears the card's interrupt status, drains received
//! packets to the stack and sends the frames it queued. The card has no MSI;
//! where the firmware's routing of its INTx pin is known it takes that line
//! (FREEHOLD D-278: level-triggered, perhaps shared) and sleeps in
//! `DRIVER_WAIT` until the card interrupts, the stack queues a frame or a
//! second passes; the status is cleared before the ring is read, so a
//! packet that lands after it interrupts again. Otherwise it is polled: the
//! same turn every millisecond.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use freehold_rt::driver::{self, Dma};
use freehold_rt::{println, thread, Psp};

freehold_rt::program!(main);

// RTL8139 registers, as byte offsets into the I/O BAR.
const IDR0: u64 = 0x00; // MAC address, 6 bytes
const TSD0: u64 = 0x10; // transmit status/command, 4 descriptors, stride 4
const TSAD0: u64 = 0x20; // transmit start address, 4 descriptors, stride 4
const RBSTART: u64 = 0x30; // receive buffer physical address
const CR: u64 = 0x37; // command register (u8)
const CAPR: u64 = 0x38; // current address of packet read (u16)
const IMR: u64 = 0x3C; // interrupt mask (u16)
const ISR: u64 = 0x3E; // interrupt status (u16)
const RCR: u64 = 0x44; // receive configuration (u32)
const CONFIG1: u64 = 0x52; // (u8)

const CR_RST: u8 = 0x10;
const CR_RE: u8 = 0x08;
const CR_TE: u8 = 0x04;
const CR_BUFE: u8 = 0x01; // receive buffer empty (read-only)

const TSD_OWN: u32 = 1 << 13; // set by the card when the descriptor is free
const RX_OK: u16 = 0x0001; // packet status: received OK

// Interrupt causes (IMR, ISR): receive OK and error, transmit error,
// receive buffer overflow, link change, receive FIFO overflow. Transmit OK
// is left out: a busy descriptor is waited for when it is next used.
const INTERRUPTS: u16 = 0x0001 | 0x0002 | 0x0008 | 0x0010 | 0x0020 | 0x0040;
/// Longest sleep with interrupts: the link and anything missed, once a second.
const IDLE_MS: u64 = 1000;

// The card has four transmit descriptors and advances through them in
// order; each gets a 2 KiB slot in one DMA buffer.
const TX_SLOTS: usize = 4;
const TX_SLOT: usize = 2048;

// The receive ring is RBLEN=00 (8 KiB + 16), with 1500 bytes of pad so the
// card (WRAP set) never splits a packet across the end.
const RX_RING: usize = 8192;
const RX_BYTES: usize = RX_RING + 16 + 1500;
const MAX_FRAME: usize = 1514;

fn main(_psp: &Psp) -> u8 {
    let Some(dev) = driver::device() else {
        println!("RTL8139: run by the driver system, not by hand.");
        return 1;
    };
    // The card's I/O BAR (BAR0 on the RTL8139).
    let Some(bar) = (0..6).find(|&i| dev.bars[i].is_some_and(|b| b.io)) else {
        println!("RTL8139: no I/O BAR on the device.");
        return 1;
    };
    let bar = bar as u8;

    driver::enable();
    driver::out8(bar, CONFIG1, 0x00); // power on
    driver::out8(bar, CR, CR_RST); // soft reset
    for _ in 0..1000 {
        if driver::in8(bar, CR) & CR_RST == 0 {
            break;
        }
    }
    if driver::in8(bar, CR) & CR_RST != 0 {
        println!("RTL8139: the card did not come out of reset.");
        return 1;
    }

    let Some(rx) = Dma::alloc(RX_BYTES) else {
        println!("RTL8139: no memory for the receive ring.");
        return 1;
    };
    let Some(tx) = Dma::alloc(TX_SLOTS * TX_SLOT) else {
        println!("RTL8139: no memory for the transmit buffers.");
        return 1;
    };
    driver::out32(bar, RBSTART, rx.phys() as u32);
    // One transmit buffer per descriptor.
    for i in 0..TX_SLOTS {
        driver::out32(
            bar,
            TSAD0 + (i * 4) as u64,
            tx.phys() as u32 + (i * TX_SLOT) as u32,
        );
    }

    // Receive: physical-address match, broadcast and multicast, WRAP set,
    // unlimited DMA burst, no receive threshold.
    let rcr: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 7) | (0b111 << 8) | (0b111 << 13);
    driver::out32(bar, RCR, rcr);
    driver::out16(bar, IMR, 0x0000); // no interrupts until there is a line
    driver::out8(bar, CR, CR_RE | CR_TE);
    // Its INTx line (D-278), if the kernel can deliver it.
    let irq = driver::interrupts_or_intx();
    if irq.is_some() {
        let isr = driver::in16(bar, ISR);
        driver::out16(bar, ISR, isr);
        driver::out16(bar, IMR, INTERRUPTS);
    }
    let how = if irq.is_some() { "interrupts (INTx)" } else { "polled" };

    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = driver::in8(bar, IDR0 + i as u64);
    }
    driver::net(mac, true);
    // Post a one-line status the DRIVERS report shows (DRIVER_STATUS): a NIC
    // is already named there by DRIVER_NET, but the same call serves a driver
    // with no stack interface, so exercise it here with the card's mode.
    let mut line = alloc::string::String::from("link up, 10/100 Mb/s, ");
    line.push_str(how);
    driver::status(&line);
    println!(
        "RTL8139: {:04x}:{:04x} at {:02x}:{:02x}.{} up, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, {}",
        dev.vendor, dev.device, dev.bus, dev.slot, dev.function,
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], how
    );

    let mut rx_offset: usize = 0;
    let mut frame = [0u8; MAX_FRAME];
    let mut tx_cur: usize = 0;
    let mut issued: u32 = 0;
    loop {
        // Clear first: a packet that lands after this interrupts anew (on
        // a level-triggered line the card must be quiet before the wait).
        let isr = driver::in16(bar, ISR);
        if isr != 0 {
            driver::out16(bar, ISR, isr);
        }
        drain_receive(bar, &rx, &mut rx_offset, &mut frame);
        if irq.is_some() {
            // Every frame the stack queued.
            while send_one(bar, &tx, &mut frame, &mut tx_cur, &mut issued) {}
            driver::wait(IDLE_MS);
        } else {
            send_one(bar, &tx, &mut frame, &mut tx_cur, &mut issued);
            thread::sleep_ms(1);
        }
    }
}

/// Hand the stack every packet the card has received.
fn drain_receive(bar: u8, rx: &Dma, offset: &mut usize, scratch: &mut [u8; MAX_FRAME]) {
    let mut guard = 0;
    while driver::in8(bar, CR) & CR_BUFE == 0 {
        let status = rx.read_u16(*offset);
        let len = rx.read_u16(*offset + 2) as usize;
        // A good packet carries its length including the 4-byte CRC.
        if status & RX_OK != 0 && (14..=MAX_FRAME + 4).contains(&len) {
            let payload = len - 4;
            rx.read(*offset + 4, &mut scratch[..payload]);
            driver::rx(&scratch[..payload]);
        }
        // Advance past this packet (header + data), 4-byte aligned.
        *offset = (*offset + len + 4 + 3) & !3;
        if *offset >= RX_RING {
            *offset -= RX_RING;
        }
        // CAPR trails the read pointer by 16 (its hardware offset).
        driver::out16(bar, CAPR, (offset.wrapping_sub(16)) as u16);
        guard += 1;
        if guard > 256 {
            break; // never spin forever on a confused ring
        }
    }
}

/// Send one frame the stack has queued, if any. The card takes the four
/// descriptors in turn, so each frame goes in the next one; a descriptor
/// used before is waited on until the card frees it (OWN set).
fn send_one(bar: u8, tx: &Dma, scratch: &mut [u8; MAX_FRAME], cur: &mut usize, issued: &mut u32) -> bool {
    let len = driver::tx(scratch);
    if len == 0 {
        return false;
    }
    let slot = *cur;
    let tsd = TSD0 + (slot * 4) as u64;
    if *issued >= TX_SLOTS as u32 {
        for _ in 0..1_000_000 {
            if driver::in32(bar, tsd) & TSD_OWN != 0 {
                break;
            }
        }
    }
    tx.write(slot * TX_SLOT, &scratch[..len]);
    // Writing the length to the descriptor (OWN clear) starts the transfer.
    driver::out32(bar, tsd, len as u32);
    *cur = (slot + 1) % TX_SLOTS;
    *issued += 1;
    true
}
