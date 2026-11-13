#!/usr/bin/env python3
"""Package the built drivers and write the catalog (FREEHOLD's D-215).

    tools/package.py [--freehold ../freehold] [--out dist]

For every drivers/NAME/DRIVER.INF: the built driver
(target/x86_64-unknown-uefi/release/NAME.efi) and its description are
zipped into OUT/NAME-VERSION.ZIP as

    NAME.EXE    the driver, a FREEHOLD program
    NAME.INF    DRIVER.INF with Source= added: the exact commits it was
                built from (this repository and the FREEHOLD runtime)

entries sorted and dated 2026-01-01, so the same files always give the same
zip (as FREEHOLD-APPS' tools/package.py does). Then OUT/CATALOG.TXT is
written: one line per device a driver covers,

    VENDOR:DEVICE CLASS NAME VERSION BYTES SHA256 ADDRESS DESCRIPTION

VENDOR:DEVICE and CLASS as in FREEHOLD's C:\\DRIVERS\\CATALOG.TXT (D-127,
D-196: DEVICE is a hex id, or * for every device of that vendor in CLASS),
ADDRESS relative to the catalog. The catalog is published unsigned; the
owner signs it (CATALOG.SIG, Ed25519, his release key) before a FREEHOLD
machine will trust it — see tools/release.sh.
"""
import argparse
import hashlib
import os
import re
import subprocess
import sys
import zipfile

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STAMP = (2026, 1, 1, 0, 0, 0)
TARGET = os.path.join(HERE, "target", "x86_64-unknown-uefi", "release")
CLASSES = {"network", "display", "storage", "sound", "audio", "input", "usb", "serial"}


def die(msg):
    print(f"package: {msg}", file=sys.stderr)
    sys.exit(1)


def read_inf(path):
    """(sections dict of key -> value, device lines, raw text)."""
    text = open(path, encoding="ascii").read()
    driver, devices, section = {}, [], None
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith(";"):
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].lower()
            continue
        if section == "driver" and "=" in line:
            k, v = line.split("=", 1)
            driver[k.strip().lower()] = v.strip()
        elif section == "devices":
            devices.append(line.split())
    return driver, devices, text


def commit(repo):
    try:
        out = subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], capture_output=True, text=True, check=True)
        dirty = subprocess.run(["git", "-C", repo, "status", "--porcelain", "--untracked-files=no"],
                               capture_output=True, text=True, check=True).stdout.strip()
        return out.stdout.strip() + ("+dirty" if dirty else "")
    except (subprocess.CalledProcessError, FileNotFoundError):
        return "unknown"


def check_device(name, dev):
    if len(dev) != 2:
        die(f"{name}: a [Devices] line is VENDOR:DEVICE CLASS, not {' '.join(dev)!r}")
    ident, cls = dev
    m = re.fullmatch(r"([0-9A-Fa-f]{4}):([0-9A-Fa-f]{4}|\*)", ident)
    if not m:
        die(f"{name}: bad device {ident!r}")
    if cls.lower() not in CLASSES and not re.fullmatch(r"[0-9A-Fa-f]{2}", cls):
        die(f"{name}: bad class {cls!r}")
    return ident.upper(), cls.lower()


def zip_bytes(entries, out):
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for n, data in sorted(entries):
            info = zipfile.ZipInfo(n, STAMP)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            z.writestr(info, data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--freehold", default=os.path.join(os.path.dirname(HERE), "freehold"))
    ap.add_argument("--out", default=os.path.join(HERE, "dist"))
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    source = (f"https://github.com/Gerszcom/FREEHOLD-DRIVERS {commit(HERE)}, built against "
              f"https://github.com/Gerszcom/FREEHOLD {commit(args.freehold)}")
    lines = []
    sums = []
    drivers_dir = os.path.join(HERE, "drivers")
    for crate in sorted(os.listdir(drivers_dir)):
        inf_path = os.path.join(drivers_dir, crate, "DRIVER.INF")
        if not os.path.isfile(inf_path):
            continue
        d, devices, text = read_inf(inf_path)
        for key in ("name", "version", "file", "description"):
            if not d.get(key):
                die(f"{inf_path}: no {key.capitalize()}=")
        name, version, file = d["name"].upper(), d["version"], d["file"].upper()
        if not re.fullmatch(r"[A-Z0-9_-]{1,8}", name) or file != f"{name}.EXE":
            die(f"{inf_path}: Name= must be a DOS name and File= NAME.EXE")
        if not re.fullmatch(r"[0-9A-Za-z.+-]+", version):
            die(f"{inf_path}: bad Version= {version!r}")
        if not devices:
            die(f"{inf_path}: no [Devices]")
        exe = os.path.join(TARGET, f"{crate}.efi")
        if not os.path.isfile(exe):
            die(f"{exe} is not built (tools/build.sh)")
        inf = text.replace("\r\n", "\n").rstrip("\n").split("\n")
        # The repository's notes on the file make way for one line on what it is.
        while inf and (inf[0].startswith(";") or not inf[0].strip()):
            inf.pop(0)
        inf.insert(0, f"; {file}, a FREEHOLD user-mode driver from Gerszcom/FREEHOLD-DRIVERS")
        # Source= goes right after the [Driver] section's last key.
        at = max(i for i, l in enumerate(inf) if "=" in l and not l.startswith(";")
                 and i < next((j for j, x in enumerate(inf) if x.strip().lower() == "[devices]"), len(inf)))
        inf.insert(at + 1, f"Source={source}")
        inf_bytes = ("\r\n".join(inf) + "\r\n").encode("ascii")
        zname = f"{name}-{version}.ZIP"
        out = os.path.join(args.out, zname)
        zip_bytes([(file, open(exe, "rb").read()), (f"{name}.INF", inf_bytes)], out)
        data = open(out, "rb").read()
        digest = hashlib.sha256(data).hexdigest()
        sums.append(f"{digest}  {zname}")
        for dev in devices:
            ident, cls = check_device(name, dev)
            lines.append(f"{ident} {cls} {name} {version} {len(data)} {digest} {zname} {d['description']}")
        print(f"package: {zname} ({len(data)} bytes, {len(devices)} device line(s))")
    head = [
        "; FREEHOLD's driver catalog (FREEHOLD's DECISIONS.md, D-090, D-127, D-196).",
        "; Written by FREEHOLD-DRIVERS' tools/package.py; do not edit by hand.",
        "; VENDOR:DEVICE CLASS NAME VERSION BYTES SHA256 ADDRESS DESCRIPTION",
        "; DEVICE is a hex id, or * for every device of that vendor in CLASS; ADDRESS",
        "; is relative to this file. CATALOG.SIG is its Ed25519 signature by the",
        "; owner's release key (C:\\FREEHOLD\\RELEASE.KEY); unsigned, it is not trusted.",
    ]
    with open(os.path.join(args.out, "CATALOG.TXT"), "wb") as f:
        f.write(("\r\n".join(head + lines) + "\r\n").encode("ascii"))
    cat = open(os.path.join(args.out, "CATALOG.TXT"), "rb").read()
    sums.append(f"{hashlib.sha256(cat).hexdigest()}  CATALOG.TXT")
    with open(os.path.join(args.out, "SHA256SUMS"), "w") as f:
        f.write("\n".join(sums) + "\n")
    print(f"package: CATALOG.TXT, {len(lines)} line(s), unsigned")


if __name__ == "__main__":
    main()
