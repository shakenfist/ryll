//! Display channel messages and the draw types they carry.
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor};

/// Surface create message
#[derive(Debug, Clone)]
pub struct SurfaceCreate {
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub flags: u32,
}

impl SurfaceCreate {
    pub const SIZE: usize = 20;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SurfaceCreate",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(SurfaceCreate {
            surface_id: cursor.read_u32::<LittleEndian>()?,
            width: cursor.read_u32::<LittleEndian>()?,
            height: cursor.read_u32::<LittleEndian>()?,
            format: cursor.read_u32::<LittleEndian>()?,
            flags: cursor.read_u32::<LittleEndian>()?,
        })
    }
}

/// Generic SpiceMsgDisplayBase: surface_id, bounding box, clip.
///
/// Shared by every display draw opcode (DRAW_COPY, DRAW_FILL,
/// COPY_BITS, DRAW_BLACKNESS, …).
#[derive(Debug, Clone)]
pub struct DrawBase {
    pub surface_id: u32,
    pub top: u32,
    pub left: u32,
    pub bottom: u32,
    pub right: u32,
    pub clip_type: u8,
    pub clip_rects: Vec<(u32, u32, u32, u32)>,
    /// Byte offset past this struct (including any variable-length clip rects)
    pub end_offset: usize,
}

impl DrawBase {
    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < 21 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for DrawBase",
            ));
        }

        let mut cursor = Cursor::new(data);
        let surface_id = cursor.read_u32::<LittleEndian>()?;
        let top = cursor.read_u32::<LittleEndian>()?;
        let left = cursor.read_u32::<LittleEndian>()?;
        let bottom = cursor.read_u32::<LittleEndian>()?;
        let right = cursor.read_u32::<LittleEndian>()?;
        let clip_type = cursor.read_u8()?;

        let mut offset = 21usize;
        let mut clip_rects = Vec::new();

        // SPICE_CLIP_TYPE_RECTS = 1: followed by u32 count + count * SpiceRect(16 bytes)
        if clip_type == 1 {
            if data.len() < offset + 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Not enough data for clip rects count",
                ));
            }
            let num_rects = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            let total_rect_bytes = num_rects.checked_mul(16).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "clip rects count overflow")
            })?;
            if data.len() < offset + total_rect_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Not enough data for clip rects",
                ));
            }
            for _ in 0..num_rects {
                let top = u32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]);
                let left = u32::from_le_bytes([
                    data[offset + 4],
                    data[offset + 5],
                    data[offset + 6],
                    data[offset + 7],
                ]);
                let bottom = u32::from_le_bytes([
                    data[offset + 8],
                    data[offset + 9],
                    data[offset + 10],
                    data[offset + 11],
                ]);
                let right = u32::from_le_bytes([
                    data[offset + 12],
                    data[offset + 13],
                    data[offset + 14],
                    data[offset + 15],
                ]);
                clip_rects.push((left, top, right, bottom));
                offset += 16;
            }
        }

        Ok(DrawBase {
            surface_id,
            top,
            left,
            bottom,
            right,
            clip_type,
            clip_rects,
            end_offset: offset,
        })
    }
}

/// 2D point with signed 32-bit coordinates (SpicePoint in draw.h).
///
/// Used both by `SpiceQMask.pos` and by `QXLCopyBits.src_pos`. Both
/// are declared as `int32_t` in the upstream SPICE headers; we
/// preserve the sign in the parser and let call sites handle
/// negatives defensively.
#[derive(Debug, Clone)]
pub struct SpicePoint {
    pub x: i32,
    pub y: i32,
}

impl SpicePoint {
    pub const SIZE: usize = 8;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpicePoint",
            ));
        }

        let mut cursor = Cursor::new(data);
        let x = cursor.read_i32::<LittleEndian>()?;
        let y = cursor.read_i32::<LittleEndian>()?;
        Ok(SpicePoint { x, y })
    }
}

/// Tagged-union brush (SpiceBrush in draw.h).
///
/// Wire format: 1-byte type tag followed by a type-dependent body.
/// * type=0 (NONE): no body.
/// * type=1 (SOLID): u32 colour (BGRX).
/// * type=2 (PATTERN): u64 pat_bitmap_offset + SpicePoint pos (16 bytes).
#[derive(Debug, Clone)]
pub enum SpiceBrush {
    None,
    Solid {
        color: u32,
    },
    Pattern {
        pat_bitmap_offset: u64,
        pos: SpicePoint,
    },
}

