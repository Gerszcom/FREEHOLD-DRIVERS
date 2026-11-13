//! HDA.EXE — a user-mode driver (D-127) for Intel High Definition Audio
//! controllers: the kernel's built-in HD Audio driver moved out of the
//! kernel (DIRECTION.md Stage 16, part 2 of the drivers task; FREEHOLD
//! D-238). The controller, its command rings, the codec walk and the
//! output stream are here; the kernel keeps the sound family — every
//! program's stream, rate conversion, mixing, the volume and the boot
//! chime — and lends this driver the ring it mixes into while something
//! plays (the sound bridge, `DRIVER_SOUND`).
//!
//! As the built-in did (D-085, D-103): the controller is stopped, reset and
//! given its command and response rings (CORB and RIRB, up to 256 entries),
//! with the immediate command registers as the fallback; every response is
//! acknowledged. For the first codec with an audio function group it reads
//! each widget, follows every connected line-out, speaker and headphone pin
//! through selectors and mixers to an analog output converter, selects the
//! connection, unmutes the amplifiers at 0 dB, enables the pin (headphone
//! drive, EAPD) and powers the widgets on. One output stream (tag 1) feeds
//! every converter on those paths: 16-bit stereo at 48 kHz when they all
//! offer it, else 44.1 kHz, else the highest rate they share.
//!
//! Idle, it holds one page of DMA memory — CORB (1 KiB), RIRB (2 KiB) and
//! the stream's buffer descriptor list (at 3 KiB) — and sleeps in the
//! kernel until a program opens a stream. Playing, it tells the mixer the
//! link position as each piece of the ring has been played: the ring is
//! cut into pieces of about 20 ms, each buffer descriptor asks for an
//! interrupt on completion, and the driver sleeps in `DRIVER_WAIT` until
//! its controller's MSI comes (FREEHOLD D-242; 1.1.0) — about fifty
//! wake-ups a second while something plays, none while nothing does.
//! A controller without MSI (or a kernel without `DRIVER_IRQ`) is polled
//! every 2 ms as 1.0.0 was. Only the output stream's interrupt is
//! enabled; the command rings stay polled. When the last stream ends it
//! stops the stream and the kernel takes the ring back. It needs no
//! `unsafe`: every register access and every byte of DMA goes through the
//! checked wrappers in `freehold_rt::driver`.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use freehold_rt::driver::{self, Bar0, Dma, SoundRing};
use freehold_rt::{println, thread, Psp};

freehold_rt::program!(main);

// Controller registers.
const GCAP: u32 = 0x00;
const GCTL: u32 = 0x08;
const STATESTS: u32 = 0x0E;
const INTCTL: u32 = 0x20;
const CORBLBASE: u32 = 0x40;
const CORBUBASE: u32 = 0x44;
const CORBWP: u32 = 0x48;
const CORBRP: u32 = 0x4A;
const CORBCTL: u32 = 0x4C;
const CORBSIZE: u32 = 0x4E;
const RIRBLBASE: u32 = 0x50;
const RIRBUBASE: u32 = 0x54;
const RIRBWP: u32 = 0x58;
const RINTCNT: u32 = 0x5A;
const RIRBCTL: u32 = 0x5C;
const RIRBSTS: u32 = 0x5D;
const RIRBSIZE: u32 = 0x5E;
const ICOI: u32 = 0x60;
const IRII: u32 = 0x64;
const ICS: u32 = 0x68;
const SD_BASE: u32 = 0x80;
const SD_SIZE: u32 = 0x20;
// Stream descriptor registers, from the descriptor's base.
const SD_CTL: u32 = 0x00;
const SD_STS: u32 = 0x03;
const SD_CTL_STREAM: u32 = 0x02;
const SD_LPIB: u32 = 0x04;
const SD_CBL: u32 = 0x08;
const SD_LVI: u32 = 0x0C;
const SD_FIFOS: u32 = 0x10;
const SD_FMT: u32 = 0x12;
const SD_BDPL: u32 = 0x18;
const SD_BDPU: u32 = 0x1C;

