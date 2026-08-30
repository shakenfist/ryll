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

#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
use tracing::debug;
use tracing::{info, warn};

/// A decoded JPEG frame: RGBA pixels plus width/height.
///
/// Every backend upholds the same invariant: `width` and
/// `height` are non-zero and no greater than
/// [`MAX_DECODED_JPEG_DIMENSION`], and `rgba.len()` is exactly
/// `width * height * 4` — which is what lets consumers index
/// `rgba` from the dimensions without re-checking.
///
/// Build one through [`DecodedJpeg::zeroed`] (backends that
/// paint into a buffer we hand them) or
/// [`DecodedJpeg::from_rgba`] (backends that hand us a finished
/// buffer). Both enforce the invariant, so the guard against a
/// hostile frame header lives in one platform-independent place
/// rather than being retyped inside each `cfg`-gated backend
/// where no CI leg can reach it. The fields stay public because
/// downstream crates destructure the struct.
pub struct DecodedJpeg {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Upper bound on per-side decoded JPEG dimension. 16384 leaves
/// headroom for >8K displays (7680×4320 fits comfortably; one
/// side at 16K just fits) while capping the resulting RGBA
/// allocation at 16384×16384×4 = 1 GiB rather than the JPEG
/// maximum of 65535×65535×4 ≈ 17 GiB — a hostile or buggy
/// server cannot force a multi-GB allocation per frame.
///
/// This is our bound, not a library default: `jpeg-decoder` 0.3
/// ships `usize::MAX` as its decoding-buffer limit, so the
/// pure-Rust backend only honours the cap because
/// `JpegDecoderRsDecoder::decode` passes
/// [`MAX_DECODED_RGBA_BYTES`] to `set_max_decoding_buffer_size`
/// first. The platform backends have no equivalent knob and are
/// bounded by [`DecodedJpeg::zeroed`] instead.
pub const MAX_DECODED_JPEG_DIMENSION: u32 = 16384;

/// Byte ceiling implied by [`MAX_DECODED_JPEG_DIMENSION`]:
/// 16384 × 16384 × 4 = 1 GiB.
///
/// Fed to `jpeg_decoder::Decoder::set_max_decoding_buffer_size`,
/// whose unit is `components × width × height` output bytes.
/// JPEG has at most 4 components, so this bounds that crate's
/// internal allocation by the same 1 GiB the RGBA output is
/// bounded by — a loose bound (a 1×65535 frame passes it and is
/// then rejected on dimensions) but one that applies *before*
/// the crate allocates rather than after.
pub const MAX_DECODED_RGBA_BYTES: usize =
    (MAX_DECODED_JPEG_DIMENSION as usize) * (MAX_DECODED_JPEG_DIMENSION as usize) * 4;

impl DecodedJpeg {
    /// Validate `width`/`height`, then allocate the zeroed RGBA
    /// buffer the backend will paint into.
    ///
    /// This is the allocation guard: it runs *before*
    /// `vec![0; w * h * 4]`, so a frame header claiming
    /// 65535×65535 costs a warning rather than 17 GiB. `backend`
    /// names the caller in that warning so a bug report says
    /// which decoder dropped the frame.
    ///
    /// Dimensions arrive as `usize` because ImageIO reports them
    /// that way; the other backends widen their `u32`, which is
    /// lossless on every target this crate builds for.
    pub fn zeroed(backend: &str, width: usize, height: usize) -> Option<Self> {
        let (width, height) = validated_dimensions(backend, width, height)?;
        Some(DecodedJpeg {
            rgba: vec![0u8; (width as usize) * (height as usize) * 4],
            width,
            height,
        })
    }

