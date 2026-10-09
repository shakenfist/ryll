//! Cursor channel messages.
//!
//! Layouts follow spice-common's `spice.proto`, `channel CursorChannel`
//! and the `Cursor` and `CursorHeader` structs before it. `RESET`, `HIDE`
//! and `INVAL_ALL` have empty bodies and need no type. `TRAIL` is out of
//! scope: no server ryll talks to sends it, and ryll ignores it.
//!
//! spice.proto's `Point16` is signed, so positions are `i16`.

use super::WireType;
use crate::constants::cursor_flags;
use crate::reader::{BoundedReader, LinkError};

/// spice.proto `CursorHeader`: what a cursor shape is. Present in a
/// [`SpiceCursor`] whenever `cursor_flags::NONE` is clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorHeader {
    /// spice.proto `unique`: the cache key for `CACHE_ME` and
    /// `FROM_CACHE`.
    pub unique_id: u64,
    /// A `cursor_type::*` value, kept raw so unknown types survive.
    pub cursor_type: u8,
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
}

impl CursorHeader {
    pub const SIZE: usize = 17;
}

impl WireType for CursorHeader {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(CursorHeader {
            unique_id: r.read_u64()?,
            cursor_type: r.read_u8()?,
            width: r.read_u16()?,
            height: r.read_u16()?,
            hot_spot_x: r.read_u16()?,
            hot_spot_y: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.unique_id.to_le_bytes());
        out.push(self.cursor_type);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.hot_spot_x.to_le_bytes());
        out.extend_from_slice(&self.hot_spot_y.to_le_bytes());
    }
}

/// spice.proto `Cursor`: the cursor carried at the end of `INIT` and
/// `SET`.
///
/// Wire format: `flags` (u16, `cursor_flags::*`); a [`CursorHeader`] unless
/// `cursor_flags::NONE` is set; then the shape data, which runs to the end
/// of the message (`uint8 data[] @as_ptr(data_size)`, outside the flags
/// switch). The data has no length field of its own, so the message body
/// bounds it, and the reader never allocates more than the body holds. Its
/// meaning depends on the header's type and size, which the reader leaves
/// to the caller: a decoder must still size its output from the
/// dimensions with `limits::rgba_len`. spice-server sends no data with
/// `NONE` or `FROM_CACHE`.
///
/// `header` is `Some` exactly when `flags` has `NONE` clear. The reader
/// keeps that invariant; a value built by hand must keep it too, or it will
/// not read back as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiceCursor {
    pub flags: u16,
    pub header: Option<CursorHeader>,
    pub data: Vec<u8>,
}

impl SpiceCursor {
    /// Size of the flags field, which is always present.
    pub const FLAGS_SIZE: usize = 2;

    /// The cursor `INIT` and `SET` carry when there is no cursor shape.
    #[must_use]
    pub fn none() -> Self {
        SpiceCursor {
            flags: cursor_flags::NONE,
            header: None,
            data: Vec::new(),
        }
    }

    /// Whether `flags` has `bit` (a `cursor_flags::*` value) set.
    #[must_use]
    pub fn has_flag(&self, bit: u16) -> bool {
        self.flags & bit != 0
    }
}

impl WireType for SpiceCursor {
    /// Reads the flags, the header when `NONE` is clear, and the rest of
    /// the body as data.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let flags = r.read_u16()?;
        let header = if flags & cursor_flags::NONE == 0 {
            Some(CursorHeader::read(r)?)
        } else {
            None
        };
        let data = r.read_bytes(r.remaining())?.to_vec();
        Ok(SpiceCursor {
            flags,
            header,
            data,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        debug_assert_eq!(
            self.header.is_none(),
            self.has_flag(cursor_flags::NONE),
            "a SpiceCursor has a header exactly when NONE is clear"
        );
        out.extend_from_slice(&self.flags.to_le_bytes());
        if let Some(header) = &self.header {
            header.write(out);
        }
        out.extend_from_slice(&self.data);
    }
}

/// The fixed fields of [`CursorInit`], before its cursor.
///
/// A type of its own so that a caller can read the position even when the
/// cursor after it is malformed: ryll's renderer does, and keeps the
/// position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorInitHead {
    pub x: i16,
    pub y: i16,
    pub trail_length: u16,
    pub trail_frequency: u16,
    pub visible: u8,
}

