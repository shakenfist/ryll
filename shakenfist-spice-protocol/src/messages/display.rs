//! Display channel messages and the draw types they carry.
//!
//! Layouts follow spice-common's `spice.proto`, `channel DisplayChannel`
//! and the structs before it. spice.proto's `Rect` and `Point` are
//! signed, so their fields are `i32`; ryll's renderer has always used the
//! same bits as `u32`, and casts at its call sites.
//!
//! Images inside a draw message are addressed by pointers, which are
//! offsets from the start of the message body. [`DrawCopy`] holds its
//! images resolved; [`SpiceCopy`] is the same body as it is on the wire.
//!
//! The draw bodies other than DRAW_COPY's and DRAW_BLEND's (`SpiceFill`,
//! `SpiceOpaque` and the rest) are still `io::Result` readers over a slice;
//! moving them onto [`BoundedReader`] is shakenfist/ryll#136.
use super::WireType;
use crate::constants::{bitmap_flags, clip_type, image_scale_mode, ropd, ImageType};
use crate::reader::{BoundedReader, LinkError};
use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{self, Cursor};

fn read_i32(r: &mut BoundedReader<'_>) -> Result<i32, LinkError> {
    Ok(i32::from_le_bytes(r.read_array()?))
}

/// Refuse a server-supplied element count before reserving room for it:
/// `count` elements of `size` bytes must fit in what is left of the body.
fn check_count(
    r: &BoundedReader<'_>,
    what: &'static str,
    count: usize,
    size: usize,
) -> Result<(), LinkError> {
    let room = r.remaining() / size;
    if count > room {
        return Err(LinkError::TooLarge {
            what,
            value: count,
            max: room,
        });
    }
    Ok(())
}

/// Read a `u32` length, then that many bytes (spice.proto's
/// `uint32 data_size; uint8 data[data_size]`).
fn read_sized_bytes(r: &mut BoundedReader<'_>) -> Result<Vec<u8>, LinkError> {
    let size = r.read_u32()? as usize;
    Ok(r.read_bytes(size)?.to_vec())
}

fn write_sized_bytes(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

/// spice.proto `Rect`: four signed edges, in wire order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

impl Rect {
    pub const SIZE: usize = 16;
}

impl WireType for Rect {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(Rect {
            top: read_i32(r)?,
            left: read_i32(r)?,
            bottom: read_i32(r)?,
            right: read_i32(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [self.top, self.left, self.bottom, self.right] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// spice.proto `Clip`: a `clip_type::*` byte, then, for `RECTS` only, a
/// `u32` count and that many [`Rect`]s inline.
///
/// `rects` is empty unless `clip_type` is `RECTS`. The reader keeps that
/// invariant; a value built by hand must keep it too, or it will not read
/// back as written. An unknown clip type is kept, with no rectangles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Clip {
    pub clip_type: u8,
    pub rects: Vec<Rect>,
}

impl Clip {
    /// No clipping (`clip_type::NONE`).
    #[must_use]
    pub fn none() -> Self {
        Clip::default()
    }

    /// Clip to `rects` (`clip_type::RECTS`).
    #[must_use]
    pub fn rects(rects: Vec<Rect>) -> Self {
        Clip {
            clip_type: clip_type::RECTS,
            rects,
        }
    }
}

impl WireType for Clip {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let clip_type = r.read_u8()?;
        let mut rects = Vec::new();
        if clip_type == clip_type::RECTS {
            let count = r.read_u32()? as usize;
            check_count(r, "num_rects", count, Rect::SIZE)?;
            rects.reserve(count);
            for _ in 0..count {
                rects.push(Rect::read(r)?);
            }
        }
        Ok(Clip { clip_type, rects })
    }

    fn write(&self, out: &mut Vec<u8>) {
        debug_assert!(
            self.clip_type == clip_type::RECTS || self.rects.is_empty(),
            "only a RECTS clip carries rectangles"
        );
        out.push(self.clip_type);
        if self.clip_type == clip_type::RECTS {
            out.extend_from_slice(&(self.rects.len() as u32).to_le_bytes());
            for rect in &self.rects {
                rect.write(out);
            }
        }
    }
}

/// spice.proto `DisplayBase` (`SpiceMsgDisplayBase`): the surface, bounding
/// box and clip that open every draw message (DRAW_COPY, DRAW_FILL,
/// COPY_BITS, DRAW_BLACKNESS, ...).
///
/// The draw-specific body starts where the reader stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawBase {
    pub surface_id: u32,
    /// spice.proto's `box`.
    pub bbox: Rect,
    pub clip: Clip,
}

impl DrawBase {
    /// The size with no clip rectangles.
    pub const MIN_SIZE: usize = 4 + Rect::SIZE + 1;
}

impl WireType for DrawBase {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(DrawBase {
            surface_id: r.read_u32()?,
            bbox: Rect::read(r)?,
            clip: Clip::read(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.surface_id.to_le_bytes());
        self.bbox.write(out);
        self.clip.write(out);
    }
}

/// 2D point with signed 32-bit coordinates (spice.proto `Point`).
///
/// Used both by `SpiceQMask.pos` and by COPY_BITS's `src_pos`. Both are
/// declared as `int32_t` in the upstream SPICE headers; we preserve the
/// sign in the parser and let call sites handle negatives defensively.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpicePoint {
    pub x: i32,
    pub y: i32,
}

impl SpicePoint {
    pub const SIZE: usize = 8;
}

impl WireType for SpicePoint {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(SpicePoint {
            x: read_i32(r)?,
            y: read_i32(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
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
                let pos = SpicePoint::decode(&data[1 + 8..1 + 16])?;
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

/// Optional mask bitmap applied to a draw op (spice.proto `QMask`).
///
/// Wire layout: flags (1, `mask_flags::*`) + pos (SpicePoint, 8) +
/// bitmap_offset (4) = 13 bytes. `bitmap_offset` is an offset from the
/// start of the message body, and 0 means the mask is null; the reader
/// preserves it and leaves interpretation to callers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpiceQMask {
    pub flags: u8,
    pub pos: SpicePoint,
    pub bitmap_offset: u32,
}

impl SpiceQMask {
    pub const SIZE: usize = 13;
}

impl WireType for SpiceQMask {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(SpiceQMask {
            flags: r.read_u8()?,
            pos: SpicePoint::read(r)?,
            bitmap_offset: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        self.pos.write(out);
        out.extend_from_slice(&self.bitmap_offset.to_le_bytes());
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
        let mask = SpiceQMask::decode(&data[mask_start..mask_start + SpiceQMask::SIZE])?;

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
        let mask = SpiceQMask::decode(&data[..SpiceQMask::SIZE])?;
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
        let mask = SpiceQMask::decode(&data[mask_start..mask_start + SpiceQMask::SIZE])?;

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

/// spice.proto `ImageDescriptor`: the 18 bytes that open every image.
///
/// `image_type` is an [`ImageType`] value, kept raw so that an unknown
/// type survives a round trip. `flags` holds `IMAGE_FLAGS_*` bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDescriptor {
    pub image_id: u64,
    pub image_type: u8,
    pub flags: u8,
    pub width: u32,
    pub height: u32,
}

impl ImageDescriptor {
    pub const SIZE: usize = 18;
}

impl WireType for ImageDescriptor {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(ImageDescriptor {
            image_id: r.read_u64()?,
            image_type: r.read_u8()?,
            flags: r.read_u8()?,
            width: r.read_u32()?,
            height: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.image_id.to_le_bytes());
        out.push(self.image_type);
        out.push(self.flags);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
    }
}

/// The palette field of a [`BitmapHeader`] (spice.proto's anonymous `pal`
/// switch in `BitmapData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapPalette {
    /// A null palette pointer: the bitmap has no palette. 32-bit and RGBA
    /// bitmaps never have one.
    None,
    /// A pointer to a spice.proto `Palette`, as an offset from the start
    /// of the message body. The reader never produces `Offset(0)`, which
    /// is written as, and reads back as, [`BitmapPalette::None`].
    Offset(u32),
    /// `bitmap_flags::PAL_FROM_CACHE` is set: the id of a palette the
    /// client cached earlier. The field is then a `u64` on the wire, not a
    /// pointer.
    FromCache(u64),
}

/// spice.proto `BitmapData` up to its pixels: the header of a `BITMAP`
/// (pixmap) image.
///
/// The header is 18 bytes, or 22 when `flags` has
/// `bitmap_flags::PAL_FROM_CACHE`, because the palette field is then a
/// `u64` cache id rather than a `u32` pointer. `palette` must agree with
/// that flag, or the value will not read back as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapHeader {
    /// A `bitmap_fmt::*` value.
    pub format: u8,
    /// `bitmap_flags::*` bits.
    pub flags: u8,
    /// Width in pixels (spice.proto's `x`).
    pub x: u32,
    /// Height in rows (spice.proto's `y`).
    pub y: u32,
    /// Bytes from the start of one row to the start of the next.
    pub stride: u32,
    pub palette: BitmapPalette,
}

impl BitmapHeader {
    /// The size with a palette pointer, which is all ryll's pixmaps.
    pub const SIZE: usize = 18;

    /// The pixel data's length in bytes, `stride * y` (spice.proto's
    /// `image_size(8, stride, y)`), or `None` if that overflows `usize`.
    #[must_use]
    pub fn data_len(&self) -> Option<usize> {
        (self.stride as usize).checked_mul(self.y as usize)
    }
}

impl WireType for BitmapHeader {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let format = r.read_u8()?;
        let flags = r.read_u8()?;
        let x = r.read_u32()?;
        let y = r.read_u32()?;
        let stride = r.read_u32()?;
        let palette = if flags & bitmap_flags::PAL_FROM_CACHE != 0 {
            BitmapPalette::FromCache(r.read_u64()?)
        } else {
            match r.read_u32()? {
                0 => BitmapPalette::None,
                offset => BitmapPalette::Offset(offset),
            }
        };
        Ok(BitmapHeader {
            format,
            flags,
            x,
            y,
            stride,
            palette,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        debug_assert_eq!(
            self.flags & bitmap_flags::PAL_FROM_CACHE != 0,
            matches!(self.palette, BitmapPalette::FromCache(_)),
            "PAL_FROM_CACHE and a FromCache palette go together"
        );
        out.push(self.format);
        out.push(self.flags);
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.stride.to_le_bytes());
        match self.palette {
            BitmapPalette::None => out.extend_from_slice(&0u32.to_le_bytes()),
            BitmapPalette::Offset(offset) => out.extend_from_slice(&offset.to_le_bytes()),
            BitmapPalette::FromCache(id) => out.extend_from_slice(&id.to_le_bytes()),
        }
    }
}

/// spice.proto `BitmapData`: a [`BitmapHeader`], then `stride * y` bytes
/// of pixel rows.
///
/// `data` must be [`BitmapHeader::data_len`] bytes long, or the value will
/// not read back as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapPayload {
    pub header: BitmapHeader,
    pub data: Vec<u8>,
}

impl WireType for BitmapPayload {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let header = BitmapHeader::read(r)?;
        let len = header.data_len().ok_or(LinkError::TooLarge {
            what: "bitmap data",
            value: usize::MAX,
            max: r.remaining(),
        })?;
        let data = r.read_bytes(len)?.to_vec();
        Ok(BitmapPayload { header, data })
    }

    fn write(&self, out: &mut Vec<u8>) {
        debug_assert_eq!(
            Some(self.data.len()),
            self.header.data_len(),
            "bitmap data is stride * y bytes"
        );
        self.header.write(out);
        out.extend_from_slice(&self.data);
    }
}

/// spice.proto `BinaryData`: a `u32` size, then that many bytes. JPEG
/// images carry one, as do QUIC, LZ, GLZ and LZ4 images.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BinaryData {
    pub data: Vec<u8>,
}

impl BinaryData {
    /// Read the size and return the data it covers without copying it.
    /// [`BinaryData::read`] is this plus the copy.
    ///
    /// # Errors
    ///
    /// [`LinkError::Truncated`] if the size or the data runs past the end
    /// of `r`.
    pub fn read_data<'a>(r: &mut BoundedReader<'a>) -> Result<&'a [u8], LinkError> {
        let size = r.read_u32()? as usize;
        r.read_bytes(size)
    }
}

