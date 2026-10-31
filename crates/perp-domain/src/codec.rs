//! Canonical binary encoding used for log records, snapshots and the state
//! hash. All integers are little-endian and fixed width; there are no
//! self-describing or variable-length integers, so an encoding is unique.
//!
//! Decoding never trusts a length found in the input: [`Reader::count`] checks a
//! claimed element count against both a hard cap and the number of bytes that
//! are actually left before anything is allocated.

use crate::sha256::{Digest, Sha256};
use std::fmt;

/// Strict-parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Where encoded bytes go.
enum Sink {
    Buf(Vec<u8>),
    Hash(Sha256, u64),
}

/// Byte sink that either collects the bytes or hashes them on the fly.
pub struct Writer {
    sink: Sink,
}

impl Writer {
    /// Collecting writer.
    pub fn new_buf() -> Writer {
        Writer {
            sink: Sink::Buf(Vec::new()),
        }
    }

    /// Collecting writer with a pre-sized buffer.
    pub fn with_capacity(n: usize) -> Writer {
        Writer {
            sink: Sink::Buf(Vec::with_capacity(n)),
        }
    }

    /// Hashing writer: nothing is stored, bytes feed SHA-256.
    pub fn new_hash() -> Writer {
        Writer {
            sink: Sink::Hash(Sha256::new(), 0),
        }
    }

    /// Append raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        match &mut self.sink {
            Sink::Buf(v) => v.extend_from_slice(b),
            Sink::Hash(h, n) => {
                h.update(b);
                *n = n.saturating_add(b.len() as u64);
            }
        }
    }

    /// Append one byte.
    pub fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }

    /// Append a bool as 0 or 1.
    pub fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    /// Append a `u16`.
    pub fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }

    /// Append a `u32`.
    pub fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    /// Append an `i32`.
    pub fn i32(&mut self, v: i32) {
        self.bytes(&v.to_le_bytes());
    }

    /// Append a `u64`.
    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    /// Append an `i64`.
    pub fn i64(&mut self, v: i64) {
        self.bytes(&v.to_le_bytes());
    }

    /// Append a `u128`.
    pub fn u128(&mut self, v: u128) {
        self.bytes(&v.to_le_bytes());
    }

    /// Append an `i128`.
    pub fn i128(&mut self, v: i128) {
        self.bytes(&v.to_le_bytes());
    }

    /// Append an element count (`u32`).
    pub fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }

    /// Number of bytes written so far.
    pub fn len(&self) -> u64 {
        match &self.sink {
            Sink::Buf(v) => v.len() as u64,
            Sink::Hash(_, n) => *n,
        }
    }

    /// True when nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take the collected bytes (empty for hashing writers).
    pub fn into_bytes(self) -> Vec<u8> {
        match self.sink {
            Sink::Buf(v) => v,
            Sink::Hash(..) => Vec::new(),
        }
    }

    /// Digest of everything written (zero digest for collecting writers).
    pub fn finish_hash(self) -> Digest {
        match self.sink {
            Sink::Hash(h, _) => h.finalize(),
            Sink::Buf(_) => [0; 32],
        }
    }
}

/// Cursor over untrusted bytes.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// New reader.
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    /// Bytes not consumed yet.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Fail unless all input was consumed.
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(DecodeError("trailing bytes"))
        }
    }

    /// Take `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(DecodeError("length overflow"))?;
        let out = self
            .data
            .get(self.pos..end)
            .ok_or(DecodeError("truncated input"))?;
        self.pos = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let s = self.bytes(N)?;
        <[u8; N]>::try_from(s).map_err(|_| DecodeError("truncated input"))
    }

    /// Read one byte.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    /// Read a strict bool (only 0 and 1 are valid).
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError("invalid bool")),
        }
    }

    /// Read a `u16`.
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// Read a `u32`.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// Read an `i32`.
    pub fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// Read a `u64`.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Read an `i64`.
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// Read a `u128`.
    pub fn u128(&mut self) -> Result<u128, DecodeError> {
        Ok(u128::from_le_bytes(self.array()?))
    }

    /// Read an `i128`.
    pub fn i128(&mut self) -> Result<i128, DecodeError> {
        Ok(i128::from_le_bytes(self.array()?))
    }

    /// Read a 32-byte digest.
    pub fn digest(&mut self) -> Result<Digest, DecodeError> {
        self.array()
    }

    /// Read an element count. It must not exceed `max`, and `count *
    /// min_item_bytes` must fit in the bytes that remain, so a hostile count
    /// can never cause a large allocation.
    pub fn count(&mut self, max: usize, min_item_bytes: usize) -> Result<usize, DecodeError> {
        let n = usize::try_from(self.u32()?).map_err(|_| DecodeError("count overflow"))?;
        if n > max {
            return Err(DecodeError("count above limit"));
        }
        let need = n
            .checked_mul(min_item_bytes.max(1))
            .ok_or(DecodeError("count overflow"))?;
        if need > self.remaining() {
            return Err(DecodeError("count exceeds remaining input"));
        }
        Ok(n)
    }
}

