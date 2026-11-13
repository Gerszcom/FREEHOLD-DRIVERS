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
  `DRIVERS /BACK name` restores the previous version. (Being built: see
  DRIVERS-DEV.md.)
- The catalog is signed with the owner's release key (as FREEHOLD's own
  releases are, D-212); every driver zip is checked against its SHA-256.

## What is here

| Driver | Devices | What it is |
|---|---|---|
| `RTL8139` (`drivers/rtl8139`) | `10EC:8139` network | Realtek RTL8139 Ethernet (D-127) |
| `NVRTX` (`drivers/nvrtx`) | `10DE:*` display | NVIDIA GeForce RTX 20–50: brings the card up to its GSP firmware (D-193..D-197); its logic is `crates/freehold-rtx`, mirrored from Gerszcom/FREEHOLD-RTX |

Each driver's `DRIVER.INF` names it, its version, its file and the devices
it covers. Both moved here from FREEHOLD's `programs/` (with
`crates/freehold-rtx`) in D-215; their earlier history is FREEHOLD's.

## Building

The drivers build against FREEHOLD's runtime (`freehold-rt`), from a
sibling checkout, as FREEHOLD-WINE and FREEHOLD-ELF do:

```
<parent>/freehold/            # github.com/Gerszcom/freehold
<parent>/freehold-drivers/    # this repository
```

```
rustup target add x86_64-unknown-uefi
tools/build.sh
```

builds every driver and packages it into `dist/`: `NAME-VERSION.ZIP`
(`NAME.EXE` and `NAME.INF`, the INF naming the exact commits it was built
from; entries sorted and dated so the same files give the same zip),
`CATALOG.TXT` and `SHA256SUMS`. The catalog has one line per device a
driver covers:

```
VENDOR:DEVICE CLASS NAME VERSION BYTES SHA256 ADDRESS DESCRIPTION
```

`VENDOR:DEVICE` and `CLASS` as in FREEHOLD's `C:\DRIVERS\CATALOG.TXT`
(D-127, D-196: `DEVICE` is a hex id, or `*` for every device of that vendor
in `CLASS`); `ADDRESS` is relative to the catalog.

## Releases

`tools/release.sh N` builds from a clean checkout of this repository and a
clean sibling FREEHOLD into `releases/vN/` — every driver's zip,
`CATALOG.TXT` and `SHA256SUMS` — which is committed: the push publishes it,
at `https://raw.githubusercontent.com/Gerszcom/FREEHOLD-DRIVERS/<commit>/releases/vN/`
(as FREEHOLD-APPS serves its zips from its repository). A published release
is never rebuilt; the next is N+1. The zips need no signature (FREEHOLD pins
the basic ones by SHA-256, and a machine checks each against the catalog),
but the catalog is trusted only with `CATALOG.SIG` beside it, its Ed25519
signature by the owner's release key; the script prints his steps (sign,
verify, commit). `tools/release.sh N --publish` also makes the same files a
GitHub release `vN` (a prerelease until it carries `CATALOG.SIG`).

| Release | Drivers | Built from | Catalog signed |
|---|---|---|---|
| `v1` | RTL8139 1.0.0-1, NVRTX 1.0.0-1 | `d497525`, against FREEHOLD `a15e184` | not yet (owner) |

## Licence

MIT, as FREEHOLD (`LICENSE`).

Work in progress: see DRIVERS-DEV.md.
