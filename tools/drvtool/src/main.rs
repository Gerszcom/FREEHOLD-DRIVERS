//! `cargo drivers` — build, package and release FREEHOLD's drivers
//! (FREEHOLD's DECISIONS.md, D-215, D-216).
//!
//! ```text
//! cargo drivers build        build every driver, package it into dist\
//! cargo drivers release N    build release vN into releases\vN\ (commit it)
//! cargo drivers current N    make vN the release machines read (CATALOG.TXT)
//! cargo drivers sign         print the owner's steps to sign that catalog
//! ```
//!
//! The online catalog a FREEHOLD machine reads (`DRIVERS /UPDATE`, D-219) is
//! `CATALOG.TXT` at the top of this repository, served from `main`: the
//! current release's catalog with each address leading into
//! `releases/vN/`. It is trusted only with `CATALOG.SIG` beside it, the
//! owner's signature.
//!
//! It runs wherever Rust does — Windows, Linux, macOS — and needs nothing
//! else: no shell, no Python, no zip or hash tool. Standard library only;
//! SHA-256, CRC-32 and the zip writer are here, a few dozen lines each.
//!
//! A driver is `drivers\NAME\` with its `DRIVER.INF` (name, version, file,
//! description, `[Devices]` lines). Packaging zips `NAME.EXE` and
//! `NAME.INF` (the INF with `Source=`: the exact FREEHOLD-DRIVERS and
//! FREEHOLD commits it was built from) into `NAME-VERSION.ZIP`, entries
//! stored, sorted and dated 2026-01-01 so the same files always give the
//! same zip, and writes `CATALOG.TXT` (one line per device:
//! `VENDOR:DEVICE CLASS NAME VERSION BYTES SHA256 ADDRESS DESCRIPTION`, a
//! USB driver's `USB:VENDOR:PRODUCT CLASS ...`, D-259) and `SHA256SUMS`.
#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const UEFI: &str = "x86_64-unknown-uefi";
const REPO: &str = "Gerszcom/FREEHOLD-DRIVERS";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("build") => build(&root(), false).map(|_| ()),
        Some("release") => release_number(&args).and_then(|n| release(&root(), n)),
        Some("current") => release_number(&args).and_then(|n| current(&root(), n)),
        Some("sign") => {
            print_sign_steps(&root());
            Ok(())
        }
        _ => {
            eprintln!(
                "usage:\n  cargo drivers build        build every driver, package it into dist\\\n  \
                 cargo drivers release N    build release vN into releases\\vN\\ (then commit it)\n  \
                 cargo drivers current N    make vN the release machines read (CATALOG.TXT)\n  \
                 cargo drivers sign         the owner's steps to sign that catalog"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drivers: {e}");
            ExitCode::FAILURE
        }
    }
}

type Result<T> = std::result::Result<T, String>;

/// The repository: this tool lives in `tools/drvtool`.
fn root() -> PathBuf {
    let tool = Path::new(env!("CARGO_MANIFEST_DIR"));
    tool.ancestors().nth(2).unwrap_or(tool).to_path_buf()
}

/// The sibling FREEHOLD checkout whose runtime the drivers build against.
fn freehold(root: &Path) -> Result<PathBuf> {
    let dir = root.parent().unwrap_or(root).join("freehold");
    if dir.join("programs").join("rt").join("Cargo.toml").is_file() {
        Ok(dir)
    } else {
        Err(format!(
            "no FREEHOLD checkout at {} — clone Gerszcom/freehold next to this repository",
            dir.display()
        ))
    }
}

fn release_number(args: &[String]) -> Result<u32> {
    args.get(1)
        .and_then(|n| n.parse().ok())
        .filter(|&n| n > 0)
        .ok_or_else(|| "give the release number: cargo drivers release N".to_string())
}

// ---------------------------------------------------------------- build

struct Driver {
    crate_name: String,
    name: String,
    version: String,
    file: String,
    description: String,
    devices: Vec<(String, String)>,
    inf: String,
}

