# FREEHOLD-DRIVERS

FREEHOLD's drivers outside the kernel: their sources, their builds, and the
online driver catalog a FREEHOLD machine reads to find drivers for its
devices (DIRECTION.md Stage 16, D-090, D-127). Drivers here are user-mode
FREEHOLD programs: each is given one device and the `DRIVER_*` system calls
that act on it alone, so a faulty driver ends only itself (aim 10).

- The FREEHOLD image carries the basic drivers, fetched from this
  repository's published releases at build time (pinned by version and
  SHA-256 in FREEHOLD), so a machine works offline on its first boot.
- Online, `CONFIG.SYS`'s `DRIVERS=AUTO|ASK|MANUAL` decides what happens to
  better drivers found here; `DRIVERS /UPDATE` looks when told, and
  `DRIVERS /BACK name` restores the previous version.
- The catalog is signed with the owner's release key (as FREEHOLD's own
  releases are, D-212); every driver zip is checked against its SHA-256.

Work in progress: see DRIVERS-DEV.md.
