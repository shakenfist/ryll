//! Stateful H.264 encoder wrapping openh264.
//!
//! Emits Annex-B-framed NAL units. Each call to [`H264Encoder::encode`]
//! returns the NAL units for a single encoded frame, with the 4-byte
//! start code `00 00 00 01` prepended to each NAL. SPS/PPS NALs are
//! emitted whenever openh264 produces them (typically alongside every
//! IDR), so the consumer always has fresh parameter sets after a
//! forced keyframe.
//!
//! Timestamps are not handled here; [`EncodedFrame::timestamp_us`] is
//! always set to 0 and is populated by the caller (the encoder task in
//! step 2c).

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

/// Split an Annex-B byte stream into individual NAL bodies (start
/// codes stripped). Recognises both 3-byte (`00 00 01`) and 4-byte
/// (`00 00 00 01`) start codes, which openh264 mixes within a single
/// access unit.
fn split_annex_b(buf: &[u8]) -> Vec<&[u8]> {
    let mut nals: Vec<&[u8]> = Vec::new();
    let len = buf.len();

    // Find the first start code; everything before it is preamble
    // (typically empty).
    let mut nal_start = match find_start_code(buf, 0) {
        Some((pos, sc_len)) => pos + sc_len,
        None => return nals,
    };

    while nal_start < len {
        match find_start_code(buf, nal_start) {
            Some((pos, sc_len)) => {
                nals.push(&buf[nal_start..pos]);
                nal_start = pos + sc_len;
            }
            None => {
                nals.push(&buf[nal_start..len]);
                break;
            }
        }
    }
    nals
}