// Codec verbs and parameters.
const GET_PARAM: u32 = 0xF00;
const GET_CONN_LIST: u32 = 0xF02;
const GET_CONFIG_DEFAULT: u32 = 0xF1C;
const GET_POWER_STATE: u32 = 0xF05;
const SET_CONN_SELECT: u32 = 0x701;
const SET_POWER_STATE: u32 = 0x705;
const SET_STREAM: u32 = 0x706;
const SET_PIN_CONTROL: u32 = 0x707;
const SET_EAPD: u32 = 0x70C;
const SET_FORMAT: u32 = 0x2; // 4-bit verb, 16-bit payload
const SET_AMP: u32 = 0x3; // 4-bit verb, 16-bit payload
const P_VENDOR: u32 = 0x00;
const P_NODES: u32 = 0x04;
const P_FG_TYPE: u32 = 0x05;
const P_WIDGET_CAPS: u32 = 0x09;
const P_PCM: u32 = 0x0A;
const P_PIN_CAPS: u32 = 0x0C;
const P_IN_AMP: u32 = 0x0D;
const P_CONN_LEN: u32 = 0x0E;
const P_OUT_AMP: u32 = 0x12;

const KIND_OUTPUT: u32 = 0;
const KIND_MIXER: u32 = 2;
const KIND_SELECTOR: u32 = 3;
const KIND_PIN: u32 = 4;

/// The stream tag the output stream and its converters share.
const STREAM_TAG: u8 = 1;
/// Where the rings and the descriptor list sit in the driver's one page.
const RIRB_OFFSET: usize = 1024;
const BDL_OFFSET: usize = 3072;
/// The ring the kernel lends is played in pieces of about this long (ms),
/// a buffer descriptor each, between 4 and `BDL_MAX` of them.
const PIECE_MS: u32 = 20;
const BDL_MAX: usize = 64;
/// How often a playing stream's position is given to the mixer when the
/// controller has no interrupt (ms).
const POLL_MS: u64 = 2;
/// With an interrupt, the longest sleep between two looks at the position
/// (ms): should an interrupt not come, the mixer (150 ms ahead) still runs.
const IRQ_WAIT_MS: u64 = 100;
/// INTCTL: global interrupt enable; a stream's enable is bit (its index).
const GIE: u32 = 1 << 31;
/// A stream's control: run, interrupt on completion; its status: buffer
/// completion, FIFO error, descriptor error (each cleared by writing 1).
const SD_RUN: u8 = 0x02;
const SD_IOCE: u8 = 0x04;
const SD_STS_ALL: u8 = 0x1C;
/// How long one wait for something to play lasts before it is asked again.
const WAIT_MS: u64 = 1000;

/// The rates a codec may offer, in the order of its PCM capability bits;
/// with each the format register's base (44.1 kHz or 48 kHz), multiplier
/// and divisor fields.
const RATES: [(u32, u16, u16, u16); 9] = [
    (8000, 0, 0, 5),
    (11025, 1, 0, 3),
    (16000, 0, 0, 2),
    (22050, 1, 0, 1),
    (32000, 0, 1, 2),
    (44100, 1, 0, 0),
    (48000, 0, 0, 0),
    (88200, 1, 1, 0),
    (96000, 0, 1, 0),
];

struct Widget {
    nid: u8,
    kind: u32,
    caps: u32,
    conns: Vec<u8>,
    pin_caps: u32,
    config: u32,
    in_amp: u32,
    out_amp: u32,
}

struct Hda {
    regs: Bar0,
    /// CORB, RIRB and the buffer descriptor list.
    page: Dma,
    corb_entries: u16,
    rirb_entries: u16,
    rirb_read: u16,
    immediate: bool,
    codec: u8,
    codec_id: u32,
    dacs: Vec<u8>,
    outputs: Vec<&'static str>,
    rates: u32,
    /// The first output stream descriptor's registers, and its index
    /// (input streams come first).
    sd: u32,
    sd_index: u32,
    mix_rate: u32,
    /// The controller interrupts on each piece played (its MSI).
    irq: bool,
}

fn now_ns() -> u64 {
    thread::monotonic_ns()
}

fn delay_us(us: u64) {
    let until = now_ns() + us * 1000;
    while now_ns() < until {
        core::hint::spin_loop();
    }
}

/// Wait up to `ms` for `done`; true if it came.
fn wait_for(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let until = now_ns() + ms * 1_000_000;
    loop {
        if done() {
            return true;
        }
        if now_ns() >= until {
            return false;
        }
        core::hint::spin_loop();
    }
}