/// Build every driver for the UEFI target and package it into `dist/`.
/// Returns the dist directory. For a release (`published`), a driver whose
/// version an earlier release already published keeps that release's zip,
/// byte for byte: a published version is never rebuilt.
fn build(root: &Path, published: bool) -> Result<PathBuf> {
    let freehold = freehold(root)?;
    let drivers = read_drivers(root)?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(root)
        .args(["build", "--release", "--target", UEFI]);
    // As FREEHOLD's build does: without the target's prebuilt library but
    // with rust-src, core and alloc are compiled locally (or when
    // FREEHOLD_BUILD_STD=1 asks for it).
    let sysroot = output(Command::new("rustc").args(["--print", "sysroot"]))?;
    let sysroot = PathBuf::from(sysroot.trim());
    let prebuilt = sysroot.join("lib").join("rustlib").join(UEFI).join("lib").is_dir();
    if std::env::var("FREEHOLD_BUILD_STD").as_deref() == Ok("1") || !prebuilt {
        let src = sysroot.join("lib").join("rustlib").join("src").join("rust").join("library");
        if !src.is_dir() {
            return Err(format!(
                "the {UEFI} target is not installed: rustup target add {UEFI}"
            ));
        }
        println!("drivers: compiling core/alloc for {UEFI} locally (build-std)");
        cmd.env("RUSTC_BOOTSTRAP", "1").args([
            "-Zbuild-std=core,alloc",
            "-Zbuild-std-features=compiler-builtins-mem",
        ]);
    }
    for d in &drivers {
        cmd.args(["-p", &d.crate_name]);
    }
    let status = cmd.status().map_err(|e| format!("cannot run {cargo}: {e}"))?;
    if !status.success() {
        return Err("the drivers did not build".into());
    }
    let dist = root.join("dist");
    if dist.exists() {
        fs::remove_dir_all(&dist).map_err(|e| format!("{}: {e}", dist.display()))?;
    }
    fs::create_dir_all(&dist).map_err(|e| format!("{}: {e}", dist.display()))?;
    package(root, &freehold, &drivers, &dist, published)?;
    Ok(dist)
}

fn read_drivers(root: &Path) -> Result<Vec<Driver>> {
    let dir = root.join("drivers");
    let mut crates: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("DRIVER.INF").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    crates.sort();
    crates.iter().map(|c| parse_inf(c, &dir.join(c).join("DRIVER.INF"))).collect()
}

fn parse_inf(crate_name: &str, path: &Path) -> Result<Driver> {
    let bad = |what: String| format!("{}: {what}", path.display());
    let inf = fs::read_to_string(path).map_err(|e| bad(e.to_string()))?;
    if !inf.is_ascii() {
        return Err(bad("not plain ASCII".into()));
    }
    let (mut section, mut keys, mut devices) = (String::new(), Vec::new(), Vec::new());
    for line in inf.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_ascii_lowercase();
        } else if section == "driver" {
            if let Some((k, v)) = line.split_once('=') {
                keys.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        } else if section == "devices" {
            let f: Vec<&str> = line.split_whitespace().collect();
            let [id, class] = f[..] else {
                return Err(bad(format!("a [Devices] line is VENDOR:DEVICE CLASS, not {line:?}")));
            };
            let (id, class) = (check_id(id).ok_or_else(|| bad(format!("bad device {id:?}")))?,
                               check_class(class).ok_or_else(|| bad(format!("bad class {class:?}")))?);
            if !check_line(&id, &class) {
                return Err(bad(format!("a USB wildcard needs an interface class, not {class:?}")));
            }
            devices.push((id, class));
        }
    }
    let key = |k: &str| {
        keys.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| bad(format!("no {}=", k[..1].to_uppercase() + &k[1..])))
    };
    let (name, version) = (key("name")?.to_ascii_uppercase(), key("version")?);
    let (file, description) = (key("file")?.to_ascii_uppercase(), key("description")?);
    if name.is_empty() || name.len() > 8 || !name.bytes().all(dos_char) || file != format!("{name}.EXE") {
        return Err(bad("Name= must be a DOS name (8 letters at most) and File= NAME.EXE".into()));
    }
    if !version.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b)) {
        return Err(bad(format!("bad Version= {version:?}")));
    }
    if devices.is_empty() {
        return Err(bad("no [Devices]".into()));
    }
    Ok(Driver { crate_name: crate_name.into(), name, version, file, description, devices, inf })
}

fn dos_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
}

