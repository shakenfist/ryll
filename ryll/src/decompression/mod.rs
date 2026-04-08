pub mod glz;
pub mod lz;
pub mod quic;

pub use glz::decompress_glz;
pub use lz::decompress_lz;
pub use quic::quic_decode;

/// Result of decompression
#[derive(Debug)]
pub struct DecompressedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub image_id: u64,
}
