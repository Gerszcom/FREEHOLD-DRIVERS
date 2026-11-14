# FREEHOLD-DRIVERS

FREEHOLD's drivers outside the kernel: their sources, their builds, and the
online driver catalog a FREEHOLD machine reads to find drivers for its
devices (DIRECTION.md Stage 16, D-090, D-127, D-215). Drivers here are
user-mode FREEHOLD programs: each is given one device and the `DRIVER_*`
system calls that act on it alone, so a faulty driver ends only itself
(aim 10).

- The FREEHOLD image carries the basic drivers, fetched from this
  repository's published releases at build time (pinned by version and
  SHA-256 in FREEHOLD's `tools/drivers.pin`), so a machine works offline on
  its first boot.
- Online, `CONFIG.SYS`'s `DRIVERS=AUTO|ASK|MANUAL` decides what happens to
  better drivers found here; `DRIVERS /UPDATE` looks when told, and
  `DRIVERS /BACK name` restores the previous version (FREEHOLD D-219).
- The catalog is signed with the owner's release key (as FREEHOLD's own
  releases are, D-212); every driver zip is checked against its SHA-256.

## What is here

| Driver | Devices | What it is |
|---|---|---|
| `RTL8139` (`drivers/rtl8139`) | `10EC:8139` network | Realtek RTL8139 Ethernet (D-127) |
| `E1000` (`drivers/e1000`) | `8086:100C`, `100E`, `100F`, `1010`, `1011`, `1026`, `1027`, `1076`, `10D3` network | Intel PRO/1000 (8254x, 82574L) Ethernet: the kernel's built-in e1000 driver moved to user mode (D-224); since 1.1.0 the 82574L (QEMU's `e1000e`) on its MSI-X interrupt, asleep in `DRIVER_WAIT` between frames, and the 8254x polled as before (D-252) |
| `VIRTNET` (`drivers/virtnet`) | `1AF4:1000`, `1041` network | virtio network device over the modern (virtio 1.x) PCI transport: the kernel's built-in virtio-net driver moved to user mode (D-231); since 1.1.0 on its MSI-X interrupt, asleep in `DRIVER_WAIT` between frames (D-242) |
| `HDA` (`drivers/hda`) | `8086:2668`, `8086:293E`; Intel, AMD/ATI, NVIDIA, VIA, SiS, ULi, Zhaoxin `sound` (any device, class 04h) | Intel High Definition Audio: the controller, its codec's output paths and one output stream, playing the ring the kernel's mixer lends it over the sound bridge (`DRIVER_SOUND`): the kernel's built-in HD Audio driver moved to user mode (D-238); since 1.1.0 on its MSI, an interrupt per ~20 ms piece played instead of a 2 ms poll (D-247) |
| `USBECM` (`drivers/usbecm`) | `USB:*:*` `cdc-ecm` (any USB device with a CDC Ethernet interface, 02h/06h) | USB Ethernet adapters of the CDC Ethernet Control Model: the kernel's built-in ECM driver moved to user mode, reaching its adapter only through the USB bridge (`DRIVER_USB`): descriptors, a configuration, control transfers, OUT transfers and an IN stream the kernel keeps queued, its completions waking the driver (D-259) |
| `NVRTX` (`drivers/nvrtx`) | `10DE:*` display | NVIDIA GeForce RTX 20–50: brings the card up to its GSP firmware (D-193..D-197); its logic is `crates/freehold-rtx`, mirrored from Gerszcom/FREEHOLD-RTX |

Each driver's `DRIVER.INF` names it, its version, its file and the devices
it covers. RTL8139 and NVRTX moved here from FREEHOLD's `programs/` (with
`crates/freehold-rtx`) in D-215; their earlier history is FREEHOLD's.

## Building

On Windows, Linux or macOS, with nothing but Rust (`rustup`): the build,
packaging and release steps are a small Rust program in this repository
(`tools/drvtool`, standard library only), run through a Cargo alias. No
shell, Python, zip or hash tool is needed.

The drivers build against FREEHOLD's runtime (`freehold-rt`), from a
sibling checkout, as FREEHOLD-WINE and FREEHOLD-ELF do:

```
<parent>\freehold\            github.com/Gerszcom/freehold
<parent>\freehold-drivers\    this repository
```

```
rustup target add x86_64-unknown-uefi
cargo drivers build
```

builds every driver and packages it into `dist\`: `NAME-VERSION.ZIP`
(`NAME.EXE` and `NAME.INF`, the INF naming the exact commits it was built
from; entries stored, sorted and dated, so the same files give the same
zip), `CATALOG.TXT` and `SHA256SUMS`. (A plain `cargo build` builds the
drivers alone.) The catalog has one line per device a driver covers:

```
VENDOR:DEVICE CLASS NAME VERSION BYTES SHA256 ADDRESS DESCRIPTION
```

`VENDOR:DEVICE` and `CLASS` as in FREEHOLD's `C:\DRIVERS\CATALOG.TXT`
(D-127, D-196: `DEVICE` is a hex id, or `*` for every device of that vendor
in `CLASS`); `ADDRESS` is relative to the catalog.

The alias builds the tool for the machine it runs on (`--target host-tuple`,
which a current Cargo understands; tested with Rust 1.97). Building on
FREEHOLD itself waits for a Rust toolchain there; the tool uses only the
standard library, so it will not be what stands in the way.

## Releases

`cargo drivers release N` builds from a clean checkout of this repository
and a clean sibling FREEHOLD into `releases\vN\` — every driver's zip,
`CATALOG.TXT` and `SHA256SUMS` — which is committed: the push publishes it,
at `https://raw.githubusercontent.com/Gerszcom/FREEHOLD-DRIVERS/<commit>/releases/vN/`
(as FREEHOLD-APPS serves its zips from its repository). A published release
is never rebuilt; the next is N+1. Neither is a published version: a driver
whose `Version=` an earlier release already published keeps that release's
zip in the new one, byte for byte (raise `Version=` to publish a change). `.gitattributes` keeps git from changing
a release's bytes (no line-ending conversion, whatever `core.autocrlf` says).
The zips need no signature (FREEHOLD pins the basic ones by SHA-256, and a
machine checks each against the catalog).

## The online catalog

A FREEHOLD machine reads `CATALOG.TXT` at the top of this repository, from
`main` (`https://raw.githubusercontent.com/Gerszcom/FREEHOLD-DRIVERS/main/CATALOG.TXT`;
`DRIVERS /UPDATE`, and at boot as `CONFIG.SYS`'s `DRIVERS=AUTO|ASK|MANUAL`
says — FREEHOLD's D-219). It is the current release's catalog with each
address leading into `releases/vN/`; `cargo drivers current N` writes it
(`cargo drivers release N` does too). A machine believes it only with
`CATALOG.SIG` beside it, its Ed25519 signature by the owner's release key
(FREEHOLD's `C:\FREEHOLD\RELEASE.KEY`, the key FREEHOLD's own releases are
signed with). `cargo drivers sign` prints his steps — one `openssl` line to
sign, one to verify, then commit; each pastes as is into PowerShell or cmd
(OpenSSL 3: Git for Windows has it). A new `CATALOG.TXT` removes the old
signature, so the catalog is never served with a signature of another one.

| Release | Drivers | Built from | Catalog signed |
|---|---|---|---|
| `v1` | RTL8139 1.0.0-1, NVRTX 1.0.0-1 | `d497525`, against FREEHOLD `a15e184` | current; `CATALOG.TXT` not yet signed (owner) |

## Licence

MIT, as FREEHOLD (`LICENSE`).

Work in progress: see DRIVERS-DEV.md.
