//! The H.264 encoder as the WebRTC pipeline and ryll's capture use it.
//!
//! The encoder itself lives in `shakenfist-spice-compression`, where a
//! SPICE server can use it without this crate. This adapts it to the
//! renderer's frames: RGBA surface pixels in, and out an
//! [`EncodedFrame`] of NAL units, each behind a four-byte start code,
//! with a timestamp the encoder task fills in.

use anyhow::Result;
use shakenfist_spice_compression::{even_dimensions, H264Config, H264RateControl, Rgbx};

use super::types::EncodedFrame;

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

/// Stateful H.264 encoder for RGBA frames, emitting one
/// [`EncodedFrame`] per call.
pub struct H264Encoder {
    inner: shakenfist_spice_compression::H264Encoder,
}

impl H264Encoder {
    /// Create a new encoder for `width × height` RGBA frames fed at
    /// `fps` frames per second.
    ///
    /// openh264 requires even dimensions, so both values are rounded
    /// down to the nearest even number. Returns `Err` if either
    /// rounded dimension is 0, or if `fps` is 0.
    ///
    /// The target bitrate is `target_bitrate_bps` of the size and
    /// rate, since openh264's own default is a flat 120 kbit/s. Rate
    /// control stays in openh264's quality mode, with frame skipping
    /// on, as this encoder has always run.
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
        // One slice, as before the encoder moved: browsers have not
        // been tried with multi-slice frames.
        let mut config = H264Config::new(w, h, fps);
        config.rate_control = H264RateControl::Quality;
        config.skip_frames = true;
        config.slices = 1;
        Ok(Self {
            inner: shakenfist_spice_compression::H264Encoder::new(config)?,
        })
    }

    /// Width in pixels (after rounding down to even).
    pub fn width(&self) -> u32 {
        self.inner.width()
    }

    /// Height in pixels (after rounding down to even).
    pub fn height(&self) -> u32 {
        self.inner.height()
    }

    /// Encode a single RGBA frame of exactly the encoder's size.
    ///
    /// `rgba` must be `width * height * 4` bytes (tight, no row
    /// padding). If `force_keyframe` is true, the frame is an IDR.
    /// The first frame always is one.
    ///
    /// Returns an [`EncodedFrame`] containing every NAL produced by
    /// openh264 (SPS/PPS/IDR/non-IDR/etc.), each prefixed with the
    /// Annex-B start code, or none when openh264 skipped the frame.
    /// `timestamp_us` is always 0; the caller populates it.
    pub fn encode(&mut self, rgba: &[u8], force_keyframe: bool) -> Result<EncodedFrame> {
        self.encode_cropped(rgba, self.width(), self.height(), force_keyframe)
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
    /// `Math.round()` of its viewport, and X will grant an odd mode.
    /// The crop costs nothing: the converter reads the kept pixels of
    /// each row in place.
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
        let image =
            Rgbx::new(rgba, src_width, src_height, src_width as usize * 4).ok_or_else(|| {
                anyhow::anyhow!("H264Encoder: {src_width}x{src_height} is not a valid image")
            })?;

        if force_keyframe {
            self.inner.force_intra_frame();
        }
        let Some(frame) = self.inner.encode_rgbx(&image)? else {
            return Ok(EncodedFrame {
                nal_units: Vec::new(),
                timestamp_us: 0,
                keyframe: false,
            });
        };

        // openh264 mixes 3- and 4-byte start codes within a frame;
        // normalise to 4-byte ones, NAL by NAL.
        let nal_units = split_annex_b(&frame.data)
            .into_iter()
            .filter(|nal| !nal.is_empty())
            .map(|nal| {
                let mut framed = Vec::with_capacity(4 + nal.len());
                framed.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                framed.extend_from_slice(nal);
                framed
            })
            .collect();
        Ok(EncodedFrame {
            nal_units,
            // Populated by EncoderTask; the encoder itself is
            // timestamp-agnostic.
            timestamp_us: 0,
            keyframe: frame.keyframe,
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
    fn rounds_an_odd_size_down() {
        let enc = H264Encoder::new(65, 63, 30).expect("init");
        assert_eq!((enc.width(), enc.height()), (64, 62));
    }

    /// An odd source encodes exactly as its top-left even crop does.
    /// A crop that read across row boundaries would shear the
    /// picture and still encode cleanly, so compare the output.
    #[test]
    fn cropping_an_odd_source_encodes_its_even_crop() {
        // Every pixel's red channel is its row and green its column,
        // so a misaligned read changes the picture.
        let (sw, sh) = (65u32, 65u32);
        let mut src = vec![0u8; (sw * sh * 4) as usize];
        for y in 0..sh {
            for x in 0..sw {
                let i = ((y * sw + x) * 4) as usize;
                src[i] = (y * 3) as u8;
                src[i + 1] = (x * 3) as u8;
                src[i + 3] = 255;
            }
        }
        let mut crop = Vec::new();
        for y in 0..64usize {
            crop.extend_from_slice(&src[y * 65 * 4..y * 65 * 4 + 64 * 4]);
        }

        let mut odd = H264Encoder::new(sw, sh, 30).expect("init");
        let mut even = H264Encoder::new(64, 64, 30).expect("init");
        let from_odd = odd.encode_cropped(&src, sw, sh, false).expect("encode");
        let from_crop = even.encode(&crop, false).expect("encode");
        assert_eq!(from_odd.nal_units, from_crop.nal_units);
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
        // prefers an SDP `profile-level-id` beginning `42`. Choosing
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
