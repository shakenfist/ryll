//! Frame source abstraction: decouples the encoder from pixel
//! delivery.
//!
//! Two production-shaped impls live here:
//!
//! - [`SyntheticFrameSource`] — deterministic test pattern,
//!   used by encoder unit tests and the `--web` Phase 4
//!   bring-up.
//! - [`RealFrameSource`] — reads from a [`SurfaceMirror`] under
//!   a non-blocking [`tokio::sync::Mutex::try_lock`]; this is
//!   the Phase 5 substrate that turns SPICE pixels into encoder
//!   input.

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

/// A `FrameSource` that reads from a [`SurfaceMirror`]'s primary
/// surface.
///
/// On each [`FrameSource::next_frame`] call:
///
/// 1. `try_lock` the mirror. If the lock is held by the
///    apply-event task, return `None` — skipping a frame is far
///    cheaper than blocking the encoder thread on lock
///    contention.
/// 2. Look up the primary surface. Return `None` if the SPICE
///    session has not yet produced a primary (e.g. the first
///    `SurfaceCreated` / `ImageReady` is still in flight).
/// 3. Check [`DisplaySurface::consume_dirty`]. Return `None`
///    when the surface is unchanged since the last call so the
///    encoder genuinely encodes-on-dirty rather than re-encoding
///    static frames at FPS cadence.
/// 4. Copy the pixel buffer into a self-owned RGBA buffer; drop
///    the lock. The returned [`FrameRef`] borrows from the
///    self-owned buffer, freeing the apply-event task to write
///    new frames concurrently.
///
/// The `try_lock` path is the right shape for the Phase 5 MVP:
/// the apply-event task holds the lock for the duration of one
/// `apply_event` call (microseconds). A persistently contended
/// lock would surface as dropped frames rather than encoder
/// stalls — investigate as a Phase 6 perf item if it shows up.
pub struct RealFrameSource {
    mirror: std::sync::Arc<tokio::sync::Mutex<crate::SurfaceMirror>>,
    /// Reused RGBA buffer; resized only when the primary
    /// surface dimensions change (mid-session resize is
    /// out-of-scope but the resize-aware code costs nothing).
    rgba_buf: Vec<u8>,
    last_dimensions: Option<(u32, u32)>,
    /// Wall-clock origin for `timestamp_us`. The trait contract
    /// requires monotonically-increasing timestamps; using
    /// `Instant::now()` at construction time + elapsed-since
    /// gives that for free.
    epoch: std::time::Instant,
}

impl RealFrameSource {
    /// Wrap an existing [`SurfaceMirror`] handle. The mirror is
    /// shared with the apply-event task — both ends use
    /// `tokio::sync::Mutex` so the apply-event side can `lock`
    /// asynchronously while the encoder side `try_lock`s
    /// synchronously from its blocking thread.
    pub fn new(mirror: std::sync::Arc<tokio::sync::Mutex<crate::SurfaceMirror>>) -> Self {
        Self {
            mirror,
            rgba_buf: Vec::new(),
            last_dimensions: None,
            epoch: std::time::Instant::now(),
        }
    }
}

impl FrameSource for RealFrameSource {
    fn next_frame(&mut self) -> Option<FrameRef<'_>> {
        // Step 1: non-blocking lock acquisition. Skip frame on
        // contention rather than block the encoder thread.
        let mut guard = self.mirror.try_lock().ok()?;

        // Step 2: primary surface lookup; absent until the SPICE
        // session has produced one.
        let surface = guard.primary_surface_mut()?;

        // Step 3: dirty check. consume_dirty also clears the
        // flag so successive identical frames are correctly
        // skipped.
        if !surface.consume_dirty() {
            return None;
        }

        let (w, h) = surface.size();
        let needed_len = (w as usize) * (h as usize) * 4;
        let pixels = surface.pixels();
        if pixels.len() != needed_len {
            // Defensive: the surface buffer should always match
            // its dimensions (DisplaySurface enforces this in
            // the constructor and clamps oversized requests),
            // but if it ever doesn't, skip the frame rather than
            // encode garbage. This branch is also where a future
            // mid-session resize would surface as a transient.
            return None;
        }

