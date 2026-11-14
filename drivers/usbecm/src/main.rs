//! USBECM.EXE — a user-mode driver (D-127) for USB Ethernet adapters of the
//! CDC Ethernet Control Model (ECM): QEMU's `usb-net` and the many real
//! adapters that offer an ECM configuration. It is the kernel's built-in
//! `usb/ecm.rs` moved out of the kernel (DIRECTION.md Stage 16, part 2 of
//! the drivers task, D-259): the same configuration chosen, the same
//! requests, frames travelling whole on the data interface's two bulk
//! endpoints, each ended by a short packet.
//!
//! It reaches its device only through the USB bridge (`DRIVER_USB`): the
//! descriptors the kernel read at boot, the configuration it selects,
//! control transfers on endpoint 0, OUT transfers, and an IN stream of
//! eight 2 KiB transfers the kernel keeps queued on the bulk IN endpoint.
//! The kernel copies every byte between its buffers and this driver, so
//! the driver never sees a physical address. A completed IN transfer wakes
//! it from `DRIVER_WAIT` as an interrupt would; a frame the stack queues
//! wakes it too. It needs no `unsafe`.
//!
//! The MAC address is the string descriptor the Ethernet Networking
//! functional descriptor names. The link is up from the start, as the
//! built-in had it: the communications interface's notification endpoint
//! is not read (QEMU's adapter answers every poll of it, about thirty a
//! second, which would wake the driver for nothing).
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use freehold_rt::driver::{self, usb};
use freehold_rt::{println, Psp};

freehold_rt::program!(main);

/// Interface classes: communications (ECM is subclass 06h) and CDC data.
const CLASS_COMM: u8 = 0x02;
const SUBCLASS_ECM: u8 = 0x06;
const CLASS_DATA: u8 = 0x0A;
/// The Ethernet Networking functional descriptor's subtype.
const ETHERNET_FUNCTIONAL: u8 = 0x0F;
/// Requests.
const SET_INTERFACE: u8 = 11;
const SET_ETHERNET_PACKET_FILTER: u8 = 0x43;
/// Directed, broadcast and all-multicast (DHCP and ARP need the first two).
const PACKET_FILTER: u16 = 0x000E;
/// The receive stream: eight transfers of 2 KiB queued, as the built-in.
const RX_SIZE: usize = 2048;
const RX_COUNT: usize = 8;
/// Ethernet frames, without the FCS.
const MIN_FRAME: usize = 14;
const MAX_FRAME: usize = 1514;
/// The longest the driver sleeps with nothing to do.
const IDLE_MS: u64 = 1000;

/// What the driver uses of its adapter's ECM configuration.
struct Ecm {
    value: u8,
    control: u8,
    mac_index: u8,
    data: u8,
    alternate: u8,
    bulk_in: u8,
    bulk_out: u8,
    out_packet: u16,
}

/// The first configuration offering CDC Ethernet: a communications
/// interface of subclass 6 and a data interface with bulk endpoints both
/// ways (adapters often offer RNDIS first).
fn find_ecm(d: &usb::Descriptors) -> Option<Ecm> {
    for c in &d.configurations {
        let Some(control) = c
            .interfaces
            .iter()
            .find(|i| i.class == CLASS_COMM && i.subclass == SUBCLASS_ECM)
        else {
            continue;
        };
        let Some(data) = c.interfaces.iter().find(|i| {
            i.class == CLASS_DATA
                && i.endpoints.iter().any(|e| e.is_bulk() && e.is_in())
                && i.endpoints.iter().any(|e| e.is_bulk() && !e.is_in())
        }) else {
            continue;
        };
        let bulk = |input: bool| data.endpoints.iter().copied().find(|e| e.is_bulk() && e.is_in() == input);
        let (Some(bulk_in), Some(bulk_out)) = (bulk(true), bulk(false)) else {
            continue;
        };
        let mac_index = control
            .functional
            .iter()
            .find(|f| f.len() >= 4 && f[2] == ETHERNET_FUNCTIONAL)
            .map_or(0, |f| f[3]);
        return Some(Ecm {
            value: c.value,
            control: control.number,
            mac_index,
            data: data.number,
            alternate: data.alternate,
            bulk_in: bulk_in.address,
            bulk_out: bulk_out.address,
            out_packet: bulk_out.packet(),
        });
    }
    None
}

