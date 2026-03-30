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

const GLZ_MAGIC: &[u8; 4] = b"  ZL";
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
    let _win_head_dist = cursor.read_u32::<BigEndian>()?;

    // Output buffer (RGBA)
    let output_size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("GLZ image dimensions overflow: {}x{}", width, height))?;
    let mut output = vec![0u8; output_size];

    // Compressed data starts after the 33-byte header.
    // This implementation follows the kerbside Python reference
    // decompressor in testclient/ryll/decompressors.py.
    let mut data_offset = 33usize;
    let mut out_idx = 0usize;

    while out_idx < output_size && data_offset < data.len() {
        let ctrl = data[data_offset];
        data_offset += 1;

        if ctrl < LZ_MAX_COPY {
            // Literal pixels: (ctrl + 1) BGR triplets
            for _ in 0..(ctrl as usize + 1) {
                if data_offset + 3 > data.len() || out_idx + 4 > output_size {
                    break;
                }
                output[out_idx] = data[data_offset + 2]; // R
                output[out_idx + 1] = data[data_offset + 1]; // G
                output[out_idx + 2] = data[data_offset]; // B
                output[out_idx + 3] = 255; // A
                data_offset += 3;
                out_idx += 4;
            }
        } else {
            // Back-reference or cross-image reference
            let mut length = (ctrl >> 5) as usize;
            let pixel_flag = (ctrl >> 4) & 0x01;
            let mut pixel_offset = (ctrl & 0x0F) as usize;

            // Variable-length run encoding
            if length == 7 {
                loop {
                    if data_offset >= data.len() {
                        break;
                    }
                    let b = data[data_offset];
                    data_offset += 1;
                    length += b as usize;
                    if b != 255 {
                        break;
                    }
                }
            }
            // length is number of pixels to copy (minimum 2 already encoded)

            // Read the next byte -- always present, adds to pixel_offset
            if data_offset >= data.len() {
                break;
            }
            let code1 = data[data_offset];
            data_offset += 1;
            pixel_offset += (code1 as usize) << 4;

            // Read another byte for image_flag and distance/offset
            if data_offset >= data.len() {
                break;
            }
            let code2 = data[data_offset];
            data_offset += 1;
            let image_flag = ((code2 >> 6) & 0x03) as usize;

            let mut image_dist: u64;

            if pixel_flag == 0 {
                // The offset is into a previous image
                image_dist = (code2 & 0x3F) as u64;
                for i in 0..image_flag {
                    if data_offset >= data.len() {
                        break;
                    }
                    let b = data[data_offset];
                    data_offset += 1;
                    image_dist += (b as u64) << (6 + (8 * i));
                }
            } else {
                // Extended pixel offset within the same image
                let pf2 = (code2 >> 5) & 0x01;
                pixel_offset += ((code2 & 0x1F) as usize) << 12;
                image_dist = 0;
                for _ in 0..image_flag {
                    if data_offset >= data.len() {
                        break;
                    }
                    let b = data[data_offset];
                    data_offset += 1;
                    image_dist += b as u64;
                }
                if pf2 != 0 {
                    if data_offset >= data.len() {
                        break;
                    }
                    let b = data[data_offset];
                    data_offset += 1;
                    pixel_offset += (b as usize) << 17;
                }
            }

            if image_dist == 0 {
                // Copy from current image (self-reference)
                pixel_offset += 1;
                let src_start = out_idx.wrapping_sub(pixel_offset * 4);

                if pixel_offset == 1 {
                    // Repeat the directly previous pixel
                    for _ in 0..length {
                        if out_idx + 4 > output_size || src_start >= output.len() {
                            break;
                        }
                        output[out_idx] = output[src_start];
                        output[out_idx + 1] = output[src_start + 1];
                        output[out_idx + 2] = output[src_start + 2];
                        output[out_idx + 3] = output[src_start + 3];
                        out_idx += 4;
                    }
                } else {
                    // Copy a block of earlier pixels, advancing the source
                    let mut src = src_start;
                    for _ in 0..length {
                        if out_idx + 4 > output_size || src + 4 > output.len() {
                            break;
                        }
                        output[out_idx] = output[src];
                        output[out_idx + 1] = output[src + 1];
                        output[out_idx + 2] = output[src + 2];
                        output[out_idx + 3] = output[src + 3];
                        out_idx += 4;
                        src += 4;
                    }
                }
            } else {
                // Copy from a previous image in the dictionary
                let source_id = image_id.wrapping_sub(image_dist);
                if let Some(prev_img) = previous_images.get(&source_id) {
                    let mut pi_idx = pixel_offset * 4;
                    for _ in 0..length {
                        if out_idx + 4 > output_size || pi_idx + 4 > prev_img.len() {
                            break;
                        }
                        output[out_idx] = prev_img[pi_idx];
                        output[out_idx + 1] = prev_img[pi_idx + 1];
                        output[out_idx + 2] = prev_img[pi_idx + 2];
                        output[out_idx + 3] = prev_img[pi_idx + 3];
                        out_idx += 4;
                        pi_idx += 4;
                    }
                } else {
                    // Image not in dictionary -- leave pixels black
                    out_idx += length * 4;
                }
            }
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
