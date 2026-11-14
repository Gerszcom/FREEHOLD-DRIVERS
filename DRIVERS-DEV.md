PAUSED 2026-11-14: waiting on the owner
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

- 2026-11-14 (run 12): **DRIVERS.EXE for USB devices online (FREEHOLD
  D-262) — Part 1's downloading now covers the USB devices kept for
  user-mode drivers.** The kernel's `DRIVERS` report ends each `USB
  devices:` line with `ifaces CCSS,...` (every interface of every
  configuration, each once; QEMU's usb-net: `0202,0a00,0206`, its ECM
  interface after RNDIS); DRIVERS.EXE parses the online catalog's `USB:`
  lines as the kernel does and matches them only against USB devices, by
  IDs and any interface of the class; everything after the match is the
  PCI path (offer for a device nothing drives, newer version with the old
  kept, fuller driver, holds, `/BACK`, the boot line, the INF's `USB:`
  lines first in the image's catalog). No new system call, no change in
  this repository (no release: the root catalog stays v8's). Kernel
  861696 → 862208 bytes. Gate: new stage 16 machine `usbdl` (drvdl's e1000
  + `qemu-xhci`/`usb-net`, test image without USBECM, test catalogs signed
  with a test key): other-key catalog refused, USBECM 1.0.0-1 found and
  installed for the adapter and running at once, started by the kernel
  after a restart, AUTO installs 1.0.0-2 at boot with 1.0.0-1 kept and runs
  it at once, `/BACK` both ways (hold respected by the boot line), 1.0.0-2
  runs after another restart. Stages 1, 4, 5, 8, 16 pass
  (`evidence/D-262-freehold.txt`). FREEHOLD `2052a4d`. The SETUP and
  graphics tasks took D-260 and D-261 while this run's gates ran; this
  run's decision is D-262 (stage 1 and usbdl rerun after the rebase).
  **With this, every unblocked item is done: the task is paused, waiting on
  the owner** (the catalog's signature and RELEASE.KEY; the NVRTX question;
  D-253, D-254, D-255).

- 2026-11-14 (run 11): **USB network adapters moved to user mode and the
  built-in removed in one step (FREEHOLD D-259): the USB bridge
  (`DRIVER_USB`) and `USBECM.EXE`** (`drivers/usbecm`, 1.0.0-1, release
  `v8`, built from `7e2f1db` against FREEHOLD `e4c70a4`; the other five
  zips v7's byte for byte; `evidence/v8-build.txt`). The kernel keeps any
  USB device no built-in role takes (not hub / boot keyboard or mouse /
  storage) for a user-mode driver, matched by a new catalog line form
  `USB:VENDOR:PRODUCT CLASS FILE DESCRIPTION` (either id `*` with an
  interface class: `cdc-ecm`, hex `CC` / `CCSS`); the driver's device is a
  `Place::Usb { controller, slot }` (PCI's `Place::Pci`), `DRIVER_DEVICE`
  says `BUS_USB` at byte 7; `DRIVER_USB` (0x008B) = DESCRIBE, CONFIGURE
  (once), CONTROL (not SET_ADDRESS/SET_CONFIGURATION), OUT (copied into a
  16 KiB kernel buffer), IN_START (≤ 16 × ≤ 4 KiB kept queued in kernel
  memory), IN_TAKE (copied out; 64 kept); a completion signals
  `EVENT_INTERRUPT`. The driver never sees a physical address. Streams and
  the configuration belong to the device, so `DRIVERS /START` takes over.
  `DRIVERS` lists `USB devices:` (`usbC.S VVVV:PPPP class CCSS FILE`) and a
  USB driver's `wake-ups: N`; DRIVERS.EXE reads that list for `/START` and
  `/BACK` only (online offers for USB devices: Next 1). `cargo drivers`
  accepts `USB:` `[Devices]` lines. USBECM ignores the ECM notification
  endpoint (QEMU's adapter answers every poll, ~30/s — seen in the first
  gate run: 315 wake-ups in 10 s idle). Measured: idle 10 s → 10 wake-ups;
  1 MiB over TCP in ~1 s. Kernel 848896 → 861696 bytes (the bridge and the
  USB catalog outweigh ecm.rs). Gate: stage 8 `usb` machine (see D-259);
  stages 1, 4, 5, 8, 16 pass on the image built from pinned v8
  (`evidence/D-259-freehold.txt`).
  FREEHOLD `e4c70a4` (ABI + runtime, additive, pushed first), then
  `c110732` (kernel, catalog, pin v8, gate, docs).
  With this, **no network or sound driver is left in the kernel**; what
  is left built in is the boot path (D-254: storage, xHCI + hubs, HID,
  PS/2, serial, RTC, console) and the display (D-255).

- 2026-11-14 (run 10): **The three designs Part 2 needed from the owner,
  written as PROPOSALs in FREEHOLD's DECISIONS.md (documentation only; no
  code, no release, no gate affected).** D-253, INTx for devices with
  neither MSI nor MSI-X (RTL8139, the 8254x): the gate's q35 + OVMF tables
  were read from OVMF's own UEFI shell (`dmem` of the ACPI reclaim range,
  the BIOS area and the EBDA, the DSDT disassembled with `iasl`;
  `evidence/D-253-intx-q35.txt`): no MP table, no `$PIR`; `_PRT` is a
  method choosing between two static packages by what `_PIC` stored, the
  APIC-mode one naming link devices `GSIA`–`GSIH` with static `_CRS`
  templates (GSI 16–23, level, active-high, shared). Recommended: a small
  `_PRT`-only AML evaluator in the kernel (no hardware access, ~600–900
  lines) and a level-triggered, shared delivery through `DRIVER_IRQ`
  (`IRQ_INTX`) / `DRIVER_WAIT` (unmask once every sharer has waited
  again); polling stays until he chooses. D-254, the boot path
  (storage, USB): recommended boot drivers inside the kernel file
  (`BOOTX64.EFI` + an archive of pinned user-mode drivers) with a block
  bridge `DRIVER_BLOCK` and a restart policy, stepwise from virtio-blk;
  PS/2, serial, RTC, the console and the USB host controller + hubs stay
  built in for good. D-255, display: a `DRIVER_DISPLAY` bridge designed
  (scanout in the driver's BAR, or lent frames + FLUSH for virtio-gpu);
  recommended to keep Bochs and virtio-gpu built in (on a virtio-gpu-only
  machine the built-in is the console's only screen before `C:`) and build
  the bridge when NVRTX needs to show a picture, with the graphics task;
  a note in GRAPHICS-DEV.md's direction. FREEHOLD `428e3f9`, `9cc37a2`,
  `dd36f40`. Found while surveying: CDC-ECM (USB network adapters) is a
  USB *class* driver, not on the boot path — it can move under the
  drivers task's own authority over a USB bridge (Next 1).

- 2026-11-14 (run 9): **The 82574L on its MSI-X (FREEHOLD D-252):
  `E1000.EXE` 1.1.0-1** (release `v7`, built from `ee9c9ce` against
  FREEHOLD `b84bcdc`; the other four zips v6's byte for byte). No kernel
  change. QEMU's `e1000e` (8086:10D3) worked with the legacy descriptors
  as they were (RFCTL.EXTEN / DEXT clear); its EERD layout differs
  (address bit 2, done bit 1) — the driver tries the card's own layout,
  then the other (the MAC came from RAL0 on QEMU, so the EEPROM path is
  not exercised there). On MSI-X: IVAR aims RXQ0/TXQ0/OTHER at entry 0,
  CTRL_EXT.PBA_SUPPORT, IMS = RXQ0|TXQ0|OTHER|LSC; on every wake (and
  timeout) ICR is read and written back (W1C; MSI-X reads don't clear)
  before the rings. MSI-only cards get RXT0|TXDW|LSC|RXDMT0|RXO; none
  (QEMU's 8254x) → polled through `DRIVER_WAIT(1)`. Measured: idle 10 s
  no interrupt, 10–11 wake-ups; 2 MiB took ~410 interrupts, ~580
  wake-ups. Gate: new stage 16 machine `e1000e` (`user_nic_checks`,
  `irq="MSI-X"`); `e1000u` checks 1.1.0-1 still polled (`E1000_VERSION`
  in gate.py). Stages 1, 4, 5, 16 pass (`evidence/D-252-freehold.txt`).
  FREEHOLD `86f7ffd`. The E1000 commit here (`ee9c9ce`) says "D-250": the
  SETUP and graphics tasks took D-250 and D-251 while this run's gates
  ran; the decision is D-252.
- 2026-11-13 (run 8): **HD Audio on its MSI (FREEHOLD D-247): `HDA.EXE`
  1.1.0-1** (release `v6`, built from `9267a84` against FREEHOLD
  `a7e80e2`; the other four zips v5's byte for byte). No kernel change:
  D-242's `DRIVER_IRQ`/`DRIVER_WAIT` sufficed. After registering as the
  sound output the driver takes the controller's MSI and enables only the
  output stream's interrupt (INTCTL GIE + its stream bit; CIE off, the
  command rings stay polled). The lent ring is played in ~20 ms pieces
  (`bdl_entries`: a power of two 4–64; 16 × 4 KiB at 48 kHz), each BDL
  entry with IOC, the stream with IOCE; playing, it sleeps in
  `DRIVER_WAIT(100 ms)`, clears SD_STS (BCIS/FIFOE/DESE) before reading
  LPIB, and calls `SOUND_PLAYED` as before. No MSI (or an older kernel):
  polled every 2 ms as 1.0.0. Status line `interrupts (MSI)` / `polled`.
  Measured: a 3 s tone took 144 interrupts and 144 wake-ups (1.0.0:
  ~1,600 polls); idle, none. Gates: stage 9 (both machines; every
  recording check as it was) + the boot log's MSI line, the DRIVERS
  status, 5 s idle before and 3 s after with no interrupt or wake-up,
  ≥ 100 interrupts for the 3 s tone with wake-ups ≤ interrupts + margin;
  9b: the driver restarted by `DRIVERS /START` takes the MSI again and
  plays the chime on it. Stages 1, 4, 5, 9, 13, 16 pass
  (`evidence/D-247-freehold.txt`; FREEHOLD `8752077`). Seen: QEMU's intel-hda sends an MSI on
  every register write while a status bit is set, so the driver issues no
  codec commands while playing (it never did).
- 2026-11-13 (run 7): **Interrupts delivered to user-mode drivers (FREEHOLD
  D-242); `VIRTNET.EXE` 1.1.0-1 on its MSI-X interrupt** (release `v5`;
  the other four zips v4's byte for byte). The kernel gained, for every
  driver: `DRIVER_IRQ` (0x0089: a vector 0x60–0x9F of its own; the
  device's MSI-X entry 0, or its MSI, aimed at the BSP; INTx off;
  `NOT_SUPPORTED` without either) and `DRIVER_WAIT` (0x008A: events
  `EVENT_INTERRUPT` / `EVENT_WORK` — the NIC bridge signals WORK for the
  first frame of a burst the stack queues — or sleep until one or a
  timeout; events kept on the process's task under the scheduler's lock,
  so none is lost). The vector is freed and MSI/MSI-X turned off when the
  driver ends; `DRIVERS` shows `interrupts: MSI-X vector 0x60, N
  delivered, M wake-ups`. Runtime: `driver::interrupts()`,
  `driver::wait(ms)` (fall back to `None` / a 1 ms sleep on an older
  kernel). VIRTNET points config + both queues at entry 0, drops
  NO_INTERRUPT, sleeps up to 1 s; status `link up, interrupts (MSI-X)`.
  Measured: idle 10 s → 0 interrupts, 11 wake-ups (polled ~10,000); a
  megabyte 0.8/0.9 s (polled 1.4/1.7 s). FREEHOLD in two commits:
  `3c47177` (ABI + runtime, additive, pushed first so v5 was built against
  a published commit), then the kernel side + pin v5 + gates (`f5a57fe`,
  rebased onto D-243/D-244). INTx is not
  delivered (needs ACPI `_PRT`, AML): E1000.EXE (QEMU's e1000/82544GC have
  no MSI), RTL8139.EXE still poll; HDA.EXE still polls its position every
  2 ms while playing. Gates: stage 16 `vnetm` (`user_nic_checks` with
  `irq="MSI-X"`: boot-log line, status, idle 10 s ≤ 30 wake-ups and no
  interrupt, the two megabytes through interrupts, `/START` takes a vector
  again), stage 16 secondary (status + MSI-X line), stage 4 secondary
  (the minute idle at the prompt: no interrupt, ≤ 130 wake-ups; it woke
  60). Stages 1, 4, 5, 9, 16 pass (`evidence/D-242-freehold.txt`).
- 2026-11-13 (run 6): **HD Audio moved to user mode and the built-in
  removed in one step (FREEHOLD D-238, `75cd3e1`): `HDA.EXE`** (`drivers/hda`,
  1.0.0-1, release `v4`; the other four zips v3's byte for byte). First the
  shared interface, designed generally: the **sound bridge**,
  `DRIVER_SOUND` (0x0088; FREEHOLD `a4f4eae`, part 1, pushed first so v4
  could be built against a published commit): `SOUND_REGISTER` (rate +
  description; one output), `SOUND_WAIT` (sleep until a stream opens; get
  the lent 64 KiB ring), `SOUND_PLAYED` (position + fetched; the kernel
  clears and mixes ahead; keep/stop), `SOUND_STOPPED` (ring back). The
  kernel keeps everything device-neutral in `drivers/sound.rs`: the sound
  family, streams, rate conversion, mixer, volume and keys, the chime.
  The ring is the kernel's, lent — so D-103's idle footprint (4 KiB: the
  driver's CORB/RIRB/BDL page) holds without a DMA-free call. The chime
  is read and queued in the driver's `SOUND_REGISTER` call (the boot
  doesn't wait for the driver); `run_beside` and the AP job queue (its
  only user was sound's start) are gone, and the per-CPU sound tick.
  Catalog class word `sound`/`audio`/`multimedia` = 04h (kernel and
  DRIVERS.EXE). The image's catalog lists 8086:2668/293E and vendor-wide
  `sound` lines (Intel, AMD/ATI, AMD, NVIDIA, VIA, SiS, ULi, Zhaoxin) —
  the built-in matched class 04/03 for any vendor; HDA.EXE ends at once
  on a multimedia device that isn't subclass 03. Kernel 828416 → 817664
  bytes. Gate: stage 9 (both machines) through HDA.EXE with every old
  check plus no built-in, HDA.EXE registered at 48 kHz, DRIVERS' `sound`
  line naming it; stage 9b adds kill → "no sound device", prompt fine,
  `DRIVERS /START` → registers again → PLAY plays (two sounds recorded).
  Stage 16 primary takes E1000.EXE's pid from the log (HDA.EXE is pid 1
  there now: the controller is first in PCI order). Stages 1, 4, 5, 9,
  13, 16 pass (`evidence/D-238-freehold.txt`).
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

1. Nothing unblocked is left (run 12 judged BLOCKED and paused the task).
   When the owner answers below, remove the PAUSED line and build what he
   chose: each its own gated increment (see 4).
2. If the owner signs `CATALOG.TXT`: check `CATALOG.SIG` against his
   `RELEASE.KEY` (`openssl pkeyutl -verify -pubin -inkey RELEASE.KEY -rawin
   -in CATALOG.TXT -sigfile CATALOG.SIG`), put his key in FREEHOLD's
   `system/FREEHOLD/RELEASE.KEY` (D-212) if it is not there, and gate a
   real-catalog `DRIVERS /CHECK` (up to date) on stage 16.
3. Note for whoever runs stage 3 here: this sandbox (2 host CPUs, the gate's
   4 vCPUs under TCG) boots in 5-8 s, over stage 3's 5 s limit, on the
   unchanged secondary machine too; run 4 passed every other stage 3 check
   with the limit raised locally (not committed). Not a regression of the
   move; worth a word to the owner if it persists.
4. When the owner answers D-253 / D-254 / D-255: build what he chose, each
   its own gated increment (D-253's (A): the evaluator, `IRQ_INTX`, then
   E1000.EXE 1.2.0 and RTL8139.EXE 1.1.0 on it; D-254's (B): DRIVER_BLOCK
   and VIRTBLK.EXE first).

## Waiting on the owner

- Sign the online driver catalog, `CATALOG.TXT` at the top of this
  repository (now release v8's, addresses into `releases/v8/`); machines read
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
- D-253 (FREEHOLD DECISIONS.md, PROPOSAL): interrupts for devices with
  only INTx (RTL8139, the older Intel e1000s; old PCI cards), which today
  are polled about a thousand times a second while their driver runs.
  Options: (A) a small evaluator in the kernel for the firmware's PCI
  interrupt table (ACPI `_PRT`) — recommended; (B) a full ACPI (AML)
  interpreter; (C) chipset-specific rules; (D) keep polling. Which one?
  Answer:
- D-254 (PROPOSAL): the drivers on the boot path — storage and USB mass
  storage, which must work before `C:` is mounted. (A) keep them in the
  kernel as now; (B) build them in this repository as user-mode drivers
  and carry them inside the kernel file, started from memory before
  `C:`, restarted if they end — recommended, one driver at a time from
  virtio-blk; (C) the same but read from the boot partition. PS/2, the
  serial port, the clock, the console and the USB controller stay in the
  kernel either way. Which one?
  Answer:
- D-255 (PROPOSAL): the display drivers (Bochs, virtio-gpu), which belong
  to the graphics task's stack. (A) keep them built in and build the
  designed display bridge only when NVRTX needs to show a picture —
  recommended; (B) move both now (a virtio-gpu-only machine's screen
  then stays dark until `C:` is mounted); (C) move Bochs only. Which one?
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
- 2026-11-13 run 6: the sound bridge (DRIVER_SOUND) and HDA.EXE built here, release v4; the built-in HD Audio removed, the kernel keeps the mixer (D-238); gates 1/4/5/9/13/16 pass.
- 2026-11-13 run 7: interrupts delivered to user-mode drivers — DRIVER_IRQ (MSI/MSI-X on a vector of the driver's) and DRIVER_WAIT (D-242); VIRTNET 1.1.0-1 on MSI-X, release v5; gates 1/4/5/9/16 pass.
- 2026-11-13 run 8: HDA.EXE 1.1.0 on its MSI (an interrupt per ~20 ms piece instead of a 2 ms poll), release v6, FREEHOLD pins v6 (D-247); gates 1/4/5/9/13/16 pass.
- 2026-11-14 run 9: E1000.EXE 1.1.0 — the 82574L (QEMU e1000e) gated for the first time, on its MSI-X interrupt; release v7, FREEHOLD pins v7 (D-252); gates 1/4/5/16 pass.
- 2026-11-14 run 10: PROPOSALs D-253 (INTx: the firmware's `_PRT`, read from q35's tables), D-254 (the boot path), D-255 (display) under "Waiting on the owner"; next: CDC-ECM over a USB bridge.
- 2026-11-14 run 11: the USB bridge (DRIVER_USB) and USBECM.EXE (CDC ECM in user mode), release v8, the built-in ecm.rs removed (D-259); gates 1/4/5/8/16 pass.
- 2026-11-14 run 12: DRIVERS.EXE matches the online catalog's USB: lines against every interface of a USB device (the report's `ifaces`), gated on stage 16 usbdl (D-262); gates 1/4/5/8/16 pass; every unblocked item done — paused, waiting on the owner.
