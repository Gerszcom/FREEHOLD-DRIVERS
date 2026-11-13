Run in progress: started 2026-11-13T11:39Z
# DRIVERS-DEV — cross-run handoff for FREEHOLD's drivers

This file is the drivers task's memory between hourly runs. Only drivers
runs write here. When a run is working, its first line is a lock:
`Run in progress: started <UTC>`; if that line is there and less than 90
minutes old, another run is working. A first line `PAUSED <date>: ...`
means the task is paused.

## The owner's brief (2026-11-13, live)

"Put all drivers in here [Gerszcom/FREEHOLD-DRIVERS] and implement the
driver downloading based on this (auto and manual)." His answers:

- **What moves:** the user-mode drivers now (`programs/rtl8139`,
  `programs/nvrtx` in FREEHOLD: sources, builds, catalog), AND, over time,
  the kernel's built-in drivers turned into user-mode drivers living here.
- **The image's copies:** FREEHOLD's build downloads the basic drivers'
  published zips from this repository's releases, pinned by version and
  SHA-256 in FREEHOLD (as it already fetches the ports), so the image works
  offline on first boot.
- **Trust:** the driver catalog is signed with the owner's release key
  (Ed25519, the same key and `openssl pkeyutl -sign -rawin` step as
  FREEHOLD's releases, D-212; public key `C:\FREEHOLD\RELEASE.KEY`); each
  zip is checked by SHA-256; all over HTTPS.
- **Downloading:** DIRECTION.md Stage 16 as written (D-090): `DRIVERS=AUTO`
  installs better drivers at boot and says what changed; `ASK` offers them;
  `MANUAL` looks only on `DRIVERS /UPDATE`. A live stick starts at AUTO; an
  installed system asks on its first boot, AUTO first. The previous version
  of every driver is kept: `DRIVERS /BACK name` restores it. At boot once
  the network is up, never as a background task (aim 3), by a visible
  AUTOEXEC.BAT line, the way `UPDATE /BOOT` does it (D-212).

## The goal (the task ends when it is reached)

Part 1 — the repository and downloading, each gated in FREEHOLD's
tools/gate.py with evidence:
1. The user-mode drivers' sources live here and build here (against a
   sibling FREEHOLD checkout's runtime crates, as FREEHOLD-WINE and
   FREEHOLD-ELF do); FREEHOLD no longer carries their sources.
2. Releases here publish each driver as a zip and the catalog
   (`CATALOG.TXT`: devices matched by PCI vendor + device or vendor + class,
   as D-196's lines, with name, version, size, SHA-256, address,
   description) with its signature `CATALOG.SIG`.
3. FREEHOLD's build fetches the basic drivers' zips from a pinned release
   (version + SHA-256 in FREEHOLD) into the image; stage 16's rtl machine
   still passes on the image built that way.
4. On FREEHOLD: `DRIVERS /UPDATE` (manual), `DRIVERS=AUTO|ASK|MANUAL` at
   boot, `DRIVERS /BACK name`; a catalog signed by another key refused; a
   zip whose hash is wrong refused leaving nothing; a newer driver installed
   with the old kept, and running after a restart; a driver for a device
   nothing local drives found and installed.

Part 2 — built-in drivers to user mode, one by one, each gated (the
device works as before through its user-mode driver, the built-in removed
from the kernel, every earlier gate passing): every built-in driver that is
not needed to reach C: at boot — the network drivers (e1000, virtio-net),
sound (HDA), and whatever else ARCHITECTURE.md lists that qualifies. The
drivers on the boot path (storage, the USB stack, the boot keyboard and
console) need a design for loading a driver before C: is mounted: write that
as a DECISIONS.md PROPOSAL under "Waiting on Roi" rather than building it;
the goal does not require moving them. Each move may need kernel work first
(interrupts delivered to drivers, the IOMMU, a stable interface per device
class — DIRECTION.md Stage 16); build what is needed generally, in
FREEHOLD, with its own decision and gate.

## Where things stand

- 2026-11-13: repository created (empty) by the owner; brief and goal
  written. In FREEHOLD: `programs/rtl8139` (D-127), `programs/nvrtx`
  (D-193..D-197), `system/DRIVERS/CATALOG.TXT` (the image's catalog, read by
  `kernel/src/drivers/user.rs`), `tools/build.sh` (PROGRAM_DIRS puts them in
  `C:\DRIVERS`), stage 16's `rtl` machine. FREEHOLD's own releases and their
  signing (D-212): `std-programs/get/src/update.rs`, `tools/release.sh`,
  BOOTING.md "Releases and UPDATE". The owner's release key is not in the
  image yet (`system/FREEHOLD/RELEASE.KEY`); gates use a test key.
- The NVIDIA driver is also developed in Gerszcom/FREEHOLD-RTX by the owner
  himself (its scheduled task is off): never touch that repository; how
  this repository and it relate is under "Waiting on Roi".

## Next, in order

1. Lay out this repository (a Cargo workspace per driver, building against
   `../freehold`), move rtl8139 and nvrtx here with their history noted
   (sources copied, FREEHOLD's removed in the same landing), a build script
   that produces the zips and an unsigned catalog, and a DECISIONS.md entry
   in FREEHOLD for the move.
2. Publishing: a release script here (zips + CATALOG.TXT; the owner signs
   CATALOG.TXT), and FREEHOLD's build fetching the pinned basic drivers.
3. FREEHOLD side: the downloader (DRIVERS /UPDATE, /BACK, the boot line and
   the setting), gated with a test catalog and test key.
4. Part 2, network drivers first.

## Waiting on Roi

- The NVIDIA RTX driver lives in two places: FREEHOLD's `programs/nvrtx`
  (merged from the RTX work) and Gerszcom/FREEHOLD-RTX, which you develop
  yourself. Should FREEHOLD-DRIVERS take FREEHOLD's copy and FREEHOLD-RTX
  stay your development repo that publishes into this catalog, or should
  FREEHOLD-RTX's sources move here too (when you say so)?
  Answer:

## Session log

- 2026-11-13: brief and goal written (from the owner's answers in chat).