/// Twelve hex digits (the MAC's string descriptor), or `None`.
fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let digits: Vec<u8> = text
        .bytes()
        .filter_map(|b| (b as char).to_digit(16).map(|d| d as u8))
        .collect();
    if digits.len() != 12 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, m) in mac.iter_mut().enumerate() {
        *m = digits[2 * i] << 4 | digits[2 * i + 1];
    }
    Some(mac)
}

fn main(_psp: &Psp) -> u8 {
    let Some(dev) = driver::device() else {
        println!("USBECM: run by the driver system, not by hand.");
        return 1;
    };
    if !dev.usb {
        println!("USBECM: {:04x}:{:04x} is not a USB device.", dev.vendor, dev.device);
        return 1;
    }
    let Some(desc) = usb::describe().as_deref().and_then(usb::Descriptors::parse) else {
        println!("USBECM: the device's descriptors cannot be read.");
        return 1;
    };
    let Some(ecm) = find_ecm(&desc) else {
        println!("USBECM: {:04x}:{:04x} offers no CDC Ethernet configuration.", dev.vendor, dev.device);
        return 1;
    };
    if let Err(e) = usb::configure(ecm.value) {
        println!("USBECM: configuration {} refused ({:?}).", ecm.value, e);
        return 1;
    }
    // The data interface carries frames in its alternate setting with the
    // endpoints (1 on an ECM adapter).
    let mut none = [0u8; 0];
    let set_interface = usb::Setup {
        request_type: 0x01,
        request: SET_INTERFACE,
        value: u16::from(ecm.alternate),
        index: u16::from(ecm.data),
        length: 0,
    };
    if let Err(e) = usb::control(set_interface, &mut none) {
        println!("USBECM: the data interface does not take alternate setting {} ({:?}).", ecm.alternate, e);
        return 1;
    }
    let mac = parse_mac(&usb::string(ecm.mac_index)).unwrap_or([0x02, 0, 0, 0, 0, dev.slot]);
    // Optional for the adapter: an error is not fatal.
    let filter = usb::Setup {
        request_type: 0x21,
        request: SET_ETHERNET_PACKET_FILTER,
        value: PACKET_FILTER,
        index: u16::from(ecm.control),
        length: 0,
    };
    let _ = usb::control(filter, &mut none);
    if let Err(e) = usb::in_start(ecm.bulk_in, RX_SIZE, RX_COUNT) {
        println!("USBECM: the receive stream cannot start ({:?}).", e);
        return 1;
    }
    driver::net(mac, true);
    driver::status(&format!("link up, USB {:04x}:{:04x}, woken by its transfers", dev.vendor, dev.device));
    println!(
        "USBECM: {:04x}:{:04x} on usb{}.{} up, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, configuration {}",
        dev.vendor, dev.device, dev.bus, dev.slot, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], ecm.value
    );

    let mut frame = [0u8; RX_SIZE];
    loop {
        // Every frame the adapter delivered, to the stack.
        while let Ok(Some(n)) = usb::in_take(ecm.bulk_in, &mut frame) {
            if (MIN_FRAME..=MAX_FRAME).contains(&n) {
                driver::rx(&frame[..n]);
            }
        }
        // Every frame the stack queued, to the adapter; one that fills its
        // last packet exactly is ended by a zero-length one.
        loop {
            let n = driver::tx(&mut frame[..MAX_FRAME]);
            if n == 0 {
                break;
            }
            if usb::out(ecm.bulk_out, &frame[..n]).is_ok()
                && ecm.out_packet > 0
                && n.is_multiple_of(usize::from(ecm.out_packet))
            {
                let _ = usb::out(ecm.bulk_out, &[]);
            }
        }
        let _ = driver::wait(IDLE_MS);
    }
}
