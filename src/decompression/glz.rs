/// GLZ decompression algorithm
///
/// GLZ is a dictionary-based compression scheme that can reference:
/// 1. Pixels within the current image (back-references)
/// 2. Pixels from previously decompressed images (cross-frame references)
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt};
use std::collections::HashMap;
use std::io::{Cursor, Read};

use super::DecompressedImage;

const GLZ_MAGIC: &[u8; 4] = b"ZL.G"; // Reversed "G.LZ"
const LZ_MAX_COPY: u8 = 32;

/// Decompress GLZ image data
///
/// # Arguments
/// * `data` - The compressed GLZ data
/// * `previous_images` - Dictionary of previously decompressed images for cross-frame refs
///
/// # Returns
/// * `DecompressedImage` with RGBA pixels
pub fn decompress_glz(
    data: &[u8],
    previous_images: &HashMap<u64, Vec<u8>>,
) -> Result<DecompressedImage> {
    if data.len() < 33 {
        return Err(anyhow!("GLZ data too short for header"));
    }

    let mut cursor = Cursor::new(data);

    // Read header (big-endian!)
    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic)?;

    // Magic is stored reversed
    if magic != *GLZ_MAGIC {
        return Err(anyhow!(
            "Invalid GLZ magic: {:?} (expected {:?})",
            magic,
            GLZ_MAGIC
        ));
    }

    let version_major = cursor.read_u16::<BigEndian>()?;
    let version_minor = cursor.read_u16::<BigEndian>()?;

    if version_major != 1 {
        return Err(anyhow!(
            "Unsupported GLZ version: {}.{}",
            version_major,
            version_minor
        ));
    }

    let type_packed = cursor.read_u8()?;
    let _img_type = type_packed & 0x0F;
    let _top_down = (type_packed >> 4) & 0x01;

    let width = cursor.read_u32::<BigEndian>()?;
    let height = cursor.read_u32::<BigEndian>()?;
    let _stride = cursor.read_u32::<BigEndian>()?;
    let image_id = cursor.read_u64::<BigEndian>()?;
    let win_head_dist = cursor.read_u32::<BigEndian>()?;

    // Output buffer (RGBA)
    let output_size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("GLZ image dimensions overflow: {}x{}", width, height))?;
    let mut output = vec![0u8; output_size];

    // Current position in compressed data (after header)
    let mut data_offset = 33usize;
    let mut out_idx = 0usize;

    while out_idx < output_size && data_offset < data.len() {
        let ctrl = data[data_offset];
        data_offset += 1;

        if ctrl < LZ_MAX_COPY {
            // Literal pixels: (ctrl + 1) RGB triplets
            let num_pixels = (ctrl + 1) as usize;

            for _ in 0..num_pixels {
                if data_offset + 3 > data.len() || out_idx + 4 > output_size {
                    break;
                }

                // BGR -> RGBA conversion
                output[out_idx] = data[data_offset + 2]; // R
                output[out_idx + 1] = data[data_offset + 1]; // G
                output[out_idx + 2] = data[data_offset]; // B
                output[out_idx + 3] = 255; // A

                data_offset += 3;
                out_idx += 4;
            }
        } else {
            // Back-reference or cross-image reference
            let mut length = ((ctrl >> 5) & 0x07) as usize;

            // Variable length encoding
            if length == 7 {
                while data_offset < data.len() {
                    let b = data[data_offset];
                    data_offset += 1;
                    length += b as usize;
                    if b != 255 {
                        break;
                    }
                }
            }
            length += 2; // Minimum copy length

            // Pixel flag and offset extraction
            let pixel_flag = (ctrl >> 4) & 0x01;

            if data_offset >= data.len() {
                break;
            }

            let mut pixel_offset: usize;
            let mut image_dist: u64 = 0;

            if pixel_flag == 0 {
                // Simple back-reference within current image
                pixel_offset = (ctrl & 0x0F) as usize;

                if data_offset >= data.len() {
                    break;
                }
                pixel_offset |= (data[data_offset] as usize) << 4;
                data_offset += 1;

                // Adjust for self-reference
                if pixel_offset == 0 {
                    pixel_offset = 1;
                }
            } else {
                // Cross-image reference
                pixel_offset = (ctrl & 0x0F) as usize;

                if data_offset >= data.len() {
                    break;
                }
                let byte1 = data[data_offset];
                data_offset += 1;

                pixel_offset |= (byte1 as usize & 0x0F) << 4;

                // Extract image distance
                image_dist = ((byte1 >> 4) & 0x0F) as u64;

                if data_offset >= data.len() {
                    break;
                }
                let byte2 = data[data_offset];
                data_offset += 1;

                pixel_offset |= (byte2 as usize) << 8;
                image_dist |= ((byte2 >> 4) as u64) << 4;

                // Check for extended encoding
                let extra_flag = (byte1 >> 4) & 0x01;
                if extra_flag != 0 && data_offset < data.len() {
                    let byte3 = data[data_offset];
                    data_offset += 1;

                    pixel_offset |= ((byte3 & 0x1F) as usize) << 12;
                    image_dist |= ((byte3 >> 5) as u64) << 8;

                    if (byte3 >> 5) == 0x07 && data_offset < data.len() {
                        let byte4 = data[data_offset];
                        data_offset += 1;
                        image_dist |= (byte4 as u64) << 11;
                    }
                }
            }

            // Calculate source image ID
            let source_id = if image_dist == 0 {
                image_id
            } else {
                image_id.saturating_sub(image_dist + win_head_dist as u64)
            };

            // Copy pixels
            let copy_bytes = length * 4;

            if image_dist == 0 || source_id == image_id {
                // Copy from current output buffer
                let src_start = out_idx.saturating_sub(pixel_offset * 4);

                for i in 0..copy_bytes {
                    if out_idx + i >= output_size {
                        break;
                    }
                    let src_idx = src_start + (i % (pixel_offset * 4));
                    if src_idx < output.len() {
                        output[out_idx + i] = output[src_idx];
                    }
                }
            } else {
                // Copy from previous image
                if let Some(prev_img) = previous_images.get(&source_id) {
                    let src_start = prev_img.len().saturating_sub(pixel_offset * 4);

                    for i in 0..copy_bytes {
                        if out_idx + i >= output_size {
                            break;
                        }
                        let src_idx = src_start + i;
                        if src_idx < prev_img.len() {
                            output[out_idx + i] = prev_img[src_idx];
                        }
                    }
                }
                // If previous image not found, pixels stay black (0)
            }

            out_idx += copy_bytes;
        }
    }

    Ok(DecompressedImage {
        width,
        height,
        pixels: output,
        image_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glz_header_parse() {
        // Minimal GLZ header (33 bytes)
        let mut header = Vec::new();
        header.extend_from_slice(GLZ_MAGIC); // Magic
        header.extend_from_slice(&[0, 1]); // Version major
        header.extend_from_slice(&[0, 0]); // Version minor
        header.push(0); // Type packed
        header.extend_from_slice(&[0, 0, 0, 2]); // Width = 2
        header.extend_from_slice(&[0, 0, 0, 2]); // Height = 2
        header.extend_from_slice(&[0, 0, 0, 8]); // Stride = 8
        header.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]); // Image ID = 1
        header.extend_from_slice(&[0, 0, 0, 0]); // Win head dist = 0

        // Add some literal pixels (ctrl=3 means 4 pixels)
        header.push(3);
        // 4 BGR triplets
        for _ in 0..4 {
            header.extend_from_slice(&[0, 128, 255]); // BGR
        }

        let result = decompress_glz(&header, &HashMap::new());
        assert!(result.is_ok());

        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 16); // 2x2x4 RGBA
    }
}
