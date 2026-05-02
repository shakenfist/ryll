//! Async driver for the H.264 encoder: 30 fps cap,
//! encode-on-dirty, keyframe-on-demand. Implementation lands
//! in step 2c.

/// Control messages sent to a running [`EncoderTask`].
#[derive(Debug)]
pub enum EncoderControl {
    /// Force the next encoded frame to be an IDR keyframe.
    /// Phase 3+ calls this whenever a new viewer attaches.
    RequestKeyframe,
    /// Stop the task.
    Stop,
}

/// Async driver around an [`H264Encoder`]. Implementation
/// lands in step 2c.
pub struct EncoderTask;