impl Hda {
    /// Send one verb and return the codec's answer.
    fn command(&mut self, nid: u8, verb: u32) -> Option<u32> {
        let word = (u32::from(self.codec) << 28) | (u32::from(nid) << 20) | (verb & 0xF_FFFF);
        if self.immediate {
            return self.command_immediate(word);
        }
        self.regs.write8(RIRBSTS, 0x05);
        let wp = (self.regs.read16(CORBWP) & 0xFF).wrapping_add(1) % self.corb_entries;
        self.page.write_u32(usize::from(wp) * 4, word);
        self.regs.write16(CORBWP, wp);
        loop {
            let index = self.rirb_read;
            let regs = &self.regs;
            if !wait_for(20, || regs.read16(RIRBWP) & 0xFF != index) {
                return None;
            }
            let index = (index + 1) % self.rirb_entries;
            self.rirb_read = index;
            let at = RIRB_OFFSET + usize::from(index) * 8;
            let response = self.page.read_u32(at);
            let extra = self.page.read_u32(at + 4);
            // Acknowledge the answer: a controller stops taking commands
            // once its response count is reached until this is cleared.
            self.regs.write8(RIRBSTS, 0x05);
            if extra & 0x10 != 0 {
                continue; // unsolicited: not our answer
            }
            return Some(response);
        }
    }

    fn command_immediate(&mut self, word: u32) -> Option<u32> {
        let regs = &self.regs;
        if !wait_for(20, || regs.read16(ICS) & 1 == 0) {
            return None;
        }
        regs.write16(ICS, 0x2); // clear "result valid"
        regs.write32(ICOI, word);
        regs.write16(ICS, 0x1);
        if !wait_for(20, || regs.read16(ICS) & 0x3 == 0x2) {
            return None;
        }
        Some(regs.read32(IRII))
    }

    fn param(&mut self, nid: u8, p: u32) -> u32 {
        self.command(nid, (GET_PARAM << 8) | p).unwrap_or(0)
    }

    fn verb12(&mut self, nid: u8, verb: u32, payload: u8) {
        let _ = self.command(nid, (verb << 8) | u32::from(payload));
    }

    fn verb4(&mut self, nid: u8, verb: u32, payload: u16) {
        let _ = self.command(nid, (verb << 16) | u32::from(payload));
    }

    fn connections(&mut self, nid: u8) -> Vec<u8> {
        let len = self.param(nid, P_CONN_LEN);
        let count = (len & 0x7F) as usize;
        let long = len & 0x80 != 0;
        let per = if long { 2 } else { 4 };
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < count {
            let word = self
                .command(nid, (GET_CONN_LIST << 8) | i as u32)
                .unwrap_or(0);
            for j in 0..per {
                if i + j >= count {
                    break;
                }
                let (entry, range) = if long {
                    let e = (word >> (16 * j)) & 0xFFFF;
                    ((e & 0x7FFF) as u8, e & 0x8000 != 0)
                } else {
                    let e = (word >> (8 * j)) & 0xFF;
                    ((e & 0x7F) as u8, e & 0x80 != 0)
                };
                match (range, out.last().copied()) {
                    (true, Some(prev)) if entry > prev => out.extend(prev + 1..=entry),
                    _ => out.push(entry),
                }
            }
            i += per;
        }
        out
    }
}

/// The 0 dB step of an amplifier, unmuted.
fn amp_0db(caps: u32) -> u16 {
    (caps & 0x7F) as u16
}

fn pin_name(config: u32) -> &'static str {
    match (config >> 20) & 0xF {
        0 => "line out",
        1 => "speaker",
        2 => "headphones",
        _ => "output",
    }
}

/// From a pin down to an output converter: the widgets on the way, each
/// with the index of the connection taken (the converter's is unused).
fn path_to_dac(
    widgets: &[Widget],
    nid: u8,
    depth: usize,
    seen: &mut Vec<u8>,
) -> Option<Vec<(u8, usize)>> {
    let w = widgets.iter().find(|w| w.nid == nid)?;
    if w.kind == KIND_OUTPUT {
        // Analog converters only (bit 9: digital).
        return (w.caps & (1 << 9) == 0).then(|| vec![(nid, 0)]);
    }
    if depth > 6 || seen.contains(&nid) {
        return None;
    }
    if !(w.kind == KIND_PIN || w.kind == KIND_MIXER || w.kind == KIND_SELECTOR) {
        return None;
    }
    seen.push(nid);
    for (i, &next) in w.conns.iter().enumerate() {
        if let Some(mut rest) = path_to_dac(widgets, next, depth + 1, seen) {
            rest.insert(0, (nid, i));
            return Some(rest);
        }
    }
    None
}

