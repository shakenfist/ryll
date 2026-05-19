//! Video decoder abstraction for SPICE video streams.
//!
//! Provides a [`VideoDecoder`] trait with a stateful
//! `decode(&mut self, packet: &[u8])` method so the display
//! channel can dispatch video frames to the right backend at
//! runtime without knowing the codec. Each stream state owns one
//! `Box<dyn VideoDecoder>` selected at `STREAM_CREATE` by
//! [`for_stream`].
//!
//! Today only MJPEG is implemented (`MJpegVideoDecoder`), which
//! wraps the existing [`JpegDecoder`] backend from phase 3 and
//! absorbs the DHT extract/inject state that used to live on
//! `StreamState`. H.264 decoding is added in phase 6B.
//!
//! The codec-type constants here mirror the wire values in the
//! SPICE protocol (`SPICE_VIDEO_CODEC_TYPE_*`).

use std::sync::Arc;

use crate::jpeg::{DecodedJpeg, JpegDecoder};

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// SPICE wire codec type for MJPEG streams
/// (`SpiceMsgDisplayStreamCreate::codec_type == 1`).
pub const SPICE_VIDEO_CODEC_TYPE_MJPEG: u8 = 1;

/// SPICE wire codec type for H.264 streams
/// (`SpiceMsgDisplayStreamCreate::codec_type == 3`).
/// Not decoded in phase 6A — [`for_stream`] returns
/// [`VideoDecoderError::UnsupportedCodec`] for this value until
/// phase 6B adds `H264VideoDecoder`.
pub const SPICE_VIDEO_CODEC_TYPE_H264: u8 = 3;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A decoded video frame: RGBA pixels plus their dimensions.
///
/// Width and height are those reported by the underlying decoder
/// and may differ slightly from the stream's advertised dimensions
/// (particularly for H.264, where the codec rounds up to macroblock
/// boundaries). The display channel clips to the stream's declared
/// destination rect before painting.
pub struct DecodedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Errors returned by [`VideoDecoder::decode`] and [`for_stream`].
#[derive(Debug, thiserror::Error)]
pub enum VideoDecoderError {
    /// The underlying codec backend reported a decode failure.
    ///
    /// The display channel increments `frames_decode_failed` and
    /// continues — the next key frame should resync the stream.
    #[error("video decode failed: {0}")]
    Decode(String),

    /// The server requested a codec that this client does not
    /// support. [`for_stream`] returns this when asked to create
    /// a decoder for an unrecognised `codec_type` byte.
    #[error("unsupported codec_type {0}")]
    UnsupportedCodec(u8),
}

/// Stateful per-stream video decoder.
///
/// Implementations take `&mut self` so decoders that maintain
/// reference frames (H.264), DHT caches (MJPEG), or other
/// per-stream state can update themselves in place. Stateless
/// decoders simply ignore `&mut self`.
///
/// The trait is `Send` so the boxed decoder can be stored in
/// a `StreamState` and moved across threads. It is NOT `Sync`
/// because each stream owns its decoder exclusively — no sharing.
pub trait VideoDecoder: Send {
    /// Attempt to decode `packet` into an RGBA frame.
    ///
    /// Returns:
    /// - `Ok(Some(frame))` — a complete frame was decoded.
    /// - `Ok(None)` — the packet was consumed but no complete
    ///   frame is ready yet (H.264 may need several packets to
    ///   assemble one frame; MJPEG always returns `Some` on
    ///   success).
    /// - `Err(VideoDecoderError::Decode(_))` — the packet was
    ///   malformed or the backend rejected it. The caller should
    ///   bump its failure counter and move on.
    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedFrame>, VideoDecoderError>;