    /// Wrap an RGBA buffer a backend has already produced.
    ///
    /// Applies the same dimension bound as [`DecodedJpeg::zeroed`]
    /// and additionally rejects a buffer whose length is not
    /// exactly `width * height * 4`. A mismatch means the decoder
    /// and the frame header disagree about the geometry, and
    /// every consumer of `rgba` indexes it assuming they agree.
    pub fn from_rgba(backend: &str, width: usize, height: usize, rgba: Vec<u8>) -> Option<Self> {
        let (width, height) = validated_dimensions(backend, width, height)?;
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected {
            warn!(
                "{}: decoded buffer is {} bytes, expected {} for {}x{}, dropping frame",
                backend,
                rgba.len(),
                expected,
                width,
                height
            );
            return None;
        }
        Some(DecodedJpeg {
            rgba,
            width,
            height,
        })
    }
}

/// Bound a frame's dimensions before anything is allocated for
/// it, narrowing them to `u32` on success.
///
/// Deliberately free of any `cfg` gate: three of the four
/// backends that call it are inside `#[cfg(target_os = ...)]`
/// bodies, and this repo's CI runs `cargo test` on Linux only,
/// so a copy per backend is a copy that is never executed by any
/// test. See `dimension_guard_tests` below.
fn validated_dimensions(backend: &str, width: usize, height: usize) -> Option<(u32, u32)> {
    let max = MAX_DECODED_JPEG_DIMENSION as usize;
    if width == 0 || height == 0 || width > max || height > max {
        warn!(
            "{}: implausible dimensions {}x{}, dropping frame (cap: {})",
            backend, width, height, MAX_DECODED_JPEG_DIMENSION
        );
        return None;
    }
    // Both sides are <= MAX_DECODED_JPEG_DIMENSION, itself a u32
    // constant, so neither narrowing cast can truncate.
    Some((width as u32, height as u32))
}

/// Gate a payload on the JPEG SOI marker (`FF D8`) before any
/// decoder touches it. Also subsumes the empty-input check every
/// backend used to open with.
///
/// This matters most on macOS. `CGImageSourceCreateWithData`
/// with no options dictionary *sniffs* the container format, so
/// bytes the server delivered as an MJPEG frame could otherwise
/// be routed into ImageIO's TIFF, HEIF, WebP, JPEG 2000 or
/// camera-RAW sub-decoders — parsers that were never in this
/// client's intended trust path, and the historical source of
/// Apple's 0-click image-parsing bugs. Requiring SOI here keeps
/// server bytes on the JPEG path on every platform, including
/// the one where the format is chosen for us.
fn is_jpeg_payload(backend: &str, data: &[u8]) -> bool {
    if data.starts_with(&[0xFF, 0xD8]) {
        return true;
    }
    warn!(
        "{}: {} byte payload does not open with the JPEG SOI marker, dropping frame: header={:02x?}",
        backend,
        data.len(),
        &data[..data.len().min(4)]
    );
    false
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
        if !is_jpeg_payload("JpegDecoderRsDecoder", data) {
            return None;
        }

        let mut decoder = jpeg_decoder::Decoder::new(data);
        // jpeg-decoder's own default limit is `usize::MAX`, so
        // without this the crate allocates whatever the frame
        // header claims before we ever get to inspect the
        // dimensions. See MAX_DECODED_RGBA_BYTES for the unit.
        decoder.set_max_decoding_buffer_size(MAX_DECODED_RGBA_BYTES);
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
        let width = info.width as usize;
        let height = info.height as usize;

        let rgba = match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => {
                let mut out = Vec::with_capacity(pixels.len() * 4 / 3);
                // as_chunks, not chunks(3): the chunk type is
                // `[u8; 3]`, so the indexing below cannot panic
                // and a short trailing remainder is discarded
                // rather than read past its end. jpeg-decoder's
                // contract says RGB24 output is a whole number of
                // pixels so the remainder is always empty — this
                // simply declines to take that on trust.
                //
                // This is what fixes the workspace MSRV at 1.88
                // (see `rust-version` in the root Cargo.toml).
                // `chunks_exact(3)` would give the same guarantees
                // on a far older toolchain, but clippy's
                // `chunks_exact_to_as_chunks` lint rejects it and
                // the workspace builds with `-D warnings`.
                let (triples, _remainder) = pixels.as_chunks::<3>();
                for chunk in triples {
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

        // Bounds the dimensions (the crate's buffer limit above
        // is a looser, byte-count bound) and checks that the
        // buffer we just built matches them.
        DecodedJpeg::from_rgba("JpegDecoderRsDecoder", width, height, rgba)
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
        if !is_jpeg_payload("MozJpegDecoder", data) {
            return None;
        }

        // libjpeg's error handler longjmps; the `mozjpeg` crate
        // turns that into a Rust panic. Catch it so a malformed
        // frame returns None rather than crashing the channel.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let decomp = mozjpeg::Decompress::new_mem(data).ok()?;
            // Bound the dimensions before asking libjpeg for
            // scanlines, so an implausible frame header costs a
            // warning rather than a multi-GB allocation.
            let (width, height) =
                validated_dimensions("MozJpegDecoder", decomp.width(), decomp.height())?;
            let mut started = decomp.rgba().ok()?;
            // `read_scanlines::<[u8; 4]>` returns one element
            // per pixel; flatten into the RGBA byte buffer the
            // trait contract requires.
            let pixels: Vec<[u8; 4]> = started.read_scanlines().ok()?;
            started.finish().ok()?;
            let mut rgba = Vec::with_capacity(pixels.len() * 4);
            for px in &pixels {
                rgba.extend_from_slice(px);
            }
            // from_rgba rejects a scanline count that disagrees
            // with the frame header.
            DecodedJpeg::from_rgba("MozJpegDecoder", width as usize, height as usize, rgba)
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
            CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
            CGImageByteOrderInfo,
        };
        use objc2_image_io::CGImageSource;

        // Reject anything that is not SOI-prefixed before a
        // CFData exists for it. This is load-bearing here, not
        // just an early-out: see `is_jpeg_payload` for why an
        // ImageIO source built from unsniffed bytes is a wider
        // attack surface than a JPEG decoder.
        if !is_jpeg_payload("ImageIoDecoder", data) {
            return None;
        }

        // CFData::from_bytes copies, so the slice doesn't need to
        // outlive the resulting CFRetained.
        let cf_data = CFData::from_bytes(data);

        // Safety: with_data is unsafe because `options` generics
        // must be of the correct type; we pass None so there's no
        // generic-typed dictionary to get wrong.
        //
        // Passing None also leaves ImageIO to sniff the container
        // format. `kCGImageSourceTypeIdentifierHint` = "public.jpeg"
        // would state the expected type, but building that options
        // dictionary needs objc2-core-foundation's `CFDictionary`
        // and `CFString` features, which this crate does not enable
        // (see Cargo.toml) — and the hint is advisory regardless:
        // ImageIO falls back to sniffing when the data does not
        // match it. The SOI check above is the guarantee.
        let source = unsafe { CGImageSource::with_data(&cf_data, None) }?;

        // Safety: count and image_at_index are unsafe for the same
        // reason (no typed options dict).
        if unsafe { source.count() } == 0 {
            warn!("ImageIoDecoder: CGImageSource produced 0 frames");
            return None;
        }
        let cg_image = unsafe { source.image_at_index(0, None) }?;

        let width = CGImage::width(Some(&cg_image));
        let height = CGImage::height(Some(&cg_image));

        // Bounds the dimensions and hands back the zeroed RGBA
        // buffer the bitmap context paints into. The context
        // borrows the pointer for the duration of its lifetime;
        // `decoded` outlives the context, which we drop below.
        let mut decoded = DecodedJpeg::zeroed("ImageIoDecoder", width, height)?;
        let bytes_per_row = width * 4;

        let color_space = CGColorSpace::new_device_rgb()?;

        // Pixel format bits: PremultipliedLast (alpha trailing) +
        // 32-bit big-endian byte order = bytes are (R, G, B, A)
        // in memory. See struct docstring for why this isn't
        // OrderDefault or Order32Little.
        let bitmap_info =
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;

        // Safety: CGBitmapContextCreate is unsafe because the
        // pixel buffer must remain valid for the context's
        // lifetime. `decoded` is a local and is not moved until
        // after the context is dropped below.
        let context = unsafe {
            CGBitmapContextCreate(
                decoded.rgba.as_mut_ptr().cast::<core::ffi::c_void>(),
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

        // Drop the context so the borrowed
        // `decoded.rgba.as_mut_ptr()` is no longer held.
        // (Strictly speaking the lifetime is over when `context`
        // goes out of scope at end-of-function, but dropping
        // explicitly here makes the dataflow obvious.)
        drop(context);
        drop(color_space);
        drop(cg_image);
        drop(source);
        drop(cf_data);

        Some(decoded)
    }

    fn name(&self) -> &'static str {
        "ImageIO"
    }
}

/// Windows WIC-backed JPEG decoder.
///
/// Wraps Windows Imaging Component:
/// `IWICImagingFactory::CreateDecoderFromStream` (with the JPEG
/// container GUID) → `GetFrame(0)` → `IWICFormatConverter` to
/// `GUID_WICPixelFormat32bppRGBA` → `CopyPixels` into a `Vec<u8>`.
///
/// # Pixel format
///
/// The converter target is `GUID_WICPixelFormat32bppRGBA` — RGBA
/// byte order in memory. WIC's "default" 32bpp format is BGRA
/// (`GUID_WICPixelFormat32bppBGRA`); using that would silently
/// produce the "all images look blue" failure mode on Windows.
/// The format converter does the R↔B swap as part of its
/// conversion pass, so the bytes the trait returns are (R, G, B,
/// A) regardless of WIC's preferred internal layout.
///
/// # COM threading — Option A: lazy per-thread `CoInitializeEx`
///
/// WIC requires the calling thread to be in a COM apartment.
/// Decode runs on tokio worker threads, which are NOT
/// COM-initialised. Two options were considered:
///
/// - **Option A (chosen): `thread_local!` cell that calls
///   `CoInitializeEx(None, COINIT_MULTITHREADED)` lazily on first
///   decode per thread.** Lower per-call latency than option B
///   (no thread-pool hop), and tokio worker threads are long-
///   lived so the one-time init cost amortises to zero across
///   the session.
/// - Option B: `tokio::task::spawn_blocking` per decode, with COM
///   init at the start of each closure. Cleaner separation but
///   adds a thread-pool hop on every frame.
///
/// `CoInitializeEx` returns `RPC_E_CHANGED_MODE` (`0x80010106`)
/// if a *different* apartment model (STA) was previously set on
/// this thread. We treat that as a non-fatal warning: WIC works
/// from either apartment, and the existing model has already
/// been honoured by whatever set it.
///
/// We deliberately never call `CoUninitialize`. Tokio worker
/// threads outlive any single decode (and outlive the
/// `WicDecoder` itself), so balancing the init would mean
/// tearing down COM mid-session — which would break any other
/// COM-using code on the same thread. The thread_local guard
/// stays alive for the thread's lifetime; this is the
/// idiomatic pattern for COM init on long-lived worker pools.
///
/// # `Send` / `Sync`
///
/// All WIC interface objects (`IWICImagingFactory`,
/// `IWICBitmapDecoder`, `IWICStream`, `IWICBitmapFrameDecode`,
/// `IWICFormatConverter`, `IWICBitmapSource`) are intentionally
/// `!Send + !Sync` in the `windows` crate — they must only be
/// used on the thread that created them. That's fine here:
/// `WicDecoder::decode()` constructs and consumes every WIC
/// object within a single synchronous call (no `.await` inside
/// it), so the COM objects never cross a thread boundary. The
/// `WicDecoder` struct itself holds no WIC state, so the
/// `impl JpegDecoder for WicDecoder` `Send + Sync` requirement
/// is trivially satisfied.
///
/// # Why we don't cache `IWICImagingFactory`
///
/// Caching the factory across calls would avoid one
/// `CoCreateInstance` per frame, but the factory is `!Send` so
/// caching it on `WicDecoder` (which must be `Send + Sync`)
/// would require a per-thread cache (`thread_local!` again). The
/// factory creation cost is small relative to the JPEG decode
/// itself; punted as a future optimisation if profiling shows
/// it matters.
#[cfg(target_os = "windows")]
pub struct WicDecoder;

#[cfg(target_os = "windows")]
impl WicDecoder {
    /// Construct a decoder. Returns `Some` unconditionally on
    /// Windows (WIC is part of the OS — there is no probe step
    /// that can fail at construction time). The `Option`-returning
    /// shape mirrors the planned VA-API selector so
    /// `best_for_platform` cascades uniformly.
    pub fn try_new() -> Option<Self> {
        Some(WicDecoder)
    }
}

#[cfg(target_os = "windows")]
impl Default for WicDecoder {
    fn default() -> Self {
        WicDecoder
    }
}

#[cfg(target_os = "windows")]
fn ensure_com_initialised() {
    use std::cell::Cell;

    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    thread_local! {
        /// `true` once `CoInitializeEx` has been attempted on this
        /// thread. We never unset it — see the struct docstring
        /// for why we don't call `CoUninitialize`.
        static COM_INIT_DONE: Cell<bool> = const { Cell::new(false) };
    }

    COM_INIT_DONE.with(|done| {
        if done.get() {
            return;
        }
        // SAFETY: CoInitializeEx is the documented entry point
        // for putting the current thread into a COM apartment.
        // It is safe to call repeatedly; subsequent calls with
        // the same model are no-ops returning `S_FALSE`, and
        // calls with a different model return `RPC_E_CHANGED_MODE`
        // (which we treat as non-fatal — WIC works from either
        // apartment).
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            // RPC_E_CHANGED_MODE = 0x80010106: a different apartment
            // model was already set on this thread (e.g. by some
            // other crate's STA initialisation). WIC works from
            // STA too, so log and proceed.
            const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;
            if hr.0 == RPC_E_CHANGED_MODE {
                warn!(
                    "WicDecoder: CoInitializeEx returned RPC_E_CHANGED_MODE; \
                     proceeding with the existing apartment model"
                );
            } else {
                warn!(
                    "WicDecoder: CoInitializeEx failed with HRESULT {:#010x}; \
                     WIC calls may still succeed if COM was init'd elsewhere",
                    hr.0 as u32
                );
            }
        }
        done.set(true);
    });
}

#[cfg(target_os = "windows")]
impl JpegDecoder for WicDecoder {
    fn decode(&self, data: &[u8]) -> Option<DecodedJpeg> {
        use windows::core::Interface;
        use windows::Win32::Graphics::Imaging::{
            CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat32bppRGBA,
            IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
            WICDecodeMetadataCacheOnLoad,
        };
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

        // Reject anything that is not SOI-prefixed. WIC would
        // fail on it anyway (the container GUID below pins the
        // JPEG decoder), but the check is cheaper here and is
        // the same gate every other backend applies.
        if !is_jpeg_payload("WicDecoder", data) {
            return None;
        }

        ensure_com_initialised();

        // SAFETY: CoCreateInstance is the documented constructor
        // for COM objects. We pass CLSID_WICImagingFactory and ask
        // for the IWICImagingFactory interface; the cast is
        // type-checked by the windows crate via the Interface
        // trait. CLSCTX_INPROC_SERVER means in-process DLL (WIC
        // lives in windowscodecs.dll).
        let factory: IWICImagingFactory =
            match unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            {
                Ok(f) => f,
                Err(e) => {
                    warn!("WicDecoder: CoCreateInstance(WICImagingFactory) failed: {e}");
                    return None;
                }
            };

        // WIC's InitializeFromMemory takes `&mut [u8]` and does
        // NOT copy: the stream reads through that pointer for its
        // whole lifetime. Conjuring a `&mut` from our `&[u8]`
        // parameter would be undefined behaviour whatever WIC then
        // does with it — a unique reference aliasing a live shared
        // borrow is something the compiler may mark `noalias` and
        // optimise against. So WIC gets a private copy instead.
        // `owned` is declared before `stream` so it drops after
        // it, and nothing reads or writes `owned` again while the
        // stream is alive. A memcpy of a few tens of KB per frame
        // is the right price for soundness on the MJPEG path.
        let mut owned = data.to_vec();

        // Create a WIC stream and initialise it from that copy.
        // CreateStream returns an IWICStream which is an
        // ISequentialStream. See the SAFETY note on the
        // InitializeFromMemory call below.
        let stream = match unsafe { factory.CreateStream() } {
            Ok(s) => s,
            Err(e) => {
                warn!("WicDecoder: IWICImagingFactory::CreateStream failed: {e}");
                return None;
            }
        };

        // SAFETY: InitializeFromMemory requires the buffer to stay
        // valid, unmoved and unaliased for the stream's lifetime.
        // `owned` is a local `Vec` we alone own: the `&mut` handed
        // to WIC is derived from that unique owner rather than
        // fabricated from a shared borrow, `owned` outlives
        // `stream` (declared earlier, so dropped later — and
        // `stream` is explicitly dropped below in any case), and
        // no other reference to its buffer exists while the stream
        // holds one.
        if let Err(e) = unsafe { stream.InitializeFromMemory(&mut owned) } {
            warn!("WicDecoder: IWICStream::InitializeFromMemory failed: {e}");
            return None;
        }

        // Build the decoder against the JPEG container format.
        // Passing the format GUID skips WIC's container sniffing —
        // we know it's JPEG (this is the MJPEG fast path).
        // DecodeMetadataCacheOnLoad keeps the decode synchronous;
        // OnDemand would defer reads until pixel access and
        // complicates the lifetime story.
        let decoder = match unsafe {
            factory.CreateDecoderFromStream(
                &stream,
                &GUID_ContainerFormatJpeg,
                WICDecodeMetadataCacheOnLoad,
            )
        } {
            Ok(d) => d,
            Err(e) => {
                warn!("WicDecoder: CreateDecoderFromStream(JPEG) failed: {e}");
                return None;
            }
        };

        // MJPEG frames are always single-image. GetFrame(0) gets
        // the IWICBitmapFrameDecode for the only frame.
        let frame = match unsafe { decoder.GetFrame(0) } {
            Ok(f) => f,
            Err(e) => {
                warn!("WicDecoder: IWICBitmapDecoder::GetFrame(0) failed: {e}");
                return None;
            }
        };

        // Pull out width/height from the frame BEFORE doing the
        // RGBA conversion so we can bound-check and skip the
        // converter+copy step on implausible inputs.
        let (mut width, mut height): (u32, u32) = (0, 0);
        if let Err(e) = unsafe { frame.GetSize(&mut width, &mut height) } {
            warn!("WicDecoder: IWICBitmapFrameDecode::GetSize failed: {e}");
            return None;
        }
        // Bounds the dimensions and allocates the zeroed output
        // buffer in one step; None here drops the frame before
        // the converter and the copy.
        let mut decoded = DecodedJpeg::zeroed("WicDecoder", width as usize, height as usize)?;

        // Set up a format converter to force RGBA byte order
        // regardless of the JPEG's native colour space. JPEGs are
        // YCbCr in the file; WIC normally decodes to BGRA. The
        // converter does the YCbCr→RGB conversion AND the
        // BGRA→RGBA byte reorder in a single pass.
        let converter = match unsafe { factory.CreateFormatConverter() } {
            Ok(c) => c,
            Err(e) => {
                warn!("WicDecoder: CreateFormatConverter failed: {e}");
                return None;
            }
        };

        // SAFETY: Initialize takes the source bitmap (the frame,
        // cast to IWICBitmapSource), the target pixel format
        // GUID, the dither mode (None — JPEG is 8bpc so no dither
        // needed), an optional palette (null — we're not going to
        // an indexed format), the alpha threshold (0.0 — no alpha
        // in JPEG), and the palette translation type (Custom is
        // the docs-recommended value when no palette is supplied).
        if let Err(e) = unsafe {
            converter.Initialize(
                &frame
                    .cast::<windows::Win32::Graphics::Imaging::IWICBitmapSource>()
                    .ok()?,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
        } {
            warn!("WicDecoder: IWICFormatConverter::Initialize(RGBA) failed: {e}");
            return None;
        }

        // 4 bytes per pixel; DecodedJpeg::zeroed has already
        // bounded width and height to MAX_DECODED_JPEG_DIMENSION
        // so this can't overflow on a 64-bit platform.
        let stride = (width as usize) * 4;

        // CopyPixels signature: an optional source rect (None = full
        // image), the destination stride in bytes, and the
        // destination buffer slice. The buffer must be at least
        // stride * height bytes; `zeroed` allocated exactly that.
        if let Err(e) =
            unsafe { converter.CopyPixels(std::ptr::null(), stride as u32, &mut decoded.rgba) }
        {
            warn!("WicDecoder: IWICBitmapSource::CopyPixels failed: {e}");
            return None;
        }

        // Drop the COM objects in reverse construction order.
        // Strictly speaking the lifetimes end at end-of-function
        // anyway, but explicit drops make the dataflow obvious
        // and ensure the stream (which reads through `owned`) is
        // released before `owned` itself drops.
        drop(converter);
        drop(frame);
        drop(decoder);
        drop(stream);
        drop(factory);

        Some(decoded)
    }

    fn name(&self) -> &'static str {
        "WIC"
    }
}

/// Linux VA-API-backed JPEG decoder, probed at startup via
/// dlopen rather than link-time bound.
///
/// # Why dlopen
///
/// The point of `VaapiDecoder` is that a single Linux binary
/// works on systems with libva (Intel/AMD/NVIDIA + Mesa VA-API
/// driver installed) AND on systems without it (headless
/// servers, minimal containers, exotic distros). A link-time
/// dependency on libva would refuse to load on the latter.
/// `libloading::Library::new("libva.so.2")` either succeeds
/// (giving us the function pointer surface) or fails cleanly
/// (and we fall through to mozjpeg).
///
/// # Probe sequence
///
/// `try_new()` returns `None` on any step's failure, so the
/// selector in `best_for_platform()` cascades to the next
/// backend. The probe is cheap — one open of the DRM render
/// node, one `vaInitialize`, and two query calls. No actual
/// decode happens at probe time.
///
///   1. `Library::new("libva.so.2")` — modern Debian/Ubuntu/
///      Fedora soname. Falls back to `libva.so.1` for older
///      Ubuntu LTS / RHEL systems.
///   2. `Library::new("libva-drm.so.2")` — same fallback
///      ladder for older sonames.
///   3. `open("/dev/dri/renderD128")` — the typical first GPU.
///      We do NOT enumerate `renderD*`; that is deferred
///      until a real multi-GPU report surfaces.
///   4. `vaGetDisplayDRM(fd)` + `vaInitialize(...)`.
///   5. `vaQueryConfigProfiles()` — scan for
///      `VAProfileJPEGBaseline` (id 19).
///   6. `vaQueryConfigEntrypoints(JPEGBaseline)` — scan for
///      `VAEntrypointVLD` (id 1).
///
/// Each failure path emits a DEBUG log line so a bug report's
/// captured console explains which step rejected VA-API. The
/// successful path logs at INFO via `best_for_platform()`.
///
/// # Decode path — currently delegated to mozjpeg
///
/// The actual VA-API decode path — populating
/// `VAPictureParameterBufferJPEGBaseline`,
/// `VAIQMatrixBufferJPEGBaseline`,
/// `VAHuffmanTableBufferJPEGBaseline`, and
/// `VASliceParameterBufferJPEGBaseline` from a parsed JPEG
/// header — is deliberately deferred. The probe proves the
/// system has a VA-API JPEG decoder available; `decode()`
/// then delegates to an embedded `MozJpegDecoder`. Selecting
/// "VA-API" as the backend name lets bug reports surface that
/// VA-API was the chosen path even while the actual decode
/// still flows through libjpeg-turbo.
///
/// What that follow-up has to dlsym and parse, and which
/// reference implementations to read, is recorded under
/// "Deferred VA-API decode path" in
/// `docs/plans/PLAN-stream-caps-and-flap-phase-03-jpeg-decoders.md`.
///
/// # Field ordering and Drop
///
/// The display must be torn down before `libva` /
/// `libva_drm` unmap — it references function pointers backed
/// by the loaded .so, and unmapping first would leave a
/// dangling pointer in any callback libva runs from its
/// destructor. The explicit `Drop` impl calls `vaTerminate`
/// and runs to completion before any field drops, so the
/// ordering holds regardless of declaration order.
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
pub struct VaapiDecoder {
    /// The libva display handle plus the `vaTerminate` pointer
    /// that releases it, behind a wrapper that only hands them
    /// out through `&mut self`. See [`vaapi::LibvaHandles`] —
    /// that privacy is what keeps the `unsafe impl Send`/`Sync`
    /// below honest.
    va: vaapi::LibvaHandles,
    /// File descriptor for `/dev/dri/renderD128`. The libva
    /// display borrows this fd; closing it before
    /// `vaTerminate` would leave the driver poking a closed
    /// fd. Owned by the decoder, closed on drop after
    /// `vaTerminate`. `#[allow(dead_code)]` because we never
    /// read the field after construction — its job is purely
    /// to extend the fd's lifetime to match the decoder's.
    #[allow(dead_code)]
    drm_fd: std::os::fd::OwnedFd,
    /// libva.so.2 handle. Kept alive so the dlsym'd function
    /// pointers stay valid for the decoder's lifetime.
    /// `#[allow(dead_code)]` because we never call methods on
    /// the Library after construction — its job is just to
    /// keep the .so mapped.
    #[allow(dead_code)]
    libva: libloading::Library,
    /// libva-drm.so.2 handle. Same lifetime story as `libva`.
    #[allow(dead_code)]
    libva_drm: libloading::Library,
    /// Fallback decoder. The actual VA-API decode path is deferred
    /// to a follow-up; today every `decode()` call delegates here.
    /// Embedded rather than constructed per-call so we share the
    /// (currently stateless) `MozJpegDecoder` instance.
    fallback: MozJpegDecoder,
}

/// Internal libva FFI surface. Kept private to the `jpeg`
/// module — this is a deliberately minimal subset of the libva
/// ABI, enough for the probe + (deferred) decode path. See
/// `<va/va.h>` for the full surface.
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
mod vaapi {
    use std::os::raw::{c_char, c_int, c_void};