/// Find the next Annex-B start code at or after `from`. Returns
/// `(position, start_code_length)` where `start_code_length` is 3
/// or 4.
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                return Some((i, 3));
            }
            if buf[i + 2] == 0 && i + 3 < buf.len() && buf[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

/// Stateful H.264 encoder. Wraps `openh264::encoder::Encoder` and
/// emits Annex-B-framed NAL units, one [`EncodedFrame`] per call to
/// [`H264Encoder::encode`].
pub struct H264Encoder {
    inner: openh264::encoder::Encoder,
    width: u32,
    height: u32,
}

impl H264Encoder {
    /// Create a new encoder for `width × height` RGBA frames.
    ///
    /// openh264 requires even dimensions, so both values are rounded
    /// down to the nearest even number. Returns `Err` if either
    /// rounded dimension is 0.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let w = width & !1;
        let h = height & !1;
        if w == 0 || h == 0 {
            anyhow::bail!(
                "H264Encoder: dimensions too small after rounding down to even: {}x{}",
                width,
                height
            );
        }
        let inner = openh264::encoder::Encoder::new()
            .map_err(|e| anyhow::anyhow!("H264Encoder: openh264 init failed: {}", e))?;
        Ok(Self {
            inner,
            width: w,
            height: h,
        })
    }

    /// Width in pixels (after rounding down to even).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels (after rounding down to even).
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Reconfigure the encoder for new dimensions. No-op when the
    /// dimensions (after even-rounding) already match. On a real
    /// change the inner openh264 encoder is rebuilt from scratch
    /// so the next encoded frame starts a fresh stream — openh264
    /// emits SPS / PPS alongside the implicit first-frame IDR,
    /// which is exactly what the browser decoder needs to switch
    /// resolution mid-WebRTC-session without renegotiation.
    ///
    /// Called by [`super::EncoderTask`] on every frame so guest-
    /// initiated display resizes (or `VDAgentMonitorsConfig`-
    /// driven ones from the browser viewport) self-heal in one
    /// frame instead of freezing the stream.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let w = width & !1;
        let h = height & !1;
        if w == 0 || h == 0 {
            anyhow::bail!(
                "H264Encoder::resize: dimensions too small after rounding down to even: {}x{}",
                width,
                height,
            );
        }
        if w == self.width && h == self.height {
            return Ok(());
        }
        let inner = openh264::encoder::Encoder::new()
            .map_err(|e| anyhow::anyhow!("H264Encoder::resize: openh264 init failed: {}", e))?;
        self.inner = inner;
        self.width = w;
        self.height = h;
        Ok(())
    }

    /// Encode a single RGBA frame.
    ///
    /// `rgba` must be `width * height * 4` bytes (tight, no row
    /// padding). If `force_keyframe` is true, the next frame is
    /// requested as an IDR via [`openh264::encoder::Encoder::force_intra_frame`].
    /// The first frame is implicitly an IDR by openh264 default.
    ///
    /// Returns an [`EncodedFrame`] containing every NAL produced by
    /// openh264 (SPS/PPS/IDR/non-IDR/etc.), each prefixed with the
    /// Annex-B start code. `timestamp_us` is always 0; the caller
    /// populates it.
    pub fn encode(&mut self, rgba: &[u8], force_keyframe: bool) -> Result<EncodedFrame> {
        use openh264::formats::{RgbaSliceU8, YUVBuffer};

        let expected_len = (self.width as usize) * (self.height as usize) * 4;
        if rgba.len() != expected_len {
            anyhow::bail!(
                "H264Encoder::encode: rgba.len() = {}, expected {} ({}x{}x4)",
                rgba.len(),
                expected_len,
                self.width,
                self.height
            );
        }

        if force_keyframe {
            self.inner.force_intra_frame();
        }

        let rgba_slice = RgbaSliceU8::new(rgba, (self.width as usize, self.height as usize));
        let yuv = YUVBuffer::from_rgb_source(rgba_slice);

        let bitstream = self
            .inner
            .encode(&yuv)
            .map_err(|e| anyhow::anyhow!("H264Encoder::encode: {}", e))?;

        // openh264 0.6 already emits each NAL with an Annex-B
        // start code prefix (3- or 4-byte). Concatenate the raw
        // bitstream and re-parse so we can normalise to 4-byte
        // start codes and inspect NAL types.
        let mut raw: Vec<u8> = Vec::new();
        for layer_idx in 0..bitstream.num_layers() {
            if let Some(layer) = bitstream.layer(layer_idx) {
                for nal_idx in 0..layer.nal_count() {
                    if let Some(nal) = layer.nal_unit(nal_idx) {
                        raw.extend_from_slice(nal);
                    }
                }
            }
        }

        let mut nal_units: Vec<Vec<u8>> = Vec::new();
        let mut is_idr = false;
        for nal_body in split_annex_b(&raw) {
            if nal_body.is_empty() {
                continue;
            }
            let nal_type = nal_body[0] & 0x1F;
            if nal_type == 5 {
                is_idr = true;
            }
            let mut framed = Vec::with_capacity(4 + nal_body.len());
            framed.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            framed.extend_from_slice(nal_body);
            nal_units.push(framed);
        }

        Ok(EncodedFrame {
            nal_units,
            // Populated by EncoderTask in step 2c; the encoder
            // itself is timestamp-agnostic.
            timestamp_us: 0,
            keyframe: is_idr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn black_frame(w: u32, h: u32) -> Vec<u8> {
        let n = (w as usize) * (h as usize);
        let mut v = vec![0u8; n * 4];
        // Set alpha to 255. openh264 ignores alpha during YUV
        // conversion, but be explicit so the buffer represents an
        // opaque black frame.
        for i in 0..n {
            v[i * 4 + 3] = 255;
        }
        v
    }

    #[test]
    fn rejects_zero_dimensions_after_rounding() {
        // 1x1 rounds down to 0x0.
        assert!(H264Encoder::new(1, 1).is_err());
    }

    #[test]
    fn first_frame_emits_idr_with_sps_and_pps() {
        let mut enc = H264Encoder::new(64, 64).expect("init");
        // First frame is implicitly an IDR (openh264 default).
        let frame = enc.encode(&black_frame(64, 64), false).expect("encode");
        assert!(frame.keyframe, "first frame should be a keyframe");

        // Strip start codes to inspect NAL types.
        let nal_types: Vec<u8> = frame.nal_units.iter().map(|n| n[4] & 0x1F).collect();
        assert!(nal_types.contains(&7), "SPS missing: {:?}", nal_types);
        assert!(nal_types.contains(&8), "PPS missing: {:?}", nal_types);
        assert!(nal_types.contains(&5), "IDR slice missing: {:?}", nal_types);
    }

    #[test]
    fn subsequent_frames_smaller_than_keyframe() {
        let mut enc = H264Encoder::new(64, 64).expect("init");
        let kf = enc.encode(&black_frame(64, 64), false).expect("kf");
        let p1 = enc.encode(&black_frame(64, 64), false).expect("p1");

        let kf_bytes: usize = kf.nal_units.iter().map(|n| n.len()).sum();
        let p1_bytes: usize = p1.nal_units.iter().map(|n| n.len()).sum();

        assert!(kf.keyframe, "frame 1 is keyframe");
        assert!(!p1.keyframe, "frame 2 is not keyframe");
        assert!(
            p1_bytes < kf_bytes,
            "non-keyframe ({} bytes) should be smaller than keyframe ({} bytes)",
            p1_bytes,
            kf_bytes
        );
    }

    #[test]
    fn forced_keyframe_marks_idr() {
        let mut enc = H264Encoder::new(64, 64).expect("init");
        let _ = enc.encode(&black_frame(64, 64), false).expect("kf");
        let _ = enc.encode(&black_frame(64, 64), false).expect("p1");
        let kf2 = enc.encode(&black_frame(64, 64), true).expect("forced kf");
        assert!(kf2.keyframe, "forced keyframe should be marked");
    }

    #[test]
    fn annex_b_start_codes_present() {
        let mut enc = H264Encoder::new(64, 64).expect("init");
        let frame = enc.encode(&black_frame(64, 64), false).expect("encode");
        for nal in &frame.nal_units {
            assert!(nal.len() >= 5, "NAL too short: {} bytes", nal.len());
            assert_eq!(
                &nal[0..4],
                &[0x00, 0x00, 0x00, 0x01],
                "missing Annex-B start code"
            );
        }
    }
}
