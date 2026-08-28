//! Video decoder abstraction for SPICE video streams.
//!
//! Provides a [`VideoDecoder`] trait with a stateful
//! `decode(&mut self, packet: &[u8])` method so the display
//! channel can dispatch video frames to the right backend at
//! runtime without knowing the codec. Each stream state owns one
//! `Box<dyn VideoDecoder>` selected at `STREAM_CREATE` by
//! [`for_stream`].
//!
//! Today MJPEG (`MJpegVideoDecoder`) wraps the per-platform
//! [`JpegDecoder`] backend and absorbs the DHT extract/inject
//! state that used to live on `StreamState`. H.264
//! (`H264VideoDecoder`) wraps `openh264::decoder::Decoder` for
//! software decode of SPICE H.264 streams.
//!
//! The codec-type constants here mirror the wire values in the
//! SPICE protocol (`SPICE_VIDEO_CODEC_TYPE_*`).

use std::sync::Arc;

use tracing::{info, warn};

use crate::jpeg::{DecodedJpeg, JpegDecoder};

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// SPICE wire codec type for MJPEG streams
/// (`SpiceMsgDisplayStreamCreate::codec_type == 1`).
pub const SPICE_VIDEO_CODEC_TYPE_MJPEG: u8 = 1;

/// SPICE wire codec type for H.264 streams
/// (`SpiceMsgDisplayStreamCreate::codec_type == 3`).
/// Decoded via [`H264VideoDecoder`] (openh264 software backend).
pub const SPICE_VIDEO_CODEC_TYPE_H264: u8 = 3;

// ---------------------------------------------------------------------------
// Decoded-frame limits
// ---------------------------------------------------------------------------

/// Upper bound on either side of a decoded video frame, in pixels.
///
/// A decoded frame costs the same allocation whichever codec
/// produced it, so the cap has to be identical across backends: if
/// the H.264 path were looser than the MJPEG one a hostile server
/// would simply announce an H.264 stream. Aliased to
/// [`crate::jpeg::MAX_DECODED_JPEG_DIMENSION`] rather than restated
/// so the two cannot drift apart; see that constant for why 16384
/// is the number (1 GiB of RGBA at the limit, with headroom for
/// displays beyond 8K).
pub const MAX_DECODED_VIDEO_DIMENSION: u32 = crate::jpeg::MAX_DECODED_JPEG_DIMENSION;

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

/// A still-image JPEG decode is already a decoded frame.
///
/// `DecodedJpeg` and `DecodedFrame` carry the same three fields for
/// two different layers: the `JpegDecoder` backends produce the
/// former, the `VideoDecoder` stream path consumes the latter. The
/// distinction is worth keeping — H.264 dimensions may be rounded up
/// to a macroblock boundary, which is a property of the video layer
/// and not of any JPEG — but the conversion between them should not
/// be spelled out field by field at each call site, where a
/// transposed `width`/`height` would compile silently.
impl From<DecodedJpeg> for DecodedFrame {
    fn from(jpeg: DecodedJpeg) -> Self {
        Self {
            rgba: jpeg.rgba,
            width: jpeg.width,
            height: jpeg.height,
        }
    }
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
        SPICE_VIDEO_CODEC_TYPE_H264 => Ok(Box::new(H264VideoDecoder::new()?)),
        other => Err(VideoDecoderError::UnsupportedCodec(other)),
    }
}

// ---------------------------------------------------------------------------
// MJPEG implementation
// ---------------------------------------------------------------------------

