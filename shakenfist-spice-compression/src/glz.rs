/// GLZ decompression algorithm
///
/// GLZ is a dictionary-based compression scheme that can reference:
/// 1. Pixels within the current image (back-references)
/// 2. Pixels from previously decompressed images (cross-frame references)
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Mutex;

use crate::DecompressedImage;

const GLZ_MAGIC: &[u8; 4] = b"  ZL";
const LZ_MAX_COPY: u8 = 32;
/// Total timeout for a cross-frame reference miss (ms).
const CROSS_REF_TIMEOUT_MS: u64 = 100;

/// Shared GLZ image dictionary with a notification mechanism
/// so cross-frame references can wake immediately when a
/// referenced image is inserted by another channel.
pub struct GlzDictionary {
    images: Mutex<HashMap<u64, Vec<u8>>>,
    notify: tokio::sync::Notify,
}

impl GlzDictionary {
    /// Create a new empty dictionary.
    pub fn new() -> Self {
        Self {
            images: Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Insert an image into the dictionary and wake any
    /// waiters blocked on a cross-frame reference.
    pub fn insert(&self, image_id: u64, pixels: Vec<u8>) {
        self.images.lock().unwrap().insert(image_id, pixels);
        self.notify.notify_waiters();
    }

    /// Remove a specific image from the dictionary.
    /// Returns true if the image was present.
    pub fn remove(&self, image_id: &u64) -> bool {
        self.images.lock().unwrap().remove(image_id).is_some()
    }

    /// Evict images with IDs below `oldest_valid`.
    /// Returns the number of entries removed.
    pub fn evict_older_than(&self, oldest_valid: u64) -> usize {
        let mut dict = self.images.lock().unwrap();
        let before = dict.len();
        dict.retain(|&id, _| id >= oldest_valid);
        before - dict.len()
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.images.lock().unwrap().clear();
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.images.lock().unwrap().len()
    }

    /// Whether the dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.images.lock().unwrap().is_empty()
    }

    /// Total bytes of pixel data stored.
    pub fn total_bytes(&self) -> usize {
        self.images.lock().unwrap().values().map(|v| v.len()).sum()
    }

    /// Snapshot all image IDs (sorted) for diagnostics.
    pub fn image_ids(&self) -> Vec<u64> {
        let dict = self.images.lock().unwrap();
        let mut ids: Vec<u64> = dict.keys().copied().collect();
        ids.sort_unstable();
        ids
    }
}

impl Default for GlzDictionary {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn decompress_glz(data: &[u8], dictionary: &GlzDictionary) -> Result<DecompressedImage> {
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
    let top_down = (type_packed >> 4) & 0x01 != 0;

    let width = cursor.read_u32::<BigEndian>()?;
    let height = cursor.read_u32::<BigEndian>()?;
    let _stride = cursor.read_u32::<BigEndian>()?;
    let image_id = cursor.read_u64::<BigEndian>()?;
    let win_head_dist = cursor.read_u32::<BigEndian>()?;

    tracing::debug!(
        "glz: header id={}, {}x{}, type={}, top_down={}, win_head_dist={}",
        image_id,
        width,
        height,
        _img_type,
        top_down,
        win_head_dist
    );

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
                for i in 0..image_flag {
                    if data_offset >= data.len() {
                        break;
                    }
                    let b = data[data_offset];
                    data_offset += 1;
                    image_dist += (b as u64) << (8 * i);
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
                // Copy from a previous image in the dictionary.
                // Clone the referenced pixels while holding the lock,
                // then drop the lock before copying into output.
                let source_id = image_id.wrapping_sub(image_dist);

                // Try an immediate lookup first (common case).
                let prev_pixels = {
                    let dict = dictionary.images.lock().unwrap();
                    dict.get(&source_id).cloned()
                };

                let prev_pixels = match prev_pixels {
                    Some(p) => p,
                    None => {
                        // Wait for the image to be inserted by another
                        // channel, with a total timeout as a safety net.
                        tracing::debug!(
                            "glz: cross-ref waiting: source_id={}, dict_size={}",
                            source_id,
                            dictionary.len()
                        );
                        let deadline = tokio::time::Instant::now()
                            + tokio::time::Duration::from_millis(CROSS_REF_TIMEOUT_MS);
                        let mut resolved = None;
                        loop {
                            let remaining =
                                deadline.saturating_duration_since(tokio::time::Instant::now());
                            if remaining.is_zero() {
                                break;
                            }
                            match tokio::time::timeout(remaining, dictionary.notify.notified())
                                .await
                            {
                                Ok(()) => {
                                    let dict = dictionary.images.lock().unwrap();
                                    if let Some(pixels) = dict.get(&source_id) {
                                        resolved = Some(pixels.clone());
                                        break;
                                    }
                                    // Notification was for a different image;
                                    // loop and wait again.
                                }
                                Err(_) => break, // timeout
                            }
                        }

                        match resolved {
                            Some(p) => p,
                            None => {
                                tracing::warn!(
                                    "glz: cross-image ref to id {} not in dictionary \
                                     after {}ms timeout (current={}, dist={})",
                                    source_id,
                                    CROSS_REF_TIMEOUT_MS,
                                    image_id,
                                    image_dist
                                );
                                out_idx += length * 4;
                                continue;
                            }
                        }
                    }
                };

                tracing::debug!(
                    "glz: cross-ref hit: source_id={}, pixel_offset={}, length={}, \
                     prev_img_len={}",
                    source_id,
                    pixel_offset,
                    length,
                    prev_pixels.len()
                );
                let mut pi_idx = pixel_offset * 4;
                for _ in 0..length {
                    if out_idx + 4 > output_size || pi_idx + 4 > prev_pixels.len() {
                        break;
                    }
                    output[out_idx] = prev_pixels[pi_idx];
                    output[out_idx + 1] = prev_pixels[pi_idx + 1];
                    output[out_idx + 2] = prev_pixels[pi_idx + 2];
                    output[out_idx + 3] = prev_pixels[pi_idx + 3];
                    out_idx += 4;
                    pi_idx += 4;
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

    Ok(DecompressedImage::new_glz(
        width,
        height,
        output,
        image_id,
        win_head_dist,
    ))
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

        let dict = GlzDictionary::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(decompress_glz(&header, &dict));
        assert!(result.is_ok());

        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 16); // 2x2x4 RGBA
    }
}
