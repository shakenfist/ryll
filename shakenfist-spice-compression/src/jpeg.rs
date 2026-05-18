//! JPEG decoder abstraction for SPICE MJPEG stream frames.
//!
//! Provides a [`JpegDecoder`] trait with a single
//! `decode(&[u8]) -> Option<DecodedJpeg>` method so the
//! display channel can swap decoder backends at runtime
//! without changing call sites. The active backend is
//! chosen once at session start by [`best_for_platform`]
//! and stored as `Arc<dyn JpegDecoder>` on the channel.
//!
//! [`JpegDecoderRsDecoder`] (wrapping the pure-Rust
//! `jpeg-decoder` crate) is the universal fallback.
//! [`MozJpegDecoder`] (wrapping `mozjpeg` with a vendored
//! SIMD-accelerated libjpeg-turbo build) is the
//! cross-platform baseline preferred ahead of it. Future
//! steps (3C–3E) add `ImageIoDecoder`, `WicDecoder`, and
//! `VaapiDecoder` to this module.

use std::sync::Arc;

use tracing::{info, warn};

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

/// SIMD-accelerated JPEG decoder backed by the `mozjpeg` crate
/// (vendored libjpeg-turbo).
///
/// This is the cross-platform baseline: it runs on every
/// platform and is significantly faster than the pure-Rust
/// `jpeg-decoder` for typical MJPEG frames. Selected ahead of
/// [`JpegDecoderRsDecoder`] whenever the `mozjpeg` feature is
/// enabled (the default).
///
/// libjpeg's error handling uses `setjmp`/`longjmp`, which the
/// Rust binding surfaces as panics. Every entry point into the
/// crate is therefore wrapped in `std::panic::catch_unwind` so
/// a malformed frame can't unwind across the FFI boundary and
/// take the channel task with it.
#[cfg(feature = "mozjpeg")]
pub struct MozJpegDecoder;

#[cfg(feature = "mozjpeg")]
impl MozJpegDecoder {
    pub fn new() -> Self {
        MozJpegDecoder
    }
}

#[cfg(feature = "mozjpeg")]
impl Default for MozJpegDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "mozjpeg")]
impl JpegDecoder for MozJpegDecoder {
    fn decode(&self, data: &[u8]) -> Option<DecodedJpeg> {
        // libjpeg's error handler longjmps; the `mozjpeg` crate
        // turns that into a Rust panic. Catch it so a malformed
        // frame returns None rather than crashing the channel.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let decomp = mozjpeg::Decompress::new_mem(data).ok()?;
            let width = decomp.width() as u32;
            let height = decomp.height() as u32;
            // Bound the allocation: 65535×65535 is the JPEG
            // maximum; a frame much larger than that is almost
            // certainly malformed and we'd rather drop it than
            // try to allocate a gigabyte of RGBA buffer.
            if width == 0 || height == 0 || width > 65535 || height > 65535 {
                warn!(
                    "MozJpegDecoder: implausible dimensions {}x{}, dropping frame",
                    width, height
                );
                return None;
            }
            let mut started = decomp.rgba().ok()?;
            // `read_scanlines::<[u8; 4]>` returns one element
            // per pixel; flatten into the RGBA byte buffer the
            // trait contract requires.
            let pixels: Vec<[u8; 4]> = started.read_scanlines().ok()?;
            started.finish().ok()?;
            let expected = (width as usize) * (height as usize);
            if pixels.len() != expected {
                warn!(
                    "MozJpegDecoder: scanline count {} != expected {} for {}x{}",
                    pixels.len(),
                    expected,
                    width,
                    height
                );
                return None;
            }
            let mut rgba = Vec::with_capacity(expected * 4);
            for px in &pixels {
                rgba.extend_from_slice(px);
            }
            Some(DecodedJpeg {
                rgba,
                width,
                height,
            })
        }));
        match result {
            Ok(opt) => opt,
            Err(_) => {
                warn!(
                    "MozJpegDecoder: panic during decode, data_len={}, header={:02x?}",
                    data.len(),
                    &data[..data.len().min(16)]
                );
                None
            }
        }
    }

    fn name(&self) -> &'static str {
        "libjpeg-turbo"
    }
}

/// Select the best available JPEG decoder for this platform.
///
/// Today the chain is:
///
/// ```text
///   macOS   → ImageIoDecoder → MozJpegDecoder → JpegDecoderRsDecoder
///   Windows → WicDecoder     → MozJpegDecoder → JpegDecoderRsDecoder
///   Linux   → VaapiDecoder*  → MozJpegDecoder → JpegDecoderRsDecoder
///   other   →                  MozJpegDecoder → JpegDecoderRsDecoder
/// ```
///
/// Step 3B implements `MozJpegDecoder` and the `JpegDecoderRsDecoder`
/// fallback. The OS-specific decoders (ImageIO/WIC/VA-API) land in
/// steps 3C–3E. `MozJpegDecoder` is only present when the
/// `mozjpeg` Cargo feature is enabled (defaulted on); building
/// without it falls back to the pure-Rust path.
///
/// The result is constructed once at session start
/// (`DisplayChannel::new`) and stored as `Arc<dyn JpegDecoder>`.
/// No re-probing happens mid-session.
///
/// Logs the selected backend at INFO so bug reports' console
/// captures show which path the session ran.
pub fn best_for_platform() -> Arc<dyn JpegDecoder> {
    let decoder: Arc<dyn JpegDecoder> = {
        #[cfg(feature = "mozjpeg")]
        {
            Arc::new(MozJpegDecoder::new())
        }
        #[cfg(not(feature = "mozjpeg"))]
        {
            Arc::new(JpegDecoderRsDecoder::new())
        }
    };
    info!("MJPEG decoder backend selected: {}", decoder.name());
    decoder
}

