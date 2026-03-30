pub mod glz;
pub mod lz;

pub use glz::decompress_glz;
pub use lz::decompress_lz;

/// Result of decompression
#[derive(Debug)]
pub struct DecompressedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>, // RGBA format
    pub image_id: u64,   // For GLZ dictionary
}