/// `VENDOR:DEVICE` — four hex digits, and four more or `*` (D-196); or a
/// USB device's `USB:VENDOR:PRODUCT`, each four hex digits or `*` (D-259).
fn check_id(id: &str) -> Option<String> {
    let hex4 = |s: &str| s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit());
    if let Some(usb) = id.strip_prefix("USB:").or_else(|| id.strip_prefix("usb:")) {
        let (v, p) = usb.split_once(':')?;
        return ((v == "*" || hex4(v)) && (p == "*" || hex4(p))).then(|| format!("USB:{}", usb.to_ascii_uppercase()));
    }
    let (v, d) = id.split_once(':')?;
    (hex4(v) && (d == "*" || hex4(d))).then(|| id.to_ascii_uppercase())
}

/// A PCI line's class word or two hex digits; a USB line's interface class
/// (D-259): `cdc-ecm`, or `CC` / `CCSS` in hex.
fn check_class(c: &str) -> Option<String> {
    let c = c.to_ascii_lowercase();
    let known = ["network", "display", "storage", "sound", "audio", "input", "usb", "serial", "cdc-ecm"];
    let hex = |n: usize| c.len() == n && c.bytes().all(|b| b.is_ascii_hexdigit());
    (known.contains(&c.as_str()) || hex(2) || hex(4)).then_some(c)
}

/// A `[Devices]` line's class for its ID: a USB ID's must say which
/// interfaces when the vendor or product is `*`, as a PCI wildcard must
/// (D-196, D-259).
fn check_line(id: &str, class: &str) -> bool {
    match id.strip_prefix("USB:") {
        Some(usb) => !usb.contains('*') || class == "cdc-ecm" || class.len() == 2 || class.len() == 4,
        None => true,
    }
}

fn package(root: &Path, freehold: &Path, drivers: &[Driver], dist: &Path, published: bool) -> Result<()> {
    let source = format!(
        "https://github.com/Gerszcom/FREEHOLD-DRIVERS {}, built against https://github.com/Gerszcom/FREEHOLD {}",
        commit(root),
        commit(freehold)
    );
    let target = root.join("target").join(UEFI).join("release");
    let (mut lines, mut sums) = (Vec::new(), String::new());
    for d in drivers {
        let exe_path = target.join(format!("{}.efi", d.crate_name));
        let exe = fs::read(&exe_path).map_err(|e| format!("{}: {e}", exe_path.display()))?;
        let inf = installed_inf(d, &source);
        let zname = format!("{}-{}.ZIP", d.name, d.version);
        let earlier = if published { published_zip(root, &zname) } else { None };
        let zip = match earlier {
            Some((tag, zip)) => {
                if !contains(&zip, &exe) {
                    println!(
                        "drivers: note: {} builds differently from {tag}'s {zname}; that published \
                         zip is kept (raise Version= to publish the change)",
                        d.file
                    );
                }
                println!("drivers: {zname} is {tag}'s, unchanged");
                zip
            }
            None => zip_stored(&mut [(d.file.clone(), exe), (format!("{}.INF", d.name), inf.into_bytes())]),
        };
        let digest = hex(&sha256(&zip));
        write(&dist.join(&zname), &zip)?;
        let _ = writeln!(sums, "{digest}  {zname}");
        for (id, class) in &d.devices {
            lines.push(format!(
                "{id} {class} {} {} {} {digest} {zname} {}",
                d.name, d.version, zip.len(), d.description
            ));
        }
        println!("drivers: {zname} ({} bytes, {} device line(s))", zip.len(), d.devices.len());
    }
    let mut catalog = String::new();
    for l in [
        "; FREEHOLD's driver catalog (FREEHOLD's DECISIONS.md, D-090, D-127, D-196).",
        "; Written by FREEHOLD-DRIVERS' cargo drivers; do not edit by hand.",
        "; VENDOR:DEVICE CLASS NAME VERSION BYTES SHA256 ADDRESS DESCRIPTION",
        "; DEVICE is a hex id, or * for every device of that vendor in CLASS; ADDRESS",
        "; is relative to this file. A USB driver's line is USB:VENDOR:PRODUCT CLASS ...,",
        "; either id * with an interface class in CLASS (cdc-ecm, or hex CC or CCSS).",
        "; CATALOG.SIG is its Ed25519 signature by the",
        "; owner's release key (C:\\FREEHOLD\\RELEASE.KEY); unsigned, it is not trusted.",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(lines)
    {
        catalog.push_str(&l);
        catalog.push_str("\r\n");
    }
    write(&dist.join("CATALOG.TXT"), catalog.as_bytes())?;
    let _ = writeln!(sums, "{}  CATALOG.TXT", hex(&sha256(catalog.as_bytes())));
    write(&dist.join("SHA256SUMS"), sums.as_bytes())?;
    println!("drivers: dist\\ — {} zip(s), CATALOG.TXT (unsigned), SHA256SUMS", drivers.len());
    Ok(())
}

/// The newest earlier release's `zname`, if one published it:
/// `(tag, bytes)`.
fn published_zip(root: &Path, zname: &str) -> Option<(String, Vec<u8>)> {
    let dir = root.join("releases");
    let mut tags: Vec<u32> = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix('v')?.parse().ok())
        .collect();
    tags.sort_unstable();
    tags.iter().rev().find_map(|n| {
        let bytes = fs::read(dir.join(format!("v{n}")).join(zname)).ok()?;
        Some((format!("v{n}"), bytes))
    })
}

