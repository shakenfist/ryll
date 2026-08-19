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
//! always set to 0 and is populated by the caller (the encoder
//! task).

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

/// Round a size down to the even dimensions H.264 requires.
///
/// 4:2:0 chroma subsampling halves both axes, so both must be even.
/// Every producer of a size the encoder will eventually see has to
/// apply the same rounding, and there are four of them in two crates
/// — the browser's requested viewport, the primary-surface size read
/// when the encoder is built, the per-frame resize check in
/// [`crate::EncoderTask`], and `ryll`'s capture writer. Each used to
/// spell `& !1` itself, and comparing a rounded size against an
/// unrounded one is exactly how the resize check came to rebuild the
/// encoder on every frame of an odd surface and then fail its length
/// check.
pub fn even_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
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
    /// Staging buffer for [`H264Encoder::encode_cropped`]. Allocated
    /// on the first odd frame and reused after that; an even source
    /// never touches it.
    crop_buf: Vec<u8>,
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
        let (w, h) = even_dimensions(width, height);
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
            crop_buf: Vec::new(),
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
            // Populated by EncoderTask; the encoder itself is
            // timestamp-agnostic.
            timestamp_us: 0,
            keyframe: is_idr,
        })
    }

    /// Encode a frame whose source dimensions may be odd, discarding
    /// the last row and/or column to reach the encoder's even size.
    ///
    /// `rgba` must be `src_width * src_height * 4` bytes, and
    /// [`even_dimensions`] of the source must equal this encoder's
    /// size — a source that rounds to anything else is a genuine
    /// resolution change and is rejected, because silently encoding a
    /// differently-shaped frame would produce a sheared picture rather
    /// than an error.
    ///
    /// Odd surfaces are not exotic. The browser asks the guest for
    /// `Math.round()` of its viewport, X will grant an odd mode, and
    /// before this existed the encoder rounded its own size down while
    /// [`encode`](Self::encode) demanded an exact match — so the first
    /// frame of an odd surface failed the length check and killed the
    /// encoder task for the life of the bridge.
    ///
    /// Cropping the last row is free; cropping the last column means a
    /// row-wise copy into a staging buffer, which is why the fast path
    /// below hands `rgba` straight through when the source is already
    /// even.
    pub fn encode_cropped(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        let expected_src = (src_width as usize) * (src_height as usize) * 4;
        if rgba.len() != expected_src {
            anyhow::bail!(
                "H264Encoder::encode_cropped: rgba.len() = {}, expected {} ({}x{}x4)",
                rgba.len(),
                expected_src,
                src_width,
                src_height
            );
        }

        let (w, h) = even_dimensions(src_width, src_height);
        if (w, h) != (self.width, self.height) {
            anyhow::bail!(
                "H264Encoder::encode_cropped: source {}x{} rounds to {}x{}, \
                 but this encoder is {}x{}",
                src_width,
                src_height,
                w,
                h,
                self.width,
                self.height
            );
        }

        // Already even in both axes: nothing to crop.
        if src_width == w && src_height == h {
            return self.encode(rgba, force_keyframe);
        }

        // An odd height alone needs no copy — the discarded row is a
        // suffix of the buffer, so a subslice is the whole crop. Doing
        // it here rather than in the row loop keeps the common
        // odd-height case allocation-free.
        if src_width == w {
            let keep = (w as usize) * (h as usize) * 4;
            return self.encode(&rgba[..keep], force_keyframe);
        }

        // Odd width: copy each kept row's leading `w` pixels.
        let src_stride = (src_width as usize) * 4;
        let dst_stride = (w as usize) * 4;
        self.crop_buf.resize(dst_stride * (h as usize), 0);
        for row in 0..(h as usize) {
            let src_off = row * src_stride;
            let dst_off = row * dst_stride;
            self.crop_buf[dst_off..dst_off + dst_stride]
                .copy_from_slice(&rgba[src_off..src_off + dst_stride]);
        }
        let buf = std::mem::take(&mut self.crop_buf);
        let out = self.encode(&buf, force_keyframe);
        self.crop_buf = buf;
        out
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
    fn bitrate_scales_with_frame_rate_too() {
        // The policy is per *pixel per frame*, so halving the cadence
        // halves the budget. Only 30fps was pinned before, which
        // would have let the fps term be dropped entirely without a
        // test noticing.
        assert_eq!(
            target_bitrate_bps(1024, 768, 60),
            2 * target_bitrate_bps(1024, 768, 30)
        );
        assert_eq!(target_bitrate_bps(1024, 768, 15), 1_179_648);
        // Still clamped at both ends whatever the cadence.
        assert_eq!(target_bitrate_bps(1024, 768, 1), MIN_BITRATE_BPS);
        assert_eq!(target_bitrate_bps(1920, 1080, 240), MAX_BITRATE_BPS);
    }

    #[test]
    fn even_dimensions_rounds_each_axis_down_independently() {
        assert_eq!(even_dimensions(64, 64), (64, 64));
        assert_eq!(even_dimensions(63, 45), (62, 44));
        assert_eq!(even_dimensions(65, 64), (64, 64));
        assert_eq!(even_dimensions(64, 65), (64, 64));
        assert_eq!(even_dimensions(1, 1), (0, 0));
    }

    /// An odd-width source is cropped column-wise, not read across
    /// row boundaries. A sheared read would have every row offset by
    /// one more pixel than the last, which shows up as a diagonal
    /// smear rather than as an error — so assert on the pixels the
    /// crop keeps, using a frame whose rows are distinguishable.
    #[test]
    fn cropping_an_odd_width_keeps_each_row_aligned() {
        // 65x64 RGBA where every pixel's red channel is its row index
        // and green is its column index, so a misaligned copy is
        // visible in the values rather than only in the picture.
        // 65 wide rather than something tiny because openh264 refuses
        // very small frames outright.
        let (sw, sh) = (65u32, 64u32);
        let mut src = vec![0u8; (sw * sh * 4) as usize];
        for y in 0..sh {
            for x in 0..sw {
                let i = ((y * sw + x) * 4) as usize;
                src[i] = y as u8;
                src[i + 1] = x as u8;
                src[i + 3] = 255;
            }
        }

        let mut enc = H264Encoder::new(sw, sh, 30).expect("init");
        assert_eq!((enc.width(), enc.height()), (64, 64));
        enc.encode_cropped(&src, sw, sh, false).expect("encode");

        // The staging buffer is what the encoder was handed. Column
        // 64 is dropped from every row; nothing else moves. Reading
        // straight through the source instead would shift each row one
        // pixel further left than the last — a diagonal shear that
        // still encodes cleanly, which is why this asserts on pixels
        // and not just on the length.
        assert_eq!(enc.crop_buf.len(), (64 * 64 * 4) as usize);
        for y in 0..64usize {
            for x in 0..64usize {
                let i = (y * 64 + x) * 4;
                assert_eq!(
                    (enc.crop_buf[i], enc.crop_buf[i + 1]),
                    (y as u8, x as u8),
                    "pixel ({}, {}) came from the wrong place — the crop sheared",
                    x,
                    y
                );
            }
        }
    }

    #[test]
    fn cropping_an_odd_height_needs_no_staging_buffer() {
        // Dropping the last row is a suffix of the buffer, so the
        // copy is skipped entirely.
        let mut enc = H264Encoder::new(64, 65, 30).expect("init");
        assert_eq!((enc.width(), enc.height()), (64, 64));
        enc.encode_cropped(&black_frame(64, 65), 64, 65, false)
            .expect("encode");
        assert!(
            enc.crop_buf.is_empty(),
            "an odd height alone should not have allocated a staging buffer"
        );
    }

    #[test]
    fn an_even_source_takes_the_fast_path() {
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
        enc.encode_cropped(&black_frame(64, 64), 64, 64, false)
            .expect("encode");
        assert!(enc.crop_buf.is_empty(), "no crop was needed");
    }

    #[test]
    fn cropping_rejects_a_source_of_the_wrong_length() {
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
        let err = enc
            .encode_cropped(&black_frame(64, 63), 64, 64, false)
            .expect_err("a short buffer must not be encoded");
        assert!(
            err.to_string().contains("expected"),
            "unhelpful error: {}",
            err
        );
    }

    /// A source that rounds to a *different* size is a resolution
    /// change, not something to crop into shape. Encoding it anyway
    /// would produce a sheared picture with no error to explain it,
    /// so the caller has to rebuild instead.
    #[test]
    fn cropping_rejects_a_genuine_resolution_change() {
        let mut enc = H264Encoder::new(64, 64, 30).expect("init");
        let err = enc
            .encode_cropped(&black_frame(32, 32), 32, 32, false)
            .expect_err("a different resolution must not be cropped into shape");
        assert!(
            err.to_string().contains("this encoder is 64x64"),
            "unhelpful error: {}",
            err
        );
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
