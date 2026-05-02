//! Stateful H.264 encoder wrapping openh264.

use anyhow::Result;

/// One encoded frame's NAL units in Annex-B framing.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Annex-B-framed NAL units. Each NAL is prefixed with the
    /// 4-byte start code `00 00 00 01`.
    pub nal_units: Vec<Vec<u8>>,
    /// Wall-clock timestamp in microseconds (origin chosen by
    /// the producer; monotonically increasing).
    pub timestamp_us: u64,
    /// Whether this frame is an IDR keyframe.
    pub keyframe: bool,
}

/// Stateful H.264 encoder. Implementation lands in step 2b.
pub struct H264Encoder {
    _todo: (),
}

impl H264Encoder {
    pub fn new(_width: u32, _height: u32) -> Result<Self> {
        anyhow::bail!("H264Encoder not yet implemented (Phase 2 step 2b)")
    }

    pub fn encode(&mut self, _rgba: &[u8], _force_keyframe: bool) -> Result<EncodedFrame> {
        anyhow::bail!("H264Encoder not yet implemented (Phase 2 step 2b)")
    }
}