    /// Opaque libva display handle. Returned by
    /// `vaGetDisplayDRM`, passed to every other libva call,
    /// released by `vaTerminate`.
    pub type VADisplay = *mut c_void;

    /// libva status code. `VA_STATUS_SUCCESS` is 0; any
    /// other value is a failure code translatable to a human
    /// string via `vaErrorStr`.
    pub type VAStatus = c_int;

    /// `VA_STATUS_SUCCESS`. The one libva status value we
    /// compare against explicitly. Other failures get logged
    /// with their numeric code so debugging an unexpected
    /// failure on a user's system is at least possible from
    /// the bug report.
    pub const VA_STATUS_SUCCESS: VAStatus = 0;

    /// `VAProfile` enum value for baseline JPEG.
    /// `<va/va.h>` declares this as enum value 19. We declare
    /// it as `c_int` to dodge having to mirror the entire
    /// `VAProfile` enum surface (it has 50+ entries and grows
    /// every libva release).
    pub const VA_PROFILE_JPEG_BASELINE: c_int = 19;

    /// `VAEntrypoint` enum value for the VLD (variable-length
    /// decode) entry point — the standard one for JPEG.
    /// `<va/va.h>` declares it as enum value 1.
    pub const VA_ENTRYPOINT_VLD: c_int = 1;

