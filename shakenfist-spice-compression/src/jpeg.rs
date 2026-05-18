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

/// macOS ImageIO-backed JPEG decoder.
///
/// Wraps `CGImageSourceCreateWithData` →
/// `CGImageSourceCreateImageAtIndex` → a `CGBitmapContext`
/// configured for true RGBA, and reads pixels back via
/// `CGBitmapContextGetData`.
///
/// # Pixel format
///
/// The bitmap context is `kCGImageAlphaPremultipliedLast |
/// kCGBitmapByteOrder32Big` so the bytes the context writes are
/// (R, G, B, A) in that order, regardless of host endianness. The
/// little-endian alternative (`kCGBitmapByteOrder32Little`) would
/// silently produce BGRA on every Mac (since macs are all
/// little-endian), which is exactly the "all images look blue"
/// failure mode this comment exists to head off.
///
/// # Why a fresh bitmap context per decode
///
/// `CGImage`s carry their own color space and channel order, which
/// can vary frame-to-frame (server-side colour conversion, embedded
/// ICC profiles, etc.). Drawing through a context we own forces
/// CoreGraphics to do the conversion to our chosen pixel format —
/// that's exactly the gotcha the comment above warns about.
#[cfg(target_os = "macos")]
pub struct ImageIoDecoder;

#[cfg(target_os = "macos")]
impl ImageIoDecoder {
    /// Construct a decoder. Returns `Some` unconditionally on macOS
    /// (ImageIO is part of the OS, so unlike libva or thumbnail
    /// codecs there is no probe step that can fail). The
    /// `Option`-returning shape matches the planned signatures of
    /// the other platform decoders so `best_for_platform` can fall
    /// through uniformly if a future failure mode appears.
    pub fn try_new() -> Option<Self> {
        Some(ImageIoDecoder)
    }
}

#[cfg(target_os = "macos")]
impl Default for ImageIoDecoder {
    fn default() -> Self {
        ImageIoDecoder
    }
}

#[cfg(target_os = "macos")]
impl JpegDecoder for ImageIoDecoder {
    fn decode(&self, data: &[u8]) -> Option<DecodedJpeg> {
        use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
        use objc2_core_graphics::{
            CGBitmapContextCreate, CGColorSpaceCreateDeviceRGB, CGContext, CGImageAlphaInfo,
            CGImageByteOrderInfo, CGImageGetHeight, CGImageGetWidth,
        };
        use objc2_image_io::CGImageSource;

        // Reject empty input early so we don't allocate a CFData
        // for a frame that's obviously not a JPEG.
        if data.is_empty() {
            return None;
        }

        // CFData::from_bytes copies, so the slice doesn't need to
        // outlive the resulting CFRetained.
        let cf_data = CFData::from_bytes(data);

        // Safety: with_data is unsafe because `options` generics
        // must be of the correct type; we pass None so there's no
        // generic-typed dictionary to get wrong.
        let source = unsafe { CGImageSource::with_data(&cf_data, None) }?;

        // Safety: count and image_at_index are unsafe for the same
        // reason (no typed options dict).
        if unsafe { source.count() } == 0 {
            warn!("ImageIoDecoder: CGImageSource produced 0 frames");
            return None;
        }
        let cg_image = unsafe { source.image_at_index(0, None) }?;

        let width = CGImageGetWidth(Some(&cg_image));
        let height = CGImageGetHeight(Some(&cg_image));

        // Bound the allocation: same defensive check as
        // MozJpegDecoder. 65535x65535 is the JPEG maximum; anything
        // larger is almost certainly malformed.
        if width == 0 || height == 0 || width > 65535 || height > 65535 {
            warn!(
                "ImageIoDecoder: implausible dimensions {}x{}, dropping frame",
                width, height
            );
            return None;
        }
        let width_u32 = width as u32;
        let height_u32 = height as u32;
        let bytes_per_row = width * 4;
        let buf_len = bytes_per_row * height;

        // Pre-allocate zeroed RGBA buffer that the bitmap context
        // will paint into. The context borrows the pointer for the
        // duration of its lifetime; we keep `rgba` alive until we
        // drop the context below.
        let mut rgba = vec![0u8; buf_len];

        let color_space = CGColorSpaceCreateDeviceRGB()?;

        // Pixel format bits: PremultipliedLast (alpha trailing) +
        // 32-bit big-endian byte order = bytes are (R, G, B, A)
        // in memory. See struct docstring for why this isn't
        // OrderDefault or Order32Little.
        let bitmap_info =
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;

        // Safety: CGBitmapContextCreate is unsafe because `data`
        // must remain valid for the context's lifetime. We hold
        // `rgba` until after the context is dropped below.
        let context = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast::<core::ffi::c_void>(),
                width,
                height,
                8,
                bytes_per_row,
                Some(&color_space),
                bitmap_info,
            )
        }?;

        // Draw the image into the context at (0,0) at its full size.
        // CoreGraphics handles the colour conversion from whatever
        // the JPEG's native colour space is into our DeviceRGB.
        let rect = CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(width as f64, height as f64),
        );
        CGContext::draw_image(Some(&context), rect, Some(&cg_image));

        // Drop the context so the borrowed `rgba.as_mut_ptr()` is
        // no longer held. (Strictly speaking the lifetime is over
        // when `context` goes out of scope at end-of-function, but
        // dropping explicitly here makes the dataflow obvious.)
        drop(context);
        drop(color_space);
        drop(cg_image);
        drop(source);
        drop(cf_data);

        Some(DecodedJpeg {
            rgba,
            width: width_u32,
            height: height_u32,
        })
    }

    fn name(&self) -> &'static str {
        "ImageIO"
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
/// Steps 3A–3C have landed: pure-Rust + libjpeg-turbo
/// cross-platform, plus ImageIO on macOS. WIC and VA-API land in
/// steps 3D–3E. `MozJpegDecoder` is only present when the
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
    // macOS: prefer ImageIO (Apple Silicon dedicated media block).
    // try_new() can't fail today but the Option-returning shape
    // mirrors the planned VA-API/WIC selectors so cascading falls
    // out uniformly if a future failure mode appears.
    #[cfg(target_os = "macos")]
    if let Some(d) = ImageIoDecoder::try_new() {
        let decoder: Arc<dyn JpegDecoder> = Arc::new(d);
        info!("MJPEG decoder backend selected: {}", decoder.name());
        return decoder;
    }

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