impl CursorInitHead {
    pub const SIZE: usize = 9;
}

impl WireType for CursorInitHead {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(CursorInitHead {
            x: i16::from_le_bytes(r.read_array()?),
            y: i16::from_le_bytes(r.read_array()?),
            trail_length: r.read_u16()?,
            trail_frequency: r.read_u16()?,
            visible: r.read_u8()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.trail_length.to_le_bytes());
        out.extend_from_slice(&self.trail_frequency.to_le_bytes());
        out.push(self.visible);
    }
}

/// `SPICE_MSG_CURSOR_INIT` (server to client): the cursor's position,
/// trail settings and visibility, then the current cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorInit {
    pub head: CursorInitHead,
    pub cursor: SpiceCursor,
}

impl WireType for CursorInit {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(CursorInit {
            head: CursorInitHead::read(r)?,
            cursor: SpiceCursor::read(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        self.head.write(out);
        self.cursor.write(out);
    }
}

/// The fixed fields of [`CursorSet`], before its cursor. See
/// [`CursorInitHead`] for why it is a type of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSetHead {
    pub x: i16,
    pub y: i16,
    pub visible: u8,
}

impl CursorSetHead {
    pub const SIZE: usize = 5;
}

impl WireType for CursorSetHead {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(CursorSetHead {
            x: i16::from_le_bytes(r.read_array()?),
            y: i16::from_le_bytes(r.read_array()?),
            visible: r.read_u8()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.push(self.visible);
    }
}

/// `SPICE_MSG_CURSOR_SET` (server to client): a new position, visibility
/// and cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSet {
    pub head: CursorSetHead,
    pub cursor: SpiceCursor,
}

impl WireType for CursorSet {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(CursorSet {
            head: CursorSetHead::read(r)?,
            cursor: SpiceCursor::read(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        self.head.write(out);
        self.cursor.write(out);
    }
}

/// `SPICE_MSG_CURSOR_MOVE` (server to client): a new position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorMove {
    pub x: i16,
    pub y: i16,
}

impl CursorMove {
    pub const SIZE: usize = 4;
}

impl WireType for CursorMove {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(CursorMove {
            x: i16::from_le_bytes(r.read_array()?),
            y: i16::from_le_bytes(r.read_array()?),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
    }
}

/// `SPICE_MSG_CURSOR_INVAL_ONE` (server to client): drop one cursor from
/// the client's cache, by its [`CursorHeader::unique_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorInvalOne {
    pub id: u64,
}

impl CursorInvalOne {
    pub const SIZE: usize = 8;
}

impl WireType for CursorInvalOne {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(CursorInvalOne { id: r.read_u64()? })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::cursor_type;
    use crate::messages::assert_round_trip;

    fn header(cursor_type: u8, width: u16, height: u16) -> CursorHeader {
        CursorHeader {
            unique_id: 1,
            cursor_type,
            width,
            height,
            hot_spot_x: 0,
            hot_spot_y: 0,
        }
    }

    /// The bytes ryll's renderer tests built by hand before this type had a
    /// writer (`build_cursor_payload`), laid out from spice.proto `Cursor`.
    fn hand_built_cursor(
        cursor_type: u8,
        width: u16,
        height: u16,
        flags: u16,
        pixel_data: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        // flags(2) + unique(8) + type(1) + width(2) + height(2) + hot x(2) + hot y(2)
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.push(cursor_type);
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(pixel_data);
        buf
    }

    #[test]
    fn cursor_header_round_trips() {
        assert_round_trip(&CursorHeader {
            unique_id: u64::MAX,
            cursor_type: 0xff,
            width: 1,
            height: 2,
            hot_spot_x: 3,
            hot_spot_y: u16::MAX,
        });
    }

