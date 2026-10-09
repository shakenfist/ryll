//! Cursor channel messages.
use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{self, Cursor};

/// Cursor init message
#[derive(Debug, Clone)]
pub struct CursorInit {
    pub x: u16,
    pub y: u16,
    #[allow(dead_code)]
    pub trail_length: u16,
    #[allow(dead_code)]
    pub trail_frequency: u16,
    pub visible: u8,
}

impl CursorInit {
    pub const SIZE: usize = 9;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for CursorInit",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(CursorInit {
            x: cursor.read_u16::<LittleEndian>()?,
            y: cursor.read_u16::<LittleEndian>()?,
            trail_length: cursor.read_u16::<LittleEndian>()?,
            trail_frequency: cursor.read_u16::<LittleEndian>()?,
            visible: cursor.read_u8()?,
        })
    }
}

/// Cursor set message
#[derive(Debug, Clone)]
pub struct CursorSet {
    pub x: u16,
    pub y: u16,
    pub visible: u8,
}

impl CursorSet {
    pub const SIZE: usize = 5;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for CursorSet",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(CursorSet {
            x: cursor.read_u16::<LittleEndian>()?,
            y: cursor.read_u16::<LittleEndian>()?,
            visible: cursor.read_u8()?,
        })
    }
}

/// SpiceCursor — flags field that precedes the optional SpiceCursorHeader.
///
/// Wire layout:
///   u16 flags
///   [SpiceCursorHeader]  — only present when FLAG_NONE is NOT set
///   [pixel data]         — only present when FLAG_FROM_CACHE is NOT set
#[derive(Debug, Clone)]
pub struct SpiceCursorHeader {
    pub flags: u16,
    pub unique_id: u64,
    pub cursor_type: u8,
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
}

impl SpiceCursorHeader {
    /// Size of just the flags field (always present).
    pub const FLAGS_SIZE: usize = 2;
    /// Size of flags + cursor header (when header is present).
    pub const SIZE: usize = 19;

    pub const FLAG_NONE: u16 = 1 << 0;
    pub const FLAG_CACHE_ME: u16 = 1 << 1;
    pub const FLAG_FROM_CACHE: u16 = 1 << 2;

    /// Read the flags field and, if FLAG_NONE is not set, the full header.
    /// Returns None when FLAG_NONE is set (no cursor data follows).
    pub fn read(data: &[u8]) -> io::Result<Option<Self>> {
        if data.len() < Self::FLAGS_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceCursor flags",
            ));
        }

        let mut cursor = Cursor::new(data);
        let flags = cursor.read_u16::<LittleEndian>()?;

        if flags & Self::FLAG_NONE != 0 {
            return Ok(None);
        }

        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceCursorHeader",
            ));
        }

        Ok(Some(SpiceCursorHeader {
            flags,
            unique_id: cursor.read_u64::<LittleEndian>()?,
            cursor_type: cursor.read_u8()?,
            width: cursor.read_u16::<LittleEndian>()?,
            height: cursor.read_u16::<LittleEndian>()?,
            hot_spot_x: cursor.read_u16::<LittleEndian>()?,
            hot_spot_y: cursor.read_u16::<LittleEndian>()?,
        }))
    }
}
