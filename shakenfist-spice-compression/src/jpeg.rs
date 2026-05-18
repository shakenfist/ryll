//! JPEG decoder abstraction for SPICE MJPEG stream frames.
//!
//! Provides a [`JpegDecoder`] trait with a single
//! `decode(&[u8]) -> Option<DecodedJpeg>` method so the
//! display channel can swap decoder backends at runtime
//! without changing call sites. The active backend is
//! chosen once at session start by [`best_for_platform`]
//! and stored as `Arc<dyn JpegDecoder>` on the channel.
//!
//! Currently only [`JpegDecoderRsDecoder`] (wrapping the
//! pure-Rust `jpeg-decoder` crate) is implemented. Future
//! steps (3B–3E) add `MozJpegDecoder`, `ImageIoDecoder`,
//! `WicDecoder`, and `VaapiDecoder` to this module.

use std::sync::Arc;

use tracing::warn;

/// A decoded JPEG frame: RGBA pixels plus width/height.
///
/// Using a named struct rather than a tuple lets each backend
/// validate dimensions before allocating the RGBA buffer —
/// a guard against runaway sizes from malformed frames.
pub struct DecodedJpeg {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Stateless JPEG decoder.
///
/// Implementations must be `Send + Sync` because the selected
/// backend is shared across the channel task via `Arc<dyn
/// JpegDecoder>`. Decoding is inherently stateless (each call
/// gets the full JPEG byte stream including SOI/EOI markers,
/// with DHT injection already performed by the caller) so
/// `&self` is the correct receiver.
pub trait JpegDecoder: Send + Sync {
    /// Decode `data` (a full JPEG byte stream) into RGBA pixels.
    ///
    /// The input is the same shape as the old `decode_mjpeg_frame`
    /// function: a complete JPEG including SOI/EOI markers, with
    /// DHT tables already injected by the caller if needed.
    ///
    /// Returns `None` on any decode failure so the caller can
    /// increment the drop counter and continue.
    fn decode(&self, data: &[u8]) -> Option<DecodedJpeg>;

    /// Human-readable backend name surfaced in bug reports.
    ///
    /// Examples: `"ImageIO"`, `"WIC"`, `"VA-API"`,
    /// `"libjpeg-turbo"`, `"jpeg-decoder"`.
    fn name(&self) -> &'static str;
}

/// Pure-Rust JPEG decoder backed by the `jpeg-decoder` crate.
///
/// This is the universal fallback backend: it runs on every
/// platform and matches the behaviour of the original
/// `decode_mjpeg_frame` function exactly.
pub struct JpegDecoderRsDecoder;

impl JpegDecoderRsDecoder {
    pub fn new() -> Self {
        JpegDecoderRsDecoder
    }
}

impl Default for JpegDecoderRsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegDecoder for JpegDecoderRsDecoder {
    fn decode(&self, data: &[u8]) -> Option<DecodedJpeg> {
        let mut decoder = jpeg_decoder::Decoder::new(data);
        let pixels = match decoder.decode() {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "MJPEG decode error: {}, data_len={}, header={:02x?}",
                    e,
                    data.len(),
                    &data[..data.len().min(16)]
                );
                return None;
            }
        };
        let info = decoder.info()?;
        let width = info.width as u32;
        let height = info.height as u32;

        let rgba = match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => {
                let mut out = Vec::with_capacity(pixels.len() * 4 / 3);
                for chunk in pixels.chunks(3) {
                    out.push(chunk[0]);
                    out.push(chunk[1]);
                    out.push(chunk[2]);
                    out.push(255);
                }
                out
            }
            jpeg_decoder::PixelFormat::L8 => {
                let mut out = Vec::with_capacity(pixels.len() * 4);
                for &gray in &pixels {
                    out.push(gray);
                    out.push(gray);
                    out.push(gray);
                    out.push(255);
                }
                out
            }
            other => {
                warn!("MJPEG decode: unsupported pixel format {:?}", other);
                return None;
            }
        };

        Some(DecodedJpeg {
            rgba,
            width,
            height,
        })
    }

    fn name(&self) -> &'static str {
        "jpeg-decoder"
    }
}

/// Select the best available JPEG decoder for this platform.
///
/// Currently always returns [`JpegDecoderRsDecoder`]. Future
/// steps (3B–3E) will insert faster backends ahead of it:
///
/// ```text
///   macOS   → ImageIoDecoder → MozJpegDecoder → JpegDecoderRsDecoder
///   Windows → WicDecoder     → MozJpegDecoder → JpegDecoderRsDecoder
///   Linux   → VaapiDecoder*  → MozJpegDecoder → JpegDecoderRsDecoder
///   other   →                  MozJpegDecoder → JpegDecoderRsDecoder
/// ```
///
/// The result is constructed once at session start
/// (`DisplayChannel::new`) and stored as `Arc<dyn JpegDecoder>`.
/// No re-probing happens mid-session.
pub fn best_for_platform() -> Arc<dyn JpegDecoder> {
    Arc::new(JpegDecoderRsDecoder::new())
}