/// Video decoder for SPICE MJPEG streams.
///
/// Wraps the per-platform [`JpegDecoder`] backend and maintains a
/// per-stream DHT cache. SPICE's MJPEG framing omits the
/// Huffman tables (`DHT` segment) from every frame after the
/// first. `MJpegVideoDecoder` remembers the DHT of the most recent
/// frame that carried one *and decoded* and injects it into
/// subsequent DHT-less frames, so the underlying JPEG decoder
/// always receives a fully-formed JPEG byte stream.
///
/// The DHT logic moved here from the pre-refactor path in
/// `display.rs:1460-1470` so the display-channel dispatch loop is
/// codec-agnostic; the only behavioural difference is that the
/// cache is now updated after a successful decode rather than
/// before the attempt.
pub struct MJpegVideoDecoder {
    inner: Arc<dyn JpegDecoder>,
    /// Huffman tables carried by the most recent frame that both
    /// carried a DHT *and* decoded successfully. Injected into
    /// later DHT-less frames. See [`MJpegVideoDecoder::decode`]
    /// for why a candidate DHT is not cached until its own frame
    /// has decoded.
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
        // A frame that carries its own DHT is decoded as-is; a
        // DHT-less frame gets the cached tables injected first.
        //
        // The cache is only updated once the frame the tables came
        // from has decoded successfully. Committing before the
        // decode would let a server plant one malformed DHT and
        // break every subsequent DHT-less frame until a frame with
        // usable tables happened along — a single bad frame becomes
        // an open-ended stall. On failure the previous cache entry
        // is left in place: it decoded something once, which is
        // more than the candidate can claim.
        let dht = extract_dht_segments(packet);
        let owned;
        let frame_data = if !dht.is_empty() {
            packet
        } else if let Some(ref cached) = self.cached_dht {
            owned = inject_dht(packet, cached);
            &owned
        } else {
            packet
        };