impl SpiceBrush {
    /// Parse a brush. Returns the brush and the number of bytes
    /// consumed (1 for the type tag + body size).
    pub fn read(data: &[u8]) -> io::Result<(Self, usize)> {
        if data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceBrush",
            ));
        }

        let brush_type = data[0];
        match brush_type {
            crate::constants::brush::NONE => Ok((SpiceBrush::None, 1)),
            crate::constants::brush::SOLID => {
                if data.len() < 1 + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Not enough data for SpiceBrush",
                    ));
                }
                let mut cursor = Cursor::new(&data[1..]);
                let color = cursor.read_u32::<LittleEndian>()?;
                Ok((SpiceBrush::Solid { color }, 1 + 4))
            }
            crate::constants::brush::PATTERN => {
                if data.len() < 1 + 16 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Not enough data for SpiceBrush",
                    ));
                }
                let mut cursor = Cursor::new(&data[1..]);
                let pat_bitmap_offset = cursor.read_u64::<LittleEndian>()?;
                let pos = SpicePoint::read(&data[1 + 8..1 + 16])?;
                Ok((
                    SpiceBrush::Pattern {
                        pat_bitmap_offset,
                        pos,
                    },
                    1 + 16,
                ))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown SpiceBrush type: {}", other),
            )),
        }
    }
}

/// Optional mask bitmap applied to a draw op (SpiceQMask in draw.h).
///
/// Wire layout: flags (1) + pos (SpicePoint, 8) + bitmap_offset (4) = 13
/// bytes. `bitmap_offset == 0` means the mask is null; the parser
/// simply preserves the offset and leaves interpretation to callers.
#[derive(Debug, Clone)]
pub struct SpiceQMask {
    pub flags: u8,
    pub pos: SpicePoint,
    pub bitmap_offset: u32,
}

impl SpiceQMask {
    pub const SIZE: usize = 13;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceQMask",
            ));
        }

        let flags = data[0];
        let pos = SpicePoint::read(&data[1..9])?;
        let mut cursor = Cursor::new(&data[9..13]);
        let bitmap_offset = cursor.read_u32::<LittleEndian>()?;

        Ok(SpiceQMask {
            flags,
            pos,
            bitmap_offset,
        })
    }
}

/// DRAW_FILL body (SpiceFill in draw.h).
///
/// Wire layout: brush (variable) + rop_descriptor (u16) + mask
/// (SpiceQMask, 13 bytes).
#[derive(Debug, Clone)]
pub struct SpiceFill {
    pub brush: SpiceBrush,
    pub rop_descriptor: u16,
    pub mask: SpiceQMask,
}

impl SpiceFill {
    /// Parse a fill. Returns the struct and total bytes consumed so
    /// the caller can locate any trailing bitmap bytes referenced by
    /// the brush or mask.
    pub fn read(data: &[u8]) -> io::Result<(Self, usize)> {
        let (brush, brush_len) = SpiceBrush::read(data)?;

        let after_brush = brush_len;
        if data.len() < after_brush + 2 + SpiceQMask::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceFill",
            ));
        }

        let mut cursor = Cursor::new(&data[after_brush..after_brush + 2]);
        let rop_descriptor = cursor.read_u16::<LittleEndian>()?;

        let mask_start = after_brush + 2;
        let mask = SpiceQMask::read(&data[mask_start..mask_start + SpiceQMask::SIZE])?;

        let total = mask_start + SpiceQMask::SIZE;
        Ok((
            SpiceFill {
                brush,
                rop_descriptor,
                mask,
            },
            total,
        ))
    }
}

/// DRAW_BLACKNESS body (SpiceBlackness in draw.h).
///
/// DRAW_WHITENESS and DRAW_INVERS share the identical wire payload,
/// so they are provided as type aliases below.
#[derive(Debug, Clone)]
pub struct SpiceBlackness {
    pub mask: SpiceQMask,
}

impl SpiceBlackness {
    pub const SIZE: usize = SpiceQMask::SIZE;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceBlackness",
            ));
        }
        let mask = SpiceQMask::read(&data[..SpiceQMask::SIZE])?;
        Ok(SpiceBlackness { mask })
    }
}

/// Alias for `SpiceBlackness` — DRAW_WHITENESS has the identical
/// wire payload (just a SpiceQMask).
pub type SpiceWhiteness = SpiceBlackness;

