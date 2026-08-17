//! Live H.264 video encoder for the SPICE→browser transcoder.
//!
//! The encoder reads RGBA pixels from a [`FrameSource`] (a
//! trait that the consumer implements over its surface state),
//! encodes via [`H264Encoder`] (a stateful wrapper around
//! openh264 producing Annex-B framed NAL units), and is
//! driven asynchronously by [`EncoderTask`] at a configurable
//! FPS cap with keyframe-on-demand.
//!
//! No network code lives here; the output is an mpsc stream
//! of [`EncodedFrame`]s that the WebRTC plumbing in Phase 3+
//! will consume.

mod frame_source;
mod h264;
mod task;

pub use frame_source::{FrameRef, FrameSource, RealFrameSource, SyntheticFrameSource};
pub use h264::{even_dimensions, EncodedFrame, H264Encoder};
pub use task::{EncoderControl, EncoderTask};