#[cfg(all(test, feature = "mozjpeg"))]
mod mozjpeg_tests {
    use super::*;

    /// Encode a 16×16 RGBA pattern via `mozjpeg::Compress`,
    /// then decode it through `MozJpegDecoder`, and assert
    /// the output is within JPEG-lossy tolerance.
    ///
    /// The pattern is a smooth diagonal gradient with the
    /// red, green and blue channels each moving on a
    /// different axis combination. This keeps high-frequency
    /// content out of the image (so JPEG quantisation /
    /// chroma subsampling don't dominate the error) while
    /// still loading every channel with non-trivial signal
    /// — a channel swap (R↔B) would still fail the per-pixel
    /// diff assertion catastrophically.
    #[test]
    fn mozjpeg_round_trip_within_tolerance() {
        const W: u32 = 16;
        const H: u32 = 16;
        // Build the source RGBA pattern. Each channel is a
        // smooth function so JPEG quantisation only introduces
        // a small per-pixel error.
        let mut src = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            for x in 0..W {
                // Scale x,y from 0..16 → 0..255-ish.
                let r = ((x * 255) / (W - 1)) as u8;
                let g = ((y * 255) / (H - 1)) as u8;
                let b = (((x + y) * 255) / (W + H - 2)) as u8;
                src.push(r);
                src.push(g);
                src.push(b);
                src.push(255);
            }
        }

        // Encode via mozjpeg. JCS_EXT_RGBA tells libjpeg the
        // input rows are 4-byte RGBA (alpha ignored on encode).
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

        // Decode via MozJpegDecoder.
        let decoder = MozJpegDecoder::new();
        let decoded = decoder
            .decode(&jpeg_bytes)
            .expect("MozJpegDecoder returned None on round-trip");
        assert_eq!(decoded.width, W, "decoded width mismatch");
        assert_eq!(decoded.height, H, "decoded height mismatch");
        assert_eq!(
            decoded.rgba.len(),
            (W * H * 4) as usize,
            "decoded buffer length wrong",
        );

        // Compare per-channel. JPEG at quality 75 keeps each
        // channel close to the original, but chroma subsampling
        // can introduce edge ringing. Tolerances: per-channel
        // max diff ≤ 20, mean ≤ 5. The opaque alpha channel is
        // synthesised by the decoder and must be exactly 255.
        let mut max_diff: u32 = 0;
        let mut sum_diff: u64 = 0;
        let mut sample_count: u64 = 0;
        for (i, (a, b)) in src.iter().zip(decoded.rgba.iter()).enumerate() {
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
        // The metrics are useful for tuning the tolerance; surface
        // them via the test harness's captured stdout so a failure
        // tells the operator what changed.
        eprintln!("mozjpeg round-trip: max_diff={max_diff} mean_diff={mean_diff:.2}");
        assert!(
            max_diff <= 20,
            "per-channel max diff {max_diff} exceeds tolerance 20 \
             (would also catch channel swaps)"
        );
        assert!(
            mean_diff <= 5.0,
            "per-channel mean diff {mean_diff:.2} exceeds tolerance 5.0"
        );

        // Channel-order sanity check independent of the per-pixel
        // tolerance: in the source the R channel grows with x and
        // the G channel grows with y. If the decoder swapped the
        // channels (the bug class this test exists to catch), the
        // mean of the top row's red would not exceed the mean of
        // the bottom row's red. Use the corner samples for a tight
        // signal.
        let pix = |x: u32, y: u32| -> [u8; 4] {
            let i = ((y * W + x) * 4) as usize;
            [
                decoded.rgba[i],
                decoded.rgba[i + 1],
                decoded.rgba[i + 2],
                decoded.rgba[i + 3],
            ]
        };
        let tl = pix(0, 0);
        let tr = pix(W - 1, 0);
        let bl = pix(0, H - 1);
        assert!(
            tr[0] > tl[0] + 100,
            "R should grow with x: top-left R={} top-right R={}",
            tl[0],
            tr[0],
        );
        assert!(
            bl[1] > tl[1] + 100,
            "G should grow with y: top-left G={} bottom-left G={}",
            tl[1],
            bl[1],
        );
    }

    #[test]
    fn mozjpeg_empty_input_returns_none() {
        let decoder = MozJpegDecoder::new();
        assert!(decoder.decode(&[]).is_none());
    }

    #[test]
    fn mozjpeg_truncated_input_returns_none() {
        let decoder = MozJpegDecoder::new();
        // SOI + APP0 stub, nothing more — not a valid JPEG.
        assert!(decoder.decode(&[0xFF, 0xD8, 0xFF, 0xE0]).is_none());
    }

    #[test]
    fn mozjpeg_decoder_name_is_libjpeg_turbo() {
        let decoder = MozJpegDecoder::new();
        assert_eq!(decoder.name(), "libjpeg-turbo");
    }
}