    #[test]
    fn spice_cursor_round_trips() {
        assert_round_trip(&SpiceCursor::none());
        // Data after NONE is not what spice-server sends, but it is on the
        // wire, so it survives.
        assert_round_trip(&SpiceCursor {
            flags: cursor_flags::NONE | 0x8000,
            header: None,
            data: vec![1, 2, 3],
        });
        assert_round_trip(&SpiceCursor {
            flags: cursor_flags::CACHE_ME,
            header: Some(header(cursor_type::ALPHA, 2, 1)),
            data: vec![9; 8],
        });
        assert_round_trip(&SpiceCursor {
            flags: cursor_flags::FROM_CACHE,
            header: Some(header(cursor_type::COLOR32, 24, 24)),
            data: Vec::new(),
        });
    }

    #[test]
    fn spice_cursor_writes_what_the_renderer_tests_built_by_hand() {
        let pixels = [0x10, 0x20, 0x30, 0x80, 0x40, 0x50, 0x60, 0xff];
        for (flags, cursor_type) in [
            (0, cursor_type::ALPHA),
            (cursor_flags::CACHE_ME, cursor_type::COLOR24),
            (cursor_flags::FROM_CACHE, cursor_type::COLOR32),
        ] {
            let cursor = SpiceCursor {
                flags,
                header: Some(header(cursor_type, 2, 1)),
                data: pixels.to_vec(),
            };
            let mut out = Vec::new();
            cursor.write(&mut out);
            let expected = hand_built_cursor(cursor_type, 2, 1, flags, &pixels);
            assert_eq!(out, expected);
            assert_eq!(SpiceCursor::decode(&expected).expect("decodes"), cursor);
        }
    }

    #[test]
    fn spice_cursor_decodes_spice_proto_layout() {
        let body = [
            0x02, 0x00, // flags: CACHE_ME
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // unique
            0x06, // type: COLOR32
            0x20, 0x00, 0x10, 0x00, // width 32, height 16
            0x03, 0x00, 0x04, 0x00, // hot spot (3, 4)
            0xaa, 0xbb, // data
        ];
        assert_eq!(
            SpiceCursor::decode(&body).expect("decodes"),
            SpiceCursor {
                flags: cursor_flags::CACHE_ME,
                header: Some(CursorHeader {
                    unique_id: 0x0102_0304_0506_0708,
                    cursor_type: cursor_type::COLOR32,
                    width: 32,
                    height: 16,
                    hot_spot_x: 3,
                    hot_spot_y: 4,
                }),
                data: vec![0xaa, 0xbb],
            }
        );
    }

    #[test]
    fn spice_cursor_with_none_set_has_no_header() {
        // NONE: whatever follows the flags is data, even header-sized
        // bytes.
        let body = hand_built_cursor(cursor_type::ALPHA, 24, 24, cursor_flags::NONE, &[]);
        let cursor = SpiceCursor::decode(&body).expect("decodes");
        assert_eq!(cursor.header, None);
        assert_eq!(cursor.data, &body[2..]);
        assert!(cursor.has_flag(cursor_flags::NONE));
        assert_eq!(
            SpiceCursor::decode(&cursor_flags::NONE.to_le_bytes()).expect("decodes"),
            SpiceCursor::none()
        );
    }

    #[test]
    fn spice_cursor_rejects_short_bodies() {
        assert!(SpiceCursor::decode(&[]).is_err());
        assert!(SpiceCursor::decode(&[0]).is_err());
        // NONE clear, so a 17-byte header must follow.
        let body = hand_built_cursor(cursor_type::ALPHA, 1, 1, 0, &[]);
        assert_eq!(body.len(), SpiceCursor::FLAGS_SIZE + CursorHeader::SIZE);
        assert!(SpiceCursor::decode(&body).is_ok());
        assert!(SpiceCursor::decode(&body[..body.len() - 1]).is_err());
    }

