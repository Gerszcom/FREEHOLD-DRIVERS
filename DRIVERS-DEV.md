Run in progress: started 2026-11-13T13:00Z (live session, on the owner's request: fix D-219's gaps)
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
as a DECISIONS.md PROPOSAL under "Waiting on the owner" rather than building it;
the goal does not require moving them. Each move may need kernel work first
(interrupts delivered to drivers, the IOMMU, a stable interface per device
class — DIRECTION.md Stage 16); build what is needed generally, in
FREEHOLD, with its own decision and gate.

## Where things stand

- 2026-11-13 (run 2): **Part 1, item 4 mostly done: the downloader**
  (FREEHOLD D-219). `DRIVERS.EXE` (a fourth program of FREEHOLD's std
  `get` crate; COMMAND.COM's `DRIVERS` runs it when given switches):
  `DRIVERS /UPDATE [/Y]`, `/CHECK`, `/BACK name`, `/BOOT` (AUTOEXEC.BAT's
  visible line, after `UPDATE /BOOT`), `/C:url`; `CONFIG.SYS`
  `DRIVERS=AUTO|ASK|MANUAL` (unset: installed ASK, live AUTO — but live
  installs nothing yet) and `DRIVERCATALOG=` (default this repository's
  `main/CATALOG.TXT`). Catalog believed only with `CATALOG.SIG` by a key in
  `C:\FREEHOLD\RELEASE.KEY`; zips by length + SHA-256; replaced driver kept
  in `C:\DRIVERS\OLD`; the driver's lines put first in the image's
  `C:\DRIVERS\CATALOG.TXT`; runs from the next restart. The kernel's
  `DRIVERS` report now ends with every PCI function and what drives it
  (`built-in`, a driver file, `none`) — what DRIVERS.EXE matches. Gated on
  stage 16's new `drvdl` machine (e1000 network + an RTL8139 nothing drives;
  test catalog + test key): forged catalog refused, bad hash refused
  leaving nothing, a driver for a device nothing local drives found and
  installed, MANUAL silent, ASK asks (once a day), AUTO installs at boot
  from the AUTOEXEC line saying what changed with the old kept, the newer
  one running after a restart, `/BACK` (evidence `evidence/D-219-freehold.txt`,
  FREEHOLD `validation/stage-16/`; FREEHOLD `4c705e0`). Stages 1, 4, 5, 16 pass.
