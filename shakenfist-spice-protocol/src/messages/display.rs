//! Display channel messages and the draw types they carry.
//!
//! Layouts follow spice-common's `spice.proto`, `channel DisplayChannel`
//! and the structs before it. spice.proto's `Rect` and `Point` are
//! signed, so their fields are `i32`; ryll's renderer has always used the
//! same bits as `u32`, and casts at its call sites.
//!
//! The draw bodies other than DRAW_COPY's (`SpiceFill`, `SpiceOpaque`
//! and the rest) are still `io::Result` readers over a slice; moving them
//! onto [`BoundedReader`] is shakenfist/ryll#136.
use super::WireType;
use crate::constants::clip_type;
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
