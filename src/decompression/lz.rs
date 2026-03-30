/// LZ decompression algorithm
///
/// LZ is a simpler compression scheme than GLZ - it only supports
/// back-references within the current image, no cross-frame references.
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt};
use std::io::{Cursor, Read};

use super::DecompressedImage;

const LZ_MAGIC: &[u8; 4] = b"  ZL";
const LZ_MAX_COPY: u8 = 32;

/// Decompress LZ image data
///
/// # Arguments
/// * `data` - The compressed LZ data
///
/// # Returns
/// * `DecompressedImage` with RGBA pixels
pub fn decompress_lz(data: &[u8]) -> Result<DecompressedImage> {
    if data.len() < 28 {
        return Err(anyhow!("LZ data too short for header"));
    }

    let mut cursor = Cursor::new(data);

    // Read header (big-endian!)
    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic)?;

    // Magic is stored reversed
    if magic != *LZ_MAGIC {
        return Err(anyhow!(
            "Invalid LZ magic: {:?} (expected {:?})",
            magic,
            LZ_MAGIC
        ));
    }

    let version_major = cursor.read_u16::<BigEndian>()?;
    let version_minor = cursor.read_u16::<BigEndian>()?;

    if version_major != 1 {
        return Err(anyhow!(
            "Unsupported LZ version: {}.{}",
            version_major,
            version_minor
        ));
    }

    // 3 bytes padding
    let mut _padding = [0u8; 3];
    cursor.read_exact(&mut _padding)?;

    let _img_type = cursor.read_u8()?;
    let width = cursor.read_u32::<BigEndian>()?;
    let height = cursor.read_u32::<BigEndian>()?;
    let _stride = cursor.read_u32::<BigEndian>()?;
    let _top_down = cursor.read_u32::<BigEndian>()?;

    // Output buffer (RGBA)
    let output_size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("LZ image dimensions overflow: {}x{}", width, height))?;
    let mut output = vec![0u8; output_size];

    // Current position in compressed data (after header)
    let mut data_offset = 28usize;
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
            // Back-reference within current image
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

            // Offset extraction (5 bits from ctrl + 8 bits from next byte)
            let mut pixel_offset = ((ctrl & 0x1F) as usize) << 8;

            if data_offset >= data.len() {
                break;
            }

            let offset_low = data[data_offset] as usize;
            data_offset += 1;

            // Check for large offset encoding
            if offset_low == 255 && pixel_offset == (31 << 8) {
                if data_offset + 2 > data.len() {
                    break;
                }
                // Read 16-bit big-endian value
                let hi = data[data_offset] as usize;
                let lo = data[data_offset + 1] as usize;
                data_offset += 2;

                pixel_offset = (hi << 8) | lo;
                pixel_offset += 8191; // Large offset base
            } else {
                pixel_offset |= offset_low;
            }

            pixel_offset += 1; // Adjust from 0-indexed

            // Copy pixels from earlier in output
            let copy_bytes = length * 4;
            let src_start = out_idx.saturating_sub(pixel_offset * 4);

            for i in 0..copy_bytes {
                if out_idx + i >= output_size {
                    break;
                }
                // Handle overlapping copies
                let src_idx = src_start + (i % (pixel_offset * 4));
                if src_idx < output.len() {
                    output[out_idx + i] = output[src_idx];
                }
            }

            out_idx += copy_bytes;
        }
    }

    Ok(DecompressedImage {
        width,
        height,
        pixels: output,
        image_id: 0, // LZ doesn't use image IDs
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lz_header_parse() {
        // Minimal LZ header (28 bytes)
        let mut header = Vec::new();
        header.extend_from_slice(b"  ZL"); // Magic
        header.extend_from_slice(&[0, 1]); // Version major
        header.extend_from_slice(&[0, 0]); // Version minor
        header.extend_from_slice(&[0, 0, 0]); // Padding
        header.push(0); // Type
        header.extend_from_slice(&[0, 0, 0, 2]); // Width = 2
        header.extend_from_slice(&[0, 0, 0, 2]); // Height = 2
        header.extend_from_slice(&[0, 0, 0, 8]); // Stride = 8
        header.extend_from_slice(&[0, 0, 0, 1]); // Top down = 1

        // Add some literal pixels (ctrl=3 means 4 pixels)
        header.push(3);
        // 4 BGR triplets
        for _ in 0..4 {
            header.extend_from_slice(&[0, 128, 255]); // BGR
        }

        let result = decompress_lz(&header);
        assert!(result.is_ok());

        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 16); // 2x2x4 RGBA
    }
}
