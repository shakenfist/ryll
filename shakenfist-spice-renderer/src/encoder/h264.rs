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
use openh264::encoder::{BitRate, EncoderConfig, FrameRate, UsageType};
use openh264::OpenH264API;

/// Bits per pixel per frame used to derive the encoder's target
/// bitrate, in thousandths (so 100 means 0.1 bits per pixel per
/// frame).
///
/// openh264's own default is a flat 120 kbit/s whatever the
/// resolution, which is a webcam-era number: a 1024x768 desktop at
/// 30fps through 120 kbit/s is unreadable. 0.1 bits per pixel per
/// frame gives ~2.4 Mbit/s at 1024x768@30 and ~6.2 Mbit/s at
/// 1920x1080@30, inside the 0.2-50 Mbit/s range we have measured
/// for real SPICE console traffic.
///
/// Held in thousandths, and the derivation done in integers,
/// because a float version rounds unpredictably at the `as u32`
/// truncation and makes the policy hard to state in a test.
const MILLIBITS_PER_PIXEL_PER_FRAME: u64 = 100;

/// Floor for the derived bitrate. Small surfaces would otherwise
/// derive a target so low that even a static desktop cannot be
/// coded cleanly.
const MIN_BITRATE_BPS: u32 = 1_000_000;

/// Ceiling for the derived bitrate, so a 4K surface cannot ask for
/// more bandwidth than a viewer is likely to have.
const MAX_BITRATE_BPS: u32 = 20_000_000;

/// Derive a target bitrate from the surface size and frame rate.
///
/// Kept separate from [`H264Encoder::new`] so the policy can be
/// tested without constructing an encoder.
fn target_bitrate_bps(width: u32, height: u32, fps: u32) -> u32 {
    let pixels_per_second = (width as u64) * (height as u64) * (fps as u64);
    let bps = pixels_per_second * MILLIBITS_PER_PIXEL_PER_FRAME / 1000;
    bps.clamp(MIN_BITRATE_BPS as u64, MAX_BITRATE_BPS as u64) as u32
}

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
    /// Create a new encoder for `width × height` RGBA frames fed at
    /// `fps` frames per second.
    ///
    /// openh264 requires even dimensions, so both values are rounded
    /// down to the nearest even number. Returns `Err` if either
    /// rounded dimension is 0, or if `fps` is 0.
    ///
    /// # Why this does not use `Encoder::new()`
    ///
    /// `Encoder::new()` takes openh264's default `EncoderConfig`,
    /// which is wrong for a desktop in three separate ways: a flat
    /// 120 kbit/s target bitrate, a `max_frame_rate` of 0 (so rate
    /// control does not know the cadence it is budgeting for), and
    /// `UsageType::CameraVideoRealTime`, which is tuned for camera
    /// noise rather than for the sharp edges and large flat regions
    /// of a UI. All three are set explicitly here.
    ///
    /// `fps` is a parameter rather than a constant because the
    /// caller owns the frame cadence — `EncoderTask` is handed an
    /// `fps_cap` and the rate controller needs the same number.
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        let w = width & !1;
        let h = height & !1;
        if w == 0 || h == 0 {
            anyhow::bail!(
                "H264Encoder: dimensions too small after rounding down to even: {}x{}",
                width,
                height
            );
        }
        if fps == 0 {
            anyhow::bail!("H264Encoder: fps must be > 0");
        }

        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(target_bitrate_bps(w, h, fps)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            // Both default to on, and openh264 supports neither for
            // screen content: it turns them off itself and prints a
            // warning to stderr while doing so, once per encoder —
            // which now means once per browser connection. Setting
            // them changes no behaviour and stops the config
            // claiming something the encoder is not doing.
            .adaptive_quantization(false)
            .background_detection(false);

        let inner = openh264::encoder::Encoder::with_api_config(OpenH264API::from_source(), config)
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
        assert!(H264Encoder::new(1, 1, 30).is_err());
    }

    #[test]
    fn rejects_zero_fps() {
        assert!(H264Encoder::new(64, 64, 0).is_err());
    }

    #[test]
    fn bitrate_scales_with_pixels_and_clamps_at_both_ends() {
        // A desktop-sized surface has to land far above openh264's
        // own 120 kbit/s default; that default is the bug this
        // policy exists to fix.
        assert_eq!(target_bitrate_bps(1024, 768, 30), 2_359_296);
        assert!(target_bitrate_bps(1920, 1080, 30) > target_bitrate_bps(1024, 768, 30));

        // Tiny surfaces hit the floor, 4K hits the ceiling.
        assert_eq!(target_bitrate_bps(64, 64, 30), MIN_BITRATE_BPS);
        assert_eq!(target_bitrate_bps(3840, 2160, 30), MAX_BITRATE_BPS);
    }

    #[test]
    fn sps_still_advertises_baseline_profile() {
        // `negotiated_h264_payload_type` in shakenfist-spice-webrtc
        // prefers an SDP `profile-level-id` beginning `42`, on the
        // strength of openh264 resolving `uiProfileIdc` to
        // PRO_BASELINE when nothing enables CABAC. Choosing
        // ScreenContentRealTime must not quietly move us off
        // baseline, or the payload type we stamp stops agreeing
        // with the bitstream we send.
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
        let frame = enc.encode(&black_frame(64, 64), false).expect("encode");
        let sps = frame
            .nal_units
            .iter()
            .find(|n| n[4] & 0x1F == 7)
            .expect("SPS present");
        // NAL: [0,0,0,1, header, profile_idc, ...]
        assert_eq!(sps[5], 0x42, "profile_idc should still be baseline");
    }

    #[test]
    fn first_frame_emits_idr_with_sps_and_pps() {
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
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
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
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
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
        let _ = enc.encode(&black_frame(64, 64), false).expect("kf");
        let _ = enc.encode(&black_frame(64, 64), false).expect("p1");
        let kf2 = enc.encode(&black_frame(64, 64), true).expect("forced kf");
        assert!(kf2.keyframe, "forced keyframe should be marked");
    }

    #[test]
    fn annex_b_start_codes_present() {
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
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