        let decoded = self.inner.decode(frame_data);
        match decoded {
            Some(jpeg) => {
                if !dht.is_empty() {
                    self.cached_dht = Some(dht);
                }
                Ok(Some(jpeg.into()))
            }
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
// H.264 implementation (openh264 software backend)
// ---------------------------------------------------------------------------

/// Video decoder for SPICE H.264 streams, backed by Cisco's
/// openh264 via the `openh264` crate (built from vendored source
/// — no runtime libopenh264 dependency).
///
/// Maintains the full per-stream H.264 codec context (SPS/PPS,
/// reference frame buffer, picture order, etc.) inside the wrapped
/// `openh264::decoder::Decoder`. The decoder is stateful: every
/// `STREAM_DATA` packet on a stream is fed in order so reference
/// frames remain valid.
///
/// `consecutive_failures` drives a two-stage escalation:
/// `H264_WARN_AFTER_CONSECUTIVE_FAILURES` failures in a row
/// promote the log from `debug!` to `warn!`, and
/// `H264_TERMINAL_CONSECUTIVE_FAILURES` of them latch the decoder
/// off entirely (see `terminal`). The counter resets on any
/// successful decode (including `Ok(None)`, which is a normal
/// "need more data" outcome — not an error).
pub struct H264VideoDecoder {
    decoder: openh264::decoder::Decoder,
    /// Caller-owned RGBA scratch buffer reused across frames to
    /// avoid reallocating on each decode. `DecodedYUV::write_rgba8`
    /// requires exactly `width * height * 4` bytes and panics
    /// otherwise, so we resize on dimension change.
    rgba_scratch: Vec<u8>,
    /// Number of consecutive failed decodes — both `Err` returns
    /// from [`openh264::decoder::Decoder::decode`] and frames we
    /// reject ourselves for implausible dimensions. Reset to zero
    /// on any successful decode (including the "not enough data
    /// yet" `Ok(None)` case).
    consecutive_failures: u32,
    /// Latched once `consecutive_failures` reaches
    /// `H264_TERMINAL_CONSECUTIVE_FAILURES`. Every later
    /// `decode` call returns the terminal error without touching
    /// the codec, so a stream that will never resync stops
    /// consuming CPU. Only a new decoder (a fresh
    /// `STREAM_CREATE`) clears it.
    terminal: bool,
    /// The SPICE wire convention we assume is Annex B framing (NALU start
    /// codes), but the assumption has not been validated against a real
    /// H.264-capable spice-server. On the first decode call per decoder
    /// instance, log the leading-byte pattern at INFO so a bug-report
    /// reader can tell whether the server is actually sending Annex B (`00
    /// 00 00 01` / `00 00 01`) or something else (AVCC length-prefixed,
    /// raw NALU, etc.). One log line per stream, then quiet.
    framing_logged: bool,
}

/// Number of consecutive H.264 decode errors before we escalate
/// the log level from `debug!` to `warn!`. Chosen to avoid noise
/// from single-packet glitches but surface persistent corruption.
const H264_WARN_AFTER_CONSECUTIVE_FAILURES: u32 = 3;

/// Number of consecutive H.264 decode errors after which the
/// decoder gives up on the stream and fails every subsequent
/// packet without invoking the codec.
///
/// A healthy stream recovers at the next key frame, so a run this
/// long — roughly two seconds at 30 fps, longer at the frame rates
/// SPICE streams typically run at — means the bitstream is not
/// going to resync. Without the latch a server can keep an
/// unbounded number of streams burning decode CPU on garbage for
/// as long as the session lasts, because nothing on the
/// `STREAM_DATA` path tears a stream down: the display channel
/// counts `frames_decode_failed` and keeps feeding packets in.
/// The latch is the decoder's own half of that policy; deciding to
/// destroy the stream is the display channel's call to make.
const H264_TERMINAL_CONSECUTIVE_FAILURES: u32 = 60;

impl H264VideoDecoder {
    /// Construct a new openh264-backed decoder.
    ///
    /// Maps any initialisation error from openh264 to
    /// [`VideoDecoderError::Decode`]. Initialisation failures are
    /// rare — openh264's own docs note this "should never error,
    /// but the underlying OpenH264 decoder has an error indication
    /// and since we don't know their code that well we just can't
    /// guarantee it."
    pub fn new() -> Result<Self, VideoDecoderError> {
        let decoder = openh264::decoder::Decoder::new()
            .map_err(|e| VideoDecoderError::Decode(format!("openh264 init: {e}")))?;
        Ok(Self {
            decoder,
            rgba_scratch: Vec::new(),
            consecutive_failures: 0,
            terminal: false,
            framing_logged: false,
        })
    }

    /// Record a failed decode and produce the error to return.
    ///
    /// Escalates the log level at
    /// `H264_WARN_AFTER_CONSECUTIVE_FAILURES` and latches
    /// `terminal` at `H264_TERMINAL_CONSECUTIVE_FAILURES`.
    fn note_failure(&mut self, msg: String) -> VideoDecoderError {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures == H264_WARN_AFTER_CONSECUTIVE_FAILURES {
            warn!(
                "H264 decode: {} consecutive failures; stream may be corrupted: {}",
                self.consecutive_failures, msg
            );
        }
        if self.consecutive_failures >= H264_TERMINAL_CONSECUTIVE_FAILURES && !self.terminal {
            self.terminal = true;
            warn!(
                "H264 decode: {} consecutive failures; giving up on this stream, \
                 all further packets will be rejected without decoding: {}",
                self.consecutive_failures, msg
            );
        }
        VideoDecoderError::Decode(msg)
    }
}

impl VideoDecoder for H264VideoDecoder {
    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedFrame>, VideoDecoderError> {
        // The stream has already failed for long enough to be
        // declared dead; reject the packet without touching the
        // codec.
        if self.terminal {
            return Err(VideoDecoderError::Decode(format!(
                "H264 decoder disabled after {} consecutive failures; \
                 the stream must be recreated to retry",
                self.consecutive_failures
            )));
        }

        // SPICE STREAM_DATA carries H.264 NAL units in Annex B
        // framing (start codes `00 00 00 01` between NALUs).
        // `openh264::decoder::Decoder::decode` accepts Annex B
        // payloads directly, so we pass the packet through
        // unmodified. The encoder side
        // (shakenfist-spice-renderer/src/encoder/h264.rs) also
        // emits Annex B, so the round-trip unit test below
        // exercises the same framing the real SPICE server uses.
        //
        // Log the leading-byte pattern of the first packet so a
        // bug-report reader can see whether the
        // server's wire framing matches the assumption. Quiet
        // after the first hit per decoder instance.
        if !self.framing_logged {
            self.framing_logged = true;
            let prefix: Vec<u8> = packet.iter().take(4).copied().collect();
            let kind = match prefix.as_slice() {
                [0x00, 0x00, 0x00, 0x01] => "Annex B (4-byte start code)",
                [0x00, 0x00, 0x01, _] => "Annex B (3-byte start code)",
                _ => "non-Annex-B (possibly AVCC length-prefixed or raw NALU — decode will likely fail)",
            };
            info!(
                "H264VideoDecoder: first packet ({} bytes) prefix={:02x?} — assumed framing: {}",
                packet.len(),
                prefix,
                kind,
            );
        }

        // The `DecodedYUV` returned by `decode` borrows the codec, so
        // nothing inside the match may take `&mut self` as a whole.
        // Failures therefore surface as a plain message and the
        // counter/latch bookkeeping happens once the borrow has ended.
        let outcome: Result<Option<DecodedFrame>, String> = match self.decoder.decode(packet) {
            // Decoder consumed the packet but has not yet assembled a
            // full picture (e.g. SPS/PPS only, or the IDR slice is
            // still missing). This is not an error — it counts as a
            // success for the failure counter.
            Ok(None) => Ok(None),
            Ok(Some(yuv)) => {
                use openh264::formats::YUVSource;

                let (w, h) = yuv.dimensions();
                // These come from the stream's SPS, which the server
                // controls. `checked_mul` alone only stops the multiply
                // wrapping — it still admits any allocation short of
                // `usize::MAX` — so cap the frame the way the JPEG
                // backends do before sizing the scratch buffer.
                let cap = MAX_DECODED_VIDEO_DIMENSION as usize;
                if w == 0 || h == 0 || w > cap || h > cap {
                    Err(format!(
                        "H264 implausible decoded dimensions {w}x{h} (cap {cap} per side)"
                    ))
                } else {
                    // The cap already rules the overflow out; the
                    // checked multiply is kept so the guard survives a
                    // future change to the cap.
                    match w.checked_mul(h).and_then(|n| n.checked_mul(4)) {
                        None => Err(format!("H264 decoded frame dimensions overflow: {w}x{h}")),
                        Some(buf_len) => {
                            if self.rgba_scratch.len() != buf_len {
                                self.rgba_scratch.resize(buf_len, 0);
                            }
                            yuv.write_rgba8(&mut self.rgba_scratch);
                            Ok(Some(DecodedFrame {
                                rgba: self.rgba_scratch.clone(),
                                width: u32::try_from(w).unwrap_or(u32::MAX),
                                height: u32::try_from(h).unwrap_or(u32::MAX),
                            }))
                        }
                    }
                }
            }
            Err(e) => Err(e.to_string()),
        };

        match outcome {
            Ok(frame) => {
                self.consecutive_failures = 0;
                Ok(frame)
            }
            Err(msg) => Err(self.note_failure(msg)),
        }
    }

    fn name(&self) -> &'static str {
        "H264 (openh264)"
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
        } else {
            // Any other marker is a length-prefixed segment: skip it.
            // The loop condition guarantees `jpeg[i + 2]` and
            // `jpeg[i + 3]` are in bounds, and `seg_len` is at least 2,
            // so `i` always advances.
            let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
            i += seg_len;
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
    fn for_stream_h264_returns_h264_decoder() {
        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(JpegDecoderRsDecoder::new());
        let result = for_stream(SPICE_VIDEO_CODEC_TYPE_H264, jpeg_dec);
        assert!(
            result.is_ok(),
            "expected Ok for H264 codec_type, got Err({:?})",
            result.err()
        );
        let dec = result.unwrap();
        assert_eq!(
            dec.name(),
            "H264 (openh264)",
            "H264VideoDecoder name should identify the openh264 backend"
        );
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

    // ------------------------------------------------------------------
    // Zero-length packet handling (both backends)
    // ------------------------------------------------------------------

    #[test]
    fn mjpeg_zero_length_packet_returns_decode_error() {
        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(JpegDecoderRsDecoder::new());
        let mut dec = MJpegVideoDecoder::new(jpeg_dec);
        match dec.decode(&[]) {
            Err(VideoDecoderError::Decode(msg)) => {
                assert!(!msg.is_empty(), "decode error should carry a message");
            }
            Err(e) => panic!("expected a Decode error for an empty packet, got {e}"),
            Ok(_) => panic!("an empty packet must not decode to a frame"),
        }
        assert!(
            dec.cached_dht.is_none(),
            "an empty packet carries no DHT to cache"
        );
    }

    #[test]
    fn h264_zero_length_packet_is_rejected_without_panic() {
        let mut dec = H264VideoDecoder::new().expect("H264VideoDecoder::new failed");
        // openh264 may either report an error or simply consume the
        // (empty) packet without producing a picture. Both are
        // acceptable; producing a frame or panicking is not.
        match dec.decode(&[]) {
            Ok(None) => {}
            Err(VideoDecoderError::Decode(msg)) => {
                assert!(!msg.is_empty(), "decode error should carry a message");
            }
            Err(e) => panic!("expected a Decode error for an empty packet, got {e}"),
            Ok(Some(_)) => panic!("an empty packet must not decode to a frame"),
        }
    }

    // ------------------------------------------------------------------
    // DHT cache poisoning (S1-10)
    // ------------------------------------------------------------------

    #[test]
    fn mjpeg_dht_from_failed_frame_is_not_cached() {
        let jpeg_dec: Arc<dyn JpegDecoder> = Arc::new(JpegDecoderRsDecoder::new());
        let mut dec = MJpegVideoDecoder::new(jpeg_dec);

        // A frame carrying a DHT segment but no image data: the decode
        // fails, so the tables it carried must not become the cache
        // every later DHT-less frame is decoded against.
        let mut frame = vec![0xFF, 0xD8]; // SOI
        frame.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x04, 0xAA, 0xBB]); // bogus DHT
        frame.extend_from_slice(&[0xFF, 0xD9]); // EOI
        assert!(
            !extract_dht_segments(&frame).is_empty(),
            "fixture must actually carry a DHT segment"
        );

        assert!(
            dec.decode(&frame).is_err(),
            "a JPEG with no image data should fail to decode"
        );
        assert!(
            dec.cached_dht.is_none(),
            "a DHT from a frame that failed to decode must not be cached"
        );
    }

    // ------------------------------------------------------------------
    // H.264 terminal-failure latch (S1-6)
    // ------------------------------------------------------------------

    #[test]
    fn h264_latches_terminal_after_threshold_failures() {
        let mut dec = H264VideoDecoder::new().expect("H264VideoDecoder::new failed");

        // Drive the policy directly rather than through the codec:
        // whether a given garbage packet makes openh264 return `Err`
        // or `Ok(None)` varies with the build, and `Ok(None)` resets
        // the counter.
        for _ in 0..H264_TERMINAL_CONSECUTIVE_FAILURES {
            let _ = dec.note_failure("synthetic failure".to_string());
        }
        assert!(
            dec.terminal,
            "decoder should latch off after {H264_TERMINAL_CONSECUTIVE_FAILURES} failures"
        );

        match dec.decode(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88]) {
            Err(VideoDecoderError::Decode(msg)) => {
                assert!(
                    msg.contains("disabled"),
                    "latched decoder should report the terminal error, got {msg}"
                );
            }
            Err(e) => panic!("expected the terminal Decode error, got {e}"),
            Ok(_) => panic!("a latched decoder must reject every packet"),
        }
        assert_eq!(
            dec.consecutive_failures, H264_TERMINAL_CONSECUTIVE_FAILURES,
            "a latched decoder must not invoke the codec again"
        );
    }
}

