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
            CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
            CGImageByteOrderInfo,
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

        let width = CGImage::width(Some(&cg_image));
        let height = CGImage::height(Some(&cg_image));

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

        let color_space = CGColorSpace::new_device_rgb()?;

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

        // Reject empty input early — WIC will fail on it anyway
        // but the error path is cheaper to short-circuit here.
        if data.is_empty() {
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

        // Create a WIC stream and initialise it from our in-memory
        // byte slice. CreateStream returns an IWICStream which is
        // an ISequentialStream; InitializeFromMemory copies our
        // buffer (it does NOT borrow), so the slice does not need
        // to outlive the stream.
        let stream = match unsafe { factory.CreateStream() } {
            Ok(s) => s,
            Err(e) => {
                warn!("WicDecoder: IWICImagingFactory::CreateStream failed: {e}");
                return None;
            }
        };

        // SAFETY: InitializeFromMemory takes a *mut u8 + length.
        // The WIC contract is that the memory must remain valid
        // for the lifetime of the stream — the docs say the call
        // does NOT copy. We keep `data` alive (it's a parameter
        // borrow) for the entire decode call, and the stream is
        // dropped at end-of-function before `data` goes out of
        // scope. The cast from &[u8] to *mut u8 is sound because
        // WIC only reads from the buffer (it's a decoder source);
        // it never writes through this pointer.
        if let Err(e) = unsafe {
            stream.InitializeFromMemory(std::slice::from_raw_parts_mut(
                data.as_ptr() as *mut u8,
                data.len(),
            ))
        } {
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
        if width == 0 || height == 0 || width > 65535 || height > 65535 {
            warn!(
                "WicDecoder: implausible dimensions {}x{}, dropping frame",
                width, height
            );
            return None;
        }

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

        // Allocate the output buffer. 4 bytes per pixel; we've
        // already bounded width*height to ≤ 65535*65535 above so
        // this can't overflow on a 64-bit platform.
        let stride = (width as usize) * 4;
        let buf_len = stride * (height as usize);
        let mut rgba = vec![0u8; buf_len];

        // CopyPixels signature: an optional source rect (None = full
        // image), the destination stride in bytes, and the
        // destination buffer slice. The buffer must be at least
        // stride * height bytes; we just allocated exactly that.
        if let Err(e) = unsafe { converter.CopyPixels(std::ptr::null(), stride as u32, &mut rgba) }
        {
            warn!("WicDecoder: IWICBitmapSource::CopyPixels failed: {e}");
            return None;
        }

        // Drop the COM objects in reverse construction order.
        // Strictly speaking the lifetimes end at end-of-function
        // anyway, but explicit drops make the dataflow obvious
        // and ensure the stream (which borrowed `data`) is
        // released before this function returns and `data` could
        // theoretically be invalidated by a caller's drop.
        drop(converter);
        drop(frame);
        drop(decoder);
        drop(stream);
        drop(factory);

        Some(DecodedJpeg {
            rgba,
            width,
            height,
        })
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
///      We do NOT enumerate `renderD*` — that's Q3 in the
///      phase plan, deferred until a real multi-GPU report
///      surfaces.
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
/// Per the phase 3E brief (clarifications #4 and #5 in the
/// step prompt), the actual VA-API decode path — populating
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
/// The follow-up step that wires up real VA-API decode will
/// need to dlsym the larger function surface (vaCreateConfig,
/// vaCreateContext, vaCreateSurfaces, vaCreateBuffer,
/// vaBeginPicture, vaRenderPicture, vaEndPicture,
/// vaSyncSurface, vaDeriveImage, vaMapBuffer, vaUnmapBuffer,
/// vaDestroyImage, vaDestroyBuffer, vaDestroyContext,
/// vaDestroyConfig, vaDestroySurfaces) and parse JPEG SOF/
/// DHT/DQT segments. Reference implementations: ffmpeg's
/// `libavcodec/vaapi_mjpeg.c` and chromium's
/// `media/gpu/vaapi/vaapi_jpeg_decoder.cc`.
///
/// # Field ordering and Drop
///
/// `display` must drop before `libva` / `libva_drm` — the
/// libva display references function pointers backed by the
/// loaded .so, and tearing down the .so first would leave a
/// dangling pointer in any callback libva runs from its
/// destructor. Rust drops fields in declaration order, so the
/// declaration order below (display first, then libraries)
/// is the correct one. Explicit `Drop` impl calls
/// `vaTerminate` to release the libva display before the .so
/// unmaps.
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
pub struct VaapiDecoder {
    /// VA display handle. Opaque pointer returned by
    /// `vaGetDisplayDRM`; passed back to `vaTerminate` on
    /// drop. Non-null while the decoder is alive.
    display: vaapi::VADisplay,
    /// File descriptor for `/dev/dri/renderD128`. The libva
    /// display borrows this fd; closing it before
    /// `vaTerminate` would leave the driver poking a closed
    /// fd. Owned by the decoder, closed on drop after
    /// `vaTerminate`. `#[allow(dead_code)]` because we never
    /// read the field after construction — its job is purely
    /// to extend the fd's lifetime to match the decoder's.
    #[allow(dead_code)]
    drm_fd: std::os::fd::OwnedFd,
    /// `vaTerminate` function pointer captured at probe time
    /// so `Drop` doesn't have to re-dlsym. Required for
    /// clean teardown.
    va_terminate: vaapi::FnVaTerminate,
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
    /// Fallback decoder. Per the phase 3E plan, the actual
    /// VA-API decode path is deferred to a follow-up; today
    /// every `decode()` call delegates here. Embedded rather
    /// than constructed per-call so we share the (currently
    /// stateless) `MozJpegDecoder` instance.
    fallback: MozJpegDecoder,
}

/// Internal libva FFI surface. Kept private to the `jpeg`
/// module — this is a deliberately minimal subset of the libva
/// ABI, enough for the probe + (deferred) decode path. See
/// `<va/va.h>` for the full surface.
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
mod vaapi {
    use std::os::raw::{c_char, c_int, c_uint, c_void};

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

    // ----- The wider surface, declared but not currently used.
    // Listed here so the follow-up implementing real VA-API
    // decode has a single place to consult. Removing these
    // typedefs would force the follow-up to re-derive them from
    // <va/va.h> — keeping them inline costs nothing and signals
    // intent.

    /// `VAStatus vaCreateConfig(VADisplay, VAProfile, VAEntrypoint, VAConfigAttrib*, int, VAConfigID*)`
    /// Not currently dlsym'd; follow-up.
    #[allow(dead_code)]
    pub type FnVaCreateConfig = unsafe extern "C" fn(
        dpy: VADisplay,
        profile: c_int,
        entrypoint: c_int,
        attrib_list: *mut c_void,
        num_attribs: c_int,
        config_id: *mut c_uint,
    ) -> VAStatus;
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
        if max_profiles <= 0 {
            // Some driver returned a non-sensible count.
            // Tear down and bail.
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
        let supported_profiles = &profiles[..num_profiles.max(0) as usize];
        if !supported_profiles.contains(&vaapi::VA_PROFILE_JPEG_BASELINE) {
            unsafe { va_terminate(display) };
            debug!("VA-API probe: VAProfileJPEGBaseline not supported by driver");
            return None;
        }

        // 6. Query entrypoints for JPEGBaseline, scan for
        // VAEntrypointVLD. Same allocation pattern as profiles.
        // SAFETY: as above.
        let max_entrypoints = unsafe { va_max_num_entrypoints(display) };
        if max_entrypoints <= 0 {
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
        let supported_entrypoints = &entrypoints[..num_entrypoints.max(0) as usize];
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
            display,
            drm_fd,
            va_terminate,
            libva,
            libva_drm,
            fallback: MozJpegDecoder::new(),
        })
    }
}

// SAFETY: VaapiDecoder holds a `VADisplay` (raw `*mut c_void`)
// which is `!Send + !Sync` by default. The libva ABI permits
// concurrent calls on the same display from multiple threads —
// the driver implements internal locking. The wider channel
// pattern wraps the decoder in `Arc<dyn JpegDecoder>` and uses
// it from a single tokio task today, but the trait contract is
// `Send + Sync` so we must promise both. The captured function
// pointer (`va_terminate`) is also a raw pointer; same
// reasoning. The other fields (`drm_fd`, `libloading::Library`,
// `MozJpegDecoder`) are all already `Send + Sync`.
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
unsafe impl Send for VaapiDecoder {}
#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
unsafe impl Sync for VaapiDecoder {}

#[cfg(all(target_os = "linux", feature = "mozjpeg"))]
impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        // Tear down the libva display before the underlying
        // .so unmaps. The Rust field-drop order matches the
        // declaration order (display.drop runs first, then
        // drm_fd, then libva, then libva_drm); calling
        // vaTerminate explicitly here is what releases the
        // libva-side resources the display refers to.
        //
        // SAFETY: self.display is non-null (proved in
        // try_new) and self.va_terminate is the captured
        // function pointer from the still-loaded libva.so.
        if !self.display.is_null() {
            let _status = unsafe { (self.va_terminate)(self.display) };
            // Don't log the status — Drop runs at session
            // shutdown and a tracing call from Drop can
            // surface in unexpected places (e.g. test
            // harnesses that consume stderr).
        }
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
        // Returns "VA-API" even though the decode currently
        // delegates to mozjpeg — the backend name reflects
        // which path was selected by best_for_platform(), not
        // which library does the bit-pushing today. The
        // follow-up step that wires up real VA-API decode
        // doesn't need to change this string.
        "VA-API"
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
/// Steps 3A–3D have landed: pure-Rust + libjpeg-turbo
/// cross-platform, ImageIO on macOS, and WIC on Windows. VA-API
/// lands in step 3E. `MozJpegDecoder` is only present when the
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

/// Tests for the Windows WIC decoder backend. These compile and
/// run only on Windows — Linux/macOS CI never executes them. The
/// real cross-platform smoke test lives in step 3H of the phase 3
/// plan and is operator-driven.
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
    /// identify itself as "VA-API" so bug reports show the
    /// chosen path. If the probe fails (no libva, no render
    /// node, no JPEGBaseline support), there is nothing to
    /// assert and the test is a no-op — the previous test
    /// covers the no-panic contract.
    #[test]
    fn vaapi_name_is_va_api_when_present() {
        if let Some(d) = VaapiDecoder::try_new() {
            assert_eq!(d.name(), "VA-API");
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
        // The name must be one of the documented values. We
        // accept either VA-API (probe passed), libjpeg-turbo
        // (mozjpeg feature on, no VA-API), or jpeg-decoder
        // (no mozjpeg) — i.e. exactly the chain documented in
        // best_for_platform's rustdoc.
        let name = decoder.name();
        assert!(
            matches!(name, "VA-API" | "libjpeg-turbo" | "jpeg-decoder"),
            "unexpected backend name on Linux: {name}"
        );
    }
}