/// Alias for `SpiceBlackness` — DRAW_INVERS has the identical wire
/// payload (just a SpiceQMask).
pub type SpiceInvers = SpiceBlackness;

/// DRAW_OPAQUE body (SpiceOpaque in draw.h).
///
/// Wire layout: src_bitmap (u32) + src_area (SpiceRect: 4*u32) +
/// brush (variable) + rop_descriptor (u16) + scale_mode (u8) + mask
/// (13 bytes). `src_bitmap` is a byte offset into the surrounding
/// message payload (same convention as `SpiceCopy.src_bitmap`);
/// image-payload decode is not implemented.
#[derive(Debug, Clone)]
pub struct SpiceOpaque {
    pub src_bitmap: u32,
    pub src_top: u32,
    pub src_left: u32,
    pub src_bottom: u32,
    pub src_right: u32,
    pub brush: SpiceBrush,
    pub rop_descriptor: u16,
    pub scale_mode: u8,
    pub mask: SpiceQMask,
}

impl SpiceOpaque {
    /// Parse an opaque draw. Returns the struct and total bytes
    /// consumed.
    pub fn read(data: &[u8]) -> io::Result<(Self, usize)> {
        // Fixed preamble: src_bitmap (4) + src_area (16) = 20 bytes.
        if data.len() < 20 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceOpaque",
            ));
        }

        let mut cursor = Cursor::new(&data[..20]);
        let src_bitmap = cursor.read_u32::<LittleEndian>()?;
        let src_top = cursor.read_u32::<LittleEndian>()?;
        let src_left = cursor.read_u32::<LittleEndian>()?;
        let src_bottom = cursor.read_u32::<LittleEndian>()?;
        let src_right = cursor.read_u32::<LittleEndian>()?;

        let (brush, brush_len) = SpiceBrush::read(&data[20..])?;
        let after_brush = 20 + brush_len;

        if data.len() < after_brush + 2 + 1 + SpiceQMask::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceOpaque",
            ));
        }

        let mut cursor = Cursor::new(&data[after_brush..after_brush + 3]);
        let rop_descriptor = cursor.read_u16::<LittleEndian>()?;
        let scale_mode = cursor.read_u8()?;

        let mask_start = after_brush + 3;
        let mask = SpiceQMask::read(&data[mask_start..mask_start + SpiceQMask::SIZE])?;

        let total = mask_start + SpiceQMask::SIZE;
        Ok((
            SpiceOpaque {
                src_bitmap,
                src_top,
                src_left,
                src_bottom,
                src_right,
                brush,
                rop_descriptor,
                scale_mode,
                mask,
            },
            total,
        ))
    }
}

/// DRAW_TRANSPARENT body (SpiceTransparent in draw.h).
///
/// Wire layout: src_bitmap (u32) + src_area (4*u32) + src_color
/// (u32, BGRX) + true_color (u32, BGRX). Total 28 bytes.
#[derive(Debug, Clone)]
pub struct SpiceTransparent {
    pub src_bitmap: u32,
    pub src_top: u32,
    pub src_left: u32,
    pub src_bottom: u32,
    pub src_right: u32,
    pub src_color: u32,
    pub true_color: u32,
}

impl SpiceTransparent {
    pub const SIZE: usize = 28;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceTransparent",
            ));
        }

        let mut cursor = Cursor::new(data);
        let src_bitmap = cursor.read_u32::<LittleEndian>()?;
        let src_top = cursor.read_u32::<LittleEndian>()?;
        let src_left = cursor.read_u32::<LittleEndian>()?;
        let src_bottom = cursor.read_u32::<LittleEndian>()?;
        let src_right = cursor.read_u32::<LittleEndian>()?;
        let src_color = cursor.read_u32::<LittleEndian>()?;
        let true_color = cursor.read_u32::<LittleEndian>()?;

        Ok(SpiceTransparent {
            src_bitmap,
            src_top,
            src_left,
            src_bottom,
            src_right,
            src_color,
            true_color,
        })
    }
}

/// DRAW_ALPHA_BLEND body (SpiceAlphaBlend in draw.h).
///
/// Wire layout: alpha_flags (u16) + alpha (u8) + src_bitmap (u32)
/// + src_area (4*u32). Total 23 bytes.
#[derive(Debug, Clone)]
pub struct SpiceAlphaBlend {
    pub alpha_flags: u16,
    pub alpha: u8,
    pub src_bitmap: u32,
    pub src_top: u32,
    pub src_left: u32,
    pub src_bottom: u32,
    pub src_right: u32,
}

