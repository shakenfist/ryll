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

#[cfg(feature = "glz")]
pub mod glz;

#[cfg(feature = "lz")]
pub mod lz;

#[cfg(feature = "lz4")]
pub mod lz4;

#[cfg(feature = "quic")]
pub mod quic;

#[cfg(feature = "glz")]
pub use glz::{decompress_glz, GlzDictionary};

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
/// This struct is `#[non_exhaustive]` so additional metadata
/// fields may be added in future minor releases without
/// breaking consumers. Construct via
/// [`DecompressedImage::new`].
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
    pub fn new(width: u32, height: u32, pixels: Vec<u8>, image_id: u64) -> Self {
        Self {
            width,
            height,
            pixels,
            image_id,
            win_head_dist: 0,
        }
    }

    /// Construct a GLZ [`DecompressedImage`] with a
    /// `win_head_dist` for dictionary eviction.
    pub fn new_glz(
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        image_id: u64,
        win_head_dist: u32,
    ) -> Self {
        Self {
            width,
            height,
            pixels,
            image_id,
            win_head_dist,
        }
    }
}