impl WireType for BinaryData {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(BinaryData {
            data: BinaryData::read_data(r)?.to_vec(),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        write_sized_bytes(out, &self.data);
    }
}

/// What follows an [`ImageDescriptor`], by its `image_type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePayload {
    /// `ImageType::Pixmap` (spice.proto `BITMAP`).
    Bitmap(BitmapPayload),
    /// `ImageType::Jpeg`.
    Jpeg(BinaryData),
    /// `ImageType::FromCache` or `ImageType::FromCacheLossless`, which
    /// spice.proto gives no data: the descriptor's `image_id` names an
    /// image the client cached earlier.
    FromCache,
    /// Any other type, whose layout this crate does not model (QUIC,
    /// LZ_RGB, GLZ_RGB, LZ_PLT, SURFACE, ZLIB_GLZ_RGB, JPEG_ALPHA, LZ4 and
    /// unknown types): every byte from the end of the descriptor to the end
    /// of the image's region. In a draw message the region ends at the
    /// next image or the end of the body; see [`DrawCopy`].
    Other(Vec<u8>),
}

impl ImagePayload {
    /// Whether this payload is the one spice.proto gives `image_type`.
    #[must_use]
    pub fn matches_type(&self, image_type: u8) -> bool {
        let modelled = match ImageType::from_u8(image_type) {
            Some(ImageType::Pixmap) => Some(matches!(self, ImagePayload::Bitmap(_))),
            Some(ImageType::Jpeg) => Some(matches!(self, ImagePayload::Jpeg(_))),
            Some(ImageType::FromCache | ImageType::FromCacheLossless) => {
                Some(matches!(self, ImagePayload::FromCache))
            }
            _ => None,
        };
        modelled.unwrap_or(matches!(self, ImagePayload::Other(_)))
    }
}

/// spice.proto `Image`: a descriptor and the payload its type selects.
///
/// The payload must be the one [`ImagePayload::matches_type`] accepts for
/// `descriptor.image_type`, or the value will not read back as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiceImage {
    pub descriptor: ImageDescriptor,
    pub payload: ImagePayload,
}

impl WireType for SpiceImage {
    /// Parse an image. An [`ImagePayload::Other`] takes every byte left in
    /// `r`, so read it from a reader bounded to the image, as
    /// [`DrawCopy::read`] does.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let descriptor = ImageDescriptor::read(r)?;
        let payload = match ImageType::from_u8(descriptor.image_type) {
            Some(ImageType::Pixmap) => ImagePayload::Bitmap(BitmapPayload::read(r)?),
            Some(ImageType::Jpeg) => ImagePayload::Jpeg(BinaryData::read(r)?),
            Some(ImageType::FromCache | ImageType::FromCacheLossless) => ImagePayload::FromCache,
            _ => ImagePayload::Other(r.read_bytes(r.remaining())?.to_vec()),
        };
        Ok(SpiceImage {
            descriptor,
            payload,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        debug_assert!(
            self.payload.matches_type(self.descriptor.image_type),
            "the payload is the one the descriptor's type selects"
        );
        self.descriptor.write(out);
        match &self.payload {
            ImagePayload::Bitmap(bitmap) => bitmap.write(out),
            ImagePayload::Jpeg(jpeg) => jpeg.write(out),
            ImagePayload::FromCache => {}
            ImagePayload::Other(bytes) => out.extend_from_slice(bytes),
        }
    }
}

/// The bytes an image pointer in a draw message addresses: from `offset`
/// to the next pointer after it in `pointers`, or to the end of `body`.
///
/// `body` is the whole message body, which offsets are measured from, and
/// `fixed_len` the length of the message's fixed fields. A null (0)
/// pointer gives `None`.
///
/// spice-server appends each pointee after the fixed fields, in pointer
/// order (`marshaller.c`, `spice_marshaller_get_ptr_submarshaller`), so
/// one image's bytes end where the next begins.
fn pointee<'a>(
    body: &'a [u8],
    fixed_len: usize,
    offset: u32,
    pointers: &[u32],
) -> Result<Option<BoundedReader<'a>>, LinkError> {
    if offset == 0 {
        return Ok(None);
    }
    let offset = offset as usize;
    if offset < fixed_len {
        return Err(LinkError::PointerIntoFixedPart { offset, fixed_len });
    }
    let end = pointers
        .iter()
        .map(|&p| p as usize)
        .filter(|&p| p > offset)
        .min()
        .unwrap_or(body.len())
        .min(body.len());
    let len = end.saturating_sub(offset);
    BoundedReader::new(body).sub_reader(offset, len).map(Some)
}

/// spice.proto `Copy`, the body of DRAW_COPY after its [`DrawBase`], as it
/// is on the wire. DRAW_BLEND's `Blend` is the same struct.
///
/// `src_bitmap` and `mask.bitmap_offset` point at images, as offsets from
/// the start of the message body, with 0 for null. [`DrawCopy`] resolves
/// them; this type is for a reader that needs to see each step, as ryll's
/// renderer does, and is read and resolved by the same code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpiceCopy {
    pub src_bitmap: u32,
    pub src_area: Rect,
    /// `ropd::*` bits.
    pub rop_descriptor: u16,
    /// An `image_scale_mode::*` value.
    pub scale_mode: u8,
    pub mask: SpiceQMask,
}

impl SpiceCopy {
    pub const SIZE: usize = 4 + Rect::SIZE + 2 + 1 + SpiceQMask::SIZE;