/// The format register's value for `rate`, 16-bit stereo.
fn format_for(rate: u32) -> Option<u16> {
    let &(_, base44, mult, div) = RATES.iter().find(|r| r.0 == rate)?;
    Some((base44 << 14) | (mult << 11) | (div << 8) | (1 << 4) | 1)
}

/// How many buffer descriptors the ring is played in: pieces of about
/// `PIECE_MS` at `rate` (16-bit stereo), a power of two from 4 to
/// `BDL_MAX`, so each is a whole number of 128-byte lines.
fn bdl_entries(rate: u32, ring: usize) -> usize {
    let piece = (rate as usize * 4 * PIECE_MS as usize / 1000).max(128);
    let mut entries = 4;
    while entries < BDL_MAX && ring / (entries * 2) >= piece {
        entries *= 2;
    }
    entries
}

/// Choose the largest ring the controller offers (256, 16 or 2 entries).
fn pick_ring_size(regs: &Bar0, reg: u32) -> u16 {
    let size = regs.read8(reg);
    let (select, entries) = if size & 0x40 != 0 {
        (2, 256)
    } else if size & 0x20 != 0 {
        (1, 16)
    } else {
        (0, 2)
    };
    regs.write8(reg, (size & !0x3) | select);
    entries
}

/// Reset the controller, give it its rings and find a codec with an
/// output. `Err` says why not.
fn start(name: &str) -> Result<Hda, String> {
    let regs = Bar0::map(0).ok_or_else(|| format!("{name}: cannot map its registers (BAR 0)"))?;
    let page = Dma::alloc(4096).ok_or_else(|| format!("{name}: no DMA memory"))?;
    let gcap = regs.read16(GCAP);
    let iss = u32::from((gcap >> 8) & 0xF);
    let oss = u32::from((gcap >> 12) & 0xF);
    let bss = u32::from((gcap >> 3) & 0x1F);
    if oss == 0 {
        return Err(format!("{name} has no output streams"));
    }
    // Stop whatever the firmware left running, then reset the controller.
    regs.write8(CORBCTL, 0);
    regs.write8(RIRBCTL, 0);
    for s in 0..iss + oss + bss {
        let at = SD_BASE + s * SD_SIZE;
        regs.write8(at + SD_CTL, regs.read8(at + SD_CTL) & !0x02);
    }
    regs.write32(INTCTL, 0);
    regs.write32(GCTL, regs.read32(GCTL) & !1);
    if !wait_for(100, || regs.read32(GCTL) & 1 == 0) {
        return Err(format!("{name} did not enter reset"));
    }
    delay_us(200);
    regs.write32(GCTL, regs.read32(GCTL) | 1);
    if !wait_for(100, || regs.read32(GCTL) & 1 == 1) {
        return Err(format!("{name} did not leave reset"));
    }
    // Codecs announce themselves within 521 us of the reset ending: wait
    // for the first, then a little for any others.
    wait_for(2, || regs.read16(STATESTS) & 0x7FFF != 0);
    delay_us(100);
    let present = regs.read16(STATESTS);
    regs.write16(STATESTS, present);

    // The command ring (CORB, 1 KiB at 0) and the answer ring (RIRB,
    // 2 KiB at 1 KiB), in the one page.
    let corb_entries = pick_ring_size(&regs, CORBSIZE);
    let rirb_entries = pick_ring_size(&regs, RIRBSIZE);
    let phys = page.phys();
    let rirb = phys + RIRB_OFFSET as u64;
    regs.write32(CORBLBASE, phys as u32);
    regs.write32(CORBUBASE, (phys >> 32) as u32);
    regs.write16(CORBWP, 0);
    regs.write16(CORBRP, 0x8000);
    wait_for(1, || regs.read16(CORBRP) & 0x8000 != 0);
    regs.write16(CORBRP, 0);
    wait_for(1, || regs.read16(CORBRP) & 0x8000 == 0);
    regs.write32(RIRBLBASE, rirb as u32);
    regs.write32(RIRBUBASE, (rirb >> 32) as u32);
    regs.write16(RIRBWP, 0x8000);
    regs.write16(RINTCNT, 1);
    regs.write8(CORBCTL, 0x02);
    // DMA on, and the response "interrupt" flag on (INTCTL stays 0, so no
    // interrupt is raised): some controllers, QEMU's among them, take no
    // more commands after RINTCNT answers until that flag is cleared.
    regs.write8(RIRBCTL, 0x03);

    let mut hda = Hda {
        regs,
        page,
        corb_entries,
        rirb_entries,
        rirb_read: 0,
        immediate: false,
        codec: 0,
        codec_id: 0,
        dacs: Vec::new(),
        outputs: Vec::new(),
        rates: 0,
        sd: SD_BASE + iss * SD_SIZE,
        sd_index: iss,
        mix_rate: 0,
        irq: false,
    };
    for codec in 0..15u8 {
        if present & (1 << codec) == 0 {
            continue;
        }
        hda.codec = codec;
        if hda.command(0, (GET_PARAM << 8) | P_VENDOR).is_none() && !hda.immediate {
            // No answer through the rings: try the immediate interface.
            hda.immediate = true;
        }
        if setup_codec(&mut hda) {
            return Ok(hda);
        }
    }
    hda.regs.write8(CORBCTL, 0);
    hda.regs.write8(RIRBCTL, 0);
    Err(format!("{name}: no codec with an output"))
}