    /// Human-readable backend name, surfaced in bug reports.
    ///
    /// Examples: `"libjpeg-turbo"`, `"ImageIO"`, `"WIC"`,
    /// `"VA-API"`, `"H264 (openh264)"`.
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// for_stream selector
// ---------------------------------------------------------------------------

/// Select and construct the appropriate [`VideoDecoder`] for the
/// given SPICE wire `codec_type`.
///
/// Called once per `STREAM_CREATE` message. The returned decoder is
/// stored as `Box<dyn VideoDecoder>` on the `StreamState` and used
/// for every subsequent `STREAM_DATA` packet on that stream.
///
/// # Arguments
///
/// * `codec_type` — the raw codec byte from the
///   `SpiceMsgDisplayStreamCreate` payload (byte 9 of the
///   fixed-width header).
/// * `jpeg_decoder` — the session-wide JPEG decoder backend
///   (selected once at channel init by
///   [`crate::jpeg::best_for_platform`]).  A clone of the `Arc`
///   is handed to [`MJpegVideoDecoder`] so each stream shares the
///   same backend instance without re-probing.
///
/// # Errors
///
/// Returns [`VideoDecoderError::UnsupportedCodec`] for any
/// `codec_type` this build does not handle. The display channel
/// logs a warning and skips the stream rather than crashing.
pub fn for_stream(
    codec_type: u8,
    jpeg_decoder: Arc<dyn JpegDecoder>,
) -> Result<Box<dyn VideoDecoder>, VideoDecoderError> {
    match codec_type {
        SPICE_VIDEO_CODEC_TYPE_MJPEG => Ok(Box::new(MJpegVideoDecoder::new(jpeg_decoder))),
        // Phase 6B adds H264VideoDecoder here.
        other => Err(VideoDecoderError::UnsupportedCodec(other)),
    }
}

// ---------------------------------------------------------------------------
// MJPEG implementation
// ---------------------------------------------------------------------------

/// Video decoder for SPICE MJPEG streams.
///
/// Wraps the phase-3 [`JpegDecoder`] backend and maintains a
/// per-stream DHT cache. SPICE's MJPEG framing omits the
/// Huffman tables (`DHT` segment) from every frame after the
/// first. `MJpegVideoDecoder` extracts the DHT from the first
/// frame that carries one and injects it into subsequent
/// DHT-less frames so the underlying JPEG decoder always receives
/// a fully-formed JPEG byte stream.
///
/// The DHT logic is identical to the pre-refactor path in
/// `display.rs:1460-1470`; it has been moved here so the
/// display-channel dispatch loop is codec-agnostic.
pub struct MJpegVideoDecoder {
    inner: Arc<dyn JpegDecoder>,
    cached_dht: Option<Vec<u8>>,
}

impl MJpegVideoDecoder {
    /// Construct a new decoder backed by `inner`.
    ///
    /// `inner` is typically the `Arc<dyn JpegDecoder>` already
    /// held on `DisplayChannel` — the caller passes a `.clone()`
    /// so each stream shares the same backend without re-probing.
    pub fn new(inner: Arc<dyn JpegDecoder>) -> Self {
        Self {
            inner,
            cached_dht: None,
        }
    }
}

impl VideoDecoder for MJpegVideoDecoder {
    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedFrame>, VideoDecoderError> {
        // Extract any DHT segment present in this packet and cache
        // it so we can inject it into future DHT-less frames.
        // If the packet has no DHT but we have a cached one, inject
        // the cached DHT before decoding.
        let dht = extract_dht_segments(packet);
        let owned;
        let frame_data = if !dht.is_empty() {
            self.cached_dht = Some(dht);
            packet
        } else if let Some(ref cached) = self.cached_dht {
            owned = inject_dht(packet, cached);
            &owned
        } else {
            packet
        };

        match self.inner.decode(frame_data) {
            Some(DecodedJpeg {
                rgba,
                width,
                height,
            }) => Ok(Some(DecodedFrame {
                rgba,
                width,
                height,
            })),
            None => Err(VideoDecoderError::Decode(
                "MJPEG decode returned None".to_string(),
            )),
        }
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

// ---------------------------------------------------------------------------
// DHT helpers (moved here from display.rs)
// ---------------------------------------------------------------------------

/// Scan `jpeg` for DHT (`0xFF 0xC4`) segments and return their
/// bytes concatenated. Returns an empty `Vec` if no DHT is found.
///
/// This is identical to the pre-refactor `extract_dht_segments`
/// in `display.rs:38-65`.
fn extract_dht_segments(jpeg: &[u8]) -> Vec<u8> {
    let mut dht = Vec::new();
    let mut i = 0;
    while i + 3 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xC4 {
            let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
            if i + seg_len <= jpeg.len() {
                dht.extend_from_slice(&jpeg[i..i + seg_len]);
            }
            i += seg_len;
        } else if marker == 0xD8 || marker == 0x00 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
        } else if marker == 0xD9 || marker == 0xDA {
            break;
        } else if i + 3 < jpeg.len() {
            let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
            i += seg_len;
        } else {
            break;
        }
    }
    dht
}

/// Inject `dht` bytes into `jpeg` immediately after the SOI marker
/// and any leading APP0/APP1 segments. Returns the modified JPEG.
///
/// This is identical to the pre-refactor `inject_dht` in
/// `display.rs:67-87`.
fn inject_dht(jpeg: &[u8], dht: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(jpeg.len() + dht.len());
    let mut i = 0;
    // SOI marker
    if jpeg.len() >= 2 && jpeg[0] == 0xFF && jpeg[1] == 0xD8 {
        out.extend_from_slice(&jpeg[..2]);
        i = 2;
    }
    // skip APP0/APP1 if present (bounds check matches extract_dht_segments)
    while i + 3 < jpeg.len() && jpeg[i] == 0xFF && (jpeg[i + 1] == 0xE0 || jpeg[i + 1] == 0xE1) {
        let seg_len = (u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize).saturating_add(2);
        if i + seg_len > jpeg.len() {
            break;
        }
        out.extend_from_slice(&jpeg[i..i + seg_len]);
        i += seg_len;
    }
    out.extend_from_slice(dht);
    out.extend_from_slice(&jpeg[i..]);
    out
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jpeg::JpegDecoderRsDecoder;

    // ------------------------------------------------------------------
    // for_stream selector tests
    // ------------------------------------------------------------------

    #[test]
    fn for_stream_mjpeg_returns_ok() {
        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(JpegDecoderRsDecoder::new());
        let result = for_stream(SPICE_VIDEO_CODEC_TYPE_MJPEG, jpeg_dec.clone());
        assert!(result.is_ok(), "expected Ok for MJPEG codec_type");
        let dec = result.unwrap();
        assert_eq!(
            dec.name(),
            jpeg_dec.name(),
            "MJpegVideoDecoder name should match underlying JpegDecoder name"
        );
    }

    #[test]
    fn for_stream_h264_returns_unsupported_in_phase_6a() {
        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(JpegDecoderRsDecoder::new());
        let result = for_stream(SPICE_VIDEO_CODEC_TYPE_H264, jpeg_dec);
        match result {
            Err(VideoDecoderError::UnsupportedCodec(3)) => {}
            Err(e) => panic!("expected UnsupportedCodec(3), got Err({e})"),
            Ok(_) => panic!("expected Err(UnsupportedCodec(3)), got Ok"),
        }
    }

    #[test]
    fn for_stream_unknown_codec_returns_unsupported() {
        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(JpegDecoderRsDecoder::new());
        let unknown = 99u8;
        let result = for_stream(unknown, jpeg_dec);
        match result {
            Err(VideoDecoderError::UnsupportedCodec(c)) => {
                assert_eq!(c, unknown, "UnsupportedCodec carries the original byte");
            }
            Err(e) => panic!("expected UnsupportedCodec, got Err({e})"),
            Ok(_) => panic!("expected Err(UnsupportedCodec), got Ok"),
        }
    }

    // ------------------------------------------------------------------
    // extract_dht_segments tests (same coverage as the old display.rs tests)
    // ------------------------------------------------------------------

    /// Build a minimal JPEG byte sequence:
    ///   SOI (FF D8) + one segment (marker + 2-byte BE length + payload) + EOI (FF D9)
    fn make_jpeg_with_marker(marker: u8, payload: &[u8]) -> Vec<u8> {
        let seg_len = (payload.len() + 2) as u16;
        let mut jpeg = vec![0xFF, 0xD8]; // SOI
        jpeg.push(0xFF);
        jpeg.push(marker);
        jpeg.extend_from_slice(&seg_len.to_be_bytes());
        jpeg.extend_from_slice(payload);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    #[test]
    fn extract_dht_segments_finds_dht_marker() {
        let payload = vec![0x01, 0x02, 0x03, 0x04];
        let jpeg = make_jpeg_with_marker(0xC4, &payload);
        let dht = extract_dht_segments(&jpeg);
        assert!(!dht.is_empty(), "expected non-empty DHT output");
        assert_eq!(dht[0], 0xFF);
        assert_eq!(dht[1], 0xC4);
        let encoded_len = u16::from_be_bytes([dht[2], dht[3]]) as usize;
        assert_eq!(encoded_len, payload.len() + 2);
        assert_eq!(&dht[4..], payload.as_slice());
    }

    #[test]
    fn extract_dht_segments_no_dht_returns_empty() {
        let jpeg = make_jpeg_with_marker(0xFE, b"hello");
        let dht = extract_dht_segments(&jpeg);
        assert!(dht.is_empty(), "expected empty Vec when no DHT present");
    }

    #[test]
    fn extract_dht_segments_empty_input_returns_empty() {
        let dht = extract_dht_segments(&[]);
        assert!(dht.is_empty());
    }

    // ------------------------------------------------------------------
    // inject_dht tests (same coverage as the old display.rs tests)
    // ------------------------------------------------------------------

    #[test]
    fn inject_dht_inserts_after_soi_when_no_app_markers() {
        let jpeg = vec![0xFF, 0xD8, 0xAA, 0xBB, 0xCC, 0xFF, 0xD9];
        let dht = vec![0xFF, 0xC4, 0x00, 0x04, 0x01, 0x02];
        let out = inject_dht(&jpeg, &dht);
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        assert_eq!(&out[2..2 + dht.len()], dht.as_slice());
        assert_eq!(&out[2 + dht.len()..], &jpeg[2..]);
    }

    #[test]
    fn inject_dht_inserts_after_app0_segment() {
        let app0_payload = vec![0x4A, 0x46, 0x49, 0x46, 0x00];
        let app0_seg_len = (app0_payload.len() + 2) as u16;
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.push(0xFF);
        jpeg.push(0xE0);
        jpeg.extend_from_slice(&app0_seg_len.to_be_bytes());
        jpeg.extend_from_slice(&app0_payload);
        jpeg.extend_from_slice(&[0xDE, 0xAD]);
        let dht = vec![0xFF, 0xC4, 0x00, 0x04, 0x01, 0x02];
        let out = inject_dht(&jpeg, &dht);
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        let app0_total = 2 + 2 + app0_payload.len();
        let app0_in_out = &out[2..2 + app0_total];
        assert_eq!(app0_in_out[0], 0xFF);
        assert_eq!(app0_in_out[1], 0xE0);
        let dht_start = 2 + app0_total;
        assert_eq!(&out[dht_start..dht_start + dht.len()], dht.as_slice());
        let remaining_start = dht_start + dht.len();
        assert_eq!(&out[remaining_start..], &[0xDE, 0xAD]);
    }

    #[test]
    fn inject_dht_output_structure_soi_app_dht_rest() {
        let jpeg = vec![0xFF, 0xD8, 0x01, 0x02, 0x03];
        let dht = vec![0xFF, 0xC4, 0x00, 0x02];
        let out = inject_dht(&jpeg, &dht);
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        let dht_pos = out.windows(dht.len()).position(|w| w == dht.as_slice());
        assert!(dht_pos.is_some(), "DHT not found in output");
        let dht_end = dht_pos.unwrap() + dht.len();
        assert_eq!(&out[dht_end..], &jpeg[2..]);
    }
}

// ---------------------------------------------------------------------------
// MJpegVideoDecoder round-trip test (requires mozjpeg feature).
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "mozjpeg"))]
mod mjpeg_round_trip_tests {
    use super::*;
    use crate::jpeg::MozJpegDecoder;

