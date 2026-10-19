//! Types shared by the encoder task and its consumers.

/// One encoded frame's NAL units in Annex-B framing.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Annex-B-framed NAL units. Each NAL is prefixed with the
    /// 4-byte start code `00 00 00 01`. Empty when the encoder
    /// skipped the frame to hold its bitrate.
    pub nal_units: Vec<Vec<u8>>,
    /// Wall-clock timestamp in microseconds (origin chosen by
    /// the producer; monotonically increasing).
    pub timestamp_us: u64,
    /// Whether this frame is an IDR keyframe.
    pub keyframe: bool,
}

/// Control messages sent to a running [`EncoderTask`].
#[derive(Debug)]
pub enum EncoderControl {
    /// Force the next encoded frame to be an IDR keyframe. Sent
    /// whenever a new viewer attaches.
    RequestKeyframe,
    /// Stop the task. The task returns `Ok(())` after the
    /// current encode (if any) completes.
    Stop,
}