/// Find the audio function group, its widgets and the paths from its
/// output pins to converters; set those paths up. False if there is none.
fn setup_codec(h: &mut Hda) -> bool {
    h.codec_id = h.param(0, P_VENDOR);
    let nodes = h.param(0, P_NODES);
    let (first, count) = (((nodes >> 16) & 0xFF) as u8, (nodes & 0xFF) as u8);
    let Some(afg) =
        (first..first.saturating_add(count)).find(|&n| h.param(n, P_FG_TYPE) & 0xFF == 1)
    else {
        return false;
    };
    h.verb12(afg, SET_POWER_STATE, 0);
    // Up to 10 ms to reach D0: polled, not slept through (D-103). A codec
    // that does not answer the question is taken to be up.
    let t0 = now_ns();
    while now_ns() - t0 < 10_000_000 {
        match h.command(afg, GET_POWER_STATE << 8) {
            Some(r) if (r >> 4) & 0xF != 0 => core::hint::spin_loop(),
            _ => break,
        }
    }
    let default_out = h.param(afg, P_OUT_AMP);
    let default_in = h.param(afg, P_IN_AMP);
    let default_pcm = h.param(afg, P_PCM);
    let nodes = h.param(afg, P_NODES);
    let (first, count) = (((nodes >> 16) & 0xFF) as u8, (nodes & 0xFF) as u8);
    let mut widgets = Vec::new();
    for nid in first..first.saturating_add(count) {
        let caps = h.param(nid, P_WIDGET_CAPS);
        let kind = (caps >> 20) & 0xF;
        let override_amp = caps & (1 << 3) != 0;
        let conns = if caps & (1 << 8) != 0 {
            h.connections(nid)
        } else {
            Vec::new()
        };
        let (pin_caps, config) = if kind == KIND_PIN {
            (
                h.param(nid, P_PIN_CAPS),
                h.command(nid, GET_CONFIG_DEFAULT << 8).unwrap_or(0),
            )
        } else {
            (0, 0)
        };
        widgets.push(Widget {
            nid,
            kind,
            caps,
            conns,
            pin_caps,
            config,
            in_amp: if override_amp {
                h.param(nid, P_IN_AMP)
            } else {
                default_in
            },
            out_amp: if override_amp {
                h.param(nid, P_OUT_AMP)
            } else {
                default_out
            },
        });
    }
    // Output pins that are connected to something: line out, speaker and
    // headphones first; any output-capable pin if the codec names none.
    let output_pin =
        |w: &Widget| w.kind == KIND_PIN && w.pin_caps & (1 << 4) != 0 && w.caps & (1 << 9) == 0;
    let mut pins: Vec<u8> = widgets
        .iter()
        .filter(|w| output_pin(w) && w.config >> 30 != 1 && (w.config >> 20) & 0xF <= 2)
        .map(|w| w.nid)
        .collect();
    if pins.is_empty() {
        pins = widgets
            .iter()
            .filter(|w| output_pin(w))
            .map(|w| w.nid)
            .collect();
    }
    let mut rates = u32::MAX;
    for pin in pins {
        let Some(path) = path_to_dac(&widgets, pin, 0, &mut Vec::new()) else {
            continue;
        };
        for (step, &(nid, index)) in path.iter().enumerate() {
            let Some(w) = widgets.iter().find(|w| w.nid == nid) else {
                continue;
            };
            if w.caps & (1 << 10) != 0 {
                h.verb12(nid, SET_POWER_STATE, 0);
            }
            let last = step + 1 == path.len();
            if !last && w.conns.len() > 1 && w.kind != KIND_MIXER {
                h.verb12(nid, SET_CONN_SELECT, index as u8);
            }
            if !last && w.kind == KIND_MIXER && w.caps & (1 << 1) != 0 {
                // Input amplifier of the connection taken: unmuted, 0 dB.
                h.verb4(
                    nid,
                    SET_AMP,
                    0x7000 | ((index as u16) << 8) | amp_0db(w.in_amp),
                );
            }
            if w.caps & (1 << 2) != 0 {
                h.verb4(nid, SET_AMP, 0xB000 | amp_0db(w.out_amp));
            }
            if w.kind == KIND_PIN {
                let hp = (w.config >> 20) & 0xF == 2 && w.pin_caps & (1 << 3) != 0;
                h.verb12(nid, SET_PIN_CONTROL, if hp { 0xC0 } else { 0x40 });
                if w.pin_caps & (1 << 16) != 0 {
                    h.verb12(nid, SET_EAPD, 0x02);
                }
            }
            if last && !h.dacs.contains(&nid) {
                h.dacs.push(nid);
                let pcm = if w.caps & (1 << 4) != 0 {
                    h.param(nid, P_PCM)
                } else {
                    default_pcm
                };
                let pcm = if pcm == 0 { default_pcm } else { pcm };
                // 16-bit samples are required.
                rates &= if pcm & (1 << 17) != 0 { pcm & 0x1FF } else { 0 };
            }
        }
        let pin_widget = widgets.iter().find(|w| w.nid == pin);
        h.outputs
            .push(pin_widget.map_or("output", |w| pin_name(w.config)));
    }
    h.outputs.dedup();
    h.rates = if h.dacs.is_empty() { 0 } else { rates };
    // Mix at 48 kHz if every converter offers it, else 44.1 kHz, else the
    // highest rate they share.
    let offered = |r: u32| {
        RATES
            .iter()
            .position(|x| x.0 == r)
            .is_some_and(|i| h.rates & (1 << i) != 0)
    };
    h.mix_rate = [48000, 44100]
        .into_iter()
        .find(|&r| offered(r))
        .or_else(|| RATES.iter().rev().map(|x| x.0).find(|&r| offered(r)))
        .unwrap_or(0);
    !h.dacs.is_empty() && h.mix_rate != 0
}