impl SpiceAlphaBlend {
    pub const SIZE: usize = 23;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SpiceAlphaBlend",
            ));
        }

        let mut cursor = Cursor::new(data);
        let alpha_flags = cursor.read_u16::<LittleEndian>()?;
        let alpha = cursor.read_u8()?;
        let src_bitmap = cursor.read_u32::<LittleEndian>()?;
        let src_top = cursor.read_u32::<LittleEndian>()?;
        let src_left = cursor.read_u32::<LittleEndian>()?;
        let src_bottom = cursor.read_u32::<LittleEndian>()?;
        let src_right = cursor.read_u32::<LittleEndian>()?;

        Ok(SpiceAlphaBlend {
            alpha_flags,
            alpha,
            src_bitmap,
            src_top,
            src_left,
            src_bottom,
            src_right,
        })
    }
}

/// Image descriptor from draw message
#[derive(Debug, Clone)]
pub struct ImageDescriptor {
    pub image_id: u64,
    pub image_type: u8,
    pub flags: u8,
    pub width: u32,
    pub height: u32,
}

impl ImageDescriptor {
    pub const SIZE: usize = 18;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for ImageDescriptor",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(ImageDescriptor {
            image_id: cursor.read_u64::<LittleEndian>()?,
            image_type: cursor.read_u8()?,
            flags: cursor.read_u8()?,
            width: cursor.read_u32::<LittleEndian>()?,
            height: cursor.read_u32::<LittleEndian>()?,
        })
    }
}

/// Display init message (client -> server)
#[derive(Debug, Clone)]
pub struct DisplayInit {
    pub cache_id: u8,
    pub cache_size: u64,
    pub glz_dict_id: u8,
    pub glz_dict_window: u32,
}