    /// A reader over the source image's bytes, or `None` if `src_bitmap`
    /// is null. The bytes run to the mask image, if that comes next, or to
    /// the end of `body`.
    ///
    /// `body` is the whole message body and `fixed_len` the length of the
    /// [`DrawBase`] and this struct, which is where the reader that read
    /// them stopped.
    ///
    /// # Errors
    ///
    /// [`LinkError::PointerIntoFixedPart`] if the offset is inside the
    /// fixed fields, and [`LinkError::BadOffset`] if it is past the end of
    /// `body`.
    pub fn src_bitmap_reader<'a>(
        &self,
        body: &'a [u8],
        fixed_len: usize,
    ) -> Result<Option<BoundedReader<'a>>, LinkError> {
        pointee(body, fixed_len, self.src_bitmap, &self.pointers())
    }

    /// As [`SpiceCopy::src_bitmap_reader`], for the mask image.
    ///
    /// # Errors
    ///
    /// As [`SpiceCopy::src_bitmap_reader`].
    pub fn mask_bitmap_reader<'a>(
        &self,
        body: &'a [u8],
        fixed_len: usize,
    ) -> Result<Option<BoundedReader<'a>>, LinkError> {
        pointee(body, fixed_len, self.mask.bitmap_offset, &self.pointers())
    }

    fn pointers(&self) -> [u32; 2] {
        [self.src_bitmap, self.mask.bitmap_offset]
    }
}

impl WireType for SpiceCopy {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(SpiceCopy {
            src_bitmap: r.read_u32()?,
            src_area: Rect::read(r)?,
            rop_descriptor: r.read_u16()?,
            scale_mode: r.read_u8()?,
            mask: SpiceQMask::read(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.src_bitmap.to_le_bytes());
        self.src_area.write(out);
        out.extend_from_slice(&self.rop_descriptor.to_le_bytes());
        out.push(self.scale_mode);
        self.mask.write(out);
    }
}

/// spice.proto `QMask` with its image resolved. [`SpiceQMask`] is the same
/// struct as it is on the wire, with the image as an offset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QMask {
    /// `mask_flags::*` bits.
    pub flags: u8,
    pub pos: SpicePoint,
    pub bitmap: Option<SpiceImage>,
}

/// `SPICE_MSG_DISPLAY_DRAW_COPY`, with its images resolved. DRAW_BLEND has
/// the same layout.
///
/// Offsets exist only on the wire. The reader follows them and checks
/// that each lies after the fixed fields and within the body; the writer
/// lays the images out after the fixed fields, source first and then the
/// mask, as spice-server does (`dcc-send.cpp`,
/// `red_marshall_qxl_draw_copy`), and points at them. So a value read from
/// any accepted layout writes out in that canonical one and reads back
/// unchanged.
///
/// Read it from a reader over the whole message body, since offsets are
/// measured from the start of the reader's buffer. An image whose payload
/// is [`ImagePayload::Other`] runs to the next image or the end of the
/// body, so unlike most readers this one does not ignore trailing bytes
/// after such an image: they are part of it.
///
/// A bitmap with a palette pointer is refused with
/// [`LinkError::Unsupported`], because palettes are not modelled. ryll
/// draws only 32-bit bitmaps, which have none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawCopy {
    pub base: DrawBase,
    pub src_bitmap: Option<SpiceImage>,
    pub src_area: Rect,
    /// `ropd::*` bits.
    pub rop_descriptor: u16,
    /// An `image_scale_mode::*` value.
    pub scale_mode: u8,
    pub mask: QMask,
}

/// Read the image at a pointee, returning it and the offset just past it.
fn read_pointee(
    region: Option<BoundedReader<'_>>,
    offset: u32,
) -> Result<Option<(SpiceImage, usize)>, LinkError> {
    let Some(mut region) = region else {
        return Ok(None);
    };
    let image = SpiceImage::read(&mut region)?;
    if let ImagePayload::Bitmap(bitmap) = &image.payload {
        if matches!(bitmap.header.palette, BitmapPalette::Offset(_)) {
            return Err(LinkError::Unsupported {
                what: "bitmap palette pointer",
            });
        }
    }
    Ok(Some((image, offset as usize + region.position())))
}

impl WireType for DrawCopy {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let body = r.slice_at(0, r.position() + r.remaining())?;
        let base = DrawBase::read(r)?;
        let copy = SpiceCopy::read(r)?;
        let fixed_len = r.position();

        let src = read_pointee(copy.src_bitmap_reader(body, fixed_len)?, copy.src_bitmap)?;
        let mask = read_pointee(
            copy.mask_bitmap_reader(body, fixed_len)?,
            copy.mask.bitmap_offset,
        )?;

        // Leave the reader after the last byte any image used.
        let end = [&src, &mask]
            .iter()
            .filter_map(|image| image.as_ref().map(|(_, end)| *end))
            .fold(fixed_len, usize::max);
        r.read_bytes(end - fixed_len)?;

        Ok(DrawCopy {
            base,
            src_bitmap: src.map(|(image, _)| image),
            src_area: copy.src_area,
            rop_descriptor: copy.rop_descriptor,
            scale_mode: copy.scale_mode,
            mask: QMask {
                flags: copy.mask.flags,
                pos: copy.mask.pos,
                bitmap: mask.map(|(image, _)| image),
            },
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        DrawCopyBuilder {
            base: &self.base,
            src_bitmap: self.src_bitmap.as_ref(),
            src_area: self.src_area,
            rop_descriptor: self.rop_descriptor,
            scale_mode: self.scale_mode,
            mask_flags: self.mask.flags,
            mask_pos: self.mask.pos,
            mask_bitmap: self.mask.bitmap.as_ref(),
        }
        .write(out);
    }
}

/// Builds a DRAW_COPY or DRAW_BLEND body from borrowed parts, so that a
/// sender need not copy its pixels into a [`DrawCopy`] first.
///
/// It lays the body out in two passes, as spice-server's marshaller does:
/// the [`DrawBase`] and [`SpiceCopy`] with null image pointers, then the
/// source image, then the mask image, and finally each pointer patched to
/// its image's offset from the start of the body. [`DrawCopy::write`] is
/// this builder, so both produce the same bytes for the same values.
#[derive(Debug, Clone, Copy)]
pub struct DrawCopyBuilder<'a> {
    base: &'a DrawBase,
    src_bitmap: Option<&'a SpiceImage>,
    src_area: Rect,
    rop_descriptor: u16,
    scale_mode: u8,
    mask_flags: u8,
    mask_pos: SpicePoint,
    mask_bitmap: Option<&'a SpiceImage>,
}

impl<'a> DrawCopyBuilder<'a> {
    /// Copy `src_area` of `src_bitmap` to `base`'s box, with `OP_PUT`,
    /// `INTERPOLATE` scaling and no mask.
    #[must_use]
    pub fn new(base: &'a DrawBase, src_bitmap: &'a SpiceImage, src_area: Rect) -> Self {
        DrawCopyBuilder {
            base,
            src_bitmap: Some(src_bitmap),
            src_area,
            rop_descriptor: ropd::OP_PUT,
            scale_mode: image_scale_mode::INTERPOLATE,
            mask_flags: 0,
            mask_pos: SpicePoint::default(),
            mask_bitmap: None,
        }
    }

    /// Set the `ropd::*` bits.
    #[must_use]
    pub fn rop_descriptor(mut self, rop_descriptor: u16) -> Self {
        self.rop_descriptor = rop_descriptor;
        self
    }

    /// Set the `image_scale_mode::*` value.
    #[must_use]
    pub fn scale_mode(mut self, scale_mode: u8) -> Self {
        self.scale_mode = scale_mode;
        self
    }

    /// Set the mask: its `mask_flags::*` bits, position and image.
    #[must_use]
    pub fn mask(mut self, flags: u8, pos: SpicePoint, bitmap: Option<&'a SpiceImage>) -> Self {
        self.mask_flags = flags;
        self.mask_pos = pos;
        self.mask_bitmap = bitmap;
        self
    }

    /// Append the body to `out`. Offsets are measured from where it starts.
    pub fn write(&self, out: &mut Vec<u8>) {
        let start = out.len();
        self.base.write(out);
        let copy_at = out.len();
        SpiceCopy {
            src_bitmap: 0,
            src_area: self.src_area,
            rop_descriptor: self.rop_descriptor,
            scale_mode: self.scale_mode,
            mask: SpiceQMask {
                flags: self.mask_flags,
                pos: self.mask_pos,
                bitmap_offset: 0,
            },
        }
        .write(out);

        let src_bitmap = append_pointee(out, start, self.src_bitmap);
        let mask_bitmap = append_pointee(out, start, self.mask_bitmap);
        patch_u32(out, copy_at, src_bitmap);
        patch_u32(out, copy_at + SpiceCopy::SIZE - 4, mask_bitmap);
    }

    /// The body as a new buffer.
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }
}

/// Append `image`, if any, and return its offset from `start`, or 0 for
/// none. A message body's length is a `u32` on the wire, so an offset
/// within one fits.
fn append_pointee(out: &mut Vec<u8>, start: usize, image: Option<&SpiceImage>) -> u32 {
    let Some(image) = image else {
        return 0;
    };
    let offset = (out.len() - start) as u32;
    image.write(out);
    offset
}

/// Overwrite the placeholder `u32` at `at`, which the caller wrote.
fn patch_u32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// `SPICE_MSG_DISPLAY_SURFACE_CREATE` (server to client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceCreate {
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    /// A `surface_fmt::*` value.
    pub format: u32,
    /// `SPICE_SURFACE_FLAGS_*`: bit 0 is PRIMARY.
    pub flags: u32,
}