impl Hda {
    fn stop_stream(&self) {
        let ctl = self.sd + SD_CTL;
        self.regs.write8(ctl, self.regs.read8(ctl) & !(SD_RUN | SD_IOCE));
        let regs = &self.regs;
        wait_for(10, || regs.read8(ctl) & SD_RUN == 0);
        self.regs.write8(self.sd + SD_STS, SD_STS_ALL);
    }

    /// Take the controller's MSI (D-242) and enable the output stream's
    /// interrupt alone; false (polled) without one.
    fn take_interrupt(&mut self) -> bool {
        if driver::interrupts().is_none() {
            return false;
        }
        self.regs.write32(INTCTL, GIE | (1 << self.sd_index));
        self.irq = true;
        true
    }

    /// Start the output stream on the ring the mixer lent: its buffer
    /// descriptor list, the stream's registers, the converters' format and
    /// tag. The bytes the controller fetches ahead of its position.
    fn start_stream(&mut self, ring: SoundRing) -> u32 {
        let format = format_for(self.mix_rate).unwrap_or(0);
        let entries = bdl_entries(self.mix_rate, ring.len);
        let piece = (ring.len / entries) as u64;
        // Interrupt on completion of each piece, when there is one.
        let ioc = u32::from(self.irq);
        for i in 0..entries {
            let at = BDL_OFFSET + i * 16;
            let address = ring.phys + i as u64 * piece;
            self.page.write_u32(at, address as u32);
            self.page.write_u32(at + 4, (address >> 32) as u32);
            self.page.write_u32(at + 8, piece as u32);
            self.page.write_u32(at + 12, ioc);
        }
        self.stop_stream();
        let ctl = self.sd + SD_CTL;
        let regs = &self.regs;
        regs.write8(ctl, 0x01);
        wait_for(10, || regs.read8(ctl) & 0x01 != 0);
        regs.write8(ctl, 0x00);
        wait_for(10, || regs.read8(ctl) & 0x01 == 0);
        let bdl = self.page.phys() + BDL_OFFSET as u64;
        regs.write32(self.sd + SD_CBL, ring.len as u32);
        regs.write16(self.sd + SD_LVI, (entries - 1) as u16);
        regs.write16(self.sd + SD_FMT, format);
        regs.write32(self.sd + SD_BDPL, bdl as u32);
        regs.write32(self.sd + SD_BDPU, (bdl >> 32) as u32);
        regs.write8(self.sd + SD_CTL_STREAM, STREAM_TAG << 4);
        for dac in self.dacs.clone() {
            self.verb4(dac, SET_FORMAT, format);
            self.verb12(dac, SET_STREAM, STREAM_TAG << 4);
        }
        let fifos = u32::from(self.regs.read16(self.sd + SD_FIFOS));
        self.regs.write8(self.sd + SD_STS, SD_STS_ALL);
        self.regs
            .write8(ctl, if self.irq { SD_RUN | SD_IOCE } else { SD_RUN });
        (fifos + 64).next_multiple_of(4).max(256)
    }

