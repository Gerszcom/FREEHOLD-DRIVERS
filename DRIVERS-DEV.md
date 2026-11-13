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

- 2026-11-13 (run 5): **virtio-net moved to user mode and the built-in
  removed in one step (FREEHOLD D-231, `853f8c6`): `VIRTNET.EXE`** (`drivers/virtnet`,
  1.0.0-1, release `v3`; E1000/RTL8139/NVRTX v2's zips byte for byte).
  The kernel gained `DRIVER_CONFIG` (0x0087: read up to 256 bytes of the
  driver's own config space, read only; `driver::config`,
  `driver::capabilities` in the runtime) and the runtime's `Bar0` 8/16-bit
  accessors (unsafe count unchanged, 279: the two volatile sites made
  generic). VIRTNET speaks the modern interface (capabilities → common /
  notify / device structures in memory BARs), so 1AF4:1041 stays covered;
  64-entry queues, polled 1 ms, avail NO_INTERRUPT. FREEHOLD pushed in two
  commits: `74bd158` (DRIVER_CONFIG, additive, pushed first so release v3
  could be built against a published commit), then the move (pin v3,
  catalog lines, `net/virtio_net.rs` and `net::init` gone, gate). No PCI
  network driver is left in the kernel; kernel 861696 → 856064 bytes.
  Gate: stage 16 secondary `stage_16_virtio_moved` (no built-in, VIRTNET
  pid up before the shell, interface 0 + PCI list + TASKS, AUTOEXEC's
  DHCP); stage 4 secondary's network (60 s idle wire, HTTPD 387 KiB to the
  host) through it; a new stage 16 machine `vnetm` (modern-only 1AF4:1041,
  `disable-legacy=on`) through the e1000u checks, now shared as
  `user_nic_checks` (DHCP, ping, 1 MiB twice, KILL, `DRIVERS /START`).
  Stages 1, 4, 5, 16 (every machine) pass, again after rebasing onto D-232
  (`evidence/D-231-freehold.txt`). Seen once in four stage-16-secondary runs on the
  development zip: `UPDATE /Y`'s first connect timed out after smoltcp's
  10 s (reported as "Connection refused"); three reruns passed, stage 4
  passed; pcap of that run was overwritten. Watch for it — if it recurs,
  suspect a lost SYN/SYN-ACK in VIRTNET's queues (or the 1 kHz poll
  starving on this 2-CPU host); interrupts for drivers (Next 3) would help.
