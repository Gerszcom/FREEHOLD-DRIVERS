//! Stage 5 groundwork — a `no_std`, `forbid(unsafe_code)` SHA-256.
//!
//! Before a GSP firmware image is handed to the card it must be verified: the
//! bytes read from `C:\FIRMWARE\` are hashed and the digest compared against
//! the one the sidecar records ([`crate::firmware::FirmwareSelection::expected_sha256`]).
//! A firmware file whose real digest does not match its sidecar is a bad copy
//! or a version mismatch, and must never be booted. That check needs a hash
//! function, and this module is it.
//!
//! The implementation is FREEHOLD's own, written from the **public FIPS 180-4
//! specification** (the Secure Hash Standard) — the padding scheme, the eight
//! initial hash values, the sixty-four round constants, the message schedule
//! and the compression function are all from that standard, a mathematical
//! definition in the public domain. No driver source and no third-party crate
//! is involved; this is clean-room in the same sense as the rest of the
//! driver, and it is `no_std`, allocation-free and `forbid(unsafe_code)` so the
//! exact code that runs inside FREEHOLD also runs in the host tests.
//!
//! It is verified in full on the host against the standard SHA-256 test
//! vectors (`tests/sha256.rs`), so unlike the on-device firmware *load* this
//! piece carries no "pending hardware" caveat: a correct SHA-256 is a correct
//! SHA-256 on any machine.
//!
//! # Example
//!
//! ```
//! use freehold_rtx::sha256::{sha256, Sha256};
//!
//! // One-shot.
//! let digest = sha256(b"abc");
//! assert_eq!(
//!     digest,
//!     [
//!         0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
//!         0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
//!         0xf2, 0x00, 0x15, 0xad,
//!     ]
//! );
//!
//! // Streaming: the same digest, fed in pieces — as a firmware file is read
//! // block by block through the VFS.
//! let mut h = Sha256::new();
//! h.update(b"a");
//! h.update(b"bc");
//! assert_eq!(h.finalize(), digest);
//! ```

/// Length in bytes of a SHA-256 digest.
///
/// Mirrors [`crate::firmware::SHA256_LEN`]; both name the same 32-byte digest.
pub const DIGEST_LEN: usize = 32;

/// Size in bytes of a SHA-256 message block.
const BLOCK_LEN: usize = 64;

/// The eight initial hash values: the first thirty-two bits of the fractional
/// parts of the square roots of the first eight primes (FIPS 180-4 §5.3.3).
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// The sixty-four round constants: the first thirty-two bits of the fractional
/// parts of the cube roots of the first sixty-four primes (FIPS 180-4 §4.2.2).
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// A streaming SHA-256 hasher.
///
/// Feed the message with [`Sha256::update`], called as many times as needed —
/// a firmware image can be hashed as it is read, without ever holding the whole
/// file at once — then take the digest with [`Sha256::finalize`]. The hasher
/// is allocation-free: it holds the 256-bit running state, a single 64-byte
/// block buffer, and the running message length.
#[derive(Clone)]
pub struct Sha256 {
    /// The eight 32-bit working hash words.
    state: [u32; 8],
    /// Bytes buffered toward the next full block.
    block: [u8; BLOCK_LEN],
    /// Number of bytes currently buffered in `block` (`0..BLOCK_LEN`).
    buffered: usize,
    /// Total number of message bytes fed so far, for the length padding.
    len: u64,
}

impl Sha256 {
    /// A fresh hasher over the empty message.
    #[must_use]
    pub const fn new() -> Self {
        Sha256 {
            state: H0,
            block: [0u8; BLOCK_LEN],
            buffered: 0,
            len: 0,
        }
    }

    /// Feed more message bytes into the hash.
    ///
    /// May be called any number of times; `h.update(a); h.update(b)` hashes
    /// exactly the concatenation of `a` and `b`.
    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);

        // Top up a partially filled block first.
        if self.buffered > 0 {
            let need = BLOCK_LEN - self.buffered;
            let take = need.min(data.len());
            self.block[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == BLOCK_LEN {
                let block = self.block;
                self.compress(&block);
                self.buffered = 0;
            }
        }

        // Consume whole blocks straight from the input, no copy.
        while data.len() >= BLOCK_LEN {
            let (block, rest) = data.split_at(BLOCK_LEN);
            let mut b = [0u8; BLOCK_LEN];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }

        // Buffer whatever is left over.
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    /// Finish the message and return the 32-byte digest.
    ///
    /// Consumes the hasher: the padding is a one-time operation, and taking the
    /// value by move keeps a finalized hasher from being fed or finalized
    /// again.
    #[must_use]
    pub fn finalize(mut self) -> [u8; DIGEST_LEN] {
        // FIPS 180-4 §5.1.1: append a single 1 bit, then enough 0 bits, then
        // the 64-bit big-endian message length in bits, so the total is a
        // multiple of the block size.
        let bit_len = self.len.wrapping_mul(8);

        // The 0x80 byte always fits: `buffered` is in `0..BLOCK_LEN`.
        self.block[self.buffered] = 0x80;
        self.buffered += 1;

        // If the 8-byte length would not fit in this block, zero-fill the rest
        // and compress, then start a fresh all-zero block for the length.
        if self.buffered > BLOCK_LEN - 8 {
            for b in &mut self.block[self.buffered..] {
                *b = 0;
            }
            let block = self.block;
            self.compress(&block);
            self.buffered = 0;
            self.block = [0u8; BLOCK_LEN];
        } else {
            for b in &mut self.block[self.buffered..BLOCK_LEN - 8] {
                *b = 0;
            }
        }

        self.block[BLOCK_LEN - 8..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.block;
        self.compress(&block);

        let mut out = [0u8; DIGEST_LEN];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state.iter()) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// The compression function over one 64-byte block (FIPS 180-4 §6.2.2).
    fn compress(&mut self, block: &[u8; BLOCK_LEN]) {
        // Message schedule: the first sixteen words are the block, big-endian;
        // the remaining forty-eight are derived from earlier words.
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (word, v) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(v);
        }
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash a whole message in one call and return its SHA-256 digest.
///
/// Equivalent to a [`Sha256::new`], one [`Sha256::update`] with `data`, and a
/// [`Sha256::finalize`].
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}
