//! Stage 5 groundwork — the GSP firmware a card needs, and the sidecar
//! description FREEHOLD keeps beside each firmware file.
//!
//! From Turing on, these GPUs run most of their resource-manager logic as
//! signed firmware on an on-board processor (the GSP). Before a driver can
//! start that firmware it must *find the right file*, confirm it is the one
//! this card wants, and know the hash it must verify the bytes against. This
//! module is that selection-and-description half — pure, allocation-free
//! logic exercised entirely on the host.
//!
//! ## FREEHOLD's firmware model
//!
//! FREEHOLD keeps device firmware as plain files in `C:\FIRMWARE\`, each
//! *named for its device*, with a text file beside it giving the firmware's
//! vendor, version, hash and licence (`DIRECTION.md` §4). The owner can see,
//! copy, replace or delete any of them; `DRIVERS` shows which file each
//! device is running, and a device whose firmware is missing says so and does
//! what it can without it. Nothing here is NVIDIA's format: the sidecar is
//! FREEHOLD's own text, and *which* firmware a given RTX chip needs is a
//! documented hardware fact (the GSP generations, and that Ada and Blackwell
//! bring the display engine up through the GSP while Turing and Ampere need
//! not). No driver source is copied or translated; this is FREEHOLD's own
//! code against the published interface, in keeping with the clean-room rule.
//!
//! ## What this module does and does not do
//!
//! It parses a sidecar ([`FirmwareSidecar::parse`]), derives from a probe the
//! firmware a card needs ([`required_firmware`]), decides whether a given
//! sidecar is acceptable to load for that card ([`select`]), and — once the
//! firmware bytes are in hand — verifies them against the sidecar's digest
//! ([`FirmwareSelection::verify`], using the crate's own [`crate::sha256`]). It
//! deliberately does **not** read files (the kernel's VFS does that) or judge
//! the firmware by its
//! licence: FREEHOLD loads what the owner put there regardless of licence —
//! the licence only governs what FREEHOLD may *redistribute* in its image, so
//! it is surfaced ([`FirmwareSidecar::licence`]) and never used to refuse a
//! load. `no_std`, `forbid(unsafe_code)`, and zero-copy: every string the
//! sidecar exposes borrows from the input, so nothing is allocated.

use crate::ids::Family;
use crate::probe::GpuProbe;
use crate::sha256::sha256;

/// Length in bytes of a SHA-256 digest.
pub const SHA256_LEN: usize = 32;

/// The hash algorithm a sidecar names for its firmware file.
///
/// Only SHA-256 is understood today; any other name parses as
/// [`HashAlgo::Other`] carrying the name, so an unfamiliar sidecar is still
/// legible (reported, not silently dropped) rather than mistaken for SHA-256.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashAlgo<'a> {
    /// SHA-256, the digest FREEHOLD records for firmware files.
    Sha256,
    /// An algorithm this build does not recognise, kept by name.
    Other(&'a str),
}

/// A parsed firmware hash: the algorithm, and the digest when it is a SHA-256
/// of the right length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareHash<'a> {
    /// The algorithm named before the `:` in the hash field.
    pub algo: HashAlgo<'a>,
    /// The decoded digest, present only for a well-formed SHA-256 (exactly
    /// 32 bytes from 64 hex digits). `None` for any other algorithm or a
    /// malformed SHA-256 digest — [`FirmwareSidecar::parse`] rejects the
    /// latter, so a parsed sidecar with `algo == Sha256` always carries the
    /// bytes.
    pub sha256: Option<[u8; SHA256_LEN]>,
}

/// The four documented fields of a firmware sidecar, each borrowing from the
/// text it was parsed from.
///
/// A sidecar reads as one `Key: value` pair per line. Keys are matched
/// case-insensitively; blank lines and lines whose first non-space character
/// is `#` are ignored; leading and trailing spaces around key and value are
/// trimmed. The four keys below are required; unknown keys are ignored so a
/// newer sidecar with extra fields still parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareSidecar<'a> {
    /// `Vendor:` — who produced the firmware (e.g. `NVIDIA`).
    pub vendor: &'a str,
    /// `Version:` — the firmware release, verbatim.
    pub version: &'a str,
    /// `Hash:` — the digest the firmware bytes must match, parsed.
    pub hash: FirmwareHash<'a>,
    /// `Licence:` — the firmware's licence, surfaced for `DRIVERS` and the
    /// owner; never used to refuse a load (see the module docs).
    pub licence: &'a str,
}

/// Why a sidecar text could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarError {
    /// A required key (`Vendor`, `Version`, `Hash`, `Licence`) was absent.
    MissingField(SidecarField),
    /// A line was not blank, not a comment, and had no `:` separating a key
    /// from a value.
    MalformedLine,
    /// A key appeared more than once with conflicting intent; a sidecar names
    /// each field once.
    DuplicateField(SidecarField),
    /// The `Hash:` value had no `algo:digest` shape (missing the `:`).
    MalformedHash,
    /// The hash named SHA-256 but the digest was not exactly 64 hex digits.
    BadSha256,
}

