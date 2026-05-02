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
