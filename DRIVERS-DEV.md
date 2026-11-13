Run in progress: started 2026-11-13T12:06Z
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

- 2026-11-13 (run 1): **Part 1, item 1 done, item 3 done; item 2 half
  done (needs the owner's signature).** RTL8139 and NVRTX (with
  `crates/freehold-rtx`, the FREEHOLD-RTX mirror NVRTX links) live here,
  copied from FREEHOLD `a15e184`, and build against `../freehold`
  (`tools/build.sh`); FREEHOLD no longer carries their sources (D-215).
  `drivers/NAME/DRIVER.INF` describes each driver; `tools/package.py` makes
  `NAME-VERSION.ZIP` (`NAME.EXE` + `NAME.INF` with `Source=` commits,
  deterministic) and `CATALOG.TXT` (`VENDOR:DEVICE CLASS NAME VERSION BYTES
  SHA256 ADDRESS DESCRIPTION`); `tools/release.sh N` writes `releases/vN/`,
  which is committed and so published (raw.githubusercontent.com by
  commit). Release `v1` = RTL8139 1.0.0-1 + NVRTX 1.0.0-1, built from
  `d497525`, committed in `9795adc` (`evidence/v1-build.txt`); its catalog
  is unsigned until the owner signs it.
- FREEHOLD side (D-215, FREEHOLD `9d0c342`): `tools/drivers.pin` (release URL at `9795adc` +
  each zip's SHA-256), `tools/drivers.sh` (fetch once into
  `target/drivers`, refuse a wrong hash or a zip with non-DOS names, unpack
  into `C:\DRIVERS`), `tools/build.sh` checks every file the image's
  `system/DRIVERS/CATALOG.TXT` names is there. Stage 16's `rtl` machine now
  also TYPEs `C:\DRIVERS\RTL8139.INF` (version + `Source=`). Gates 1, 4, 5,
  16 pass on the image built by fetching from the published release
  (`evidence/D-215-freehold.txt`).
- GitHub releases can't be created from these sessions: `gh release create
  v1 ... --repo Gerszcom/FREEHOLD-DRIVERS` → "HTTP 403: Creating, editing, or
  deleting releases is not permitted for this session type"
  (api.github.com/repos/Gerszcom/FREEHOLD-DRIVERS/releases). Hence releases
  as committed `releases/vN/` (as FREEHOLD-APPS serves `ZIPS/`);
  `tools/release.sh N --publish` makes the same bytes a GitHub release when
  the owner runs it.
- The owner's release key is not in FREEHOLD's image yet
  (`system/FREEHOLD/RELEASE.KEY`, D-212); gates use test keys.
- The NVIDIA driver is also developed in Gerszcom/FREEHOLD-RTX by the owner
  himself: never touch that repository. `crates/freehold-rtx` here is the
  mirror FREEHOLD carried (pinned upstream commit in its Cargo.toml).

## Next, in order

1. The online catalog's address and layout, then the downloader. Decide
   (record in D-216): the catalog a machine reads is
   `https://raw.githubusercontent.com/Gerszcom/FREEHOLD-DRIVERS/main/CATALOG.TXT`
   + `CATALOG.SIG` at the repository root (the latest release's catalog,
   addresses `releases/vN/NAME-VER.ZIP`), written by `tools/release.sh`;
   `CONFIG.SYS` `DRIVERS=AUTO|ASK|MANUAL` and a `DRIVERCATALOG=` (or
   reuse `CATALOG=`'s pattern) line. Then the program: `DRIVERS` is a
   COMMAND.COM built-in (`programs/command/src/builtins.rs:90`,
   `sys::drivers_report`); `DRIVERS /UPDATE`, `/BACK NAME`, `/BOOT` should
   run a std program built like UPDATE (`std-programs/get`, a fourth
   program of that crate: its HTTPS, signature check against
   `C:\FREEHOLD\RELEASE.KEY`, zip unpacking). Install = new files beside,
   old kept (e.g. `C:\DRIVERS\OLD\NAME.*`), the image catalog's line for
   that driver added/updated; the kernel starts drivers only at boot, so a
   new driver runs after a restart (say so). Watch UPDATE (D-212): it
   replaces `C:\DRIVERS` wholesale, carrying "the owner's files" — a
   downloaded driver must survive an UPDATE (or UPDATE must keep the newer
   driver); design that with it. Gate on stage 16 with a test catalog
   served by the stage's HTTPS test server and a test key: forged catalog
   refused, bad hash refused leaving nothing, newer driver installed with
   the old kept and running after restart, `/BACK`, a driver for a device
   nothing local drives (e.g. an RTL8139 machine whose image catalog lacks
   the line) found and installed, AUTO's visible AUTOEXEC.BAT line, ASK
   asking, MANUAL silent.
2. Part 2, network drivers first (e1000, virtio-net as user-mode drivers:
   check the DRIVER_* calls suffice — interrupts aren't delivered, drivers
   poll; e1000 is the gates' NIC on most machines, so the built-in can only
   go once the user-mode one passes every network gate). Then HDA sound.
3. The boot-path PROPOSAL (storage, USB, boot keyboard/console): loading a
   driver before C: is mounted — write it in DECISIONS.md as a PROPOSAL.

## Waiting on Roi

- Sign release v1's driver catalog (nothing else is blocked on it yet; a
  machine will refuse an unsigned catalog). With your release key, in a
  checkout of this repository:
  `openssl pkeyutl -sign -inkey freehold-release.pem -rawin -in releases/v1/CATALOG.TXT -out releases/v1/CATALOG.SIG`,
  check it with `openssl pkeyutl -verify -pubin -inkey RELEASE.KEY -rawin -in releases/v1/CATALOG.TXT -sigfile releases/v1/CATALOG.SIG`,
  then commit and push `releases/v1/CATALOG.SIG`. Optional: `tools/release.sh 1 --publish`
  to have it as a GitHub release too (these sessions can't create releases).
  Answer:
- The NVIDIA RTX driver lives in two places: FREEHOLD's `programs/nvrtx`
  (merged from the RTX work) — now moved here as `drivers/nvrtx` with its
  `crates/freehold-rtx` mirror (D-215) — and Gerszcom/FREEHOLD-RTX, which
  you develop yourself. Should this repository keep that copy, refreshed
  from FREEHOLD-RTX by commit, with FREEHOLD-RTX your development repo that
  publishes into this catalog, or should FREEHOLD-RTX's sources move here
  too (when you say so)?
  Answer:

## Session log

- 2026-11-13: brief and goal written (from the owner's answers in chat).
- 2026-11-13 run 1: drivers moved here (RTL8139, NVRTX), build/package/release scripts, release v1 published as releases/v1; FREEHOLD builds C:\DRIVERS from the pinned v1 zips (D-215); gates 1/4/5/16 pass.