- 2026-11-13 (run 4): **The kernel's e1000 driver removed (FREEHOLD
  D-229): every e1000 runs `E1000.EXE`** (release v2, unchanged; nothing
  here changed but this file and `evidence/D-229-freehold.txt`). Checked
  first: nothing used the network before `C:` was mounted (the stack starts
  only on `IPCFG`; the installer menu and SETUP use none). What changed was
  timing and numbering, fixed in the kernel for every user-mode network
  driver: the boot now waits (5 s at most) after starting a driver for a
  network controller until its interface is registered or it ended, before
  the next device and the shell — so AUTOEXEC.BAT's `IPCFG /AUTO` (and a
  server's `IPCFG DHCP`) finds the card, and interfaces stay numbered in PCI
  order. Drivers are pids before the shell, so the shell is pid 1 + the
  drivers started; `TASKS` shows a driver's kind as `driver`. The gate
  harness takes the shell's pid from its start line (`expect_shell`,
  `vm.shell_pid`); stage 16 primary checks the move (no built-in, E1000.EXE
  interface 0 up before the shell, DHCP from AUTOEXEC's line); drvdl sees
  its e1000 driven by E1000.EXE. Kernel 864256 → 861696 bytes. FREEHOLD `34abcc4`; stages 1, 4, 5, 16 pass (`evidence/D-229-freehold.txt`).
- 2026-11-13 (run 3): **Part 2 begun: `E1000.EXE`, the kernel's e1000
  driver in user mode** (`drivers/e1000`, 1.0.0-1; FREEHOLD D-224). Same
  rings and register sequence as `net/e1000.rs`, over `DRIVER_MAP` (BAR0) +
  `DRIVER_DMA` + the NIC bridge: the existing `DRIVER_*` calls sufficed, no
  kernel change. Published in release `v2` (with RTL8139/NVRTX as v1
  published them: `cargo drivers release` now reuses a published version's
  zip byte for byte; the RTL8139/NVRTX EXEs build differently with today's
  toolchain, so it says so and keeps v1's). Root `CATALOG.TXT` is v2's
  (still unsigned). FREEHOLD pins v2 and its image catalog lists all nine
  e1000 IDs for `E1000.EXE`; the built-in still claims its own eight first,
  so today E1000.EXE drives only the 82544GC (100C). Gated on stage 16's new
  `e1000u` machine (QEMU `e1000-82544gc`): started from the catalog, DHCP,
  ping, 1 MiB fetched twice over TCP and compared, KILL stops the device,
  `DRIVERS /START` and ping again; stages 1, 4, 5, 16 pass
  (`evidence/D-224-freehold.txt`, `evidence/v2-build.txt`).
- 2026-11-13 (live, the owner: "fix the gaps"): **D-219's gaps closed
  (FREEHOLD D-221).** A driver installed runs at once: the kernel's new
  `DRIVERS_START` call starts the catalog's driver for every device with no
  running driver; DRIVERS.EXE ends the old driver (KILL), waits, calls it;
  a restarted network driver keeps its interface number; `DRIVERS /START`
  restarts an ended driver. Live media installs (until its restart).
  Fuller drivers: the driver the signed catalog lists first for a device
  another user-mode driver drives takes its place (`OLD\NAME.WAS`; `/BACK
  NAME` gives it back). `/BACK` holds the version gone back from
  (`C:\DRIVERS\HOLD.TXT`) so the boot line won't reinstall it. UPDATE keeps
  drivers installed online (`C:\DRIVERS\ONLINE.TXT`) unless the release has
  newer ones. Gated on stage 16 `drvdl` (extended) and `rtl` (`/START` +
  ping through the same interface); stages 1, 4, 5, 16 pass (evidence
  `evidence/D-221-freehold.txt`). **Part 1 is now complete except the
  owner's signature on `CATALOG.TXT`** (and his key in FREEHOLD's image).
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

1. HDA sound as a user-mode driver: needs the audio bridge (a user-mode
   sound driver's equivalent of the NIC bridge, what `PLAY`, the chime and
   the Windows add-on's sound (FREEHOLD-WINE D-209) write to) — a shared
   interface: design it as a decision first (what the kernel's sound layer
   keeps — the mixer/stream API programs use — and what moves: the
   controller, CORB/RIRB, codec walk, stream DMA). The chime plays at boot
   from the kernel (D-103) before the shell, i.e. before `C:\DRIVERS` runs
   anything: check what that means (chime from the driver once it is up?
   the chime is the owner's design — if it must move later, PROPOSAL). Stage
   9 (the sound gates, `-sound.wav` captured) is the gate to keep passing.
2. The boot-path PROPOSAL (storage, USB — including the USB stack's
   CDC-ECM network adapters, which are USB class drivers, not PCI — the
   boot keyboard/console): loading a driver before C: is mounted — write
   it in DECISIONS.md as a PROPOSAL and put it under "Waiting on the
   owner".
3. Interrupts delivered to user-mode drivers (DIRECTION Stage 16): today a
   network driver polls with a 1 ms sleep, a thousand wake-ups a second
   per card even when idle — now on both test machines. On this two-CPU
   sandbox that cost was visible once (stage 7's four-programs-at-once
   ratio missed on the primary machine, passed on a second run). A general
   kernel change (deliver a device's MSI/INTx to its driver as a wait it
   can block on): its own decision, and it helps every driver that moved.
4. Note for whoever runs stage 3 here: this sandbox (2 host CPUs, the gate's
   4 vCPUs under TCG) boots in 5-8 s, over stage 3's 5 s limit, on the
   unchanged secondary machine too; run 4 passed every other stage 3 check
   with the limit raised locally (not committed). Not a regression of the
   move; worth a word to the owner if it persists.

## Waiting on the owner

- Sign the online driver catalog, `CATALOG.TXT` at the top of this
  repository (now release v3's, addresses into `releases/v3/`); machines read
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
- 2026-11-13 (live, owner's request): D-219's gaps fixed — drivers run at once (DRIVERS_START), fuller drivers by catalog order, /BACK holds, UPDATE keeps online drivers, live media installs, DRIVERS /START (D-221); gates 1/4/5/16 pass.
- 2026-11-13 run 3: E1000.EXE (the kernel's e1000 in user mode) built here, release v2, FREEHOLD pins v2 and gates it on stage 16 e1000u (D-224); `cargo drivers release` reuses published versions; gates 1/4/5/16 pass.
- 2026-11-13 run 4: built-in e1000 removed from the kernel, every e1000 runs E1000.EXE; the boot waits for network drivers' interfaces; TASKS shows drivers; gate harness takes the shell's pid (D-229); gates 1/2/4/5/7/15/16 pass (after rebasing onto D-228: 1/4/5/16 again), 3 except its sandbox-bound boot-time limit.
- 2026-11-13 run 5: VIRTNET.EXE (virtio-net in user mode) built here, release v3; FREEHOLD gains DRIVER_CONFIG + 8/16-bit BAR access, pins v3, the built-in virtio-net removed (D-231); gates 1/4/5/16 pass.