        // Step 4: copy pixels into the self-owned buffer so we
        // can release the lock before returning. Resize the
        // buffer only on dimension change.
        if self.last_dimensions != Some((w, h)) {
            self.rgba_buf.resize(needed_len, 0);
            self.last_dimensions = Some((w, h));
        }
        self.rgba_buf.copy_from_slice(pixels);
        drop(guard);

        let timestamp_us = self.epoch.elapsed().as_micros() as u64;
        Some(FrameRef {
            width: w,
            height: h,
            rgba: &self.rgba_buf,
            timestamp_us,
        })
    }
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
    use crate::channels::ChannelEvent;
    use crate::SurfaceMirror;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

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

    /// Build a mirror with a primary surface populated by a
    /// known image, then assert `RealFrameSource::next_frame`
    /// returns those exact pixels at the surface's reported
    /// dimensions.
    #[tokio::test(flavor = "current_thread")]
    async fn real_frame_source_returns_primary_pixels() {
        let mirror = Arc::new(TokioMutex::new(SurfaceMirror::new()));
        // Populate the primary surface with a 4x4 all-blue image.
        let pixels: Vec<u8> = (0..16).flat_map(|_| [0u8, 0, 255, 255]).collect();
        {
            let mut m = mirror.lock().await;
            m.apply_event(&ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 4,
                height: 4,
            });
            m.apply_event(&ChannelEvent::ImageReady {
                display_channel_id: 0,
                surface_id: 0,
                left: 0,
                top: 0,
                width: 4,
                height: 4,
                pixels: pixels.clone(),
                image_id: 0,
                produced_at_secs: 0.0,
            });
        }

        let mut src = RealFrameSource::new(mirror.clone());
        let frame = src.next_frame().expect("frame after dirty");
        assert_eq!(frame.width, 4);
        assert_eq!(frame.height, 4);
        assert_eq!(frame.rgba.len(), 4 * 4 * 4);
        assert_eq!(&frame.rgba[0..4], &[0, 0, 255, 255]);
        // Last pixel.
        assert_eq!(&frame.rgba[(15 * 4)..(15 * 4 + 4)], &[0, 0, 255, 255]);
    }

    /// Without a primary surface, `next_frame` returns `None`
    /// rather than panicking. This is the path that
    /// `EncoderInfra::restart` guards against by returning
    /// `Err` when no primary surface exists yet.
    #[tokio::test(flavor = "current_thread")]
    async fn real_frame_source_none_when_no_primary() {
        let mirror = Arc::new(TokioMutex::new(SurfaceMirror::new()));
        let mut src = RealFrameSource::new(mirror);
        assert!(src.next_frame().is_none());
    }

    /// After a dirty frame is consumed, the next call returns
    /// `None` until the surface is mutated again. Confirms the
    /// encode-on-dirty contract.
    #[tokio::test(flavor = "current_thread")]
    async fn real_frame_source_skips_when_clean() {
        let mirror = Arc::new(TokioMutex::new(SurfaceMirror::new()));
        {
            let mut m = mirror.lock().await;
            m.apply_event(&ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 2,
                height: 2,
            });
        }
        let mut src = RealFrameSource::new(mirror.clone());
        // First call: surface was just created (dirty), so we
        // get a frame.
        assert!(src.next_frame().is_some());
        // Second call: nothing changed, dirty cleared, expect None.
        assert!(src.next_frame().is_none());
        // Mutate; expect a frame again.
        {
            let mut m = mirror.lock().await;
            let pixels: Vec<u8> = (0..4).flat_map(|_| [10u8, 20, 30, 255]).collect();
            m.apply_event(&ChannelEvent::ImageReady {
                display_channel_id: 0,
                surface_id: 0,
                left: 0,
                top: 0,
                width: 2,
                height: 2,
                pixels,
                image_id: 0,
                produced_at_secs: 0.0,
            });
        }
        assert!(src.next_frame().is_some());
    }
}
