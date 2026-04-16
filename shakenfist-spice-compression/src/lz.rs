/// LZ decompression algorithm
///
/// LZ is a simpler compression scheme than GLZ - it only supports
/// back-references within the current image, no cross-frame references.
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt};
use std::io::{Cursor, Read};

use crate::DecompressedImage;

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
    let top_down = cursor.read_u32::<BigEndian>()? != 0;

    // Output buffer (RGBA)
    let output_size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("LZ image dimensions overflow: {}x{}", width, height))?;
    let mut output = vec![0u8; output_size];

    // Compressed data starts after the 28-byte header.
    // This implementation follows the kerbside Python reference
    // decompressor in testclient/ryll/decompressors.py.
    let mut data_offset = 28usize;
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
            // Back-reference within current image
            let mut length = (ctrl >> 5) as usize;
            let mut pixel_offset = ((ctrl & 0x1F) as usize) << 8;

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

            // Read offset low byte
            if data_offset >= data.len() {
                break;
            }
            let code = data[data_offset] as usize;
            data_offset += 1;
            pixel_offset += code;

            // Large offset encoding
            if code == 255 && (pixel_offset - code) == (31 << 8) {
                if data_offset + 2 > data.len() {
                    break;
                }
                pixel_offset =
                    ((data[data_offset] as usize) << 8) | (data[data_offset + 1] as usize);
                pixel_offset += 8191;
                data_offset += 2;
            }

            pixel_offset += 1;

            // Copy pixels from earlier in output
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
        }
    }

    if !top_down {
        let row_bytes = width as usize * 4;
        let mut flipped = vec![0u8; output.len()];
        for y in 0..height as usize {
            let src = y * row_bytes;
            let dst = (height as usize - 1 - y) * row_bytes;
            flipped[dst..dst + row_bytes].copy_from_slice(&output[src..src + row_bytes]);
        }
        output = flipped;
    }

    Ok(DecompressedImage {
        width,
        height,
        pixels: output,
        image_id: 0,
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
