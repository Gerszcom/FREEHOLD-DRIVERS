//! Stage 5 groundwork — typed wire serialization for RPC payloads.
//!
//! [`crate::rpc`] carries each message as an envelope — a kind, a function
//! selector, a request id and a status word — in front of a run of *argument*
//! bytes. To the envelope those argument bytes are opaque: it copies them,
//! checksums them (one layer down, in [`crate::frame`]) and hands them over,
//! without knowing what they mean. This module is the layer that gives those
//! bytes a *shape*. A real RPC argument block is a sequence of typed fields —
//! a handle here, a 64-bit GPU address there, a length, a flag — packed one
//! after another in a documented order. The GSP firmware dictates *which*
//! fields each function takes and *in what order*; this module is the
//! allocation-free machinery that reads and writes such a sequence, so the
//! function catalog above it (the concrete request/result structs, still to
//! come — see `ROADMAP.md`) is written as plain field-by-field code rather than
//! hand-computed byte offsets.
//!
//! ## Two cursors
//!
//! A [`Writer`] appends little-endian fields to the front of a mutable buffer,
//! advancing a position as it goes; a [`Reader`] consumes them back off an
//! immutable buffer in the same order. Both are bounds-checked on every
//! operation — a write that would run past the buffer's end, or a read that
//! would run past the bytes that are there, fails with a [`CodecError`] and
//! moves the cursor no further, so a short or truncated message is caught at the
//! field that overruns rather than read as garbage. Neither cursor owns or
//! allocates memory: the bytes live in the caller's slice (on hardware, a slice
//! of a mapped shared region), exactly as the envelope's do.
//!
//! ## Why little-endian, and why host-verifiable
//!
//! Every multi-byte field is encoded least-significant-byte-first, matching the
//! x86-64 host and the GPU's own register and message conventions, and the same
//! choice the envelope header (`crate::rpc`) and the ring indices
//! (`crate::msgq`) already make — so one endianness runs through the whole
//! stack. Packing integers into a byte run in a fixed order and reading them
//! back is *ordinary computer science*, correct or incorrect on any machine, so
//! this module — like the ring, the framing, the envelope and the SHA-256
//! beneath it — is written and proven here against ordinary byte buffers with no
//! pending-hardware caveat. What it does **not** fix — because the firmware
//! dictates it — is *which* fields a given RPC carries and *in what order*; that
//! is the function catalog's data, filled in and confirmed against the published
//! GSP interface later. Clean-room: this is FREEHOLD's own serializer; no driver
//! source is copied or translated.
//!
//! `no_std`, `forbid(unsafe_code)`, allocation-free.

/// Why a [`Writer`] or [`Reader`] operation could not be carried out.
///
/// Every variant leaves its cursor unmoved, so the buffer written so far — or
/// the fields already read — stay valid, and the caller can report the failure
/// without worrying about a half-written field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// A [`Writer`] operation would run past the end of its buffer. Carries how
    /// many more bytes the field needs and how many the buffer still has.
    Overflow {
        /// Bytes the field would write.
        needed: usize,
        /// Bytes left in the buffer from the cursor onward.
        remaining: usize,
    },
    /// A [`Reader`] operation would run past the end of the available bytes.
    /// Signals a message shorter than its declared shape — a truncated or
    /// malformed payload.
    Underflow {
        /// Bytes the field would read.
        needed: usize,
        /// Bytes left in the buffer from the cursor onward.
        remaining: usize,
    },
    /// [`Reader::finish`] found bytes still unread. Signals a message *longer*
    /// than its declared shape — extra trailing bytes a strict reader rejects.
    TrailingBytes {
        /// Bytes left unread when the reader was expected to be empty.
        remaining: usize,
    },
}