impl SurfaceCreate {
    pub const SIZE: usize = 20;
}

impl WireType for SurfaceCreate {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(SurfaceCreate {
            surface_id: r.read_u32()?,
            width: r.read_u32()?,
            height: r.read_u32()?,
            format: r.read_u32()?,
            flags: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [
            self.surface_id,
            self.width,
            self.height,
            self.format,
            self.flags,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// `SPICE_MSG_DISPLAY_SURFACE_DESTROY` (server to client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceDestroy {
    pub surface_id: u32,
}

impl WireType for SurfaceDestroy {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(SurfaceDestroy {
            surface_id: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.surface_id.to_le_bytes());
    }
}

/// One head of a [`DisplayMonitorsConfig`] (spice.proto `Head`). Unlike
/// the vdagent's monitor positions, spice.proto makes `x` and `y`
/// unsigned here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayHead {
    pub monitor_id: u32,
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub flags: u32,
}

impl DisplayHead {
    pub const SIZE: usize = 28;
}

impl WireType for DisplayHead {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(DisplayHead {
            monitor_id: r.read_u32()?,
            surface_id: r.read_u32()?,
            width: r.read_u32()?,
            height: r.read_u32()?,
            x: r.read_u32()?,
            y: r.read_u32()?,
            flags: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [
            self.monitor_id,
            self.surface_id,
            self.width,
            self.height,
            self.x,
            self.y,
            self.flags,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// `SPICE_MSG_DISPLAY_MONITORS_CONFIG` (server to client): a `u16` count,
/// a `u16` `max_allowed`, then that many [`DisplayHead`]s.
///
/// Not the guest agent's monitors message, which is
/// [`vd_agent::MonitorsConfig`](super::vd_agent::MonitorsConfig).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMonitorsConfig {
    pub max_allowed: u16,
    /// At most `u16::MAX` heads, the most the count can say.
    pub heads: Vec<DisplayHead>,
}

impl WireType for DisplayMonitorsConfig {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let count = r.read_u16()? as usize;
        let max_allowed = r.read_u16()?;
        check_count(r, "monitors_config count", count, DisplayHead::SIZE)?;
        let mut heads = Vec::with_capacity(count);
        for _ in 0..count {
            heads.push(DisplayHead::read(r)?);
        }
        Ok(DisplayMonitorsConfig { max_allowed, heads })
    }

    fn write(&self, out: &mut Vec<u8>) {
        debug_assert!(self.heads.len() <= u16::MAX as usize);
        out.extend_from_slice(&(self.heads.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.max_allowed.to_le_bytes());
        for head in &self.heads {
            head.write(out);
        }
    }
}

/// `SPICE_MSG_DISPLAY_STREAM_CREATE` (server to client): a video stream
/// drawn into `dest` on a surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCreate {
    pub surface_id: u32,
    pub id: u32,
    /// `stream_flags::*`.
    pub flags: u8,
    /// A `SPICE_VIDEO_CODEC_TYPE_*` value.
    pub codec_type: u8,
    pub stamp: u64,
    pub stream_width: u32,
    pub stream_height: u32,
    pub src_width: u32,
    pub src_height: u32,
    pub dest: Rect,
    pub clip: Clip,
}

impl StreamCreate {
    /// The size with a clip of type NONE.
    pub const MIN_SIZE: usize = 51;
}

impl WireType for StreamCreate {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamCreate {
            surface_id: r.read_u32()?,
            id: r.read_u32()?,
            flags: r.read_u8()?,
            codec_type: r.read_u8()?,
            stamp: r.read_u64()?,
            stream_width: r.read_u32()?,
            stream_height: r.read_u32()?,
            src_width: r.read_u32()?,
            src_height: r.read_u32()?,
            dest: Rect::read(r)?,
            clip: Clip::read(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.surface_id.to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.push(self.flags);
        out.push(self.codec_type);
        out.extend_from_slice(&self.stamp.to_le_bytes());
        for v in [
            self.stream_width,
            self.stream_height,
            self.src_width,
            self.src_height,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        self.dest.write(out);
        self.clip.write(out);
    }
}

/// spice.proto `StreamDataHeader`: which stream a frame belongs to, and
/// when to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDataHeader {
    pub id: u32,
    pub multi_media_time: u32,
}

impl WireType for StreamDataHeader {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamDataHeader {
            id: r.read_u32()?,
            multi_media_time: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.multi_media_time.to_le_bytes());
    }
}

/// `SPICE_MSG_DISPLAY_STREAM_DATA` (server to client): one encoded frame,
/// as a `u32` size and that many bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamData {
    pub base: StreamDataHeader,
    pub data: Vec<u8>,
}

impl WireType for StreamData {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamData {
            base: StreamDataHeader::read(r)?,
            data: read_sized_bytes(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        self.base.write(out);
        write_sized_bytes(out, &self.data);
    }
}

/// `SPICE_MSG_DISPLAY_STREAM_DATA_SIZED` (server to client): a frame that
/// also carries its size and destination, for a stream whose size
/// changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDataSized {
    pub base: StreamDataHeader,
    pub width: u32,
    pub height: u32,
    pub dest: Rect,
    pub data: Vec<u8>,
}

impl WireType for StreamDataSized {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamDataSized {
            base: StreamDataHeader::read(r)?,
            width: r.read_u32()?,
            height: r.read_u32()?,
            dest: Rect::read(r)?,
            data: read_sized_bytes(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        self.base.write(out);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        self.dest.write(out);
        write_sized_bytes(out, &self.data);
    }
}

/// `SPICE_MSG_DISPLAY_STREAM_CLIP` (server to client): a stream's new
/// clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamClip {
    pub id: u32,
    pub clip: Clip,
}

impl WireType for StreamClip {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamClip {
            id: r.read_u32()?,
            clip: Clip::read(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        self.clip.write(out);
    }
}

/// `SPICE_MSG_DISPLAY_STREAM_DESTROY` (server to client).
/// `STREAM_DESTROY_ALL` has an empty body and needs no type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDestroy {
    pub id: u32,
}

impl WireType for StreamDestroy {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamDestroy { id: r.read_u32()? })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
    }
}

/// `SPICE_MSG_DISPLAY_STREAM_ACTIVATE_REPORT` (server to client): start
/// sending [`StreamReport`]s for a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamActivateReport {
    pub stream_id: u32,
    pub unique_id: u32,
    pub max_window_size: u32,
    pub timeout_ms: u32,
}

impl WireType for StreamActivateReport {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamActivateReport {
            stream_id: r.read_u32()?,
            unique_id: r.read_u32()?,
            max_window_size: r.read_u32()?,
            timeout_ms: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [
            self.stream_id,
            self.unique_id,
            self.max_window_size,
            self.timeout_ms,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// `SPICE_MSGC_DISPLAY_INIT` (client to server): the client's pixmap cache
/// and GLZ dictionary. spice.proto makes both sizes signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInit {
    pub cache_id: u8,
    /// In pixels.
    pub cache_size: i64,
    pub glz_dict_id: u8,
    /// In pixels.
    pub glz_dict_window: i32,
}

impl WireType for DisplayInit {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(DisplayInit {
            cache_id: r.read_u8()?,
            cache_size: i64::from_le_bytes(r.read_array()?),
            glz_dict_id: r.read_u8()?,
            glz_dict_window: read_i32(r)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.cache_id);
        out.extend_from_slice(&self.cache_size.to_le_bytes());
        out.push(self.glz_dict_id);
        out.extend_from_slice(&self.glz_dict_window.to_le_bytes());
    }
}

/// `SPICE_MSGC_DISPLAY_STREAM_REPORT` (client to server): how a stream's
/// frames have been arriving.
///
/// `num_frames == 0` with `num_drops == u32::MAX` says the client cannot
/// decode the stream, and `audio_delay == u32::MAX` says there is no audio
/// playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReport {
    pub stream_id: u32,
    pub unique_id: u32,
    pub start_frame_mm_time: u32,
    pub end_frame_mm_time: u32,
    pub num_frames: u32,
    pub num_drops: u32,
    pub last_frame_delay: i32,
    pub audio_delay: u32,
}

impl StreamReport {
    pub const SIZE: usize = 32;
}

impl WireType for StreamReport {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(StreamReport {
            stream_id: r.read_u32()?,
            unique_id: r.read_u32()?,
            start_frame_mm_time: r.read_u32()?,
            end_frame_mm_time: r.read_u32()?,
            num_frames: r.read_u32()?,
            num_drops: r.read_u32()?,
            last_frame_delay: read_i32(r)?,
            audio_delay: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [
            self.stream_id,
            self.unique_id,
            self.start_frame_mm_time,
            self.end_frame_mm_time,
            self.num_frames,
            self.num_drops,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.last_frame_delay.to_le_bytes());
        out.extend_from_slice(&self.audio_delay.to_le_bytes());
    }
}

/// `SPICE_MSGC_DISPLAY_PREFERRED_COMPRESSION` (client to server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferredCompression {
    /// An `image_compression::*` value.
    pub image_compression: u8,
}

impl WireType for PreferredCompression {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(PreferredCompression {
            image_compression: r.read_u8()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.image_compression);
    }
}

/// `SPICE_MSGC_DISPLAY_PREFERRED_VIDEO_CODEC_TYPE` (client to server): a
/// `u8` count, then that many `SPICE_VIDEO_CODEC_TYPE_*` bytes, most
/// preferred first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferredVideoCodecType {
    /// At most 255, the most the count can say.
    pub codecs: Vec<u8>,
}