// ---------------------------------------------------------------------------
// MJpegVideoDecoder round-trip test (requires mozjpeg feature).
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "mozjpeg"))]
mod mjpeg_round_trip_tests {
    use super::*;
    use crate::jpeg::MozJpegDecoder;

    /// Encode a small frame via `mozjpeg::Compress`, then decode it through
    /// `MJpegVideoDecoder`, and assert the RGBA is within JPEG-lossy tolerance.
    /// Mirrors the `mozjpeg_round_trip_within_tolerance` test in `jpeg.rs`, but
    /// exercises the `VideoDecoder` trait wrapper.
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

// ---------------------------------------------------------------------------
// H264VideoDecoder round-trip test
// ---------------------------------------------------------------------------
//
// Encodes a small RGBA fixture via `openh264::encoder::Encoder`
// (the same crate the real renderer uses in
// `shakenfist-spice-renderer/src/encoder/h264.rs`) and feeds the
// emitted Annex-B NAL units back through `H264VideoDecoder`.
// Asserts the decoded RGBA is within H.264-lossy tolerance.
//
// We encode TWO frames of the same content and decode all NAL
// units in order, because:
//   1. The first frame is an IDR carrying SPS+PPS, which the
//      decoder needs before any picture can be produced.
//   2. openh264's `decode_frame_no_delay` is happy to return the
//      decoded IDR on the first call, but feeding a second access
//      unit lets the round-trip exercise the steady-state path
//      that real SPICE streams hit (most frames are non-IDR).
//   3. Asserting against the second decoded frame gives motion
//      estimation a chance to align with the source, keeping the
//      tolerance reasonable.
//
// Tolerances target H.264 at low resolution: per-channel max diff
// up to 50, mean up to 15. H.264 with no rate control hint at
// 64x64 is unusually lossy compared to 1080p video — these
// numbers are calibrated to the worst case, not the typical one.
#[cfg(test)]
mod h264_round_trip_tests {
    use super::*;

