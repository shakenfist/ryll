//! Frame source abstraction: decouples the encoder from pixel
//! delivery. Phase 4–5 will provide a production implementation
//! over the renderer's surface map; Phase 2 ships only a
//! synthetic test implementation (added in step 2d).

/// A reference to one frame's pixel data.
///
/// The reference is valid until the next call to
/// [`FrameSource::next_frame`]. Implementations may copy pixels
/// into an internal staging buffer to satisfy this lifetime
/// contract when the underlying surface is concurrently written.
pub struct FrameRef<'a> {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Raw RGBA pixel data, tightly packed: 4 bytes per pixel,
    /// row-major, top-left origin. Length must equal
    /// `width * height * 4`.
    pub rgba: &'a [u8],
    /// Wall-clock timestamp in microseconds. The origin is
    /// chosen by the producer (e.g. encoder start instant) and
    /// must increase monotonically across successive frames.
    /// Used by the task layer to derive RTP timestamps in
    /// Phase 3+.
    pub timestamp_us: u64,
}

/// Provides frames of RGBA pixels to the encoder.
///
/// Implementors handle dirty tracking and any synchronisation
/// with concurrent writers. The encoder calls [`next_frame`]
/// on each tick and skips the tick when `None` is returned,
/// so implementations should return `None` when no new pixels
/// are available since the last call.
///
/// # Thread safety
///
/// `FrameSource` must be `Send + 'static` because the encoder
/// task runs on tokio's blocking thread pool.
pub trait FrameSource: Send + 'static {
    /// Acquire the next frame to encode.
    ///
    /// Returns `Some(FrameRef)` when new pixels are available
    /// since the last call, `None` when the surface is
    /// unchanged (causing the encoder task to skip this tick).
    fn next_frame(&mut self) -> Option<FrameRef<'_>>;
}

/// A synthetic, unbounded `FrameSource` that generates deterministic
/// RGBA frames suitable for testing the encoder pipeline end-to-end.
///
/// Each frame contains:
/// - A static 32-pixel checkerboard background in two shades of grey.
/// - An animated horizontal band whose centre advances by 4 pixels per
///   frame (wrapping at the frame height). The band blends a vivid red
///   colour over the checkerboard within ±20 pixels of the centre,
///   giving the encoder real inter-frame motion to encode.
///
/// `next_frame` always returns `Some`; the caller is responsible for
/// stopping the encoder via [`crate::EncoderControl::Stop`].
pub struct SyntheticFrameSource {
    width: u32,
    height: u32,
    /// Reused RGBA buffer; avoids a heap allocation on every frame.
    buffer: Vec<u8>,
    frame_idx: u64,
}

impl SyntheticFrameSource {
    /// Create a new synthetic source for `width × height` RGBA frames.
    /// Both dimensions should be even (required by the H.264 encoder).
    pub fn new(width: u32, height: u32) -> Self {
        let n = (width as usize) * (height as usize) * 4;
        Self {
            width,
            height,
            buffer: vec![0u8; n],
            frame_idx: 0,
        }
    }

    /// Index of the frame that the next call to `next_frame` will produce.
    pub fn frame_index(&self) -> u64 {
        self.frame_idx
    }
}

impl FrameSource for SyntheticFrameSource {
    fn next_frame(&mut self) -> Option<FrameRef<'_>> {
        let w = self.width as usize;
        let h = self.height as usize;
        let band_centre = ((self.frame_idx as f32 * 4.0) % h as f32) as i32;

        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;

                // Static 32-pixel checkerboard: two grey tones.
                let tile = ((x / 32) + (y / 32)) % 2;
                let (cr, cg, cb) = if tile == 0 {
                    (64u8, 64u8, 64u8)
                } else {
                    (128u8, 128u8, 128u8)
                };

                // Animated band: blend saturated red within ±20 px of centre.
                let dist = (y as i32 - band_centre).abs();
                let (r, g, b) = if dist <= 20 {
                    // Linear blend from checkerboard to red: full red at
                    // centre (dist=0), full checkerboard at edge (dist=20).
                    let t = dist as f32 / 20.0; // 0 at centre, 1 at edge
                    let r = (255.0 * (1.0 - t) + cr as f32 * t) as u8;
                    let g = (0.0 * (1.0 - t) + cg as f32 * t) as u8;
                    let b = (0.0 * (1.0 - t) + cb as f32 * t) as u8;
                    (r, g, b)
                } else {
                    (cr, cg, cb)
                };

                self.buffer[idx] = r;
                self.buffer[idx + 1] = g;
                self.buffer[idx + 2] = b;
                self.buffer[idx + 3] = 255;
            }
        }

        let timestamp_us = self.frame_idx * 33_333;
        self.frame_idx += 1;

        Some(FrameRef {
            width: self.width,
            height: self.height,
            rgba: &self.buffer,
            timestamp_us,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_frame_source_produces_frames_with_advancing_timestamps() {
        let mut s = SyntheticFrameSource::new(64, 64);
        let f0 = s.next_frame().expect("frame 0").timestamp_us;
        let f1 = s.next_frame().expect("frame 1").timestamp_us;
        let f2 = s.next_frame().expect("frame 2").timestamp_us;
        assert_eq!(f0, 0);
        assert_eq!(f1, 33_333);
        assert_eq!(f2, 66_666);
    }

    #[test]
    fn synthetic_frame_source_buffer_is_correct_length() {
        let mut s = SyntheticFrameSource::new(64, 32);
        let frame = s.next_frame().expect("frame");
        assert_eq!(frame.rgba.len(), 64 * 32 * 4);
        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 32);
    }
}