- Here: the online catalog is `CATALOG.TXT` at the repository root (release
  v1's, addresses `releases/v1/...`), written by the new `cargo drivers
  current N` (and by `release N`); `cargo drivers sign` now prints the steps
  to sign that root catalog (`CATALOG.SIG`). It is unsigned until the owner
  signs it, and his key is not in FREEHOLD's image yet, so real machines
  say nothing at boot.
- 2026-11-13 (live, the owner): "you made it build on Linux when I use
  Windows and FREEHOLD". The bash/Python scripts (`tools/build.sh`,
  `package.py`, `release.sh`) are replaced by `tools/drvtool`, a std-only
  Rust program run as `cargo drivers build | release N | sign N` (alias in
  `.cargo/config.toml`, `--target host-tuple`), so building needs nothing
  but Rust on Windows, Linux or macOS (FREEHOLD D-216). Zips are now stored
  (not deflated), still deterministic; release v1 is untouched (published
  bytes are never rebuilt). `.gitattributes` keeps `releases/**` byte-exact
  under Windows' `core.autocrlf`. Checked: built and packaged on Linux,
  `cargo check --target x86_64-pc-windows-msvc` clean, clippy clean, its
  SHA-256 matches `sha256sum`, the zips pass `unzip -t` and Python's
  zipfile, the driver EXE is byte-identical to v1's, and the printed
  `openssl` sign/verify lines work with a test key. Not run on a real
  Windows machine here. FREEHOLD's own build (`tools/build.sh`, with
  `tools/drivers.sh`) is still bash: its ports need gcc and the rest — a
  separate question for the owner. Building on FREEHOLD itself needs a Rust
  toolchain there.
- 2026-11-13 (run 1): **Part 1, item 1 done, item 3 done; item 2 half
  done (needs the owner's signature).** RTL8139 and NVRTX (with
  `crates/freehold-rtx`, the FREEHOLD-RTX mirror NVRTX links) live here,
  copied from FREEHOLD `a15e184`, and build against `../freehold`
  (`cargo drivers build`); FREEHOLD no longer carries their sources (D-215).
  `drivers/NAME/DRIVER.INF` describes each driver; packaging makes
  `NAME-VERSION.ZIP` (`NAME.EXE` + `NAME.INF` with `Source=` commits,
  deterministic) and `CATALOG.TXT` (`VENDOR:DEVICE CLASS NAME VERSION BYTES
  SHA256 ADDRESS DESCRIPTION`); `cargo drivers release N` writes `releases/vN/`,
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
  `cargo drivers sign N` prints the `gh release create` line that makes the
  same bytes a GitHub release when the owner runs it.
- The owner's release key is not in FREEHOLD's image yet
  (`system/FREEHOLD/RELEASE.KEY`, D-212); gates use test keys.
- The NVIDIA driver is also developed in Gerszcom/FREEHOLD-RTX by the owner
  himself: never touch that repository. `crates/freehold-rtx` here is the
  mirror FREEHOLD carried (pinned upstream commit in its Cargo.toml).

## Next, in order

1. A newly installed driver started at once (FREEHOLD kernel work, its own
   decision, general: "start the catalog's driver for an unclaimed
   device now", and for a replaced driver, end the running one and start
   the new — a call or a `DRIVERS /START name` path through the kernel,
   which owns PCI claims). Lets live media use DRIVERS=AUTO (D-219 left it
   installing nothing) and makes AUTO's result immediate. Gate on `drvdl`.
2. A downloaded driver keeps its place across UPDATE (D-212 replaces
   `C:\DRIVERS` wholesale, carrying only files the release lacks, and the
   release's catalog wins): in UPDATE's install, for each driver in the old
   `C:\DRIVERS` whose INF version is newer than the release's (or that the
   release lacks), keep its files and catalog lines (the code is in the same
   crate: `drivers.rs` can expose the INF/catalog helpers). Gate on stage
   16's update check or `drvdl`.
3. "Fuller" drivers (a different driver for an already-driven device):
   needs a way to say which is fuller — e.g. an INF `Replaces=NAME` key that
   the catalog carries; decide with the RTX work (NVRTX basic vs. full).
4. Part 2, network drivers first (e1000, virtio-net as user-mode drivers:
   check the DRIVER_* calls suffice — interrupts aren't delivered, drivers
   poll; e1000 is the gates' NIC on most machines, so the built-in can only
   go once the user-mode one passes every network gate). Then HDA sound.
5. The boot-path PROPOSAL (storage, USB, boot keyboard/console): loading a
   driver before C: is mounted — write it in DECISIONS.md as a PROPOSAL.

## Waiting on the owner

- Sign the online driver catalog, `CATALOG.TXT` at the top of this
  repository (release v1's, addresses into `releases/v1/`); machines read
  it with `DRIVERS /UPDATE` and at boot (FREEHOLD D-219) and refuse it
  unsigned. In a checkout of this repository, `cargo drivers sign` prints
  the exact lines (two `openssl` lines — sign, then verify against
  RELEASE.KEY — and the commit); they paste as is into PowerShell or cmd.
  (This replaces the earlier request to sign `releases/v1/CATALOG.TXT`:
  the root one is what machines read.) Nothing else is blocked on it; for
  real machines to check it, your `RELEASE.KEY` must also be in FREEHOLD's
  `system/FREEHOLD/` (D-212).
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
- 2026-11-13 (live, owner's request): build/package/release made cross-platform — `cargo drivers` (tools/drvtool, std-only Rust) replaces the bash/Python scripts (D-216).
- 2026-11-13 run 2: the downloader — DRIVERS.EXE (/UPDATE, /CHECK, /BACK, /BOOT per DRIVERS=), the PCI list in DRIVERS, the online CATALOG.TXT here (cargo drivers current N); gated on stage 16 drvdl (D-219); gates 1/4/5/16 pass.