    /// Sanity bound on the profile and entrypoint counts a
    /// driver may report from `vaMaxNumProfiles` /
    /// `vaMaxNumEntrypoints`. libva 2.x defines roughly 60
    /// profiles and 15 entrypoints, so 1024 is far beyond any
    /// plausible growth.
    ///
    /// Neither `vaQueryConfigProfiles` nor
    /// `vaQueryConfigEntrypoints` takes a capacity argument —
    /// the driver writes up to the count it just reported — so
    /// allocating less than that count would be a heap overflow
    /// rather than a truncation. The only safe response to an
    /// implausible count is therefore to refuse VA-API outright,
    /// which is what the probe does. Trust boundary here is the
    /// local libva driver, not the SPICE server; this guards
    /// against a broken driver reporting `i32::MAX` (an ~8 GiB
    /// allocation), not against an attacker.
    pub const VA_MAX_QUERY_ENTRIES: c_int = 1024;

    // Function pointer typedefs. Naming convention: `FnVa<Name>`
    // mirrors the C function name with the leading `va`
    // capitalised. Calling convention: every libva function
    // uses the C ABI; on Linux that's `extern "C"`.

    /// `int vaGetDisplayDRM(int fd)`
    pub type FnVaGetDisplayDRM = unsafe extern "C" fn(fd: c_int) -> VADisplay;

    /// `VAStatus vaInitialize(VADisplay, int*, int*)`
    pub type FnVaInitialize =
        unsafe extern "C" fn(dpy: VADisplay, major: *mut c_int, minor: *mut c_int) -> VAStatus;

    /// `VAStatus vaTerminate(VADisplay)`
    pub type FnVaTerminate = unsafe extern "C" fn(dpy: VADisplay) -> VAStatus;

    /// `const char *vaErrorStr(VAStatus)`
    pub type FnVaErrorStr = unsafe extern "C" fn(status: VAStatus) -> *const c_char;