    /// The last stream ended: stop the controller's stream and let go of
    /// the ring.
    fn stop(&mut self) {
        self.stop_stream();
        for dac in self.dacs.clone() {
            self.verb12(dac, SET_STREAM, 0);
        }
        self.regs.write32(self.sd + SD_BDPL, 0);
        self.regs.write32(self.sd + SD_BDPU, 0);
    }

    /// Play the ring until the mixer says stop.
    fn play(&mut self, ring: SoundRing) {
        let fetched = self.start_stream(ring);
        let len = ring.len as u32;
        loop {
            if self.irq {
                // Until a piece has been played (or, should its interrupt
                // not come, a while); then acknowledge it before looking
                // at the position, so the next piece interrupts again.
                driver::wait(IRQ_WAIT_MS);
                self.regs.write8(self.sd + SD_STS, SD_STS_ALL);
            } else {
                thread::sleep_ms(POLL_MS);
            }
            let position = self.regs.read32(self.sd + SD_LPIB) % len.max(1);
            if !driver::sound_played(position, fetched) {
                break;
            }
        }
        self.stop();
        driver::sound_stopped();
    }
}

fn main(_psp: &Psp) -> u8 {
    let Some(dev) = driver::device() else {
        println!("HDA: run by the driver system, not by hand.");
        return 1;
    };
    let name = format!("HD Audio {:04x}:{:04x}", dev.vendor, dev.device);
    if dev.class != 0x04 || dev.subclass != 0x03 {
        // A catalog line names a vendor's multimedia devices; this one is
        // not an HD Audio controller.
        let why = format!(
            "{:04x}:{:04x} is not an HD Audio controller (class {:02x}{:02x})",
            dev.vendor, dev.device, dev.class, dev.subclass
        );
        driver::status(&why);
        println!("HDA: {}", why);
        return 2;
    }
    driver::enable();
    let mut h = match start(&name) {
        Ok(h) => h,
        Err(why) => {
            driver::status(&why);
            println!("HDA: {}", why);
            return 1;
        }
    };
    let description = format!(
        "{}, codec {}: {:04x}:{:04x}, {} through {} converter(s){}",
        name,
        h.codec,
        h.codec_id >> 16,
        h.codec_id & 0xFFFF,
        h.outputs.join(", "),
        h.dacs.len(),
        if h.immediate { ", immediate commands" } else { "" }
    );
    if !driver::sound_register(h.mix_rate, &description) {
        h.stop();
        println!("HDA: another driver is the sound output.");
        return 1;
    }
    driver::status(if h.take_interrupt() {
        "interrupts (MSI)"
    } else {
        "polled"
    });
    loop {
        if let Some(ring) = driver::sound_wait(WAIT_MS) {
            h.play(ring);
        }
    }
}
