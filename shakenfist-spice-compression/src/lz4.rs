/// SPICE LZ4 image decompression.
///
/// Format: 1 byte top_down, 1 byte spice_format, then per-row blocks
/// each with a 4-byte big-endian compressed size followed by the LZ4
/// compressed row data. Returns RGBA pixels.
use tracing::{debug, warn};

use crate::DecompressedImage;

/// Decompress a SPICE LZ4 image.
///
/// All or nothing: any row that cannot be decoded — a truncated
/// payload, a row whose declared compressed size runs past the end
/// of the buffer, an LZ4 error, or an unimplemented pixel format —
/// fails the whole image and returns `None`. Returning the partial
/// image instead would hand the caller attacker-chosen pixels with
/// black filler for the rest and no way to tell that had happened.
pub fn decompress_spice_lz4(data: &[u8], width: usize, height: usize) -> Option<DecompressedImage> {
    if data.len() < 2 || width == 0 || height == 0 {
        warn!("display: LZ4 data too short or zero dimensions");
        return None;
    }

    let top_down = data[0] != 0;
    let spice_format = data[1];

    debug!(
        "display: LZ4 header: top_down={}, format={}, first_16_bytes={:02x?}",
        top_down,
        spice_format,
        &data[..data.len().min(16)]
    );

    // Bytes per pixel based on spice bitmap format.
    // Format 0 (INVALID) is treated as 32BIT — some servers
    // or proxies send this for standard BGRX data.
    let bpp: usize = match spice_format {
        0 | 4 => 4, // SPICE_BITMAP_FMT_32BIT (BGRX) or unspecified
        6 => 4,     // SPICE_BITMAP_FMT_RGBA (BGRA)
        3 => 3,     // SPICE_BITMAP_FMT_24BIT (BGR)
        2 => 2,     // SPICE_BITMAP_FMT_16BIT
        other => {
            warn!("display: LZ4 unsupported spice format: {}", other);
            return None;
        }
    };

    let row_bytes = width * bpp;
    let total_pixels = width.checked_mul(height)?;
    let rgba_size = total_pixels.checked_mul(4)?;
    let mut rgba = vec![0u8; rgba_size];

    let mut offset = 2usize; // skip top_down + format bytes
    for row in 0..height {
        if offset + 4 > data.len() {
            warn!("display: LZ4 truncated at row {}/{}", row, height);
            return None;
        }

        let enc_size = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + enc_size > data.len() {
            warn!(
                "display: LZ4 row {} enc_size={} exceeds data ({})",
                row,
                enc_size,
                data.len() - offset
            );
            return None;
        }

        let row_data = match lz4_flex::decompress(&data[offset..offset + enc_size], row_bytes) {
            Ok(d) => d,
            Err(e) => {
                warn!("display: LZ4 row {} decompression failed: {}", row, e);
                return None;
            }
        };
        offset += enc_size;

        // Convert decoded row to RGBA
        let dst_row = if top_down { row } else { height - 1 - row };
        let dst_row_start = dst_row * width * 4;
        match bpp {
            4 => {
                // BGRX or BGRA → RGBA
                let has_alpha = spice_format == 6;
                for x in 0..width {
                    let s = x * 4;
                    let d = dst_row_start + x * 4;
                    if s + 3 < row_data.len() && d + 3 < rgba.len() {
                        rgba[d] = row_data[s + 2]; // R
                        rgba[d + 1] = row_data[s + 1]; // G
                        rgba[d + 2] = row_data[s]; // B
                        rgba[d + 3] = if has_alpha { row_data[s + 3] } else { 255 };
                    }
                }
            }
            3 => {
                // BGR → RGBA
                for x in 0..width {
                    let s = x * 3;
                    let d = dst_row_start + x * 4;
                    if s + 2 < row_data.len() && d + 3 < rgba.len() {
                        rgba[d] = row_data[s + 2];
                        rgba[d + 1] = row_data[s + 1];
                        rgba[d + 2] = row_data[s];
                        rgba[d + 3] = 255;
                    }
                }
            }
            _ => {
                // 16-bit: skip for now
                warn!("display: LZ4 16-bit format not implemented");
                return None;
            }
        }
    }

    Some(DecompressedImage::new(width as u32, height as u32, rgba, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return bytes-per-pixel for the spice_format values used in tests.
    fn bpp_for_format(spice_format: u8) -> usize {
        match spice_format {
            0 | 4 | 6 => 4,
            3 => 3,
            _ => panic!("bpp_for_format: unsupported test format {}", spice_format),
        }
    }

    /// Build a wire-format SPICE LZ4 image from raw pixel rows.
    ///
    /// Each row must be exactly `width * bpp_for_format(spice_format)`
    /// bytes in the on-the-wire byte order (BGRX/BGRA/BGR — i.e. as
    /// the spice-server would have sent it).
    fn encode_spice_lz4(
        top_down: bool,
        spice_format: u8,
        width: usize,
        rows: &[Vec<u8>],
    ) -> Vec<u8> {
        let bpp = bpp_for_format(spice_format);
        let row_bytes = width * bpp;
        let mut out = Vec::new();
        out.push(if top_down { 1 } else { 0 });
        out.push(spice_format);
        for row in rows {
            assert_eq!(
                row.len(),
                row_bytes,
                "test row wrong size for format {}",
                spice_format
            );
            let comp = lz4_flex::compress(row);
            out.extend_from_slice(&(comp.len() as u32).to_be_bytes());
            out.extend_from_slice(&comp);
        }
        out
    }

    // ---------------------------------------------------------------
    // Test 1: format 4 (BGRX), top_down = true, single row
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_format4_bgrx_top_down() {
        // Two pixels: red and blue-ish.
        // Wire format is BGRX.
        //   red:      R=255, G=0,   B=0   -> BGRX [0,   0,   255, 0]
        //   blue-ish: R=0,   G=128, B=255 -> BGRX [255, 128, 0,   0]
        let rows = vec![vec![0u8, 0, 255, 0, 255, 128, 0, 0]];
        let data = encode_spice_lz4(true, 4, 2, &rows);
        let result = decompress_spice_lz4(&data, 2, 1);
        assert!(result.is_some(), "expected Some, got None");
        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        // BGRX -> RGBA: R=src[2], G=src[1], B=src[0], A=255
        assert_eq!(
            img.pixels,
            vec![255, 0, 0, 255, 0, 128, 255, 255],
            "RGBA mismatch for format 4 BGRX"
        );
    }

    // ---------------------------------------------------------------
    // Test 2: format 6 (BGRA), alpha preserved, single row
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_format6_bgra() {
        // Two pixels with explicit alpha.
        // Wire format is BGRA.
        //   pixel 1: B=10, G=20, R=30, A=128 -> BGRA [10, 20, 30, 128]
        //   pixel 2: B=50, G=60, R=70, A=200 -> BGRA [50, 60, 70, 200]
        let rows = vec![vec![10u8, 20, 30, 128, 50, 60, 70, 200]];
        let data = encode_spice_lz4(true, 6, 2, &rows);
        let result = decompress_spice_lz4(&data, 2, 1);
        assert!(result.is_some(), "expected Some, got None");
        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        // BGRA -> RGBA: R=src[2], G=src[1], B=src[0], A=src[3]
        assert_eq!(
            img.pixels,
            vec![30, 20, 10, 128, 70, 60, 50, 200],
            "RGBA mismatch for format 6 BGRA"
        );
    }

    // ---------------------------------------------------------------
    // Test 3: format 3 (BGR 24-bit), single row
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_format3_bgr24() {
        // Two pixels, 3 bytes each.
        // Wire format is BGR.
        //   pixel 1: B=10, G=20, R=30 -> BGR [10, 20, 30]
        //   pixel 2: B=40, G=50, R=60 -> BGR [40, 50, 60]
        let rows = vec![vec![10u8, 20, 30, 40, 50, 60]];
        let data = encode_spice_lz4(true, 3, 2, &rows);
        let result = decompress_spice_lz4(&data, 2, 1);
        assert!(result.is_some(), "expected Some, got None");
        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        // BGR -> RGBA: R=src[2], G=src[1], B=src[0], A=255
        assert_eq!(
            img.pixels,
            vec![30, 20, 10, 255, 60, 50, 40, 255],
            "RGBA mismatch for format 3 BGR"
        );
    }

    // ---------------------------------------------------------------
    // Test 4: format 0 treated as BGRX (identical output to format 4)
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_format0_treated_as_bgrx() {
        // Same wire bytes as the format-4 test.
        let rows = vec![vec![0u8, 0, 255, 0, 255, 128, 0, 0]];
        let data = encode_spice_lz4(true, 0, 2, &rows);
        let result = decompress_spice_lz4(&data, 2, 1);
        assert!(result.is_some(), "expected Some, got None for format 0");
        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        // Format 0 is treated as BGRX, so output is identical to format 4.
        assert_eq!(
            img.pixels,
            vec![255, 0, 0, 255, 0, 128, 255, 255],
            "format 0 output should match format 4 BGRX"
        );
    }

    // ---------------------------------------------------------------
    // Test 5: top_down = false, row order is inverted
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_bottom_up_row_order() {
        // Two rows, 1 pixel wide, format 4 (BGRX).
        // Wire row 0 (first in data) = red:   BGRX [0, 0, 255, 0]
        // Wire row 1                  = green: BGRX [0, 255, 0, 0]
        //
        // With top_down=false:
        //   wire row 0 -> dst row (2 - 1 - 0) = 1 (bottom)
        //   wire row 1 -> dst row (2 - 1 - 1) = 0 (top)
        //
        // Expected output: top row = green RGBA [0, 255, 0, 255],
        //                  bottom row = red RGBA [255, 0, 0, 255].
        let rows = vec![vec![0u8, 0, 255, 0], vec![0u8, 255, 0, 0]];
        let data = encode_spice_lz4(false, 4, 1, &rows);
        let result = decompress_spice_lz4(&data, 1, 2);
        assert!(result.is_some(), "expected Some, got None for bottom-up");
        let img = result.unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 2);
        // Row 0 of output (top) should be green.
        assert_eq!(
            img.pixels[0..4],
            [0, 255, 0, 255],
            "top row should be green (bottom-up)"
        );
        // Row 1 of output (bottom) should be red.
        assert_eq!(
            img.pixels[4..8],
            [255, 0, 0, 255],
            "bottom row should be red (bottom-up)"
        );
    }

    // ---------------------------------------------------------------
    // Test 6: multi-row image (exercises the per-row loop > once)
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_multi_row() {
        // Four rows, 1 pixel wide, format 4 (BGRX), top_down = true.
        // Each row is a different distinct colour so cross-contamination
        // shows up.
        //   row 0: red   -> BGRX [0, 0, 255, 0]     -> RGBA [255, 0, 0, 255]
        //   row 1: green -> BGRX [0, 255, 0, 0]     -> RGBA [0, 255, 0, 255]
        //   row 2: blue  -> BGRX [255, 0, 0, 0]     -> RGBA [0, 0, 255, 255]
        //   row 3: white -> BGRX [255, 255, 255, 0] -> RGBA [255, 255, 255, 255]
        let rows = vec![
            vec![0u8, 0, 255, 0],
            vec![0u8, 255, 0, 0],
            vec![255u8, 0, 0, 0],
            vec![255u8, 255, 255, 0],
        ];
        let data = encode_spice_lz4(true, 4, 1, &rows);
        let result = decompress_spice_lz4(&data, 1, 4);
        assert!(result.is_some(), "expected Some for multi-row");
        let img = result.unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 4);
        assert_eq!(img.pixels.len(), 16);
        assert_eq!(img.pixels[0..4], [255, 0, 0, 255], "row 0 should be red");
        assert_eq!(img.pixels[4..8], [0, 255, 0, 255], "row 1 should be green");
        assert_eq!(img.pixels[8..12], [0, 0, 255, 255], "row 2 should be blue");
        assert_eq!(
            img.pixels[12..16],
            [255, 255, 255, 255],
            "row 3 should be white"
        );
    }

    // ---------------------------------------------------------------
    // Test 7: truncated input is rejected outright
    //
    // The decoder used to `break` out of the row loop and still
    // return `Some`, leaving the undecoded rows as the zeros the
    // buffer was allocated with. That silently painted a partial,
    // server-chosen image with black filler and gave the caller no
    // way to notice. Truncation now fails the whole image so the
    // caller can drop the frame.
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_truncated_returns_none() {
        // Two rows, 1 pixel wide, format 4 (BGRX).
        // Row 0: red.  Row 1: green.
        let rows = vec![vec![0u8, 0, 255, 0], vec![0u8, 255, 0, 0]];
        let mut data = encode_spice_lz4(true, 4, 1, &rows);

        // Truncate the last byte of row 1's compressed payload. Row 0
        // decodes, then the decoder reads row 1's 4-byte enc_size and
        // finds `offset + enc_size > data.len()`.
        //
        // data layout:
        //   [top_down, fmt, sz0[4], comp_row0..., sz1[4], comp_row1...]
        let truncate_to = data.len() - 1;
        data.truncate(truncate_to);

        assert!(
            decompress_spice_lz4(&data, 1, 2).is_none(),
            "a truncated payload must be rejected, not returned as a partial image"
        );
    }

    // ---------------------------------------------------------------
    // Test 7b: truncation before a row header is also rejected
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_truncated_row_header_returns_none() {
        let rows = vec![vec![0u8, 0, 255, 0], vec![0u8, 255, 0, 0]];
        let mut data = encode_spice_lz4(true, 4, 1, &rows);

        // Cut two bytes into row 1's 4-byte size field, so the decoder
        // hits the `offset + 4 > data.len()` guard rather than the
        // enc_size one.
        let row1_size_field_start = data.len() - (4 + lz4_flex::compress(&rows[1]).len());
        data.truncate(row1_size_field_start + 2);

        assert!(
            decompress_spice_lz4(&data, 1, 2).is_none(),
            "a payload truncated inside a row header must be rejected"
        );
    }

    // ---------------------------------------------------------------
    // Test 8: zero width or height returns None
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_zero_dimensions_returns_none() {
        // The guard at lz4.rs:12-15 returns None for zero width or height.
        assert!(
            decompress_spice_lz4(&[], 0, 10).is_none(),
            "zero width should return None"
        );
        assert!(
            decompress_spice_lz4(&[], 10, 0).is_none(),
            "zero height should return None"
        );
    }

    // ---------------------------------------------------------------
    // Test 9: absurd dimensions hit the checked_mul guards
    //
    // width and height reach the decoder straight off the wire, so
    // the two `checked_mul`s are the only thing between a hostile
    // header and a wrapped, undersized allocation.
    //
    // Note the widths below stay at or under `usize::MAX / 4`: the
    // `row_bytes = width * bpp` on the line above the guards is still
    // an unchecked multiply, so a wider value panics there in a debug
    // build before the guards are reached. Bounding that multiply is
    // a separate change; this test covers the guards that exist.
    // ---------------------------------------------------------------
    #[test]
    fn decompress_spice_lz4_absurd_dimensions_returns_none() {
        // Just the two header bytes: top_down + format 4 (BGRX). The
        // dimension guards fire before any row data is needed.
        let data = vec![1u8, 4];

        assert!(
            decompress_spice_lz4(&data, usize::MAX / 4, 8).is_none(),
            "width * height overflow must return None"
        );
        assert!(
            decompress_spice_lz4(&data, 2, usize::MAX).is_none(),
            "overflow driven by an absurd height must return None"
        );
        // width * height fits; the 4-bytes-per-pixel RGBA size does not.
        assert!(
            decompress_spice_lz4(&data, usize::MAX / 4, 2).is_none(),
            "RGBA size overflow must return None"
        );
    }
}