    /// `int vaMaxNumProfiles(VADisplay)`
    pub type FnVaMaxNumProfiles = unsafe extern "C" fn(dpy: VADisplay) -> c_int;

    /// `VAStatus vaQueryConfigProfiles(VADisplay, VAProfile*, int*)`
    pub type FnVaQueryConfigProfiles = unsafe extern "C" fn(
        dpy: VADisplay,
        profile_list: *mut c_int,
        num_profiles: *mut c_int,
    ) -> VAStatus;

    /// `int vaMaxNumEntrypoints(VADisplay)`
    pub type FnVaMaxNumEntrypoints = unsafe extern "C" fn(dpy: VADisplay) -> c_int;

    /// `VAStatus vaQueryConfigEntrypoints(VADisplay, VAProfile, VAEntrypoint*, int*)`
    pub type FnVaQueryConfigEntrypoints = unsafe extern "C" fn(
        dpy: VADisplay,
        profile: c_int,
        entrypoint_list: *mut c_int,
        num_entrypoints: *mut c_int,
    ) -> VAStatus;

    /// The libva state a live `VaapiDecoder` owns: the display
    /// handle and the `vaTerminate` pointer that releases it.
    ///
    /// The fields are private to this submodule and reachable
    /// only through `&mut self`, which is the compile-time half
    /// of the `unsafe impl Send`/`Sync for VaapiDecoder`
    /// argument further down this file. Those impls are sound
    /// only while nothing touches the raw `VADisplay` from a
    /// shared borrow, and `decode(&self, ..)` holds nothing
    /// else. So a future real VA-API decode path cannot quietly
    /// start calling libva from `decode`: it has to come here
    /// and widen the accessors first, which is the point at
    /// which the `Send`/`Sync` reasoning must be revisited. The
    /// answer then is an internal `Mutex<VADisplay>` — libva
    /// thread-safety is driver-dependent — not a `&self`
    /// accessor.
    pub struct LibvaHandles {
        display: VADisplay,
        va_terminate: FnVaTerminate,
    }

    impl LibvaHandles {
        /// `display` must be the non-null handle from a
        /// successful `vaGetDisplayDRM` + `vaInitialize` pair,
        /// and `va_terminate` the `vaTerminate` symbol dlsym'd
        /// from a libva the caller keeps mapped for at least as
        /// long as the returned value.
        pub fn new(display: VADisplay, va_terminate: FnVaTerminate) -> Self {
            LibvaHandles {
                display,
                va_terminate,
            }
        }

