//! Live H.264 video encoder for the SPICE→browser transcoder.
//!
//! The encoder reads RGBA pixels from a [`FrameSource`] (a
//! trait that the consumer implements over its surface state),
//! encodes via [`H264Encoder`] (an RGBA adapter over the
//! openh264 encoder in `shakenfist-spice-compression`, producing
//! Annex-B framed NAL units), and is
//! driven asynchronously by [`EncoderTask`] at a configurable
//! FPS cap with keyframe-on-demand.
//!
//! No network code lives here; the output is an mpsc stream
//! of [`EncodedFrame`]s that the WebRTC plumbing consumes.

mod frame_source;
mod h264;
mod task;
mod types;

pub use frame_source::{FrameRef, FrameSource, RealFrameSource, SyntheticFrameSource};
pub use h264::H264Encoder;
pub use shakenfist_spice_compression::even_dimensions;
pub use task::EncoderTask;
pub use types::{EncodedFrame, EncoderControl};