/// One of the sidecar's required fields, for precise error reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarField {
    Vendor,
    Version,
    Hash,
    Licence,
}

impl<'a> FirmwareSidecar<'a> {
    /// Parse a sidecar text into its four fields.
    ///
    /// Returns [`SidecarError`] for a missing required field, a line that is
    /// neither blank, a comment nor `key: value`, a duplicated field, or a
    /// hash that is malformed or claims SHA-256 without a 32-byte digest.
    pub fn parse(text: &'a str) -> Result<FirmwareSidecar<'a>, SidecarError> {
        let mut vendor: Option<&str> = None;
        let mut version: Option<&str> = None;
        let mut hash_raw: Option<&str> = None;
        let mut licence: Option<&str> = None;

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // Split only on the first `:`: the hash value is itself
            // `algo:digest`, so its inner `:` must stay in the value.
            let (key, value) = trimmed.split_once(':').ok_or(SidecarError::MalformedLine)?;
            let key = key.trim();
            let value = value.trim();

            if key.eq_ignore_ascii_case("vendor") {
                set_once(&mut vendor, value, SidecarField::Vendor)?;
            } else if key.eq_ignore_ascii_case("version") {
                set_once(&mut version, value, SidecarField::Version)?;
            } else if key.eq_ignore_ascii_case("hash") {
                set_once(&mut hash_raw, value, SidecarField::Hash)?;
            } else if key.eq_ignore_ascii_case("licence") || key.eq_ignore_ascii_case("license") {
                set_once(&mut licence, value, SidecarField::Licence)?;
            }
            // Unknown keys are ignored so a newer sidecar still parses.
        }

        let vendor = vendor.ok_or(SidecarError::MissingField(SidecarField::Vendor))?;
        let version = version.ok_or(SidecarError::MissingField(SidecarField::Version))?;
        let hash_raw = hash_raw.ok_or(SidecarError::MissingField(SidecarField::Hash))?;
        let licence = licence.ok_or(SidecarError::MissingField(SidecarField::Licence))?;

        Ok(FirmwareSidecar {
            vendor,
            version,
            hash: parse_hash(hash_raw)?,
            licence,
        })
    }

    /// Whether this sidecar describes NVIDIA firmware — the only vendor a
    /// GSP firmware for an RTX card can come from. The comparison is
    /// case-insensitive so `NVIDIA`, `Nvidia` and `nvidia` all match.
    pub fn is_nvidia(&self) -> bool {
        self.vendor.eq_ignore_ascii_case("NVIDIA")
    }
}

/// Store `value` into `slot` the first time; a second non-empty write of a
/// different value is a [`SidecarError::DuplicateField`].
fn set_once<'a>(
    slot: &mut Option<&'a str>,
    value: &'a str,
    field: SidecarField,
) -> Result<(), SidecarError> {
    match slot {
        Some(existing) if *existing != value => Err(SidecarError::DuplicateField(field)),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value);
            Ok(())
        }
    }
}

/// Parse a `algo:digest` hash string into a [`FirmwareHash`].
fn parse_hash(raw: &str) -> Result<FirmwareHash<'_>, SidecarError> {
    let (algo, digest) = raw.split_once(':').ok_or(SidecarError::MalformedHash)?;
    let algo = algo.trim();
    let digest = digest.trim();
    if algo.eq_ignore_ascii_case("sha256") || algo.eq_ignore_ascii_case("sha-256") {
        let bytes = decode_sha256(digest).ok_or(SidecarError::BadSha256)?;
        Ok(FirmwareHash {
            algo: HashAlgo::Sha256,
            sha256: Some(bytes),
        })
    } else {
        Ok(FirmwareHash {
            algo: HashAlgo::Other(algo),
            sha256: None,
        })
    }
}

/// Decode exactly 64 hex digits into a 32-byte digest. Returns `None` for any
/// wrong length or non-hex character. Case-insensitive.
fn decode_sha256(hex: &str) -> Option<[u8; SHA256_LEN]> {
    let bytes = hex.as_bytes();
    if bytes.len() != SHA256_LEN * 2 {
        return None;
    }
    let mut out = [0u8; SHA256_LEN];
    let mut i = 0;
    while i < SHA256_LEN {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(out)
}

/// One hex digit's value, or `None` for a non-hex byte.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The firmware a probed card needs, as this driver asks FREEHOLD for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareRequest {
    /// The file this card's firmware is kept under in `C:\FIRMWARE\`, named
    /// for the device per FREEHOLD's model. A stable, per-family base name
    /// (no directory, no extension) so the owner can see at a glance which
    /// file a card uses: `GSP-TURING`, `GSP-AMPERE`, `GSP-ADA`,
    /// `GSP-BLACKWELL`.
    pub file_basename: &'static str,
    /// The family this request is for.
    pub family: Family,
    /// Whether the GSP firmware is *required* to bring this card's display
    /// engine up. True for Ada and Blackwell, whose display path runs through
    /// the GSP; false for Turing and Ampere, which can be driven without it
    /// (a documented difference — see `validation/stage-15`). When false, a
    /// missing firmware file is reported, not fatal: the card still comes up.
    pub display_requires_gsp: bool,
}