        /// Release the display, at most once. Takes `&mut self`
        /// deliberately — see the type docstring.
        ///
        /// # Safety
        ///
        /// The libva shared object the `vaTerminate` pointer was
        /// dlsym'd from must still be mapped. `VaapiDecoder`
        /// guarantees that by calling this from its `Drop`,
        /// which runs before its `libloading::Library` fields
        /// drop.
        pub unsafe fn terminate(&mut self) {
            if self.display.is_null() {
                return;
            }
            let _status = (self.va_terminate)(self.display);
            // Null the handle so a second call is a no-op rather
            // than a use-after-free.
            self.display = std::ptr::null_mut();
        }
    }
}

#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
impl VaapiDecoder {
    /// Probe for VA-API availability on this Linux system and
    /// build a `VaapiDecoder` if all checks pass. Returns
    /// `None` for any failure so the caller in
    /// `best_for_platform()` can fall through to the next
    /// backend.
    ///
    /// Each failure path logs at DEBUG with the reason — see
    /// the struct docstring for the rationale.
    pub fn try_new() -> Option<Self> {
        use std::os::fd::AsRawFd;
        use std::os::raw::c_int;

        // 1. Load libva. Try the modern soname first
        //    (libva.so.2 — current Debian/Ubuntu/Fedora) then
        //    fall back to the older soname (libva.so.1 — old
        //    Ubuntu LTS, RHEL 7). The plan's brief said .so.1;
        //    on the dev host (Debian 13) only .so.2 exists, so
        //    we try both rather than failing on systems that
        //    only have one or the other.
        //
        // SAFETY: Library::new is unsafe because loading a
        // shared library runs its initialisers, which can do
        // anything. libva's init is well-trodden ground and
        // does not have surprising side effects.
        let libva = match unsafe { libloading::Library::new("libva.so.2") } {
            Ok(l) => l,
            Err(_) => match unsafe { libloading::Library::new("libva.so.1") } {
                Ok(l) => l,
                Err(e) => {
                    debug!("VA-API probe: libva.so.{{2,1}} not loadable: {e}");
                    return None;
                }
            },
        };

        // 2. Load libva-drm. Same fallback ladder.
        // SAFETY: see above.
        let libva_drm = match unsafe { libloading::Library::new("libva-drm.so.2") } {
            Ok(l) => l,
            Err(_) => match unsafe { libloading::Library::new("libva-drm.so.1") } {
                Ok(l) => l,
                Err(e) => {
                    debug!("VA-API probe: libva-drm.so.{{2,1}} not loadable: {e}");
                    return None;
                }
            },
        };

        // dlsym the probe-time function surface. Each lookup
        // is unsafe because we're asserting that the symbol
        // has the exact signature the typedef claims; if a
        // future libva changes the ABI of one of these
        // functions, dlsym still succeeds and we'll segfault
        // on call. That's the standard dlopen risk and is
        // accepted because libva's ABI is stable across major
        // versions.
        //
        // Symbol bytes terminate with a NUL — libloading wants
        // a byte slice ending in `\0`.
        //
        // SAFETY block: each `get` is sound iff (a) the symbol
        // exists in the loaded .so and (b) the signature we
        // claim matches what libva exports. (a) is checked by
        // the `?` operator; (b) is enforced by libva's stable
        // ABI.
        macro_rules! sym {
            ($lib:expr, $name:expr, $ty:ty) => {{
                let bytes: &[u8] = $name;
                match unsafe { $lib.get::<$ty>(bytes) } {
                    Ok(s) => *s,
                    Err(e) => {
                        debug!(
                            "VA-API probe: dlsym({}) failed: {e}",
                            std::str::from_utf8(&bytes[..bytes.len().saturating_sub(1)])
                                .unwrap_or("?")
                        );
                        return None;
                    }
                }
            }};
        }

        let va_get_display_drm: vaapi::FnVaGetDisplayDRM =
            sym!(libva_drm, b"vaGetDisplayDRM\0", vaapi::FnVaGetDisplayDRM);
        let va_initialize: vaapi::FnVaInitialize =
            sym!(libva, b"vaInitialize\0", vaapi::FnVaInitialize);
        let va_terminate: vaapi::FnVaTerminate =
            sym!(libva, b"vaTerminate\0", vaapi::FnVaTerminate);
        let va_error_str: vaapi::FnVaErrorStr = sym!(libva, b"vaErrorStr\0", vaapi::FnVaErrorStr);
        let va_max_num_profiles: vaapi::FnVaMaxNumProfiles =
            sym!(libva, b"vaMaxNumProfiles\0", vaapi::FnVaMaxNumProfiles);
        let va_query_config_profiles: vaapi::FnVaQueryConfigProfiles = sym!(
            libva,
            b"vaQueryConfigProfiles\0",
            vaapi::FnVaQueryConfigProfiles
        );
        let va_max_num_entrypoints: vaapi::FnVaMaxNumEntrypoints = sym!(
            libva,
            b"vaMaxNumEntrypoints\0",
            vaapi::FnVaMaxNumEntrypoints
        );
        let va_query_config_entrypoints: vaapi::FnVaQueryConfigEntrypoints = sym!(
            libva,
            b"vaQueryConfigEntrypoints\0",
            vaapi::FnVaQueryConfigEntrypoints
        );

        // 3. Open the DRM render node. Read-write because
        // libva's driver init writes to it. `O_CLOEXEC` so a
        // future fork doesn't leak the fd into a child.
        let drm_fd = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/renderD128")
        {
            Ok(f) => std::os::fd::OwnedFd::from(f),
            Err(e) => {
                debug!("VA-API probe: /dev/dri/renderD128 not accessible: {e}");
                return None;
            }
        };

        // 4. vaGetDisplayDRM + vaInitialize. The display
        // pointer must be non-null for vaInitialize to be
        // valid; the libva header documents that a failed
        // vaGetDisplayDRM returns NULL.
        // SAFETY: vaGetDisplayDRM takes the drm fd by value;
        // we pass the raw fd of the OwnedFd we just opened,
        // which is valid until `drm_fd` drops at end-of-scope.
        let display = unsafe { va_get_display_drm(drm_fd.as_raw_fd()) };
        if display.is_null() {
            debug!("VA-API probe: vaGetDisplayDRM returned NULL");
            return None;
        }

        let mut major: c_int = 0;
        let mut minor: c_int = 0;
        // SAFETY: vaInitialize takes the display pointer we
        // just got (non-null per check above) and two out
        // pointers to c_int locals.
        let status = unsafe { va_initialize(display, &mut major, &mut minor) };
        if status != vaapi::VA_STATUS_SUCCESS {
            // SAFETY: vaErrorStr returns a pointer to a
            // static C string inside libva. Always non-null
            // for any status code libva itself produces; we
            // tolerate NULL just in case some driver returns
            // a vendor-specific code libva doesn't recognise.
            let msg = unsafe {
                let p = va_error_str(status);
                if p.is_null() {
                    "unknown".to_string()
                } else {
                    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            debug!("VA-API probe: vaInitialize failed: {msg} (status {status})");
            return None;
        }
        debug!("VA-API probe: libva {major}.{minor} initialised");

        // 5. Query supported profiles, scan for
        // VAProfileJPEGBaseline. vaMaxNumProfiles tells us
        // how big a buffer to allocate.
        // SAFETY: pointer is non-null + initialised.
        let max_profiles = unsafe { va_max_num_profiles(display) };
        if max_profiles <= 0 || max_profiles > vaapi::VA_MAX_QUERY_ENTRIES {
            // Some driver returned a non-sensible count. Tear
            // down and bail rather than sizing an allocation
            // from it — see VA_MAX_QUERY_ENTRIES.
            // SAFETY: vaTerminate on the display we
            // initialised.
            unsafe { va_terminate(display) };
            debug!("VA-API probe: vaMaxNumProfiles returned {max_profiles}");
            return None;
        }
        let mut profiles: Vec<c_int> = vec![0; max_profiles as usize];
        let mut num_profiles: c_int = 0;
        // SAFETY: profiles.as_mut_ptr is valid for
        // `max_profiles` elements; num_profiles is a stack
        // local out-pointer.
        let status =
            unsafe { va_query_config_profiles(display, profiles.as_mut_ptr(), &mut num_profiles) };
        if status != vaapi::VA_STATUS_SUCCESS {
            unsafe { va_terminate(display) };
            debug!("VA-API probe: vaQueryConfigProfiles failed: status {status}");
            return None;
        }
        // Clamp to what we actually allocated: a driver that
        // reports more results than the buffer it was handed
        // would otherwise panic the probe on a slice index.
        let returned = (num_profiles.max(0) as usize).min(profiles.len());
        let supported_profiles = &profiles[..returned];
        if !supported_profiles.contains(&vaapi::VA_PROFILE_JPEG_BASELINE) {
            unsafe { va_terminate(display) };
            debug!("VA-API probe: VAProfileJPEGBaseline not supported by driver");
            return None;
        }

        // 6. Query entrypoints for JPEGBaseline, scan for
        // VAEntrypointVLD. Same allocation pattern as profiles.
        // SAFETY: as above.
        let max_entrypoints = unsafe { va_max_num_entrypoints(display) };
        if max_entrypoints <= 0 || max_entrypoints > vaapi::VA_MAX_QUERY_ENTRIES {
            unsafe { va_terminate(display) };
            debug!("VA-API probe: vaMaxNumEntrypoints returned {max_entrypoints}");
            return None;
        }
        let mut entrypoints: Vec<c_int> = vec![0; max_entrypoints as usize];
        let mut num_entrypoints: c_int = 0;
        // SAFETY: entrypoints.as_mut_ptr valid for
        // max_entrypoints; num_entrypoints is a stack local.
        let status = unsafe {
            va_query_config_entrypoints(
                display,
                vaapi::VA_PROFILE_JPEG_BASELINE,
                entrypoints.as_mut_ptr(),
                &mut num_entrypoints,
            )
        };
        if status != vaapi::VA_STATUS_SUCCESS {
            unsafe { va_terminate(display) };
            debug!("VA-API probe: vaQueryConfigEntrypoints(JPEGBaseline) failed: status {status}");
            return None;
        }
        // Clamped for the same reason as the profile list above.
        let returned = (num_entrypoints.max(0) as usize).min(entrypoints.len());
        let supported_entrypoints = &entrypoints[..returned];
        if !supported_entrypoints.contains(&vaapi::VA_ENTRYPOINT_VLD) {
            unsafe { va_terminate(display) };
            debug!("VA-API probe: VAEntrypointVLD not supported for JPEGBaseline");
            return None;
        }

        // All checks passed. Build the decoder. The actual
        // decode path (deferred — see struct docstring)
        // currently delegates to mozjpeg.
        debug!("VA-API probe: passed (libva {major}.{minor}, JPEGBaseline+VLD supported)");
        Some(VaapiDecoder {
            va: vaapi::LibvaHandles::new(display, va_terminate),
            drm_fd,
            libva,
            libva_drm,
            fallback: MozJpegDecoder::new(),
        })
    }
}

// SAFETY: VaapiDecoder holds a `VADisplay` (raw `*mut c_void`)
// which is `!Send + !Sync` by default, plus a captured function
// pointer (`va_terminate`). The `JpegDecoder` trait requires
// `Send + Sync` so we must promise both.
//
// What makes the promise sound TODAY: `decode(&self, ...)`
// delegates to `self.fallback.decode(...)` (the embedded
// `MozJpegDecoder`, which is `Send + Sync`) and never touches
// the libva handles. Those are reachable only through
// `vaapi::LibvaHandles`, whose fields are private to that
// submodule and whose only accessor takes `&mut self` — so
// "decode never touches libva" is enforced by the compiler
// here, not by whoever next edits `decode` remembering to read
// this comment. That is the tripwire: adding a real VA-API
// decode path means widening `LibvaHandles`, and the type's
// docstring sends you back here.
//
// When that happens this comment stops being accurate: libva
// thread-safety is driver-dependent (Intel iHD, Mesa, etc. each
// implement their own locking policy) and there is no portable
// guarantee that `VADisplay` tolerates concurrent calls. At
// that point the libva calls must be wrapped in an internal
// `Mutex<VADisplay>` so we own the synchronisation rather than
// trusting whichever driver happens to be loaded.
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
unsafe impl Send for VaapiDecoder {}
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
unsafe impl Sync for VaapiDecoder {}

#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        // Tear down the libva display before the underlying
        // .so unmaps. `Drop::drop` for the struct runs to
        // completion before any field drops, so terminating
        // here is what releases the libva-side resources while
        // the .so and the DRM fd are both still alive.
        //
        // SAFETY: `LibvaHandles::terminate` requires the libva
        // .so to still be mapped; it is — `libva` and
        // `libva_drm` are fields of `self` and drop only after
        // this method returns. The status is deliberately not
        // logged: Drop runs at session shutdown and a tracing
        // call from Drop can surface in unexpected places (e.g.
        // test harnesses that consume stderr).
        unsafe { self.va.terminate() };
        // drm_fd, libva, libva_drm, fallback drop in order
        // after this returns.
    }
}

#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
impl JpegDecoder for VaapiDecoder {
    fn decode(&self, data: &[u8]) -> Option<DecodedJpeg> {
        // Deferred: real VA-API decode path. See struct
        // docstring (the "Decode path — currently delegated
        // to mozjpeg" section) for the full rationale. The
        // selector's choice of "VA-API" still wins because
        // the probe succeeded; the actual decode happens
        // through the embedded fallback until the follow-up
        // step lands.
        self.fallback.decode(data)
    }