    /// Build a 64×64 RGBA quadrant fixture: four solid colours,
    /// one per corner. Width is a multiple of 8 so the YUV→RGBA
    /// converter hits its f32x8 fast path on the decode side; the
    /// dimensions are H.264 macroblock-aligned (16-pixel
    /// multiples) which keeps the encoder out of edge-padding
    /// pathology.
    fn quadrant_fixture() -> (u32, u32, Vec<u8>) {
        const W: u32 = 64;
        const H: u32 = 64;
        let mut buf = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            for x in 0..W {
                let (r, g, b) = match (x < W / 2, y < H / 2) {
                    (true, true) => (220, 30, 30),    // top-left red
                    (false, true) => (30, 220, 30),   // top-right green
                    (true, false) => (30, 30, 220),   // bottom-left blue
                    (false, false) => (220, 220, 30), // bottom-right yellow
                };
                buf.push(r);
                buf.push(g);
                buf.push(b);
                buf.push(255);
            }
        }
        (W, H, buf)
    }

    /// Encode `rgba` via `openh264::encoder::Encoder` and return
    /// the concatenated Annex-B byte stream produced by the
    /// encoder for that frame. Mirrors the encoder-side flow in
    /// `shakenfist-spice-renderer/src/encoder/h264.rs`.
    fn encode_one_frame(
        enc: &mut openh264::encoder::Encoder,
        rgba: &[u8],
        w: u32,
        h: u32,
    ) -> Vec<u8> {
        use openh264::formats::{RgbaSliceU8, YUVBuffer};

        let rgba_slice = RgbaSliceU8::new(rgba, (w as usize, h as usize));
        let yuv = YUVBuffer::from_rgb_source(rgba_slice);
        let bitstream = enc.encode(&yuv).expect("openh264 encode failed");

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
        raw
    }