/// Whether `hay` holds `needle` (a stored zip holds its files' bytes as is).
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The INF as installed: one line on what it is in place of the
/// repository's notes, `Source=` after the `[Driver]` keys, CRLF.
fn installed_inf(d: &Driver, source: &str) -> String {
    let mut lines: Vec<String> = d.inf.lines().map(|l| l.trim_end().to_string()).collect();
    while lines.first().is_some_and(|l| l.starts_with(';') || l.is_empty()) {
        lines.remove(0);
    }
    lines.insert(0, format!("; {}, a FREEHOLD user-mode driver from {REPO}", d.file));
    let end = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case("[devices]"))
        .unwrap_or(lines.len());
    let at = lines[..end].iter().rposition(|l| l.contains('=') && !l.starts_with(';')).map_or(end, |i| i + 1);
    lines.insert(at, format!("Source={source}"));
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.iter().map(|l| format!("{l}\r\n")).collect()
}

// -------------------------------------------------------------- release

fn release(root: &Path, n: u32) -> Result<()> {
    let tag = format!("v{n}");
    let dir = root.join("releases").join(&tag);
    if dir.exists() {
        return Err(format!(
            "releases\\{tag} exists; a published release is never rebuilt (make v{})",
            n + 1
        ));
    }
    let freehold = freehold(root)?;
    for (repo, what) in [(root, "this repository"), (freehold.as_path(), "the FREEHOLD checkout")] {
        if commit(repo).ends_with("+changes") {
            return Err(format!(
                "{what} ({}) has uncommitted changes; a release names the commits it was built from",
                repo.display()
            ));
        }
    }
    let dist = build(root, true)?;
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in fs::read_dir(&dist).map_err(|e| e.to_string())?.flatten() {
        let to = dir.join(entry.file_name());
        fs::copy(entry.path(), &to).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    println!("drivers: releases\\{tag}\\ written. Commit and push it: that publishes it.");
    print!("{}", fs::read_to_string(dir.join("SHA256SUMS")).unwrap_or_default());
    current(root, n)
}

/// `cargo drivers current N`: the online catalog (`CATALOG.TXT` at the top
/// of the repository) becomes release N's, its addresses leading into
/// `releases/vN/`. A signature of another catalog is removed with it.
fn current(root: &Path, n: u32) -> Result<()> {
    let rel = format!("releases/v{n}");
    let from = root.join("releases").join(format!("v{n}")).join("CATALOG.TXT");
    let text = fs::read_to_string(&from).map_err(|e| format!("{}: {e}", from.display()))?;
    let catalog = online_catalog(&text, &rel);
    let to = root.join("CATALOG.TXT");
    if fs::read_to_string(&to).ok().as_deref() == Some(catalog.as_str()) {
        println!("drivers: CATALOG.TXT is already release {n}'s.");
    } else {
        write(&to, catalog.as_bytes())?;
        let _ = fs::remove_file(root.join("CATALOG.SIG"));
        println!("drivers: CATALOG.TXT is now release {n}'s (addresses in {rel}/). Commit and push it.");
    }
    print_sign_steps(root);
    Ok(())
}

/// A release's catalog as the online one: each line's address (the
/// seventh field) prefixed with the release's directory; comments kept,
/// and one saying which release it is.
fn online_catalog(text: &str, rel: &str) -> String {
    let mut out = String::new();
    let mut said = false;
    for line in text.lines() {
        let t = line.trim_end();
        if t.starts_with(';') || t.trim().is_empty() {
            out.push_str(t);
            out.push_str("\r\n");
            continue;
        }
        if !said {
            out.push_str(&format!("; The current release: {rel}.\r\n"));
            said = true;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let mut f: Vec<String> = f.iter().map(|s| s.to_string()).collect();
        f[6] = format!("{rel}/{}", f[6]);
        out.push_str(&f.join(" "));
        out.push_str("\r\n");
    }
    out
}

fn print_sign_steps(root: &Path) {
    if root.join("CATALOG.SIG").is_file() {
        println!("drivers: CATALOG.SIG is there; the online catalog is signed.");
        return;
    }
    // One line each, so they paste the same into PowerShell, cmd or a Unix
    // shell. OpenSSL 3 (Git for Windows carries it) has -rawin.
    println!(
        "\nTo sign the online catalog (the owner, with the key only he holds), in this repository:\n\n  \
         openssl pkeyutl -sign -inkey freehold-release.pem -rawin -in CATALOG.TXT -out CATALOG.SIG\n  \
         openssl pkeyutl -verify -pubin -inkey RELEASE.KEY -rawin -in CATALOG.TXT -sigfile CATALOG.SIG\n  \
         git add CATALOG.SIG\n  \
         git commit -m \"The online driver catalog's signature\"\n  \
         git push\n\n\
         The first makes the signature, the second checks it against the public key FREEHOLD\n\
         ships (C:\\FREEHOLD\\RELEASE.KEY). Machines read CATALOG.TXT and CATALOG.SIG from\n\
         https://raw.githubusercontent.com/{REPO}/main/ (DRIVERS /UPDATE).\n"
    );
}

// ------------------------------------------------------------- helpers

fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(format!("{:?} failed", cmd.get_program()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A checkout's commit, with `+changes` when tracked files differ from it.
fn commit(repo: &Path) -> String {
    let git = |args: &[&str]| output(Command::new("git").arg("-C").arg(repo).args(args));
    match git(&["rev-parse", "HEAD"]) {
        Ok(head) => {
            let changed = git(&["status", "--porcelain", "--untracked-files=no"])
                .map(|s| !s.trim().is_empty())
                .unwrap_or(true);
            format!("{}{}", head.trim(), if changed { "+changes" } else { "" })
        }
        Err(_) => "unknown".into(),
    }
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ zip

/// A zip of `files`, sorted by name, each entry stored (no compression:
/// FREEHOLD's unzip reads stored and deflated entries alike) and dated
/// 2026-01-01 00:00, so the same files always give the same bytes.
fn zip_stored(files: &mut [(String, Vec<u8>)]) -> Vec<u8> {
    const DATE: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data) in files.iter() {
        let (crc, size, offset) = (crc32(data), data.len() as u32, out.len() as u32);
        let common = |v: &mut Vec<u8>| {
            v.extend_from_slice(&10u16.to_le_bytes()); // version needed: 1.0
            v.extend_from_slice(&0u16.to_le_bytes()); // flags
            v.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            v.extend_from_slice(&0u16.to_le_bytes()); // time 00:00
            v.extend_from_slice(&DATE.to_le_bytes());
            v.extend_from_slice(&crc.to_le_bytes());
            v.extend_from_slice(&size.to_le_bytes()); // compressed
            v.extend_from_slice(&size.to_le_bytes()); // uncompressed
            v.extend_from_slice(&(name.len() as u16).to_le_bytes());
            v.extend_from_slice(&0u16.to_le_bytes()); // extra length
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        common(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by: MS-DOS, 2.0
        common(&mut central);
        central.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let (at, len, count) = (out.len() as u32, central.len() as u32, files.len() as u16);
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // this disk, the central directory's disk
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out
}

/// CRC-32 (IEEE 802.3, reflected, as zip uses).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

// -------------------------------------------------------------- SHA-256

/// SHA-256 (FIPS 180-4).
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (hh, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}