/// The firmware a card needs, derived from its probe.
///
/// Returns `None` for silicon whose family this build does not recognise
/// ([`Family::Unknown`]): a future card is still identified by
/// [`crate::probe`], but this driver will not guess which firmware it wants.
pub fn required_firmware(probe: &GpuProbe) -> Option<FirmwareRequest> {
    let (file_basename, display_requires_gsp) = match probe.family {
        Family::Turing => ("GSP-TURING", false),
        Family::Ampere => ("GSP-AMPERE", false),
        Family::Ada => ("GSP-ADA", true),
        Family::Blackwell => ("GSP-BLACKWELL", true),
        Family::Unknown => return None,
    };
    Some(FirmwareRequest {
        file_basename,
        family: probe.family,
        display_requires_gsp,
    })
}

/// Why a sidecar is not acceptable to load for a given card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectError {
    /// The driver does not recognise the card's family, so it cannot say
    /// which firmware the card needs (see [`required_firmware`]).
    UnknownFamily,
    /// The sidecar's vendor is not NVIDIA: the wrong firmware for this card.
    WrongVendor,
    /// The sidecar's hash is not a usable SHA-256 digest, so the firmware
    /// bytes could not be verified before loading. A sidecar must carry a
    /// verifiable hash to be trusted.
    UnverifiableHash,
}

/// What [`select`] decided: the firmware this card should load, with the file
/// to read and the digest its bytes must match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareSelection<'a> {
    /// The firmware the card asked for (file name, family, requirement).
    pub request: FirmwareRequest,
    /// The accepted sidecar describing that file.
    pub sidecar: FirmwareSidecar<'a>,
    /// The SHA-256 digest the firmware bytes must match once read. Always
    /// present on a successful selection — [`select`] rejects a sidecar whose
    /// hash is not a usable SHA-256.
    pub expected_sha256: [u8; SHA256_LEN],
}

/// Why firmware bytes failed verification against their selected sidecar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The firmware bytes' SHA-256 did not equal the sidecar's expected digest:
    /// a corrupt or truncated copy, or a firmware/sidecar version mismatch. The
    /// driver must refuse to boot the GSP — never load unverified bytes.
    /// Carries the digest actually computed, for diagnostics and logging
    /// (`DRIVERS`), alongside the expected one in [`FirmwareSelection`].
    DigestMismatch {
        /// The SHA-256 actually computed over the firmware bytes.
        computed: [u8; SHA256_LEN],
    },
}

impl<'a> FirmwareSelection<'a> {
    /// Verify firmware bytes against the digest this selection expects.
    ///
    /// Computes the SHA-256 of `firmware_bytes` with the crate's own
    /// [`crate::sha256`] and compares it to [`FirmwareSelection::expected_sha256`],
    /// the digest the sidecar recorded. On a match the firmware is the file the
    /// sidecar describes and may be loaded; on a mismatch the bytes are corrupt
    /// or the wrong version and the GSP must not be booted
    /// ([`VerifyError::DigestMismatch`]).
    ///
    /// This is the last host-side gate before the firmware is handed to the
    /// card. The read of the file and the on-device load are elsewhere (the
    /// VFS and the FREEHOLD runtime); this is pure, allocation-free computation
    /// over bytes already in memory, so it is exercised in full on the host.
    pub fn verify(&self, firmware_bytes: &[u8]) -> Result<(), VerifyError> {
        let computed = sha256(firmware_bytes);
        if computed == self.expected_sha256 {
            Ok(())
        } else {
            Err(VerifyError::DigestMismatch { computed })
        }
    }
}

/// Decide whether `sidecar` is the right, verifiable firmware to load for the
/// card described by `probe`.
///
/// On success the returned [`FirmwareSelection`] names the file to read and
/// the digest its bytes must match — the input to the later load-and-verify
/// step, which this crate does not perform. Rejects an unrecognised family, a
/// non-NVIDIA sidecar, or a sidecar without a usable SHA-256 to verify
/// against. It does **not** judge the firmware's licence (FREEHOLD loads what
/// the owner placed, whatever its licence) and does not read or hash any
/// file.
pub fn select<'a>(
    probe: &GpuProbe,
    sidecar: FirmwareSidecar<'a>,
) -> Result<FirmwareSelection<'a>, SelectError> {
    let request = required_firmware(probe).ok_or(SelectError::UnknownFamily)?;
    if !sidecar.is_nvidia() {
        return Err(SelectError::WrongVendor);
    }
    let expected_sha256 = match sidecar.hash {
        FirmwareHash {
            algo: HashAlgo::Sha256,
            sha256: Some(digest),
        } => digest,
        _ => return Err(SelectError::UnverifiableHash),
    };
    Ok(FirmwareSelection {
        request,
        sidecar,
        expected_sha256,
    })
}
