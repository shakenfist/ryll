//! Generic, SPICE-agnostic infrastructure for safely parsing
//! untrusted wire input.
//!
//! This module deliberately contains no SPICE-specific
//! knowledge. It provides two building blocks:
//!
//! - [`LinkError`] — a taxonomy of the ways an untrusted byte
//!   stream can fail to parse.
//! - [`BoundedReader`] — a cursor over a `&[u8]` that tracks
//!   position and enforces bounds, so that every read is
//!   panic-free for arbitrary input.
//!
//! It is used by the SPICE link-handshake parsers and is
//! intended to be retrofitted across the workspace (tracked as
//! shakenfist/ryll#136).

/// The ways parsing an untrusted byte stream can fail.
///
/// Variants are `PartialEq`/`Eq` so tests can assert on the
/// exact error, including the diagnostic fields.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// The input ended before a read could complete: a read of
    /// `needed` bytes was attempted with only `available`
    /// bytes remaining.
    #[error("truncated input: needed {needed} bytes, only {available} available")]
    Truncated { needed: usize, available: usize },

    /// A declared count or size exceeds a sanity limit. `what`
    /// names the field, `value` is the declared value, and
    /// `max` is the enforced ceiling.
    #[error("{what} too large: {value} exceeds maximum of {max}")]
    TooLarge {
        what: &'static str,
        value: usize,
        max: usize,
    },

    /// An offset-addressed region falls outside the buffer:
    /// `offset + len` does not fit within `buffer_len` (or the
    /// addition overflowed).
    #[error(
        "bad offset: region [{offset}, {offset}+{len}) falls outside buffer of length {buffer_len}"
    )]
    BadOffset {
        offset: usize,
        len: usize,
        buffer_len: usize,
    },

    /// The four-byte magic at the start of a message did not
    /// match the expected value.
    ///
    /// Placeholder for the SPICE link-handshake parser (next
    /// step); consumed by the `SpiceLinkHeader` reader.
    #[error("bad magic: found {found:02x?}")]
    BadMagic { found: [u8; 4] },

    /// The declared protocol version is not supported.
    ///
    /// Placeholder for the SPICE link-handshake parser (next
    /// step); consumed by the `SpiceLinkHeader` reader.
    #[error("unsupported protocol version: {major}.{minor}")]
    UnsupportedVersion { major: u32, minor: u32 },

    /// The client requested an authentication mechanism the
    /// server does not implement.
    ///
    /// Placeholder for the SPICE link-handshake parser (next
    /// step); consumed by the authentication negotiation code.
    #[error("unsupported auth mechanism: {mechanism}")]
    UnsupportedAuthMechanism { mechanism: u32 },

    /// Decryption of an authentication payload failed.
    ///
    /// Placeholder for the SPICE link-handshake parser (next
    /// step); consumed by the password-decrypt path.
    #[error("decryption failed")]
    DecryptFailed,

    /// A field expected to contain UTF-8 text did not.
    ///
    /// Placeholder for the SPICE link-handshake parser (next
    /// step); consumed by string-field readers.
    #[error("invalid UTF-8 in text field")]
    BadUtf8,

    /// The RSA public key supplied to
    /// [`SpiceLinkReply::serialize`](crate::link::SpiceLinkReply::serialize)
    /// was not exactly 162 bytes (the fixed DER SubjectPublicKeyInfo size
    /// for a 1024-bit RSA key). This is a programming error on our side
    /// (a malformed key never reaches here from wire input), but a typed
    /// error is cleaner than a panic.
    #[error("bad RSA public key length: expected 162 bytes, got {len}")]
    BadKeyLength { len: usize },
}