    #[test]
    fn cursor_init_round_trips() {
        assert_round_trip(&CursorInitHead {
            x: -1,
            y: i16::MAX,
            trail_length: 2,
            trail_frequency: 3,
            visible: 1,
        });
        let head = CursorInitHead {
            x: 1,
            y: i16::MIN,
            trail_length: 0,
            trail_frequency: u16::MAX,
            visible: 0,
        };
        assert_round_trip(&CursorInit {
            head: head.clone(),
            cursor: SpiceCursor::none(),
        });
        assert_round_trip(&CursorInit {
            head,
            cursor: SpiceCursor {
                flags: 0,
                header: Some(header(cursor_type::COLOR24, 1, 1)),
                data: vec![1, 2, 3],
            },
        });
    }

    #[test]
    fn cursor_init_decodes_spice_proto_layout() {
        let mut body = vec![
            0x10, 0x00, // x 16
            0xfe, 0xff, // y -2
            0x05, 0x00, // trail_length
            0x06, 0x00, // trail_frequency
            0x01, // visible
        ];
        body.extend_from_slice(&cursor_flags::NONE.to_le_bytes());
        assert_eq!(
            CursorInit::decode(&body).expect("decodes"),
            CursorInit {
                head: CursorInitHead {
                    x: 16,
                    y: -2,
                    trail_length: 5,
                    trail_frequency: 6,
                    visible: 1,
                },
                cursor: SpiceCursor::none(),
            }
        );
        // The cursor's flags are not optional.
        assert!(CursorInit::decode(&body[..CursorInitHead::SIZE]).is_err());
        assert!(CursorInitHead::decode(&body[..CursorInitHead::SIZE]).is_ok());
        assert!(CursorInitHead::decode(&body[..CursorInitHead::SIZE - 1]).is_err());
    }

    #[test]
    fn cursor_set_round_trips() {
        assert_round_trip(&CursorSetHead {
            x: i16::MIN,
            y: 7,
            visible: 0,
        });
        let head = CursorSetHead {
            x: -7,
            y: i16::MAX,
            visible: 1,
        };
        assert_round_trip(&CursorSet {
            head,
            cursor: SpiceCursor {
                flags: cursor_flags::FROM_CACHE,
                header: Some(header(cursor_type::ALPHA, 24, 24)),
                data: Vec::new(),
            },
        });
    }

    #[test]
    fn cursor_set_decodes_spice_proto_layout() {
        let mut body = vec![
            0x34, 0x12, // x 0x1234
            0x00, 0x80, // y i16::MIN
            0x01, // visible
        ];
        body.extend_from_slice(&hand_built_cursor(
            cursor_type::COLOR32,
            1,
            1,
            0,
            &[1, 2, 3, 4],
        ));
        assert_eq!(
            CursorSet::decode(&body).expect("decodes"),
            CursorSet {
                head: CursorSetHead {
                    x: 0x1234,
                    y: i16::MIN,
                    visible: 1,
                },
                cursor: SpiceCursor {
                    flags: 0,
                    header: Some(header(cursor_type::COLOR32, 1, 1)),
                    data: vec![1, 2, 3, 4],
                },
            }
        );
        assert!(CursorSet::decode(&body[..CursorSetHead::SIZE + 1]).is_err());
        assert!(CursorSetHead::decode(&body[..CursorSetHead::SIZE - 1]).is_err());
    }

    #[test]
    fn cursor_move_round_trips_and_decodes() {
        assert_round_trip(&CursorMove { x: -300, y: 300 });
        assert_eq!(
            CursorMove::decode(&[0xd4, 0xfe, 0x2c, 0x01, 0xff]).expect("decodes"),
            CursorMove { x: -300, y: 300 }
        );
        assert!(CursorMove::decode(&[0, 0, 0]).is_err());
    }

    #[test]
    fn cursor_inval_one_round_trips_and_decodes() {
        assert_round_trip(&CursorInvalOne { id: u64::MAX - 1 });
        assert_eq!(
            CursorInvalOne::decode(&[8, 7, 6, 5, 4, 3, 2, 1]).expect("decodes"),
            CursorInvalOne {
                id: 0x0102_0304_0506_0708
            }
        );
        assert!(CursorInvalOne::decode(&[0; 7]).is_err());
    }
}