    fn name(&self) -> &'static str {
        // The probe succeeded (libva loaded, JPEG-baseline
        // profile + VLD entrypoint advertised) so
        // best_for_platform() chose this backend; the actual
        // decode currently delegates to mozjpeg. Surfaced name
        // includes the qualifier so a bug-report reader can tell
        // which library is doing the bit-pushing today vs which
        // path was selected. When the real VA-API decode path
        // lands, drop the parenthetical and the backend name
        // reflects truth without any reader needing to remember
        // the deferred-decode caveat.
        "VA-API (probed, mozjpeg fallback)"
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
/// Pure-Rust + libjpeg-turbo are available cross-platform,
/// ImageIO on macOS, and WIC on Windows; VA-API is the newest
/// addition. `MozJpegDecoder` is only present when the
/// `mozjpeg` Cargo feature is enabled (defaulted on); building
/// without it falls back to the pure-Rust path.
///
/// `*` `VaapiDecoder` probes for VA-API capability but
/// currently delegates every `decode()` call to its embedded
/// `MozJpegDecoder`; the real VA-API decode path is not
/// implemented yet, which is why it reports its backend as
/// "VA-API (probed, mozjpeg fallback)".
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

    // Windows: prefer WIC (uses the OS codec stack, which engages
    // GPU acceleration on hardware where the driver supports it).
    // Like ImageIoDecoder, try_new() can't fail today — WIC is
    // part of the OS — but the Option-returning shape keeps the
    // cascade uniform with VA-API.
    #[cfg(target_os = "windows")]
    if let Some(d) = WicDecoder::try_new() {
        let decoder: Arc<dyn JpegDecoder> = Arc::new(d);
        info!("MJPEG decoder backend selected: {}", decoder.name());
        return decoder;
    }