    /// Encode a small frame via `mozjpeg::Compress`, then decode it
    /// through `MJpegVideoDecoder`, and assert the RGBA is within
    /// JPEG-lossy tolerance. Mirrors the `mozjpeg_round_trip_within_tolerance`
    /// test in `jpeg.rs` (phase 3B pattern) but exercises the new
    /// `VideoDecoder` trait wrapper.
    #[test]
    fn mjpeg_video_decoder_round_trip_within_tolerance() {
        const W: u32 = 16;
        const H: u32 = 16;

        let mut src = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            for x in 0..W {
                let r = ((x * 255) / (W - 1)) as u8;
                let g = ((y * 255) / (H - 1)) as u8;
                let b = (((x + y) * 255) / (W + H - 2)) as u8;
                src.push(r);
                src.push(g);
                src.push(b);
                src.push(255);
            }
        }

        let jpeg_bytes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_EXT_RGBA);
            comp.set_size(W as usize, H as usize);
            comp.set_quality(75.0);
            let mut started = comp
                .start_compress(Vec::new())
                .expect("start_compress failed");
            started
                .write_scanlines(&src)
                .expect("write_scanlines failed");
            started.finish().expect("finish failed")
        }))
        .expect("encode panicked");
        assert!(!jpeg_bytes.is_empty(), "encoder produced empty output");

        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(MozJpegDecoder::new());
        let mut video_dec = MJpegVideoDecoder::new(jpeg_dec);

        let frame = video_dec
            .decode(&jpeg_bytes)
            .expect("MJpegVideoDecoder returned Err on first frame")
            .expect("MJpegVideoDecoder returned None on first frame");

        assert_eq!(frame.width, W, "decoded width mismatch");
        assert_eq!(frame.height, H, "decoded height mismatch");
        assert_eq!(
            frame.rgba.len(),
            (W * H * 4) as usize,
            "decoded buffer length wrong"
        );

        let mut max_diff: u32 = 0;
        let mut sum_diff: u64 = 0;
        let mut sample_count: u64 = 0;
        for (i, (a, b)) in src.iter().zip(frame.rgba.iter()).enumerate() {
            if i % 4 == 3 {
                assert_eq!(*b, 255, "alpha at byte {i} should be 255, got {b}");
                continue;
            }
            let diff = (*a as i32 - *b as i32).unsigned_abs();
            max_diff = max_diff.max(diff);
            sum_diff += diff as u64;
            sample_count += 1;
        }
        let mean_diff = sum_diff as f64 / sample_count as f64;
        eprintln!("mjpeg video_dec round-trip: max_diff={max_diff} mean_diff={mean_diff:.2}");
        assert!(
            max_diff <= 20,
            "per-channel max diff {max_diff} exceeds tolerance 20"
        );
        assert!(
            mean_diff <= 5.0,
            "per-channel mean diff {mean_diff:.2} exceeds tolerance 5.0"
        );
    }

    /// Verify the DHT-injection path: encode a JPEG, strip its DHT
    /// segment, then decode it with `MJpegVideoDecoder`. The first
    /// decode (with DHT) should cache it; the second decode (without
    /// DHT) should succeed because the cached DHT is injected.
    #[test]
    fn mjpeg_video_decoder_dht_injection_path() {
        const W: u32 = 8;
        const H: u32 = 8;

        let src: Vec<u8> = (0..(W * H * 4))
            .map(|i| match i % 4 {
                3 => 255,
                _ => ((i * 17) % 256) as u8,
            })
            .collect();

        // Encode once; this JPEG includes a DHT segment.
        let jpeg_with_dht = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_EXT_RGBA);
            comp.set_size(W as usize, H as usize);
            comp.set_quality(85.0);
            let mut started = comp
                .start_compress(Vec::new())
                .expect("start_compress failed");
            started
                .write_scanlines(&src)
                .expect("write_scanlines failed");
            started.finish().expect("finish failed")
        }))
        .expect("encode panicked");

        // Strip the DHT segment from a copy so we can hand a DHT-less
        // frame as the second decode. Walk the markers and skip 0xC4.
        let jpeg_without_dht = strip_dht_segments(&jpeg_with_dht);

        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(MozJpegDecoder::new());
        let mut video_dec = MJpegVideoDecoder::new(jpeg_dec);

        // First frame: has DHT → decoder caches it.
        let first = video_dec.decode(&jpeg_with_dht);
        assert!(first.is_ok(), "first frame (with DHT) should decode Ok");
        assert!(
            first.unwrap().is_some(),
            "first frame should produce a DecodedFrame"
        );

        // Second frame: DHT stripped → decoder should inject cached DHT.
        let second = video_dec.decode(&jpeg_without_dht);
        assert!(
            second.is_ok(),
            "second frame (DHT-less, cached DHT injected) should decode Ok, got {:?}",
            second.err()
        );
        assert!(
            second.unwrap().is_some(),
            "second frame should produce a DecodedFrame"
        );
    }

    /// Walk `jpeg` and return a copy with all DHT (0xFF 0xC4)
    /// segments removed. Used by the DHT-injection test.
    fn strip_dht_segments(jpeg: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < jpeg.len() {
            if i + 1 < jpeg.len() && jpeg[i] == 0xFF {
                let marker = jpeg[i + 1];
                if marker == 0xD8 {
                    out.extend_from_slice(&jpeg[i..i + 2]);
                    i += 2;
                    continue;
                }
                if marker == 0xD9 || marker == 0xDA {
                    out.extend_from_slice(&jpeg[i..]);
                    break;
                }
                if marker == 0xC4 {
                    // DHT — skip it.
                    if i + 3 < jpeg.len() {
                        let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
                        i += seg_len;
                    } else {
                        break;
                    }
                    continue;
                }
                // Any other segment: copy through.
                if i + 3 < jpeg.len() {
                    let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
                    if i + seg_len <= jpeg.len() {
                        out.extend_from_slice(&jpeg[i..i + seg_len]);
                        i += seg_len;
                    } else {
                        out.extend_from_slice(&jpeg[i..]);
                        break;
                    }
                } else {
                    out.extend_from_slice(&jpeg[i..]);
                    break;
                }
            } else {
                out.push(jpeg[i]);
                i += 1;
            }
        }
        out
    }
}