    #[test]
    fn h264_round_trip_within_tolerance() {
        let (w, h, src) = quadrant_fixture();

        // Encode two frames of identical content. The first is an
        // implicit IDR (openh264 default); the second is a P-frame
        // that closely tracks the reference, which is the
        // steady-state case for SPICE STREAM_DATA.
        let mut enc = openh264::encoder::Encoder::new().expect("openh264 encoder init failed");
        let frame1 = encode_one_frame(&mut enc, &src, w, h);
        let frame2 = encode_one_frame(&mut enc, &src, w, h);
        assert!(
            !frame1.is_empty(),
            "encoder produced empty bitstream for frame 1"
        );
        assert!(
            !frame2.is_empty(),
            "encoder produced empty bitstream for frame 2"
        );

        let mut dec = H264VideoDecoder::new().expect("H264VideoDecoder::new failed");

        // First access unit: contains SPS+PPS+IDR slice. Feed the
        // entire access unit as one packet (matches how SPICE
        // delivers it).
        let first = dec.decode(&frame1).expect("first decode returned Err");
        assert!(
            first.is_some(),
            "first IDR access unit should decode to a frame"
        );

        // Second access unit: P-frame. The decoder may have
        // returned the IDR on the first call, so the second call's
        // output is the picture we compare against the source.
        let second = dec
            .decode(&frame2)
            .expect("second decode returned Err")
            .unwrap_or_else(|| {
                // If the encoder buffers the P-frame's output one
                // call further, fall back to the first frame for
                // comparison. Encoders should not, but this keeps
                // the test resilient to crate-version flushing
                // changes.
                first.expect("second decode returned None AND first was None")
            });

        assert_eq!(second.width, w, "decoded width mismatch");
        assert_eq!(second.height, h, "decoded height mismatch");
        assert_eq!(
            second.rgba.len(),
            (w * h * 4) as usize,
            "decoded RGBA length mismatch"
        );

        let mut max_diff: u32 = 0;
        let mut sum_diff: u64 = 0;
        let mut sample_count: u64 = 0;
        for (i, (a, b)) in src.iter().zip(second.rgba.iter()).enumerate() {
            if i % 4 == 3 {
                // openh264 writes alpha = 255 explicitly in
                // `write_rgba8` for the YUV420 → RGBA conversion.
                assert_eq!(*b, 255, "alpha at byte {i} should be 255, got {b}");
                continue;
            }
            let diff = (i32::from(*a) - i32::from(*b)).unsigned_abs();
            max_diff = max_diff.max(diff);
            sum_diff += u64::from(diff);
            sample_count += 1;
        }
        let mean_diff = sum_diff as f64 / sample_count as f64;
        eprintln!("h264 round-trip: max_diff={max_diff} mean_diff={mean_diff:.2}");
        assert!(
            max_diff <= 50,
            "per-channel max diff {max_diff} exceeds tolerance 50"
        );
        assert!(
            mean_diff <= 15.0,
            "per-channel mean diff {mean_diff:.2} exceeds tolerance 15.0"
        );

        // After two successful decodes, the consecutive-failure
        // counter should be zero. Verifying this here keeps the
        // Q5 escalation behaviour exercised end-to-end without a
        // separate test that constructs an invalid bitstream
        // (which is fiddly to do reliably across openh264 builds).
        assert_eq!(
            dec.consecutive_failures, 0,
            "consecutive_failures should reset on successful decode"
        );
    }

    #[test]
    fn h264_consecutive_failures_increments_on_error() {
        // Feed obviously-invalid bytes (no NAL start codes) and
        // confirm the failure counter advances. Three errors in a
        // row triggers the warn-level log (asserted by inspection
        // — capturing tracing output across versions is brittle).
        let mut dec = H264VideoDecoder::new().expect("H264VideoDecoder::new failed");
        let garbage = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];

        let mut error_count = 0u32;
        for _ in 0..4 {
            if dec.decode(&garbage).is_err() {
                error_count += 1;
            }
        }
        assert!(
            error_count > 0,
            "expected at least one decode error on garbage input"
        );
        assert!(
            dec.consecutive_failures > 0,
            "consecutive_failures should be > 0 after error returns"
        );
    }
}