/// Canonical encoding.
pub trait Encode {
    /// Write the canonical encoding of `self`.
    fn encode(&self, w: &mut Writer);
}

/// Strict decoding of untrusted bytes.
pub trait Decode: Sized {
    /// Parse one value; must reject anything non-canonical.
    fn decode(r: &mut Reader<'_>) -> Result<Self, DecodeError>;
}

/// Encode a value to a fresh byte vector.
pub fn to_bytes<T: Encode>(v: &T) -> Vec<u8> {
    let mut w = Writer::new_buf();
    v.encode(&mut w);
    w.into_bytes()
}

/// Decode a value from `bytes`, requiring that all bytes are consumed.
pub fn from_bytes<T: Decode>(bytes: &[u8]) -> Result<T, DecodeError> {
    let mut r = Reader::new(bytes);
    let v = T::decode(&mut r)?;
    r.finish()?;
    Ok(v)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip() {
        let mut w = Writer::new_buf();
        w.u8(7);
        w.bool(true);
        w.u16(65000);
        w.u32(4_000_000_000);
        w.i32(-5);
        w.u64(u64::MAX);
        w.i64(i64::MIN);
        w.u128(u128::MAX);
        w.i128(i128::MIN);
        let b = w.into_bytes();
        let mut r = Reader::new(&b);
        assert_eq!(r.u8().unwrap(), 7);
        assert!(r.bool().unwrap());
        assert_eq!(r.u16().unwrap(), 65000);
        assert_eq!(r.u32().unwrap(), 4_000_000_000);
        assert_eq!(r.i32().unwrap(), -5);
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert_eq!(r.i64().unwrap(), i64::MIN);
        assert_eq!(r.u128().unwrap(), u128::MAX);
        assert_eq!(r.i128().unwrap(), i128::MIN);
        r.finish().unwrap();
    }

    #[test]
    fn truncated_input_is_an_error_at_every_length() {
        let mut w = Writer::new_buf();
        w.u64(1);
        w.i128(2);
        let b = w.into_bytes();
        for n in 0..b.len() {
            let mut r = Reader::new(&b[..n]);
            let res = r.u64().and_then(|_| r.i128());
            assert!(res.is_err(), "len {n}");
        }
    }

    #[test]
    fn strict_bool_rejects_other_values() {
        for v in 2..=255u8 {
            assert!(Reader::new(&[v]).bool().is_err());
        }
    }

    #[test]
    fn hostile_count_is_rejected_before_allocation() {
        let mut w = Writer::new_buf();
        w.u32(u32::MAX);
        let b = w.into_bytes();
        assert!(Reader::new(&b).count(usize::MAX, 1).is_err());
        let mut w = Writer::new_buf();
        w.u32(1000);
        w.bytes(&[0; 10]);
        let b = w.into_bytes();
        // within the cap but more than the remaining bytes can hold
        assert!(Reader::new(&b).count(2000, 1).is_err());
        assert!(Reader::new(&b).count(999, 0).is_err());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        struct One;
        impl Decode for One {
            fn decode(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
                r.u8().map(|_| One)
            }
        }
        assert!(from_bytes::<One>(&[1]).is_ok());
        assert_eq!(
            from_bytes::<One>(&[1, 2]).err(),
            Some(DecodeError("trailing bytes"))
        );
    }

    #[test]
    fn hash_writer_matches_hash_of_buffer() {
        let mut a = Writer::new_buf();
        let mut b = Writer::new_hash();
        for i in 0..100u64 {
            a.u64(i);
            b.u64(i);
        }
        assert_eq!(b.len(), 800);
        assert_eq!(crate::sha256::sha256(&a.into_bytes()), b.finish_hash());
    }

    #[test]
    fn encoding_is_little_endian_fixed_width() {
        let mut w = Writer::new_buf();
        w.u32(1);
        assert_eq!(w.into_bytes(), vec![1, 0, 0, 0]);
    }

    #[test]
    fn empty_writer_reports_empty() {
        let w = Writer::new_buf();
        assert!(w.is_empty());
        assert_eq!(Writer::new_hash().len(), 0);
    }
}