impl WireType for PreferredVideoCodecType {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let count = r.read_u8()? as usize;
        Ok(PreferredVideoCodecType {
            codecs: r.read_bytes(count)?.to_vec(),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        debug_assert!(self.codecs.len() <= u8::MAX as usize);
        out.push(self.codecs.len() as u8);
        out.extend_from_slice(&self.codecs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{bitmap_fmt, mask_flags};
    use crate::messages::assert_round_trip;

    fn le32(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn rect(top: i32, left: i32, bottom: i32, right: i32) -> Rect {
        Rect {
            top,
            left,
            bottom,
            right,
        }
    }

    // --- Geometry and DrawBase tests ---

    #[test]
    fn rect_and_clip_round_trip() {
        assert_round_trip(&Rect {
            top: -1,
            left: i32::MIN,
            bottom: i32::MAX,
            right: 0,
        });
        assert_round_trip(&Clip::none());
        assert_round_trip(&Clip::rects(Vec::new()));
        assert_round_trip(&Clip::rects(vec![rect(1, 2, 3, 4), rect(-5, -6, 7, 8)]));
        // An unknown clip type survives, and carries no rectangles.
        assert_round_trip(&Clip {
            clip_type: 9,
            rects: Vec::new(),
        });
    }

    #[test]
    fn clip_count_beyond_body_is_error() {
        let mut data = vec![clip_type::RECTS];
        data.extend_from_slice(&u32::MAX.to_le_bytes());
        data.extend_from_slice(&[0u8; 16]);
        assert_eq!(
            Clip::decode(&data),
            Err(LinkError::TooLarge {
                what: "num_rects",
                value: u32::MAX as usize,
                max: 1,
            })
        );
    }

    #[test]
    fn draw_base_round_trips() {
        assert_round_trip(&DrawBase {
            surface_id: 1,
            bbox: rect(10, 20, 30, 40),
            clip: Clip::none(),
        });
        assert_round_trip(&DrawBase {
            surface_id: u32::MAX,
            bbox: rect(-10, -20, 30, 40),
            clip: Clip::rects(vec![rect(1, 2, 3, 4)]),
        });
    }

    #[test]
    fn draw_base_decodes_spice_proto_layout() {
        // spice.proto DisplayBase: uint32 surface_id; Rect box (top, left,
        // bottom, right, each int32); Clip (type, then for RECTS a uint32
        // count and the rects).
        let mut data = le32(&[5, 0, 0, 100, 200]);
        data.push(clip_type::RECTS);
        data.extend_from_slice(&le32(&[2, 1, 2, 3, 4]));
        data.extend_from_slice(&(-5i32).to_le_bytes());
        data.extend_from_slice(&le32(&[6, 7, 8]));
        data.push(0xff); // the draw body starts here
        assert_eq!(data.len(), 58);

        let mut r = BoundedReader::new(&data);
        let base = DrawBase::read(&mut r).expect("decodes");
        assert_eq!(
            base,
            DrawBase {
                surface_id: 5,
                bbox: rect(0, 0, 100, 200),
                clip: Clip::rects(vec![rect(1, 2, 3, 4), rect(-5, 6, 7, 8)]),
            }
        );
        assert_eq!(r.position(), 57, "the reader stops where the body starts");

        let mut minimal = le32(&[1, 10, 20, 30, 40]);
        minimal.push(clip_type::NONE);
        assert_eq!(minimal.len(), DrawBase::MIN_SIZE);
        assert_eq!(
            DrawBase::decode(&minimal).expect("decodes").bbox,
            rect(10, 20, 30, 40)
        );
        assert!(DrawBase::decode(&minimal[..20]).is_err());
        // A RECTS clip whose rectangles are cut short.
        assert!(DrawBase::decode(&data[..56]).is_err());
    }

    // --- SpicePoint and SpiceQMask tests ---

    #[test]
    fn spice_point_and_qmask_round_trip() {
        assert_round_trip(&SpicePoint { x: -7, y: 42 });
        assert_round_trip(&SpiceQMask {
            flags: 1,
            pos: SpicePoint { x: 10, y: -20 },
            bitmap_offset: 0x1000,
        });
    }

    #[test]
    fn spice_point_decodes_signed_coordinates() {
        let mut data = (-7i32).to_le_bytes().to_vec();
        data.extend_from_slice(&42i32.to_le_bytes());
        assert_eq!(
            SpicePoint::decode(&data).expect("decodes"),
            SpicePoint { x: -7, y: 42 }
        );
        assert!(SpicePoint::decode(&data[..7]).is_err());
    }

    #[test]
    fn spice_qmask_decodes_spice_proto_layout() {
        // flags (1) + pos (2 x int32) + bitmap offset (uint32) = 13 bytes.
        let mut data = vec![1u8];
        data.extend_from_slice(&le32(&[10, 20, 0x1000]));
        assert_eq!(
            SpiceQMask::decode(&data).expect("decodes"),
            SpiceQMask {
                flags: 1,
                pos: SpicePoint { x: 10, y: 20 },
                bitmap_offset: 0x1000,
            }
        );
        assert!(SpiceQMask::decode(&data[..12]).is_err());
    }

    // --- Surface and monitors tests ---

    #[test]
    fn surface_messages_round_trip() {
        assert_round_trip(&SurfaceCreate {
            surface_id: 0,
            width: 1024,
            height: 768,
            format: crate::constants::surface_fmt::FMT_32_XRGB,
            flags: 1,
        });
        assert_round_trip(&SurfaceDestroy { surface_id: 3 });
    }

    #[test]
    fn surface_create_decodes_spice_proto_layout() {
        let data = le32(&[0, 1024, 768, 32, 1]);
        assert_eq!(
            SurfaceCreate::decode(&data).expect("decodes"),
            SurfaceCreate {
                surface_id: 0,
                width: 1024,
                height: 768,
                format: 32,
                flags: 1,
            }
        );
        assert!(SurfaceCreate::decode(&data[..19]).is_err());
        assert_eq!(
            SurfaceDestroy::decode(&[7, 0, 0, 0]),
            Ok(SurfaceDestroy { surface_id: 7 })
        );
        assert!(SurfaceDestroy::decode(&[7, 0, 0]).is_err());
    }

    fn head(monitor_id: u32) -> DisplayHead {
        DisplayHead {
            monitor_id,
            surface_id: 0,
            width: 1920,
            height: 1080,
            x: 1920 * monitor_id,
            y: 0,
            flags: 0,
        }
    }

    #[test]
    fn monitors_config_round_trips() {
        assert_round_trip(&DisplayHead { y: 10, ..head(1) });
        assert_round_trip(&DisplayMonitorsConfig {
            max_allowed: 4,
            heads: Vec::new(),
        });
        assert_round_trip(&DisplayMonitorsConfig {
            max_allowed: 4,
            heads: vec![head(0), head(1)],
        });
    }

    #[test]
    fn monitors_config_decodes_spice_proto_layout() {
        // uint16 count; uint16 max_allowed; Head (seven uint32s) * count.
        let mut data = vec![1, 0, 4, 0];
        data.extend_from_slice(&le32(&[0, 0, 1920, 1080, 0, 0, 0]));
        assert_eq!(
            DisplayMonitorsConfig::decode(&data).expect("decodes"),
            DisplayMonitorsConfig {
                max_allowed: 4,
                heads: vec![head(0)],
            }
        );
        // A count the body cannot hold is refused before reserving.
        data[0] = 2;
        assert!(matches!(
            DisplayMonitorsConfig::decode(&data),
            Err(LinkError::TooLarge { .. })
        ));
    }

    // --- Stream tests ---

    fn stream_create() -> StreamCreate {
        StreamCreate {
            surface_id: 0,
            id: 7,
            flags: crate::constants::stream_flags::TOP_DOWN,
            codec_type: 1,
            stamp: 0x0102_0304_0506_0708,
            stream_width: 640,
            stream_height: 480,
            src_width: 1280,
            src_height: 960,
            dest: rect(10, 20, 490, 660),
            clip: Clip::none(),
        }
    }

    #[test]
    fn stream_messages_round_trip() {
        assert_round_trip(&stream_create());
        assert_round_trip(&StreamCreate {
            clip: Clip::rects(vec![rect(10, 20, 100, 200)]),
            ..stream_create()
        });
        assert_round_trip(&StreamDataHeader {
            id: u32::MAX,
            multi_media_time: 0,
        });
        let base = StreamDataHeader {
            id: 7,
            multi_media_time: 12345,
        };
        assert_round_trip(&StreamData {
            base: base.clone(),
            data: vec![0xff, 0xd8, 0xff, 0xd9],
        });
        assert_round_trip(&StreamData {
            base: base.clone(),
            data: Vec::new(),
        });
        assert_round_trip(&StreamDataSized {
            base,
            width: 320,
            height: 240,
            dest: rect(0, 0, 240, 320),
            data: vec![1, 2, 3],
        });
        assert_round_trip(&StreamClip {
            id: 7,
            clip: Clip::rects(vec![rect(1, 2, 3, 4)]),
        });
        assert_round_trip(&StreamDestroy { id: 7 });
        assert_round_trip(&StreamActivateReport {
            stream_id: 7,
            unique_id: 9,
            max_window_size: 30,
            timeout_ms: 1000,
        });
    }

    #[test]
    fn stream_create_decodes_spice_proto_layout() {
        // surface_id, id (uint32 each); flags, codec_type (uint8 each);
        // stamp (uint64); stream and src sizes (four uint32s); dest Rect;
        // Clip.
        let mut data = le32(&[0, 7]);
        data.extend_from_slice(&[1, 1]);
        data.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        data.extend_from_slice(&le32(&[640, 480, 1280, 960, 10, 20, 490, 660]));
        data.push(clip_type::NONE);
        assert_eq!(data.len(), StreamCreate::MIN_SIZE);
        assert_eq!(
            StreamCreate::decode(&data).expect("decodes"),
            stream_create()
        );
        // Ryll's renderer used to accept 50 bytes, stopping before the
        // clip; spice.proto makes the clip type part of the message.
        assert!(StreamCreate::decode(&data[..50]).is_err());
    }

    #[test]
    fn stream_data_decodes_spice_proto_layout() {
        // StreamDataHeader (id, multi_media_time); uint32 data_size; data.
        let mut data = le32(&[7, 12345, 3]);
        data.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]); // one trailing byte
        let frame = StreamData::decode(&data).expect("decodes");
        assert_eq!(frame.base.id, 7);
        assert_eq!(frame.base.multi_media_time, 12345);
        assert_eq!(frame.data, vec![0xaa, 0xbb, 0xcc]);
        // A data_size running past the body is refused.
        assert!(StreamData::decode(&data[..14]).is_err());

        // ... plus uint32 width, height and a dest Rect before data_size.
        let mut sized = le32(&[7, 12345, 320, 240, 0, 0, 240, 320, 2]);
        sized.extend_from_slice(&[0xaa, 0xbb]);
        let frame = StreamDataSized::decode(&sized).expect("decodes");
        assert_eq!((frame.width, frame.height), (320, 240));
        assert_eq!(frame.dest, rect(0, 0, 240, 320));
        assert_eq!(frame.data, vec![0xaa, 0xbb]);
        assert!(StreamDataSized::decode(&sized[..37]).is_err());
    }

    #[test]
    fn small_stream_messages_decode_spice_proto_layout() {
        let mut clip = le32(&[7]);
        clip.push(clip_type::NONE);
        assert_eq!(
            StreamClip::decode(&clip),
            Ok(StreamClip {
                id: 7,
                clip: Clip::none(),
            })
        );
        assert!(StreamClip::decode(&clip[..4]).is_err());
        assert_eq!(
            StreamDestroy::decode(&le32(&[7])),
            Ok(StreamDestroy { id: 7 })
        );
        let report = le32(&[7, 9, 30, 1000]);
        assert_eq!(
            StreamActivateReport::decode(&report),
            Ok(StreamActivateReport {
                stream_id: 7,
                unique_id: 9,
                max_window_size: 30,
                timeout_ms: 1000,
            })
        );
        assert!(StreamActivateReport::decode(&report[..15]).is_err());
    }

    // --- Client message tests ---

    #[test]
    fn client_messages_round_trip() {
        assert_round_trip(&DisplayInit {
            cache_id: 1,
            cache_size: -1,
            glz_dict_id: 1,
            glz_dict_window: i32::MIN,
        });
        assert_round_trip(&StreamReport {
            stream_id: 0x1111_2222,
            unique_id: 0xdead_beef,
            start_frame_mm_time: 100,
            end_frame_mm_time: 200,
            num_frames: 5,
            num_drops: 1,
            last_frame_delay: -42,
            audio_delay: u32::MAX,
        });
        assert_round_trip(&PreferredCompression {
            image_compression: crate::constants::image_compression::AUTO_GLZ,
        });
        assert_round_trip(&PreferredVideoCodecType { codecs: Vec::new() });
        assert_round_trip(&PreferredVideoCodecType { codecs: vec![3, 1] });
    }

    /// The bytes ryll's display channel sends at link-up, laid out from
    /// spice.proto's client messages.
    #[test]
    fn client_messages_write_spice_proto_layout() {
        let mut init = Vec::new();
        DisplayInit {
            cache_id: 1,
            cache_size: 20 * 1024 * 1024,
            glz_dict_id: 1,
            glz_dict_window: 3 * 1024 * 1024,
        }
        .write(&mut init);
        // uint8 id; int64 size; uint8 id; int32 window.
        assert_eq!(
            init,
            vec![1, 0x00, 0x00, 0x40, 0x01, 0, 0, 0, 0, 1, 0x00, 0x00, 0x30, 0x00]
        );
        assert_eq!(
            DisplayInit::decode(&init).expect("decodes").cache_size,
            20 * 1024 * 1024
        );

        let mut report = Vec::new();
        StreamReport {
            stream_id: 0x1111_2222,
            unique_id: 0xdead_beef,
            start_frame_mm_time: 100,
            end_frame_mm_time: 200,
            num_frames: 5,
            num_drops: 1,
            last_frame_delay: -42,
            audio_delay: u32::MAX,
        }
        .write(&mut report);
        let mut expected = le32(&[0x1111_2222, 0xdead_beef, 100, 200, 5, 1]);
        expected.extend_from_slice(&(-42i32).to_le_bytes());
        expected.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(report, expected);
        assert_eq!(report.len(), StreamReport::SIZE);

        let mut codecs = Vec::new();
        PreferredVideoCodecType { codecs: vec![3, 1] }.write(&mut codecs);
        assert_eq!(codecs, vec![2, 3, 1]);
        assert!(PreferredVideoCodecType::decode(&[2, 3]).is_err());
    }

    // --- Image and DRAW_COPY tests ---

    fn descriptor(image_type: ImageType, id: u64) -> ImageDescriptor {
        ImageDescriptor {
            image_id: id,
            image_type: image_type as u8,
            flags: 0,
            width: 2,
            height: 2,
        }
    }

    /// A top-down 32-bit 2x2 bitmap whose rows are padded to 12 bytes.
    fn bitmap_image(id: u64) -> SpiceImage {
        SpiceImage {
            descriptor: descriptor(ImageType::Pixmap, id),
            payload: ImagePayload::Bitmap(BitmapPayload {
                header: BitmapHeader {
                    format: bitmap_fmt::BIT32,
                    flags: bitmap_flags::TOP_DOWN,
                    x: 2,
                    y: 2,
                    stride: 12,
                    palette: BitmapPalette::None,
                },
                data: (1..=24).collect(),
            }),
        }
    }

    fn jpeg_image(id: u64) -> SpiceImage {
        SpiceImage {
            descriptor: descriptor(ImageType::Jpeg, id),
            payload: ImagePayload::Jpeg(BinaryData {
                data: vec![0xFF, 0xD8, 0xFF, 0xD9],
            }),
        }
    }

    fn from_cache_image(id: u64) -> SpiceImage {
        SpiceImage {
            descriptor: descriptor(ImageType::FromCache, id),
            payload: ImagePayload::FromCache,
        }
    }

    /// An LZ4 image, which this crate does not model: its bytes are kept
    /// as they are.
    fn other_image(id: u64) -> SpiceImage {
        SpiceImage {
            descriptor: descriptor(ImageType::Lz4, id),
            payload: ImagePayload::Other(vec![1, 8, 0, 0, 0, 3, 0xAA, 0xBB, 0xCC]),
        }
    }

    fn draw_copy(src_bitmap: Option<SpiceImage>, mask_bitmap: Option<SpiceImage>) -> DrawCopy {
        DrawCopy {
            base: DrawBase {
                surface_id: 0,
                bbox: rect(10, 20, 12, 22),
                clip: Clip::rects(vec![rect(10, 20, 11, 22)]),
            },
            src_bitmap,
            src_area: rect(0, 0, 2, 2),
            rop_descriptor: ropd::OP_PUT,
            scale_mode: image_scale_mode::NEAREST,
            mask: QMask {
                flags: mask_flags::INVERS,
                pos: SpicePoint { x: -1, y: 1 },
                bitmap: mask_bitmap,
            },
        }
    }

    #[test]
    fn image_descriptor_round_trips_and_decodes_spice_proto_layout() {
        assert_round_trip(&ImageDescriptor {
            image_id: u64::MAX,
            image_type: 200,
            flags: 0xFF,
            width: 1,
            height: u32::MAX,
        });

        let mut data = Vec::new();
        data.extend_from_slice(&0xDEAD_BEEF_CAFE_BABEu64.to_le_bytes()); // id
        data.push(3); // type
        data.push(7); // flags
        data.extend_from_slice(&le32(&[1920, 1080])); // width, height
        assert_eq!(data.len(), ImageDescriptor::SIZE);
        assert_eq!(
            ImageDescriptor::decode(&data).unwrap(),
            ImageDescriptor {
                image_id: 0xDEAD_BEEF_CAFE_BABE,
                image_type: 3,
                flags: 7,
                width: 1920,
                height: 1080,
            }
        );
        assert!(ImageDescriptor::decode(&data[..17]).is_err());
    }

    fn bitmap_header(flags: u8, palette: BitmapPalette) -> BitmapHeader {
        BitmapHeader {
            format: bitmap_fmt::BIT8,
            flags,
            x: 3,
            y: 2,
            stride: 4,
            palette,
        }
    }

    #[test]
    fn bitmap_header_round_trips_each_palette() {
        assert_round_trip(&bitmap_header(bitmap_flags::TOP_DOWN, BitmapPalette::None));
        assert_round_trip(&bitmap_header(0, BitmapPalette::Offset(99)));
        assert_round_trip(&bitmap_header(
            bitmap_flags::PAL_FROM_CACHE,
            BitmapPalette::FromCache(u64::MAX),
        ));
        assert_round_trip(&BitmapHeader {
            format: 200,
            flags: !bitmap_flags::PAL_FROM_CACHE,
            x: u32::MAX,
            y: u32::MAX,
            stride: u32::MAX,
            palette: BitmapPalette::None,
        });
    }

    #[test]
    fn bitmap_header_decodes_spice_proto_layout() {
        // format, flags, x, y, stride, then a u32 palette pointer.
        let mut data = vec![bitmap_fmt::BIT32, bitmap_flags::TOP_DOWN];
        data.extend_from_slice(&le32(&[3, 2, 12, 0]));
        assert_eq!(data.len(), BitmapHeader::SIZE);
        let header = BitmapHeader::decode(&data).unwrap();
        assert_eq!(
            header,
            BitmapHeader {
                format: bitmap_fmt::BIT32,
                flags: bitmap_flags::TOP_DOWN,
                x: 3,
                y: 2,
                stride: 12,
                palette: BitmapPalette::None,
            }
        );
        assert_eq!(header.data_len(), Some(24));
        assert!(BitmapHeader::decode(&data[..17]).is_err());

        data[14..18].copy_from_slice(&40u32.to_le_bytes());
        assert_eq!(
            BitmapHeader::decode(&data).unwrap().palette,
            BitmapPalette::Offset(40)
        );

        // PAL_FROM_CACHE makes the palette field a u64 cache id, so the
        // header is 22 bytes.
        let mut cached = vec![bitmap_fmt::BIT8, bitmap_flags::PAL_FROM_CACHE];
        cached.extend_from_slice(&le32(&[3, 2, 4]));
        cached.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        let mut r = BoundedReader::new(&cached);
        assert_eq!(
            BitmapHeader::read(&mut r).unwrap().palette,
            BitmapPalette::FromCache(0x0102_0304_0506_0708)
        );
        assert_eq!(r.position(), 22);
        assert!(BitmapHeader::decode(&cached[..21]).is_err());
    }

    #[test]
    fn bitmap_payload_round_trips_and_reads_stride_times_rows() {
        let ImagePayload::Bitmap(bitmap) = bitmap_image(1).payload else {
            unreachable!()
        };
        assert_round_trip(&bitmap);
        assert_round_trip(&BitmapPayload {
            header: bitmap_header(0, BitmapPalette::None),
            data: vec![7; 8],
        });

        // The data is stride * y bytes; anything after it is not the
        // bitmap's.
        let mut data = Vec::new();
        bitmap_header(0, BitmapPalette::None).write(&mut data);
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let mut r = BoundedReader::new(&data);
        let bitmap = BitmapPayload::read(&mut r).unwrap();
        assert_eq!(bitmap.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.remaining(), 1);
        assert_eq!(
            BitmapPayload::decode(&data[..BitmapHeader::SIZE + 7]),
            Err(LinkError::Truncated {
                needed: 8,
                available: 7,
            })
        );
    }

    #[test]
    fn binary_data_round_trips_and_decodes_spice_proto_layout() {
        assert_round_trip(&BinaryData::default());
        assert_round_trip(&BinaryData {
            data: vec![1, 2, 3],
        });

        let data = [2, 0, 0, 0, 0xAB, 0xCD, 0xEF];
        let mut r = BoundedReader::new(&data);
        assert_eq!(BinaryData::read_data(&mut r).unwrap(), &[0xAB, 0xCD]);
        assert_eq!(r.remaining(), 1);
        assert_eq!(
            BinaryData::decode(&data[..5]),
            Err(LinkError::Truncated {
                needed: 2,
                available: 1,
            })
        );
    }

    #[test]
    fn spice_image_round_trips_each_payload() {
        assert_round_trip(&bitmap_image(1));
        assert_round_trip(&jpeg_image(2));
        assert_round_trip(&from_cache_image(3));
        assert_round_trip(&SpiceImage {
            descriptor: descriptor(ImageType::FromCacheLossless, 4),
            payload: ImagePayload::FromCache,
        });
        assert_round_trip(&other_image(5));
        assert_round_trip(&SpiceImage {
            descriptor: ImageDescriptor {
                image_type: 250,
                ..descriptor(ImageType::Pixmap, 6)
            },
            payload: ImagePayload::Other(Vec::new()),
        });
    }

    #[test]
    fn spice_image_decodes_spice_proto_layout() {
        // A JPEG: descriptor, then BinaryData.
        let mut data = Vec::new();
        data.extend_from_slice(&9u64.to_le_bytes());
        data.extend_from_slice(&[ImageType::Jpeg as u8, 0]);
        data.extend_from_slice(&le32(&[2, 2, 4]));
        data.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xD9]);
        assert_eq!(SpiceImage::decode(&data).unwrap(), jpeg_image(9));

        // A cache hit is only its descriptor; the bytes after it are not
        // the image's.
        let mut data = Vec::new();
        descriptor(ImageType::FromCache, 3).write(&mut data);
        data.push(0xEE);
        let mut r = BoundedReader::new(&data);
        assert_eq!(SpiceImage::read(&mut r).unwrap(), from_cache_image(3));
        assert_eq!(r.remaining(), 1);

        // A type this crate does not model keeps every remaining byte.
        let mut data = Vec::new();
        descriptor(ImageType::GlzRgb, 4).write(&mut data);
        data.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            SpiceImage::decode(&data).unwrap().payload,
            ImagePayload::Other(vec![1, 2, 3])
        );
    }

    #[test]
    fn spice_copy_round_trips_and_decodes_spice_proto_layout() {
        assert_round_trip(&SpiceCopy::default());
        let copy = SpiceCopy {
            src_bitmap: 57,
            src_area: rect(1, 2, 3, 4),
            rop_descriptor: ropd::OP_PUT,
            scale_mode: image_scale_mode::NEAREST,
            mask: SpiceQMask {
                flags: mask_flags::INVERS,
                pos: SpicePoint { x: -5, y: 6 },
                bitmap_offset: 93,
            },
        };
        assert_round_trip(&copy);

        // src_bitmap, src_area, rop_descriptor, scale_mode, then the QMask.
        let mut data = le32(&[57, 1, 2, 3, 4]);
        data.extend_from_slice(&ropd::OP_PUT.to_le_bytes());
        data.extend_from_slice(&[image_scale_mode::NEAREST, mask_flags::INVERS]);
        data.extend_from_slice(&(-5i32).to_le_bytes());
        data.extend_from_slice(&le32(&[6, 93]));
        assert_eq!(data.len(), SpiceCopy::SIZE);
        assert_eq!(SpiceCopy::decode(&data).unwrap(), copy);
        assert!(SpiceCopy::decode(&data[..SpiceCopy::SIZE - 1]).is_err());
    }

    #[test]
    fn draw_copy_round_trips_each_payload_and_a_mask() {
        for image in [
            bitmap_image(1),
            jpeg_image(2),
            from_cache_image(3),
            other_image(4),
        ] {
            assert_round_trip(&draw_copy(Some(image.clone()), None));
            // A mask image follows the source, which ends where it starts.
            assert_round_trip(&draw_copy(Some(image.clone()), Some(other_image(5))));
            assert_round_trip(&draw_copy(Some(other_image(6)), Some(image)));
        }
        assert_round_trip(&draw_copy(None, None));
        assert_round_trip(&draw_copy(None, Some(bitmap_image(7))));
        assert_round_trip(&DrawCopy {
            base: DrawBase {
                surface_id: 3,
                bbox: rect(0, 0, 0, 0),
                clip: Clip::none(),
            },
            ..draw_copy(Some(from_cache_image(8)), Some(from_cache_image(9)))
        });
    }

    /// The bytes ryll's renderer tests built by hand for DRAW_COPY before
    /// the builder existed: the base, the 36-byte Copy with `src_bitmap`
    /// pointing just past it and no mask, then the image.
    fn hand_laid_draw_copy(clip_rects: &[(u32, u32, u32, u32)], image: &[u8]) -> Vec<u8> {
        let mut v = le32(&[0, 0, 0, 1, 1]); // surface_id, box: top, left, bottom, right
        if clip_rects.is_empty() {
            v.push(clip_type::NONE);
        } else {
            v.push(clip_type::RECTS);
            v.extend_from_slice(&(clip_rects.len() as u32).to_le_bytes());
            for (top, left, bottom, right) in clip_rects {
                v.extend_from_slice(&le32(&[*top, *left, *bottom, *right]));
            }
        }
        let src_bitmap = (v.len() + 36) as u32;
        v.extend_from_slice(&le32(&[src_bitmap, 0, 0, 2, 2])); // src_bitmap, src_area
        v.extend_from_slice(&[0u8; 16]); // rop, scale_mode, mask
        v.extend_from_slice(image);
        v
    }

    #[test]
    fn draw_copy_builder_matches_hand_laid_bytes() {
        for clip_rects in [&[][..], &[(0, 0, 1, 1), (5, 6, 7, 8)][..]] {
            let base = DrawBase {
                surface_id: 0,
                bbox: rect(0, 0, 1, 1),
                clip: if clip_rects.is_empty() {
                    Clip::none()
                } else {
                    Clip::rects(
                        clip_rects
                            .iter()
                            .map(|&(t, l, b, r)| rect(t as i32, l as i32, b as i32, r as i32))
                            .collect(),
                    )
                },
            };
            for image in [bitmap_image(1), from_cache_image(2)] {
                let mut image_bytes = Vec::new();
                image.write(&mut image_bytes);
                let expected = hand_laid_draw_copy(clip_rects, &image_bytes);

                let built = DrawCopyBuilder::new(&base, &image, rect(0, 0, 2, 2))
                    .rop_descriptor(0)
                    .build();
                assert_eq!(built, expected);

                // DrawCopy::write is the builder, and the bytes read back.
                let value = DrawCopy::decode(&expected).unwrap();
                assert_eq!(value.src_bitmap.as_ref(), Some(&image));
                assert_eq!(value.mask, QMask::default());
                let mut written = Vec::new();
                value.write(&mut written);
                assert_eq!(written, expected);
            }
        }
    }

    #[test]
    fn draw_copy_builder_offsets_are_from_the_body_start() {
        let value = draw_copy(Some(jpeg_image(1)), Some(from_cache_image(2)));
        let mut body = Vec::new();
        value.write(&mut body);

        // A body appended after other bytes carries the same offsets.
        let mut out = vec![0xAA; 6];
        DrawCopyBuilder::new(&value.base, &jpeg_image(1), value.src_area)
            .scale_mode(image_scale_mode::NEAREST)
            .mask(
                mask_flags::INVERS,
                SpicePoint { x: -1, y: 1 },
                Some(&from_cache_image(2)),
            )
            .write(&mut out);
        assert_eq!(&out[6..], &body[..]);

        // The pointers lead to the source, then the mask straight after it.
        let mut r = BoundedReader::new(&body);
        DrawBase::read(&mut r).unwrap();
        let copy = SpiceCopy::read(&mut r).unwrap();
        let fixed_len = r.position();
        assert_eq!(copy.src_bitmap as usize, fixed_len);
        assert_eq!(
            copy.mask.bitmap_offset as usize,
            fixed_len + ImageDescriptor::SIZE + 4 + 4
        );
        assert_eq!(body.len(), copy.mask.bitmap_offset as usize + 18);
    }

    #[test]
    fn draw_copy_bounds_an_unmodelled_image_at_the_mask() {
        // Hand-lay a source of an unmodelled type and a mask after it: the
        // source's bytes stop where the mask starts.
        let base = DrawBase {
            surface_id: 0,
            bbox: rect(0, 0, 1, 1),
            clip: Clip::none(),
        };
        let mut body = Vec::new();
        base.write(&mut body);
        let fixed_len = body.len() + SpiceCopy::SIZE;
        let mut src = Vec::new();
        descriptor(ImageType::Quic, 1).write(&mut src);
        src.extend_from_slice(&[1, 2, 3]);
        SpiceCopy {
            src_bitmap: fixed_len as u32,
            mask: SpiceQMask {
                bitmap_offset: (fixed_len + src.len()) as u32,
                ..SpiceQMask::default()
            },
            ..SpiceCopy::default()
        }
        .write(&mut body);
        body.extend_from_slice(&src);
        from_cache_image(2).write(&mut body);

        let value = DrawCopy::decode(&body).unwrap();
        assert_eq!(
            value.src_bitmap.unwrap().payload,
            ImagePayload::Other(vec![1, 2, 3])
        );
        assert_eq!(value.mask.bitmap, Some(from_cache_image(2)));
    }

    #[test]
    fn draw_copy_rewrites_another_layout_in_spice_server_order() {
        // The mask before the source, with a gap between the fixed fields
        // and the first image: accepted, and written back in canonical
        // order, which then reads back unchanged.
        let base = DrawBase {
            surface_id: 0,
            bbox: rect(0, 0, 1, 1),
            clip: Clip::none(),
        };
        let mut body = Vec::new();
        base.write(&mut body);
        let fixed_len = body.len() + SpiceCopy::SIZE;
        let mask_at = fixed_len + 3;
        let src_at = mask_at + ImageDescriptor::SIZE;
        SpiceCopy {
            src_bitmap: src_at as u32,
            mask: SpiceQMask {
                bitmap_offset: mask_at as u32,
                ..SpiceQMask::default()
            },
            ..SpiceCopy::default()
        }
        .write(&mut body);
        body.extend_from_slice(&[0xEE; 3]);
        from_cache_image(2).write(&mut body);
        other_image(1).write(&mut body);

        let mut r = BoundedReader::new(&body);
        let value = DrawCopy::read(&mut r).unwrap();
        assert_eq!(r.position(), body.len());
        assert_eq!(value.src_bitmap, Some(other_image(1)));
        assert_eq!(value.mask.bitmap, Some(from_cache_image(2)));
        assert_round_trip(&value);
    }

    #[test]
    fn draw_copy_refuses_bad_pointers() {
        let base = DrawBase {
            surface_id: 0,
            bbox: rect(0, 0, 1, 1),
            clip: Clip::rects(vec![rect(0, 0, 1, 1)]),
        };
        let fixed_len = 4 + Rect::SIZE + 1 + 4 + Rect::SIZE + SpiceCopy::SIZE;
        let body = |src_bitmap: u32, mask_bitmap: u32| {
            let mut body = Vec::new();
            base.write(&mut body);
            SpiceCopy {
                src_bitmap,
                mask: SpiceQMask {
                    bitmap_offset: mask_bitmap,
                    ..SpiceQMask::default()
                },
                ..SpiceCopy::default()
            }
            .write(&mut body);
            assert_eq!(body.len(), fixed_len);
            from_cache_image(1).write(&mut body);
            body
        };

        let good = fixed_len as u32;
        assert!(DrawCopy::decode(&body(good, 0)).is_ok());
        // Into the clip rectangles, and into the Copy itself.
        for into_fixed in [30, fixed_len as u32 - 1] {
            assert_eq!(
                DrawCopy::decode(&body(into_fixed, 0)),
                Err(LinkError::PointerIntoFixedPart {
                    offset: into_fixed as usize,
                    fixed_len,
                })
            );
            assert_eq!(
                DrawCopy::decode(&body(good, into_fixed)),
                Err(LinkError::PointerIntoFixedPart {
                    offset: into_fixed as usize,
                    fixed_len,
                })
            );
        }
        // Past the end, and too close to it for a descriptor.
        let len = fixed_len + ImageDescriptor::SIZE;
        assert_eq!(
            DrawCopy::decode(&body(len as u32 + 1, 0)),
            Err(LinkError::BadOffset {
                offset: len + 1,
                len: 0,
                buffer_len: len,
            })
        );
        assert!(matches!(
            DrawCopy::decode(&body(good + 1, 0)),
            Err(LinkError::Truncated { .. })
        ));
        // A truncated fixed part.
        assert!(DrawCopy::decode(&body(good, 0)[..fixed_len - 1]).is_err());
    }

    #[test]
    fn draw_copy_refuses_a_palette_pointer() {
        let mut image = bitmap_image(1);
        if let ImagePayload::Bitmap(bitmap) = &mut image.payload {
            bitmap.header.palette = BitmapPalette::Offset(4);
        }
        let mut body = Vec::new();
        draw_copy(Some(image), None).write(&mut body);
        assert_eq!(
            DrawCopy::decode(&body),
            Err(LinkError::Unsupported {
                what: "bitmap palette pointer",
            })
        );

        // A palette from the cache is a value, not a pointer.
        let mut image = bitmap_image(1);
        if let ImagePayload::Bitmap(bitmap) = &mut image.payload {
            bitmap.header.flags |= bitmap_flags::PAL_FROM_CACHE;
            bitmap.header.palette = BitmapPalette::FromCache(77);
        }
        assert_round_trip(&draw_copy(Some(image), None));
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