    // Linux: probe for VA-API. try_new() returns None on any
    // failure (missing libva, missing render node, driver
    // doesn't support JPEGBaseline+VLD, etc.) and emits a
    // DEBUG log line explaining which step rejected it. The
    // INFO log here records the *outcome* — "VA-API selected"
    // or "VA-API unavailable, falling back" — so a bug report's
    // captured console makes the selection visible at a glance
    // without having to enable debug logging.
    // VA-API requires the embedded MozJpegDecoder fallback (see
    // VaapiDecoder docs); gated on both target_os AND the
    // mozjpeg feature so a build with `--no-default-features
    // --features jpeg` still compiles, just without VA-API.
    #[cfg(all(target_os = "linux", feature = "mozjpeg"))]
    if let Some(d) = VaapiDecoder::try_new() {
        let decoder: Arc<dyn JpegDecoder> = Arc::new(d);
        info!("MJPEG decoder backend selected: {}", decoder.name());
        return decoder;
    } else {
        info!("VA-API unavailable on this Linux system; falling back to next backend");
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
/// real cross-platform smoke test is operator-driven; see
/// `docs/plans/PLAN-stream-caps-and-flap.md`.
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

/// Tests for the Windows WIC decoder backend. These compile and
/// run only on Windows — Linux/macOS CI never executes them. The
/// real cross-platform smoke test is operator-driven; see
/// `docs/plans/PLAN-stream-caps-and-flap.md`.
///
/// Uses the same `swatches.jpg` fixture as the macOS test (32x32,
/// four 16x16 quadrants painted red / green / blue / yellow at
/// quality 85). Asserting against the same fixture means a future
/// regression that swaps R↔B in either backend is caught
/// symmetrically.
#[cfg(all(test, target_os = "windows"))]
mod wic_tests {
    use super::*;

    /// The fixture JPEG. Shared with the macOS ImageIO test.
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/swatches.jpg");
    const FIXTURE_W: u32 = 32;
    const FIXTURE_H: u32 = 32;

    /// Per-channel tolerance for the centre-of-quadrant samples.
    /// Matches the ImageIO test's tolerance so cross-platform
    /// regressions are visible at the same threshold.
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
    fn wic_decodes_swatches_in_rgba_order() {
        let decoder = WicDecoder::try_new().expect("WicDecoder::try_new returned None");
        assert_eq!(decoder.name(), "WIC");

        let decoded = decoder
            .decode(FIXTURE)
            .expect("WicDecoder returned None on fixture");
        assert_eq!(decoded.width, FIXTURE_W, "width mismatch");
        assert_eq!(decoded.height, FIXTURE_H, "height mismatch");
        assert_eq!(
            decoded.rgba.len(),
            (FIXTURE_W * FIXTURE_H * 4) as usize,
            "rgba buffer length wrong",
        );

        // Sample the centre of each quadrant (offset 8 from each
        // edge — 8 pixels into a 16-pixel quadrant). If we'd
        // targeted GUID_WICPixelFormat32bppBGRA instead of RGBA,
        // every channel here would be wrong (R↔B swap), so this
        // test catches the canonical "everything is blue" bug.
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
    fn wic_empty_input_returns_none() {
        let decoder = WicDecoder::try_new().unwrap();
        assert!(decoder.decode(&[]).is_none());
    }

    #[test]
    fn wic_truncated_input_returns_none() {
        let decoder = WicDecoder::try_new().unwrap();
        // SOI + APP0 stub, nothing more — not a valid JPEG.
        assert!(decoder.decode(&[0xFF, 0xD8, 0xFF, 0xE0]).is_none());
    }

    #[test]
    fn wic_garbage_input_returns_none() {
        let decoder = WicDecoder::try_new().unwrap();
        // 32 bytes of "definitely not a JPEG".
        let junk: Vec<u8> = (0..32u8).collect();
        assert!(decoder.decode(&junk).is_none());
    }

    #[test]
    fn wic_decoder_name_is_wic() {
        let decoder = WicDecoder::try_new().unwrap();
        assert_eq!(decoder.name(), "WIC");
    }
}

/// Tests for the Linux VA-API decoder backend. These compile
/// and run only on Linux with the `mozjpeg` feature on (the
/// default). The probe path is environment-dependent: on a
/// developer host with libva installed it returns `Some`; on a
/// minimal CI runner without libva it returns `None`. Both
/// outcomes are valid — the test asserts only that
/// `try_new()` does not panic, and that any resulting decoder
/// names itself "VA-API" and decodes a known fixture through
/// its mozjpeg fallback.
#[cfg(all(test, target_os = "linux", feature = "mozjpeg"))]
mod vaapi_tests {
    use super::*;

    /// Calling `try_new()` must never panic, regardless of
    /// whether libva is installed. This is the contract the
    /// `best_for_platform()` selector relies on.
    #[test]
    fn vaapi_try_new_does_not_panic() {
        // Drop whatever it returns — the assertion is just
        // that this expression runs to completion.
        let _ = VaapiDecoder::try_new();
    }

    /// If the probe succeeds, the resulting decoder must
    /// identify itself with a "VA-API" prefix so bug reports
    /// show the chosen path. The full name today reads
    /// "VA-API (probed, mozjpeg fallback)" — the parenthetical
    /// disappears once the real VA-API decode path lands.
    /// Asserting `starts_with` keeps this test honest across
    /// that planned transition. If the probe fails (no libva,
    /// no render node, no JPEGBaseline support), there is
    /// nothing to assert and the test is a no-op.
    #[test]
    fn vaapi_name_starts_with_va_api_when_present() {
        if let Some(d) = VaapiDecoder::try_new() {
            assert!(
                d.name().starts_with("VA-API"),
                "VaapiDecoder.name() = {:?}, expected to start with \"VA-API\"",
                d.name(),
            );
        }
    }

    /// If the probe succeeds, decode must succeed too — the
    /// fallback path delegates to mozjpeg, which we already
    /// know works (see `mozjpeg_round_trip_within_tolerance`
    /// above). This test exists to catch a regression where
    /// the fallback wiring breaks (e.g. decode() returns None
    /// instead of self.fallback.decode(data)).
    #[test]
    fn vaapi_decodes_via_fallback_when_present() {
        let Some(decoder) = VaapiDecoder::try_new() else {
            return; // libva not available; skip
        };

        // Build a tiny JPEG via mozjpeg::Compress, decode it
        // through VaapiDecoder.decode (which today delegates
        // to its embedded MozJpegDecoder).
        const W: u32 = 8;
        const H: u32 = 8;
        let mut src = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            for x in 0..W {
                let r = ((x * 255) / (W - 1)) as u8;
                let g = ((y * 255) / (H - 1)) as u8;
                src.extend_from_slice(&[r, g, 128, 255]);
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
        let decoded = decoder
            .decode(&jpeg_bytes)
            .expect("VaapiDecoder returned None on round-trip");
        assert_eq!(decoded.width, W);
        assert_eq!(decoded.height, H);
        assert_eq!(decoded.rgba.len(), (W * H * 4) as usize);
    }

    /// Empty input must return None, not panic, even when
    /// VA-API is available. The fallback handles this case
    /// already; we just re-check that the delegation works.
    #[test]
    fn vaapi_empty_input_returns_none_when_present() {
        if let Some(decoder) = VaapiDecoder::try_new() {
            assert!(decoder.decode(&[]).is_none());
        }
    }
}

/// Cross-cutting assertion for `best_for_platform()` on Linux:
/// no matter which backend is selected (VA-API, libjpeg-turbo,
/// or pure-Rust jpeg-decoder), the returned Arc is usable.
/// Tests on every platform implicitly verify the selector
/// works; this one is here so a regression that returns null
/// or panics inside `best_for_platform()` on Linux fires a
/// loud test failure rather than a runtime crash.
#[cfg(all(test, target_os = "linux"))]
mod best_for_platform_linux_tests {
    use super::*;

    #[test]
    fn best_for_platform_returns_a_decoder_on_linux() {
        let decoder = best_for_platform();
        // The name must come from the documented chain. We accept:
        //   - any name starting with "VA-API" (probe passed; the
        //     full string today is "VA-API (probed, mozjpeg
        //     fallback)" and loses the parenthetical when the
        //     real decode path lands),
        //   - "libjpeg-turbo" (mozjpeg feature on, no VA-API),
        //   - "jpeg-decoder" (no mozjpeg).
        // i.e. exactly the chain documented in best_for_platform's
        // rustdoc.
        let name = decoder.name();
        let acceptable =
            name.starts_with("VA-API") || matches!(name, "libjpeg-turbo" | "jpeg-decoder");
        assert!(acceptable, "unexpected backend name on Linux: {name}");
    }
}

/// Tests for the platform-independent decode guards.
///
/// These are the point of `validated_dimensions`,
/// `DecodedJpeg::zeroed` / `from_rgba` and `is_jpeg_payload`
/// being free of any `cfg` gate: one allocation bound and one
/// SOI gate protect all four backends, proved once here rather
/// than separately per platform. Before the helpers existed the
/// bound was three structurally identical copies, two of them
/// inside `#[cfg(target_os = "macos")]` and
/// `#[cfg(target_os = "windows")]` bodies — and a fourth backend
/// had no bound at all. The platform copies were compiled and
/// their tests were run, but only on the macOS and Windows legs,
/// which run in the merge tier rather than per pull request; and
/// no test on any platform fed the guard an oversized image to
/// confirm it rejected one. Both are why this module is
/// un-`cfg`'d and runs everywhere.
#[cfg(test)]
mod dimension_guard_tests {
    use super::*;

    /// The 32×32 four-swatch fixture shared with the macOS and
    /// Windows backend tests.
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/swatches.jpg");

    #[test]
    fn max_rgba_bytes_is_the_square_of_the_dimension_cap() {
        assert_eq!(MAX_DECODED_RGBA_BYTES, 1024 * 1024 * 1024);
    }

    #[test]
    fn validated_dimensions_accepts_plausible_frames() {
        assert_eq!(validated_dimensions("t", 1, 1), Some((1, 1)));
        assert_eq!(validated_dimensions("t", 1920, 1080), Some((1920, 1080)));
        let max = MAX_DECODED_JPEG_DIMENSION as usize;
        assert_eq!(
            validated_dimensions("t", max, max),
            Some((MAX_DECODED_JPEG_DIMENSION, MAX_DECODED_JPEG_DIMENSION)),
            "the cap itself must be accepted, not rejected"
        );
    }

    #[test]
    fn validated_dimensions_rejects_zero_sides() {
        assert!(validated_dimensions("t", 0, 16).is_none());
        assert!(validated_dimensions("t", 16, 0).is_none());
        assert!(validated_dimensions("t", 0, 0).is_none());
    }

    #[test]
    fn validated_dimensions_rejects_oversized_sides() {
        let over = MAX_DECODED_JPEG_DIMENSION as usize + 1;
        assert!(validated_dimensions("t", over, 16).is_none());
        assert!(validated_dimensions("t", 16, over).is_none());
        // The JPEG maximum, and the value a hostile frame header
        // would most plausibly claim.
        assert!(validated_dimensions("t", 65535, 65535).is_none());
        assert!(validated_dimensions("t", usize::MAX, usize::MAX).is_none());
    }

    #[test]
    fn zeroed_allocates_exactly_four_bytes_per_pixel() {
        let frame = DecodedJpeg::zeroed("t", 7, 5).expect("7x5 is plausible");
        assert_eq!(frame.width, 7);
        assert_eq!(frame.height, 5);
        assert_eq!(frame.rgba.len(), 7 * 5 * 4);
        assert!(frame.rgba.iter().all(|&b| b == 0), "buffer must be zeroed");
    }

    /// The guard that matters: an implausible frame header must
    /// be refused *before* the multi-gigabyte allocation, not
    /// after. If this ever regresses the test process itself
    /// tries to allocate 17 GiB.
    #[test]
    fn zeroed_refuses_to_allocate_for_implausible_dimensions() {
        assert!(DecodedJpeg::zeroed("t", 65535, 65535).is_none());
        assert!(DecodedJpeg::zeroed("t", 0, 0).is_none());
    }

    #[test]
    fn from_rgba_accepts_a_correctly_sized_buffer() {
        let frame = DecodedJpeg::from_rgba("t", 3, 2, vec![0xAB; 3 * 2 * 4]).expect("exact length");
        assert_eq!((frame.width, frame.height), (3, 2));
        assert_eq!(frame.rgba.len(), 24);
    }

    #[test]
    fn from_rgba_rejects_a_buffer_that_disagrees_with_the_header() {
        // Short by one pixel: every consumer indexes `rgba` from
        // the dimensions, so this would be an out-of-bounds read
        // downstream rather than a dropped frame.
        assert!(DecodedJpeg::from_rgba("t", 3, 2, vec![0; 3 * 2 * 4 - 4]).is_none());
        assert!(DecodedJpeg::from_rgba("t", 3, 2, vec![0; 3 * 2 * 4 + 4]).is_none());
        assert!(DecodedJpeg::from_rgba("t", 3, 2, Vec::new()).is_none());
    }

    #[test]
    fn from_rgba_applies_the_dimension_bound_too() {
        assert!(DecodedJpeg::from_rgba("t", 65535, 1, vec![0; 4]).is_none());
    }

    #[test]
    fn is_jpeg_payload_accepts_only_soi_prefixed_data() {
        assert!(is_jpeg_payload("t", &[0xFF, 0xD8]));
        assert!(is_jpeg_payload("t", FIXTURE));

        assert!(!is_jpeg_payload("t", &[]));
        assert!(!is_jpeg_payload("t", &[0xFF]));
        // EOI, not SOI.
        assert!(!is_jpeg_payload("t", &[0xFF, 0xD9, 0x00]));
        // The containers ImageIO would otherwise sniff its way
        // into: TIFF (little- and big-endian), RIFF/WebP, PNG.
        assert!(!is_jpeg_payload("t", b"II*\0\0\0\0\0"));
        assert!(!is_jpeg_payload("t", b"MM\0*\0\0\0\0"));
        assert!(!is_jpeg_payload("t", b"RIFF\0\0\0\0WEBP"));
        assert!(!is_jpeg_payload("t", b"\x89PNG\r\n\x1a\n"));
    }

    /// The pure-Rust backend had no dimension bound at all
    /// before these helpers landed. Decoding the shared fixture
    /// proves the guard is wired into the normal path without
    /// rejecting valid frames, and that
    /// `set_max_decoding_buffer_size` does not cap a real one.
    #[test]
    fn jpeg_decoder_rs_decodes_the_fixture_within_the_bound() {
        let decoder = JpegDecoderRsDecoder::new();
        let decoded = decoder
            .decode(FIXTURE)
            .expect("JpegDecoderRsDecoder returned None on the fixture");
        assert_eq!(decoded.width, 32);
        assert_eq!(decoded.height, 32);
        assert_eq!(decoded.rgba.len(), 32 * 32 * 4);
    }

    #[test]
    fn jpeg_decoder_rs_rejects_non_jpeg_payloads() {
        let decoder = JpegDecoderRsDecoder::new();
        assert!(decoder.decode(&[]).is_none());
        // A TIFF header delivered as an MJPEG frame. On macOS
        // this is the payload that used to reach ImageIO's TIFF
        // decoder; every backend now refuses it up front.
        assert!(decoder.decode(b"II*\0\0\0\0\0").is_none());
    }
}