/// A forward-only cursor that appends little-endian fields to a byte buffer.
///
/// Built over a caller-provided mutable slice; every `put_*` writes at the
/// current position and advances it, and [`finish`](Writer::finish) returns the
/// bytes written so far — the slice a caller hands to the envelope as its
/// argument bytes. A write that will not fit leaves the cursor where it was and
/// returns [`CodecError::Overflow`], so a partially-built message is never
/// published.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    /// A writer positioned at the start of `buf`.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Writer { buf, pos: 0 }
    }

    /// How many bytes have been written so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// How many bytes remain before the buffer is full.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Reserve `n` bytes for the next field, returning the slice to fill, or
    /// [`CodecError::Overflow`] if that many will not fit.
    fn take(&mut self, n: usize) -> Result<&mut [u8], CodecError> {
        let remaining = self.buf.len() - self.pos;
        match self.pos.checked_add(n) {
            Some(end) if end <= self.buf.len() => {
                let start = self.pos;
                self.pos = end;
                Ok(&mut self.buf[start..end])
            }
            _ => Err(CodecError::Overflow {
                needed: n,
                remaining,
            }),
        }
    }

    /// Append a `u8`.
    pub fn put_u8(&mut self, value: u8) -> Result<(), CodecError> {
        self.take(1)?[0] = value;
        Ok(())
    }

    /// Append a little-endian `u16`.
    pub fn put_u16(&mut self, value: u16) -> Result<(), CodecError> {
        self.take(2)?.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Append a little-endian `u32`.
    pub fn put_u32(&mut self, value: u32) -> Result<(), CodecError> {
        self.take(4)?.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Append a little-endian `u64`.
    pub fn put_u64(&mut self, value: u64) -> Result<(), CodecError> {
        self.take(8)?.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Append a little-endian `i32` (two's complement, as its `u32` bit
    /// pattern).
    pub fn put_i32(&mut self, value: i32) -> Result<(), CodecError> {
        self.take(4)?.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Append a bool as a single byte: `1` for true, `0` for false.
    pub fn put_bool(&mut self, value: bool) -> Result<(), CodecError> {
        self.put_u8(value as u8)
    }

    /// Append raw bytes verbatim.
    pub fn put_bytes(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.take(bytes.len())?.copy_from_slice(bytes);
        Ok(())
    }

    /// Append `n` zero bytes — padding a field up to an alignment or a fixed
    /// slot the firmware's layout reserves.
    pub fn put_zeros(&mut self, n: usize) -> Result<(), CodecError> {
        for b in self.take(n)? {
            *b = 0;
        }
        Ok(())
    }

    /// Finish writing and return the bytes written (`buf[..position]`).
    pub fn finish(self) -> &'a [u8] {
        &self.buf[..self.pos]
    }
}

/// A forward-only cursor that consumes little-endian fields from a byte buffer.
///
/// Built over a caller-provided immutable slice — the argument bytes the
/// envelope handed back; every `get_*` reads at the current position and
/// advances it, in the same order a [`Writer`] wrote them. A read that would run
/// past the end leaves the cursor where it was and returns
/// [`CodecError::Underflow`], so a truncated message is caught at the field that
/// overruns.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// How many bytes have been read so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// How many bytes remain to be read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether every byte has been read.
    pub fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    /// Consume `n` bytes for the next field, returning the slice, or
    /// [`CodecError::Underflow`] if fewer than `n` remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let remaining = self.buf.len() - self.pos;
        match self.pos.checked_add(n) {
            Some(end) if end <= self.buf.len() => {
                let start = self.pos;
                self.pos = end;
                Ok(&self.buf[start..end])
            }
            _ => Err(CodecError::Underflow {
                needed: n,
                remaining,
            }),
        }
    }

    /// Read a `u8`.
    pub fn get_u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    /// Read a little-endian `u16`.
    pub fn get_u16(&mut self) -> Result<u16, CodecError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little-endian `u32`.
    pub fn get_u32(&mut self) -> Result<u32, CodecError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a little-endian `u64`.
    pub fn get_u64(&mut self) -> Result<u64, CodecError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read a little-endian `i32`.
    pub fn get_i32(&mut self) -> Result<i32, CodecError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a bool encoded by [`Writer::put_bool`]: `0` is false, any non-zero
    /// byte is true.
    pub fn get_bool(&mut self) -> Result<bool, CodecError> {
        Ok(self.get_u8()? != 0)
    }

    /// Borrow the next `n` bytes verbatim, advancing past them.
    pub fn get_bytes(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        self.take(n)
    }

    /// Advance past `n` bytes without reading them — skipping a reserved or
    /// don't-care field in the firmware's layout.
    pub fn skip(&mut self, n: usize) -> Result<(), CodecError> {
        self.take(n).map(|_| ())
    }

    /// Assert the buffer is fully consumed, returning
    /// [`CodecError::TrailingBytes`] if any bytes remain.
    ///
    /// A strict decoder calls this after reading every field, so a reply longer
    /// than the shape it expected — a layout mismatch — is caught rather than
    /// silently ignored. A lenient decoder that tolerates trailing bytes simply
    /// does not call it.
    pub fn finish(self) -> Result<(), CodecError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes {
                remaining: self.buf.len() - self.pos,
            })
        }
    }
}