/// A cursor over a byte slice that tracks position and enforces
/// bounds on every read.
///
/// Every method is panic-free for arbitrary input: there is no
/// slice indexing that can panic and no arithmetic that can
/// overflow. Reads that would run past the end of the buffer
/// return [`LinkError::Truncated`]; offset-addressed accesses
/// that fall outside the buffer return [`LinkError::BadOffset`].
///
/// Integer reads are little-endian, matching the SPICE wire
/// format.
#[derive(Debug, Clone)]
pub struct BoundedReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BoundedReader<'a> {
    /// Create a reader positioned at the start of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The number of unread bytes remaining after the current
    /// position.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// The current read position, measured in bytes from the
    /// start of the buffer.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Read the next `n` bytes and advance the position.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Truncated`] if fewer than `n` bytes
    /// remain.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], LinkError> {
        let available = self.remaining();
        if n > available {
            return Err(LinkError::Truncated {
                needed: n,
                available,
            });
        }
        // `self.pos + n` cannot overflow: `n <= available` and
        // `self.pos + available == self.data.len()`.
        let end = self.pos + n;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Read a fixed-size `[u8; N]` array and advance the
    /// position.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Truncated`] if fewer than `N` bytes
    /// remain.
    pub fn read_array<const N: usize>(&mut self) -> Result<[u8; N], LinkError> {
        let slice = self.read_bytes(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    /// Read a single byte and advance the position.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Truncated`] if no bytes remain.
    pub fn read_u8(&mut self) -> Result<u8, LinkError> {
        Ok(self.read_array::<1>()?[0])
    }

    /// Read a little-endian `u16` and advance the position.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Truncated`] if fewer than 2 bytes
    /// remain.
    pub fn read_u16(&mut self) -> Result<u16, LinkError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    /// Read a little-endian `u32` and advance the position.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Truncated`] if fewer than 4 bytes
    /// remain.
    pub fn read_u32(&mut self) -> Result<u32, LinkError> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    /// Read a little-endian `u64` and advance the position.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Truncated`] if fewer than 8 bytes
    /// remain.
    pub fn read_u64(&mut self) -> Result<u64, LinkError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    /// Read `count` little-endian `u32` values into a `Vec`.
    ///
    /// The `count > max` sanity check runs *before* any
    /// allocation, so a hostile `count` cannot trigger a large
    /// speculative `Vec::with_capacity`.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::TooLarge`] (naming `"count"`) if
    /// `count > max`, before allocating. Returns
    /// [`LinkError::Truncated`] if fewer than `count * 4` bytes
    /// remain.
    pub fn read_vec_u32(&mut self, count: usize, max: usize) -> Result<Vec<u32>, LinkError> {
        if count > max {
            return Err(LinkError::TooLarge {
                what: "count",
                value: count,
                max,
            });
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.read_u32()?);
        }
        Ok(out)
    }

    /// Return a bounds-checked slice of the *original* buffer.
    ///
    /// The `offset` is absolute (measured from the start of the
    /// buffer) and independent of the current read position;
    /// the position is not advanced. This suits SPICE messages
    /// whose sub-structures are addressed by offset from the
    /// message start.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::BadOffset`] if `offset + len`
    /// overflows or exceeds the buffer length.
    pub fn slice_at(&self, offset: usize, len: usize) -> Result<&'a [u8], LinkError> {
        let buffer_len = self.data.len();
        let end = offset.checked_add(len).filter(|&e| e <= buffer_len);
        match end {
            Some(end) => Ok(&self.data[offset..end]),
            None => Err(LinkError::BadOffset {
                offset,
                len,
                buffer_len,
            }),
        }
    }

    /// Return a new [`BoundedReader`] over the region addressed
    /// by [`slice_at`](Self::slice_at).
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::BadOffset`] if `offset + len`
    /// overflows or exceeds the buffer length.
    pub fn sub_reader(&self, offset: usize, len: usize) -> Result<BoundedReader<'a>, LinkError> {
        Ok(BoundedReader::new(self.slice_at(offset, len)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_integers_little_endian() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let mut r = BoundedReader::new(&data);
        assert_eq!(r.read_u8().unwrap(), 0x01);
        assert_eq!(r.read_u16().unwrap(), 0x0302);
        assert_eq!(r.read_u32().unwrap(), 0x0706_0504);
        // One byte consumed by the trailing 0x08 read below.
        assert_eq!(r.remaining(), 1);
        assert_eq!(r.read_u8().unwrap(), 0x08);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_u64_little_endian() {
        let data = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut r = BoundedReader::new(&data);
        assert_eq!(r.read_u64().unwrap(), 1);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn underrun_u8_returns_truncated() {
        let data: [u8; 0] = [];
        let mut r = BoundedReader::new(&data);
        assert_eq!(
            r.read_u8(),
            Err(LinkError::Truncated {
                needed: 1,
                available: 0,
            })
        );
    }

    #[test]
    fn underrun_u16_returns_truncated() {
        let data = [0xaa];
        let mut r = BoundedReader::new(&data);
        assert_eq!(
            r.read_u16(),
            Err(LinkError::Truncated {
                needed: 2,
                available: 1,
            })
        );
    }

    #[test]
    fn underrun_u32_returns_truncated() {
        let data = [0xaa, 0xbb, 0xcc];
        let mut r = BoundedReader::new(&data);
        assert_eq!(
            r.read_u32(),
            Err(LinkError::Truncated {
                needed: 4,
                available: 3,
            })
        );
    }

    #[test]
    fn underrun_u64_returns_truncated() {
        let data = [0u8; 7];
        let mut r = BoundedReader::new(&data);
        assert_eq!(
            r.read_u64(),
            Err(LinkError::Truncated {
                needed: 8,
                available: 7,
            })
        );
    }

    #[test]
    fn read_bytes_underrun_reports_available() {
        let data = [0x01, 0x02];
        let mut r = BoundedReader::new(&data);
        assert_eq!(
            r.read_bytes(5),
            Err(LinkError::Truncated {
                needed: 5,
                available: 2,
            })
        );
        // A failed read does not advance the position.
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn read_array_exact_fit_consumes_buffer() {
        let data = [0xde, 0xad, 0xbe, 0xef];
        let mut r = BoundedReader::new(&data);
        assert_eq!(r.read_array::<4>().unwrap(), [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_vec_u32_count_exceeds_max_returns_too_large() {
        // A large `count` that would blow up allocation must be
        // rejected before any Vec is built.
        let data: [u8; 0] = [];
        let mut r = BoundedReader::new(&data);
        assert_eq!(
            r.read_vec_u32(usize::MAX, 4),
            Err(LinkError::TooLarge {
                what: "count",
                value: usize::MAX,
                max: 4,
            })
        );
    }

    #[test]
    fn read_vec_u32_reads_count_values() {
        let data = [0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00];
        let mut r = BoundedReader::new(&data);
        assert_eq!(r.read_vec_u32(2, 8).unwrap(), vec![1, 2]);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_vec_u32_truncated_when_short() {
        // count within max, but not enough bytes present.
        let data = [0x01, 0x00, 0x00];
        let mut r = BoundedReader::new(&data);
        assert_eq!(
            r.read_vec_u32(1, 8),
            Err(LinkError::Truncated {
                needed: 4,
                available: 3,
            })
        );
    }

    #[test]
    fn slice_at_within_bounds() {
        let data = [0, 1, 2, 3, 4, 5];
        let r = BoundedReader::new(&data);
        assert_eq!(r.slice_at(2, 3).unwrap(), &[2, 3, 4]);
        // slice_at is independent of position.
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn slice_at_exceeding_buffer_returns_bad_offset() {
        let data = [0, 1, 2, 3];
        let r = BoundedReader::new(&data);
        assert_eq!(
            r.slice_at(2, 3),
            Err(LinkError::BadOffset {
                offset: 2,
                len: 3,
                buffer_len: 4,
            })
        );
    }

    #[test]
    fn slice_at_overflowing_returns_bad_offset() {
        let data = [0, 1, 2, 3];
        let r = BoundedReader::new(&data);
        assert_eq!(
            r.slice_at(usize::MAX, 1),
            Err(LinkError::BadOffset {
                offset: usize::MAX,
                len: 1,
                buffer_len: 4,
            })
        );
    }

    #[test]
    fn sub_reader_reads_from_region() {
        let data = [0, 0, 0x2a, 0x00, 0x00, 0x00];
        let r = BoundedReader::new(&data);
        let mut sub = r.sub_reader(2, 4).unwrap();
        assert_eq!(sub.read_u32().unwrap(), 42);
        assert_eq!(sub.remaining(), 0);
    }

    #[test]
    fn sub_reader_out_of_range_returns_bad_offset() {
        let data = [0, 1, 2, 3];
        let r = BoundedReader::new(&data);
        assert_eq!(
            r.sub_reader(3, 5).err(),
            Some(LinkError::BadOffset {
                offset: 3,
                len: 5,
                buffer_len: 4,
            })
        );
    }

    #[test]
    fn position_tracking_across_mixed_reads() {
        let data = [0xaa, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let mut r = BoundedReader::new(&data);
        assert_eq!(r.position(), 0);
        r.read_u8().unwrap();
        assert_eq!(r.position(), 1);
        r.read_u16().unwrap();
        assert_eq!(r.position(), 3);
        r.read_bytes(2).unwrap();
        assert_eq!(r.position(), 5);
        r.read_array::<2>().unwrap();
        assert_eq!(r.position(), 7);
        assert_eq!(r.remaining(), 0);
    }
}