/// Tests for the macOS ImageIO decoder backend. These compile and
/// run only on macOS — Linux/Windows CI never executes them. The
/// real cross-platform smoke test lives in step 3H of the phase 3
/// plan and is operator-driven.
///
/// The fixture is a 32x32 JPEG with four 16x16 quadrants painted
/// red / green / blue / yellow, generated at quality 85 by
/// `tools/gen-swatches-jpeg`. JPEG colour space conversion and
/// chroma subsampling smear the swatch edges, so the per-pixel
/// asserts sample near each quadrant's centre (well clear of the
/// boundary) and use a generous tolerance of ±35.
#[cfg(all(test, target_os = "macos"))]
mod imageio_tests {
    use super::*;

    /// The fixture JPEG. Generated once via `tools/gen-swatches-jpeg`
    /// and committed under `tests/fixtures/` so the test is
    /// reproducible without re-encoding.
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/swatches.jpg");
    const FIXTURE_W: u32 = 32;
    const FIXTURE_H: u32 = 32;

    /// Per-channel tolerance for the centre-of-quadrant samples.
    /// JPEG quality 85 with chroma subsampling shifts the pure
    /// primary colours by up to ~30 in the worst case (yellow
    /// quadrant, blue channel) so 35 is comfortable.
    const TOL: i32 = 35;

    fn pixel(decoded: &DecodedJpeg, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * decoded.width + x) * 4) as usize;
        [
            decoded.rgba[i],
            decoded.rgba[i + 1],
            decoded.rgba[i + 2],
            decoded.rgba[i + 3],
        ]
    }

    fn near(actual: u8, expected: u8, label: &str) {
        let diff = (actual as i32 - expected as i32).abs();
        assert!(
            diff <= TOL,
            "{label}: got {actual}, expected ~{expected} (diff {diff} > tol {TOL})"
        );
    }

    #[test]
    fn imageio_decodes_swatches_in_rgba_order() {
        let decoder = ImageIoDecoder::try_new().expect("ImageIoDecoder::try_new returned None");
        assert_eq!(decoder.name(), "ImageIO");

        let decoded = decoder
            .decode(FIXTURE)
            .expect("ImageIoDecoder returned None on fixture");
        assert_eq!(decoded.width, FIXTURE_W, "width mismatch");
        assert_eq!(decoded.height, FIXTURE_H, "height mismatch");
        assert_eq!(
            decoded.rgba.len(),
            (FIXTURE_W * FIXTURE_H * 4) as usize,
            "rgba buffer length wrong",
        );

        // Sample the centre of each quadrant (offset 8 from each
        // edge — 8 pixels into a 16-pixel quadrant). If the byte
        // order were Order32Little instead of Order32Big, every
        // channel here would be wrong (R↔B swap), so this test
        // catches the canonical "everything is blue" bug.
        let tl = pixel(&decoded, 8, 8); // expected red    (255, 0,   0,   255)
        let tr = pixel(&decoded, 24, 8); // expected green  (0,   255, 0,   255)
        let bl = pixel(&decoded, 8, 24); // expected blue   (0,   0,   255, 255)
        let br = pixel(&decoded, 24, 24); // expected yellow (255, 255, 0,   255)

        near(tl[0], 255, "TL.R");
        near(tl[1], 0, "TL.G");
        near(tl[2], 0, "TL.B");
        assert_eq!(tl[3], 255, "TL.A should be 255 (opaque)");

        near(tr[0], 0, "TR.R");
        near(tr[1], 255, "TR.G");
        near(tr[2], 0, "TR.B");
        assert_eq!(tr[3], 255, "TR.A should be 255 (opaque)");

        near(bl[0], 0, "BL.R");
        near(bl[1], 0, "BL.G");
        near(bl[2], 255, "BL.B");
        assert_eq!(bl[3], 255, "BL.A should be 255 (opaque)");

        near(br[0], 255, "BR.R");
        near(br[1], 255, "BR.G");
        near(br[2], 0, "BR.B");
        assert_eq!(br[3], 255, "BR.A should be 255 (opaque)");
    }

    #[test]
    fn imageio_empty_input_returns_none() {
        let decoder = ImageIoDecoder::try_new().unwrap();
        assert!(decoder.decode(&[]).is_none());
    }

    #[test]
    fn imageio_truncated_input_returns_none() {
        let decoder = ImageIoDecoder::try_new().unwrap();
        // SOI + APP0 stub, nothing more — not a valid JPEG.
        assert!(decoder.decode(&[0xFF, 0xD8, 0xFF, 0xE0]).is_none());
    }

    #[test]
    fn imageio_garbage_input_returns_none() {
        let decoder = ImageIoDecoder::try_new().unwrap();
        // 32 bytes of "definitely not a JPEG".
        let junk: Vec<u8> = (0..32u8).collect();
        assert!(decoder.decode(&junk).is_none());
    }

    #[test]
    fn imageio_decoder_name_is_imageio() {
        let decoder = ImageIoDecoder::try_new().unwrap();
        assert_eq!(decoder.name(), "ImageIO");
    }
}