/// A value that can serialize itself into a [`Writer`] as a run of wire fields.
///
/// The function catalog's request and result structs implement this (and
/// [`WireDecode`]) so a call is encoded by writing its fields in order rather
/// than by computing byte offsets. [`encoded_len`](WireEncode::encoded_len)
/// reports how many bytes [`encode`](WireEncode::encode) will write, so a caller
/// can size or bound-check a scratch buffer before encoding.
pub trait WireEncode {
    /// Write this value's fields, in order, into `w`.
    fn encode(&self, w: &mut Writer<'_>) -> Result<(), CodecError>;

    /// The exact number of bytes [`encode`](WireEncode::encode) writes.
    fn encoded_len(&self) -> usize;
}

/// A value that can be reconstructed from a [`Reader`] — the inverse of
/// [`WireEncode`].
pub trait WireDecode: Sized {
    /// Read this value's fields, in order, from `r`.
    fn decode(r: &mut Reader<'_>) -> Result<Self, CodecError>;
}

/// Generate `WireEncode` + `WireDecode` for a fixed-size primitive, delegating
/// to the matching `Writer`/`Reader` methods.
macro_rules! wire_primitive {
    ($ty:ty, $put:ident, $get:ident, $len:expr) => {
        impl WireEncode for $ty {
            fn encode(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
                w.$put(*self)
            }
            fn encoded_len(&self) -> usize {
                $len
            }
        }
        impl WireDecode for $ty {
            fn decode(r: &mut Reader<'_>) -> Result<Self, CodecError> {
                r.$get()
            }
        }
    };
}

wire_primitive!(u8, put_u8, get_u8, 1);
wire_primitive!(u16, put_u16, get_u16, 2);
wire_primitive!(u32, put_u32, get_u32, 4);
wire_primitive!(u64, put_u64, get_u64, 8);
wire_primitive!(i32, put_i32, get_i32, 4);
wire_primitive!(bool, put_bool, get_bool, 1);

/// Encode `value` into `buf`, returning the bytes written.
///
/// A convenience over building a [`Writer`] by hand for callers that just want
/// a struct's bytes: fails with [`CodecError::Overflow`] if `buf` is smaller
/// than `value.encoded_len()`.
pub fn encode_into<'a, T: WireEncode>(
    value: &T,
    buf: &'a mut [u8],
) -> Result<&'a [u8], CodecError> {
    let mut w = Writer::new(buf);
    value.encode(&mut w)?;
    Ok(w.finish())
}

/// Decode a `T` from exactly `buf`, requiring every byte to be consumed.
///
/// A convenience over building a [`Reader`] by hand: after decoding it calls
/// [`Reader::finish`], so a buffer longer than `T`'s shape is rejected with
/// [`CodecError::TrailingBytes`]. A caller that tolerates trailing bytes uses a
/// [`Reader`] directly and omits the [`finish`](Reader::finish) check.
pub fn decode_exact<T: WireDecode>(buf: &[u8]) -> Result<T, CodecError> {
    let mut r = Reader::new(buf);
    let value = T::decode(&mut r)?;
    r.finish()?;
    Ok(value)
}
