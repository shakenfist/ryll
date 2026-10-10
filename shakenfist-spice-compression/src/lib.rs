//! Pure-Rust implementations of the SPICE image-stream
//! decompression algorithms: QUIC (the SPICE wavelet/arithmetic
//! codec, not the QUIC transport protocol), GLZ
//! (dictionary-based cross-frame LZ), LZ (single-frame LZ),
//! and LZ4.
//!
//! Each algorithm is gated behind a Cargo feature (`quic`,
//! `glz`, `lz`, `lz4`) with all four enabled by default.
//! Consumers who only need a subset can disable default
//! features and opt in to the ones they need.
//!
//! All four decoders return a [`DecompressedImage`] on success
//! (with the historical exception of `quic_decode`, which
//! returns `Option<Vec<u8>>` and leaves the wrapping to the
//! caller — this asymmetry will be smoothed out before the
//! first published release).
//!
//! Extracted from the
//! [ryll](https://github.com/shakenfist/ryll) SPICE client.

pub mod byte_bounded_lru;

pub mod limits;

#[cfg(feature = "encode")]
pub mod encode;

#[cfg(feature = "glz")]
pub mod glz;

#[cfg(feature = "jpeg")]
pub mod jpeg;

#[cfg(feature = "lz")]
pub mod lz;

#[cfg(feature = "lz4")]
pub mod lz4;

#[cfg(feature = "quic")]
pub mod quic;

/// Video decoder abstraction for SPICE video streams.
///
/// The `video` module is gated on the `jpeg` feature because
/// [`video::MJpegVideoDecoder`] depends on [`jpeg::JpegDecoder`].
/// Enabling the default feature set (which includes `jpeg`)
/// makes the entire module available.
#[cfg(feature = "jpeg")]
pub mod video;

pub use byte_bounded_lru::{ByteBoundedLru, InsertOutcome, RefusedReason};

pub use limits::{rgba_len, MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS};

#[cfg(feature = "encode")]
pub use encode::{encode_spice_lz4, Bgrx};

#[cfg(feature = "glz")]
pub use glz::{decompress_glz, GlzDictionary};

#[cfg(feature = "jpeg")]
pub use jpeg::{best_for_platform, DecodedJpeg, JpegDecoder, JpegDecoderRsDecoder};

#[cfg(feature = "jpeg")]
pub use video::{
    for_stream, DecodedFrame, MJpegVideoDecoder, VideoDecoder, VideoDecoderError,
    SPICE_VIDEO_CODEC_TYPE_H264, SPICE_VIDEO_CODEC_TYPE_MJPEG,
};

#[cfg(feature = "mozjpeg")]
pub use jpeg::MozJpegDecoder;

#[cfg(all(feature = "jpeg", target_os = "macos"))]
pub use jpeg::ImageIoDecoder;

#[cfg(all(feature = "jpeg", target_os = "windows"))]
pub use jpeg::WicDecoder;

#[cfg(all(feature = "jpeg", feature = "mozjpeg", target_os = "linux"))]
pub use jpeg::VaapiDecoder;

#[cfg(feature = "lz")]
pub use lz::decompress_lz;

#[cfg(feature = "lz4")]
pub use lz4::decompress_spice_lz4;

#[cfg(feature = "quic")]
pub use quic::quic_decode;

/// A decompressed SPICE image: raw RGBA pixels plus their
/// dimensions and an image id used for cross-frame GLZ
/// dictionary lookup.
///
/// Invariant: `width` and `height` pass [`limits::rgba_len`], and
/// `pixels.len()` is exactly the length it returns. Every
/// consumer indexes `pixels` from the dimensions, so a buffer that
/// disagrees with them is an out-of-bounds slice waiting to happen
/// (issue #174). The constructors enforce this, which is why they
/// return `Option`.
///
/// This struct is `#[non_exhaustive]` so additional metadata
/// fields may be added in future minor releases without
/// breaking consumers, and so code outside this crate cannot
/// build one with a struct literal and bypass the invariant.
/// Construct via [`DecompressedImage::new`] or
/// [`DecompressedImage::new_glz`]. The fields stay public for
/// reading; code that mutates them is responsible for keeping the
/// invariant.
#[derive(Debug)]
#[non_exhaustive]
pub struct DecompressedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub image_id: u64,
    /// GLZ sliding-window distance: images older than
    /// `image_id - win_head_dist` may be evicted from the
    /// shared dictionary. Zero for non-GLZ images.
    pub win_head_dist: u32,
}

impl DecompressedImage {
    /// Construct a new [`DecompressedImage`] from its core
    /// fields. Sets `win_head_dist` to 0 (non-GLZ default).
    ///
    /// Returns `None` when the dimensions are refused by
    /// [`limits::rgba_len`] or `pixels.len()` is not the length it
    /// returns. Both mean the buffer and the dimensions disagree,
    /// or the image is larger than any decoder may produce, and
    /// either way the image must be dropped rather than painted.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>, image_id: u64) -> Option<Self> {
        Self::new_glz(width, height, pixels, image_id, 0)
    }

    /// Construct a GLZ [`DecompressedImage`] with a
    /// `win_head_dist` for dictionary eviction.
    ///
    /// Refuses the same inputs as [`DecompressedImage::new`].
    pub fn new_glz(
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        image_id: u64,
        win_head_dist: u32,
    ) -> Option<Self> {
        if limits::rgba_len(width as usize, height as usize) != Some(pixels.len()) {
            return None;
        }
        Some(Self {
            width,
            height,
            pixels,
            image_id,
            win_head_dist,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_a_correctly_sized_buffer() {
        let img = DecompressedImage::new(3, 2, vec![0xAB; 3 * 2 * 4], 7).expect("exact length");
        assert_eq!((img.width, img.height, img.image_id), (3, 2, 7));
        assert_eq!(img.pixels.len(), 24);
        assert_eq!(img.win_head_dist, 0);

        let img = DecompressedImage::new_glz(3, 2, vec![0; 24], 7, 5).expect("exact length");
        assert_eq!(img.win_head_dist, 5);
    }

    /// The #174 shape: a small buffer paired with large
    /// dimensions. Consumers slice `pixels` from the dimensions,
    /// so accepting this would panic downstream.
    #[test]
    fn new_refuses_a_buffer_that_disagrees_with_the_dimensions() {
        assert!(DecompressedImage::new(3, 2, vec![0; 3 * 2 * 4 - 4], 0).is_none());
        assert!(DecompressedImage::new(3, 2, vec![0; 3 * 2 * 4 + 4], 0).is_none());
        assert!(DecompressedImage::new(10000, 10000, vec![0; 2 * 2 * 4], 0).is_none());
        assert!(DecompressedImage::new_glz(3, 2, Vec::new(), 0, 0).is_none());
    }

    #[test]
    fn new_refuses_dimensions_the_limits_refuse() {
        assert!(DecompressedImage::new(0, 0, Vec::new(), 0).is_none());
        assert!(DecompressedImage::new(0, 2, Vec::new(), 0).is_none());
        assert!(DecompressedImage::new_glz(0, 0, Vec::new(), 0, 0).is_none());
        // u32::MAX x 1 would need 16 GiB to match; the dimension
        // check refuses it whatever the buffer holds.
        assert!(DecompressedImage::new(u32::MAX, 1, vec![0; 4], 0).is_none());
    }
}
