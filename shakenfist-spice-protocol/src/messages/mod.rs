//! SPICE protocol message structures and serialization.
//!
//! Messages are grouped by channel into submodules. Every public item is
//! re-exported here, so callers refer to `messages::X` regardless of which
//! submodule defines it.

use crate::reader::{BoundedReader, LinkError};

pub mod agent_stream;
mod common;
mod cursor;
mod display;
mod inputs;
mod main;
pub mod vd_agent;

pub use common::*;
pub use cursor::*;
pub use display::*;
pub use inputs::*;
pub use main::*;

// Size constants follow one convention. `SIZE` is the length of a body
// whose wire size is fixed. `MIN_SIZE` is the floor of a variable-length
// body: its length with every variable part empty. A constant naming one
// part of a body says which part (`HEADER_SIZE`, `FLAGS_SIZE`). `Ping`
// predates the convention: its `SIZE` is the fixed fields before its
// padding.

/// A SPICE message body with a fixed wire layout.
pub trait WireType: Sized {
    /// Parse a body. Trailing bytes are ignored.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError>;
    /// Append the body's wire encoding to `out`.
    ///
    /// Writers are infallible: a length or count is cast to the `u32`,
    /// `u16` or `u8` field the wire gives it, and a value too large for
    /// its field is truncated. Callers must keep bodies within the wire's
    /// limits; some writers `debug_assert!` that they have.
    fn write(&self, out: &mut Vec<u8>);
    /// Parse a whole message body.
    fn decode(body: &[u8]) -> Result<Self, LinkError> {
        Self::read(&mut BoundedReader::new(body))
    }
}

/// Write `value`, read it back, and assert it is unchanged and that the
/// reader consumed exactly the bytes written.
#[cfg(test)]
pub(crate) fn assert_round_trip<T: WireType + PartialEq + std::fmt::Debug>(value: &T) {
    let mut buf = Vec::new();
    value.write(&mut buf);
    let mut reader = BoundedReader::new(&buf);
    let back = T::read(&mut reader).expect("a written value reads back");
    assert_eq!(&back, value);
    assert_eq!(
        reader.position(),
        buf.len(),
        "reader consumed every written byte"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Pair {
        a: u8,
        b: u32,
    }

    impl WireType for Pair {
        fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
            Ok(Pair {
                a: r.read_u8()?,
                b: r.read_u32()?,
            })
        }

        fn write(&self, out: &mut Vec<u8>) {
            out.push(self.a);
            out.extend_from_slice(&self.b.to_le_bytes());
        }
    }

    #[test]
    fn assert_round_trip_accepts_a_faithful_type() {
        assert_round_trip(&Pair {
            a: 7,
            b: 0x0102_0304,
        });
    }

    #[test]
    fn decode_ignores_trailing_bytes_and_rejects_short_bodies() {
        let pair = Pair::decode(&[7, 4, 3, 2, 1, 99]).expect("decodes");
        assert_eq!(
            pair,
            Pair {
                a: 7,
                b: 0x0102_0304
            }
        );
        assert!(Pair::decode(&[7, 4]).is_err());
    }
}