impl DisplayInit {
    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.cache_id)?;
        buf.write_u64::<LittleEndian>(self.cache_size)?;
        buf.write_u8(self.glz_dict_id)?;
        buf.write_u32::<LittleEndian>(self.glz_dict_window)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DrawBase tests ---

    fn draw_copy_base_minimal() -> Vec<u8> {
        let mut data = Vec::new();
        // surface_id = 1 (u32 LE, offset 0)
        data.extend_from_slice(&1u32.to_le_bytes());
        // top = 10 (u32 LE, offset 4)
        data.extend_from_slice(&10u32.to_le_bytes());
        // left = 20 (u32 LE, offset 8)
        data.extend_from_slice(&20u32.to_le_bytes());
        // bottom = 30 (u32 LE, offset 12)
        data.extend_from_slice(&30u32.to_le_bytes());
        // right = 40 (u32 LE, offset 16)
        data.extend_from_slice(&40u32.to_le_bytes());
        // clip_type = 0 (u8, offset 20)
        data.push(0u8);
        data
    }

    #[test]
    fn test_draw_copy_base_minimal_clip_type_zero() {
        let data = draw_copy_base_minimal();
        assert_eq!(data.len(), 21);

        let msg = DrawBase::read(&data).expect("DrawBase minimal read failed");
        assert_eq!(msg.surface_id, 1);
        assert_eq!(msg.top, 10);
        assert_eq!(msg.left, 20);
        assert_eq!(msg.bottom, 30);
        assert_eq!(msg.right, 40);
        assert_eq!(msg.clip_type, 0);
        assert!(msg.clip_rects.is_empty());
        assert_eq!(msg.end_offset, 21);
    }

    #[test]
    fn test_draw_copy_base_with_clip_rects() {
        let mut data = Vec::new();
        // surface_id = 5
        data.extend_from_slice(&5u32.to_le_bytes());
        // top = 0, left = 0, bottom = 100, right = 200
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&100u32.to_le_bytes());
        data.extend_from_slice(&200u32.to_le_bytes());
        // clip_type = 1
        data.push(1u8);
        // num_rects = 2 (u32 LE)
        data.extend_from_slice(&2u32.to_le_bytes());
        // rect 0: top=1, left=2, bottom=3, right=4
        data.extend_from_slice(&1u32.to_le_bytes()); // top
        data.extend_from_slice(&2u32.to_le_bytes()); // left
        data.extend_from_slice(&3u32.to_le_bytes()); // bottom
        data.extend_from_slice(&4u32.to_le_bytes()); // right
                                                     // rect 1: top=5, left=6, bottom=7, right=8
        data.extend_from_slice(&5u32.to_le_bytes()); // top
        data.extend_from_slice(&6u32.to_le_bytes()); // left
        data.extend_from_slice(&7u32.to_le_bytes()); // bottom
        data.extend_from_slice(&8u32.to_le_bytes()); // right

        // Expected: 21 (header) + 4 (count) + 2*16 (rects) = 57 bytes
        assert_eq!(data.len(), 57);

        let msg = DrawBase::read(&data).expect("DrawBase with clip rects failed");
        assert_eq!(msg.surface_id, 5);
        assert_eq!(msg.clip_type, 1);
        assert_eq!(msg.clip_rects.len(), 2);
        // clip_rects are stored as (left, top, right, bottom)
        assert_eq!(msg.clip_rects[0], (2, 1, 4, 3));
        assert_eq!(msg.clip_rects[1], (6, 5, 8, 7));
        assert_eq!(msg.end_offset, 57);
    }

    #[test]
    fn test_draw_copy_base_too_short() {
        let data = vec![0u8; 20]; // one byte short of the 21-byte minimum
        let result = DrawBase::read(&data);
        assert!(
            result.is_err(),
            "Expected error for too-short DrawBase input"
        );
    }

    // --- ImageDescriptor tests ---

    #[test]
    fn test_image_descriptor_valid() {
        let mut data = Vec::new();
        // image_id = 0xDEADBEEFCAFEBABE (u64 LE, offset 0)
        data.extend_from_slice(&0xDEAD_BEEF_CAFE_BABEu64.to_le_bytes());
        // image_type = 3 (u8, offset 8)
        data.push(3u8);
        // flags = 7 (u8, offset 9)
        data.push(7u8);
        // width = 1920 (u32 LE, offset 10)
        data.extend_from_slice(&1920u32.to_le_bytes());
        // height = 1080 (u32 LE, offset 14)
        data.extend_from_slice(&1080u32.to_le_bytes());

        assert_eq!(data.len(), 18);

        let desc = ImageDescriptor::read(&data).expect("ImageDescriptor valid read failed");
        assert_eq!(desc.image_id, 0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(desc.image_type, 3);
        assert_eq!(desc.flags, 7);
        assert_eq!(desc.width, 1920);
        assert_eq!(desc.height, 1080);
    }

    #[test]
    fn test_image_descriptor_too_short() {
        let data = vec![0u8; 17]; // one byte short of the 18-byte minimum
        let result = ImageDescriptor::read(&data);
        assert!(
            result.is_err(),
            "Expected error for too-short ImageDescriptor input"
        );
    }

    // --- SpicePoint tests ---

    #[test]
    fn test_spice_point_valid() {
        let mut data = Vec::new();
        // x = -7 (i32 LE, offset 0)
        data.extend_from_slice(&(-7i32).to_le_bytes());
        // y = 42 (i32 LE, offset 4)
        data.extend_from_slice(&42i32.to_le_bytes());

        assert_eq!(data.len(), 8);

        let pt = SpicePoint::read(&data).expect("SpicePoint valid read failed");
        assert_eq!(pt.x, -7);
        assert_eq!(pt.y, 42);
    }

    #[test]
    fn test_spice_point_too_short() {
        let data = vec![0u8; 7]; // one byte short of the 8-byte minimum
        let result = SpicePoint::read(&data);
        assert!(
            result.is_err(),
            "Expected error for too-short SpicePoint input"
        );
    }

    // --- SpiceBrush tests ---

    #[test]
    fn test_spice_brush_none() {
        // type = 0 (NONE), no body
        let data = vec![0u8];
        let (brush, consumed) = SpiceBrush::read(&data).expect("SpiceBrush NONE read failed");
        assert!(matches!(brush, SpiceBrush::None));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_spice_brush_solid() {
        let mut data = Vec::new();
        // type = 1 (SOLID)
        data.push(1u8);
        // color = 0x11223344 (u32 LE BGRX)
        data.extend_from_slice(&0x1122_3344u32.to_le_bytes());

        assert_eq!(data.len(), 5);

        let (brush, consumed) = SpiceBrush::read(&data).expect("SpiceBrush SOLID read failed");
        match brush {
            SpiceBrush::Solid { color } => assert_eq!(color, 0x1122_3344),
            other => panic!("Expected Solid variant, got {:?}", other),
        }
        assert_eq!(consumed, 5);
    }

    #[test]
    fn test_spice_brush_pattern() {
        let mut data = Vec::new();
        // type = 2 (PATTERN)
        data.push(2u8);
        // pat_bitmap_offset = 0xDEADBEEF (u64 LE)
        data.extend_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        // pos.x = 3, pos.y = -4 (i32 LE each)
        data.extend_from_slice(&3i32.to_le_bytes());
        data.extend_from_slice(&(-4i32).to_le_bytes());

        assert_eq!(data.len(), 17);

        let (brush, consumed) = SpiceBrush::read(&data).expect("SpiceBrush PATTERN read failed");
        match brush {
            SpiceBrush::Pattern {
                pat_bitmap_offset,
                pos,
            } => {
                assert_eq!(pat_bitmap_offset, 0xDEAD_BEEF);
                assert_eq!(pos.x, 3);
                assert_eq!(pos.y, -4);
            }
            other => panic!("Expected Pattern variant, got {:?}", other),
        }
        assert_eq!(consumed, 17);
    }

    #[test]
    fn test_spice_brush_unknown_type() {
        // type = 99 — not 0/1/2
        let data = vec![99u8];
        let result = SpiceBrush::read(&data);
        let err = result.expect_err("Expected InvalidData for unknown brush type");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // --- SpiceQMask tests ---

    #[test]
    fn test_spice_qmask_null() {
        let mut data = Vec::new();
        // flags = 0 (offset 0)
        data.push(0u8);
        // pos.x = 0, pos.y = 0 (offsets 1 and 5)
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        // bitmap_offset = 0 (null, offset 9)
        data.extend_from_slice(&0u32.to_le_bytes());

        assert_eq!(data.len(), 13);

        let mask = SpiceQMask::read(&data).expect("SpiceQMask null read failed");
        assert_eq!(mask.flags, 0);
        assert_eq!(mask.pos.x, 0);
        assert_eq!(mask.pos.y, 0);
        assert_eq!(mask.bitmap_offset, 0);
    }

    #[test]
    fn test_spice_qmask_non_null() {
        let mut data = Vec::new();
        // flags = 1 (INVERS, offset 0)
        data.push(1u8);
        // pos.x = 10, pos.y = 20 (offsets 1 and 5)
        data.extend_from_slice(&10i32.to_le_bytes());
        data.extend_from_slice(&20i32.to_le_bytes());
        // bitmap_offset = 0x1000 (non-null, offset 9)
        data.extend_from_slice(&0x1000u32.to_le_bytes());

        assert_eq!(data.len(), 13);

        let mask = SpiceQMask::read(&data).expect("SpiceQMask non-null read failed");
        assert_eq!(mask.flags, 1);
        assert_eq!(mask.pos.x, 10);
        assert_eq!(mask.pos.y, 20);
        assert_eq!(mask.bitmap_offset, 0x1000);
    }

    #[test]
    fn test_spice_qmask_too_short() {
        let data = vec![0u8; 12]; // one byte short of the 13-byte minimum
        let result = SpiceQMask::read(&data);
        assert!(
            result.is_err(),
            "Expected error for too-short SpiceQMask input"
        );
    }

    // --- SpiceFill tests ---

    #[test]
    fn test_spice_fill_solid_brush() {
        let mut data = Vec::new();
        // brush: type=1 (SOLID), color=0xAABBCCDD — 5 bytes
        data.push(1u8);
        data.extend_from_slice(&0xAABB_CCDDu32.to_le_bytes());
        // rop_descriptor = 0x000C (u16) — 2 bytes
        data.extend_from_slice(&0x000Cu16.to_le_bytes());
        // mask: flags=0, pos=(0,0), bitmap_offset=0 — 13 bytes
        data.push(0u8);
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());

        // Expected: 5 + 2 + 13 = 20 bytes
        assert_eq!(data.len(), 20);

        let (fill, consumed) = SpiceFill::read(&data).expect("SpiceFill SOLID read failed");
        match fill.brush {
            SpiceBrush::Solid { color } => assert_eq!(color, 0xAABB_CCDD),
            other => panic!("Expected Solid brush, got {:?}", other),
        }
        assert_eq!(fill.rop_descriptor, 0x000C);
        assert_eq!(fill.mask.flags, 0);
        assert_eq!(fill.mask.bitmap_offset, 0);
        assert_eq!(consumed, 20);
    }

    #[test]
    fn test_spice_fill_pattern_brush() {
        let mut data = Vec::new();
        // brush: type=2 (PATTERN), pat_bitmap_offset=0x40, pos=(1,2)
        // — 17 bytes
        data.push(2u8);
        data.extend_from_slice(&0x40u64.to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes());
        data.extend_from_slice(&2i32.to_le_bytes());
        // rop_descriptor = 0x00CC — 2 bytes
        data.extend_from_slice(&0x00CCu16.to_le_bytes());
        // mask: flags=1, pos=(5,6), bitmap_offset=0x80 — 13 bytes
        data.push(1u8);
        data.extend_from_slice(&5i32.to_le_bytes());
        data.extend_from_slice(&6i32.to_le_bytes());
        data.extend_from_slice(&0x80u32.to_le_bytes());

        // Expected: 17 + 2 + 13 = 32 bytes
        assert_eq!(data.len(), 32);

        let (fill, consumed) = SpiceFill::read(&data).expect("SpiceFill PATTERN read failed");
        match fill.brush {
            SpiceBrush::Pattern {
                pat_bitmap_offset,
                pos,
            } => {
                assert_eq!(pat_bitmap_offset, 0x40);
                assert_eq!(pos.x, 1);
                assert_eq!(pos.y, 2);
            }
            other => panic!("Expected Pattern brush, got {:?}", other),
        }
        assert_eq!(fill.rop_descriptor, 0x00CC);
        assert_eq!(fill.mask.flags, 1);
        assert_eq!(fill.mask.pos.x, 5);
        assert_eq!(fill.mask.pos.y, 6);
        assert_eq!(fill.mask.bitmap_offset, 0x80);
        assert_eq!(consumed, 32);
    }

    // --- SpiceBlackness tests (shared with Whiteness/Invers aliases) ---

    #[test]
    fn test_spice_blackness_valid() {
        let mut data = Vec::new();
        // mask: flags=0, pos=(7,8), bitmap_offset=0 — 13 bytes
        data.push(0u8);
        data.extend_from_slice(&7i32.to_le_bytes());
        data.extend_from_slice(&8i32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());

        assert_eq!(data.len(), 13);

        let blackness = SpiceBlackness::read(&data).expect("SpiceBlackness valid read failed");
        assert_eq!(blackness.mask.flags, 0);
        assert_eq!(blackness.mask.pos.x, 7);
        assert_eq!(blackness.mask.pos.y, 8);
        assert_eq!(blackness.mask.bitmap_offset, 0);
    }

    #[test]
    fn test_spice_blackness_too_short() {
        // 12 bytes (one short of the 13-byte SpiceQMask body).
        let data = vec![0u8; 12];
        let result = SpiceBlackness::read(&data);
        assert!(
            matches!(result, Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "expected UnexpectedEof, got {:?}",
            result
        );
    }

    // --- SpiceFill too-short --

    #[test]
    fn test_spice_fill_too_short() {
        // Brush::None is a 1-byte tag; rop_descriptor is 2 bytes; mask is
        // 13 bytes. A 10-byte payload (brush + rop + 7 bytes of mask) is
        // short enough that the mask parse must fail.
        let mut data = Vec::new();
        data.push(crate::constants::brush::NONE); // 1
        data.extend_from_slice(&0u16.to_le_bytes()); // 2
        data.extend_from_slice(&[0u8; 7]); // 7 (mask needs 13)

        let result = SpiceFill::read(&data);
        assert!(
            matches!(result, Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "expected UnexpectedEof, got {:?}",
            result
        );
    }

    // --- SpiceOpaque tests ---

    #[test]
    fn test_spice_opaque_solid_brush() {
        let mut data = Vec::new();
        // src_bitmap = 0x100 (offset 0)
        data.extend_from_slice(&0x100u32.to_le_bytes());
        // src_area: top=10, left=20, bottom=30, right=40 (offsets 4..20)
        data.extend_from_slice(&10u32.to_le_bytes());
        data.extend_from_slice(&20u32.to_le_bytes());
        data.extend_from_slice(&30u32.to_le_bytes());
        data.extend_from_slice(&40u32.to_le_bytes());
        // brush: SOLID, colour=0x11223344 — 5 bytes (offsets 20..25)
        data.push(1u8);
        data.extend_from_slice(&0x1122_3344u32.to_le_bytes());
        // rop_descriptor = 0x000C (offset 25..27)
        data.extend_from_slice(&0x000Cu16.to_le_bytes());
        // scale_mode = 0 (offset 27)
        data.push(0u8);
        // mask: flags=0, pos=(0,0), bitmap_offset=0 — 13 bytes (offsets 28..41)
        data.push(0u8);
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());

        // Expected: 4 + 16 + 5 + 2 + 1 + 13 = 41 bytes
        assert_eq!(data.len(), 41);

        let (opaque, consumed) = SpiceOpaque::read(&data).expect("SpiceOpaque SOLID read failed");
        assert_eq!(opaque.src_bitmap, 0x100);
        assert_eq!(opaque.src_top, 10);
        assert_eq!(opaque.src_left, 20);
        assert_eq!(opaque.src_bottom, 30);
        assert_eq!(opaque.src_right, 40);
        match opaque.brush {
            SpiceBrush::Solid { color } => assert_eq!(color, 0x1122_3344),
            other => panic!("Expected Solid brush, got {:?}", other),
        }
        assert_eq!(opaque.rop_descriptor, 0x000C);
        assert_eq!(opaque.scale_mode, 0);
        assert_eq!(opaque.mask.flags, 0);
        assert_eq!(opaque.mask.bitmap_offset, 0);
        assert_eq!(consumed, 41);
    }

    #[test]
    fn test_spice_opaque_too_short() {
        // 19 bytes — shorter than the 20-byte fixed preamble
        // (src_bitmap u32 + src_area 4×u32).
        let data = vec![0u8; 19];
        let result = SpiceOpaque::read(&data);
        assert!(
            matches!(result, Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "expected UnexpectedEof, got {:?}",
            result
        );
    }

    // --- SpiceTransparent tests ---

    #[test]
    fn test_spice_transparent_valid() {
        let mut data = Vec::new();
        // src_bitmap = 0x200 (offset 0)
        data.extend_from_slice(&0x200u32.to_le_bytes());
        // src_area: top=1, left=2, bottom=3, right=4 (offsets 4..20)
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        // src_color = 0xAABBCCDD (offset 20..24)
        data.extend_from_slice(&0xAABB_CCDDu32.to_le_bytes());
        // true_color = 0x11223344 (offset 24..28)
        data.extend_from_slice(&0x1122_3344u32.to_le_bytes());

        assert_eq!(data.len(), 28);

        let t = SpiceTransparent::read(&data).expect("SpiceTransparent valid read failed");
        assert_eq!(t.src_bitmap, 0x200);
        assert_eq!(t.src_top, 1);
        assert_eq!(t.src_left, 2);
        assert_eq!(t.src_bottom, 3);
        assert_eq!(t.src_right, 4);
        assert_eq!(t.src_color, 0xAABB_CCDD);
        assert_eq!(t.true_color, 0x1122_3344);
    }

    #[test]
    fn test_spice_transparent_too_short() {
        let data = vec![0u8; 27]; // one byte short of the 28-byte minimum
        let result = SpiceTransparent::read(&data);
        assert!(
            result.is_err(),
            "Expected error for too-short SpiceTransparent input"
        );
    }

    // --- SpiceAlphaBlend tests ---

    #[test]
    fn test_spice_alpha_blend_valid() {
        let mut data = Vec::new();
        // alpha_flags = 0x0001 (u16, offset 0..2)
        data.extend_from_slice(&0x0001u16.to_le_bytes());
        // alpha = 128 (u8, offset 2)
        data.push(128u8);
        // src_bitmap = 0x300 (u32, offset 3..7)
        data.extend_from_slice(&0x300u32.to_le_bytes());
        // src_area: top=5, left=6, bottom=7, right=8 (offsets 7..23)
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(&6u32.to_le_bytes());
        data.extend_from_slice(&7u32.to_le_bytes());
        data.extend_from_slice(&8u32.to_le_bytes());

        assert_eq!(data.len(), 23);

        let ab = SpiceAlphaBlend::read(&data).expect("SpiceAlphaBlend valid read failed");
        assert_eq!(ab.alpha_flags, 0x0001);
        assert_eq!(ab.alpha, 128);
        assert_eq!(ab.src_bitmap, 0x300);
        assert_eq!(ab.src_top, 5);
        assert_eq!(ab.src_left, 6);
        assert_eq!(ab.src_bottom, 7);
        assert_eq!(ab.src_right, 8);
    }

    #[test]
    fn test_spice_alpha_blend_too_short() {
        let data = vec![0u8; 22]; // one byte short of the 23-byte minimum
        let result = SpiceAlphaBlend::read(&data);
        assert!(
            result.is_err(),
            "Expected error for too-short SpiceAlphaBlend input"
        );
    }
}
