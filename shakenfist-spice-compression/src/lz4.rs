//! SPICE LZ4 image decompression (`SPICE_IMAGE_TYPE_LZ4`).
//!
//! An LZ4 image is a spice.proto `BinaryData`: a little-endian u32
//! `data_size` and then that many bytes, which are what this module
//! decodes. Those bytes are a top-down flag, a `SPICE_BITMAP_FMT_*`
//! format byte, and then one or more blocks, each a big-endian u32
//! length followed by one raw LZ4 block (no frame header, no
//! checksum). The blocks are dependent: each may refer back into the
//! output of the blocks before it, so they decode in order into one
//! buffer of `height` unpadded rows. Rows are in memory order, so
//! the first decoded row is the top one only when the top-down flag
//! is set. The framing was confirmed against spice-server 0.15.2
//! captures; see docs/spice-protocol.md.
use tracing::{debug, warn};

use crate::{limits, DecompressedImage};

/// The `SPICE_BITMAP_FMT_*` values an LZ4 image can carry
/// (spice-protocol enums.h:216-229). This crate does not depend on
/// the protocol crate, so they are repeated here.
mod bitmap_fmt {
    /// 16-bit x555: a little-endian u16 with red in bits 10-14,
    /// green in 5-9 and blue in 0-4.
    pub const BIT16: u8 = 6;
    /// 24-bit: B, G, R.
    pub const BIT24: u8 = 7;
    /// 32-bit: B, G, R and an unused byte.
    pub const BIT32: u8 = 8;
    /// 32-bit with alpha: B, G, R, A.
    pub const RGBA: u8 = 9;
}

/// How far back an LZ4 match can reach. A block can refer only to
/// the last this-many bytes of earlier output, so they are all the
/// dictionary it needs.
const LZ4_MAX_DISTANCE: usize = 64 * 1024;

/// Decompress a SPICE LZ4 image.
///
/// `data` is the image's `BinaryData` body, the bytes after its
/// `data_size`, and `width` and `height` come from its image
/// descriptor.
///
/// All or nothing: a body too short for its two header bytes, an
/// unknown format, no blocks, a block whose length runs past the end
/// of the body, an LZ4 error, or blocks that decode to anything but
/// exactly `height` rows fails the whole image and returns `None`.
/// Returning a partial image instead would hand the caller
/// attacker-chosen pixels with black filler for the rest and no way
/// to tell that had happened.
pub fn decompress_spice_lz4(data: &[u8], width: usize, height: usize) -> Option<DecompressedImage> {
    let [top_down, format, blocks @ ..] = data else {
        warn!("display: LZ4 data too short: {} bytes", data.len());
        return None;
    };
    let top_down = *top_down != 0;
    let format = *format;

    debug!(
        "display: LZ4 header: top_down={}, format={}, {} bytes of blocks",
        top_down,
        format,
        blocks.len()
    );

    let bpp: usize = match format {
        bitmap_fmt::BIT16 => 2,
        bitmap_fmt::BIT24 => 3,
        bitmap_fmt::BIT32 | bitmap_fmt::RGBA => 4,
        other => {
            warn!("display: LZ4 unsupported bitmap format: {}", other);
            return None;
        }
    };

    // `rgba_len` refuses a zero side, an over-large side and an
    // over-large pixel count before anything is allocated. With at
    // most 4 bytes a pixel the decoded size is no larger than the
    // RGBA one, but `width` and `height` come off the wire, so it
    // gets its own checked multiply rather than leaning on that.
    let Some(rgba_size) = limits::rgba_len(width, height) else {
        warn!(
            "display: LZ4 image dimensions refused: {}x{}",
            width, height
        );
        return None;
    };
    let row_bytes = width.checked_mul(bpp)?;
    let mut decoded = vec![0u8; row_bytes.checked_mul(height)?];

    if blocks.is_empty() {
        warn!("display: LZ4 image has no blocks");
        return None;
    }
    let mut blocks = blocks;
    let mut pos = 0usize;
    while !blocks.is_empty() {
        let Some((len, rest)) = blocks.split_first_chunk::<4>() else {
            warn!(
                "display: LZ4 block length truncated after {} decoded bytes",
                pos
            );
            return None;
        };
        let len = u32::from_be_bytes(*len) as usize;
        let Some((block, rest)) = rest.split_at_checked(len) else {
            warn!(
                "display: LZ4 block of {} bytes runs past the data ({} left)",
                len,
                rest.len()
            );
            return None;
        };
        blocks = rest;

        if pos == decoded.len() {
            warn!("display: LZ4 block after the image is complete");
            return None;
        }
        // The dictionary is the output so far and the block decodes
        // into the rest, so split the buffer between them rather than
        // copying either. lz4_flex never writes past the slice it is
        // given, so a block that decodes too much is an error here.
        let (done, tail) = decoded.split_at_mut(pos);
        let dict = &done[pos.saturating_sub(LZ4_MAX_DISTANCE)..];
        match lz4_flex::block::decompress_into_with_dict(block, tail, dict) {
            Ok(n) => pos += n,
            Err(e) => {
                warn!(
                    "display: LZ4 block decompression failed after {} decoded bytes: {}",
                    pos, e
                );
                return None;
            }
        }
    }
    if pos != decoded.len() {
        warn!(
            "display: LZ4 blocks decoded {} bytes, expected {}",
            pos,
            decoded.len()
        );
        return None;
    }

    let mut rgba = vec![0u8; rgba_size];
    let src_rows = decoded.chunks_exact(row_bytes);
    let dst_rows = rgba.chunks_exact_mut(width * 4);
    if top_down {
        src_rows
            .zip(dst_rows)
            .for_each(|(src, dst)| convert_row(format, src, dst));
    } else {
        src_rows
            .zip(dst_rows.rev())
            .for_each(|(src, dst)| convert_row(format, src, dst));
    }

    // `rgba_len` accepted these dimensions above, which caps each side
    // at `MAX_IMAGE_DIMENSION`, so the casts cannot truncate and `new`
    // cannot refuse a buffer it sized.
    DecompressedImage::new(width as u32, height as u32, rgba, 0)
}

/// Convert one decoded row of `format` pixels to RGBA. Alpha is kept
/// only for `RGBA`; the other formats are opaque.
fn convert_row(format: u8, src: &[u8], dst: &mut [u8]) {
    let dst = dst.as_chunks_mut::<4>().0.iter_mut();
    match format {
        bitmap_fmt::BIT16 => {
            // Widen each 5-bit channel to 8 bits by repeating its top
            // bits, so 0 stays 0 and 31 becomes 255.
            let expand = |v: u16| {
                let v = (v & 0x1f) as u8;
                (v << 3) | (v >> 2)
            };
            for (s, d) in src.as_chunks::<2>().0.iter().zip(dst) {
                let v = u16::from_le_bytes(*s);
                *d = [expand(v >> 10), expand(v >> 5), expand(v), 255];
            }
        }
        bitmap_fmt::BIT24 => {
            for (&[b, g, r], d) in src.as_chunks::<3>().0.iter().zip(dst) {
                *d = [r, g, b, 255];
            }
        }
        bitmap_fmt::RGBA => {
            for (&[b, g, r, a], d) in src.as_chunks::<4>().0.iter().zip(dst) {
                *d = [r, g, b, a];
            }
        }
        _ => {
            for (&[b, g, r, _], d) in src.as_chunks::<4>().0.iter().zip(dst) {
                *d = [r, g, b, 255];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an LZ4 image body from blocks of raw pixel bytes, each
    /// compressed on its own (so not referring back into earlier
    /// blocks) and framed with a big-endian length.
    fn lz4_body(top_down: bool, format: u8, blocks: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![u8::from(top_down), format];
        for block in blocks {
            push_block(&mut out, &lz4_flex::block::compress(block));
        }
        out
    }

    fn push_block(out: &mut Vec<u8>, compressed: &[u8]) {
        out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
        out.extend_from_slice(compressed);
    }

    /// B,G,R,X pixels as RGBA with A = 255.
    fn bgrx_to_rgba(pixels: &[u8]) -> Vec<u8> {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[b, g, r, _]| [r, g, b, 255])
            .collect()
    }

    // ---------------------------------------------------------------
    // Pixel formats, from hand-built bodies
    // ---------------------------------------------------------------

    #[test]
    fn decompress_spice_lz4_bit32_ignores_the_x_byte() {
        // Two pixels, B,G,R,X with a nonzero X that must not leak into
        // alpha:
        //   red:      R=255, G=0,   B=0   -> [0,   0,   255, 0x55]
        //   blue-ish: R=0,   G=128, B=255 -> [255, 128, 0,   0xAA]
        let data = lz4_body(true, 8, &[&[0, 0, 255, 0x55, 255, 128, 0, 0xAA]]);
        let img = decompress_spice_lz4(&data, 2, 1).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.pixels, vec![255, 0, 0, 255, 0, 128, 255, 255]);
    }

    #[test]
    fn decompress_spice_lz4_rgba_keeps_alpha() {
        // B,G,R,A: [10, 20, 30, 128] and [50, 60, 70, 200].
        let data = lz4_body(true, 9, &[&[10, 20, 30, 128, 50, 60, 70, 200]]);
        let img = decompress_spice_lz4(&data, 2, 1).unwrap();
        assert_eq!(img.pixels, vec![30, 20, 10, 128, 70, 60, 50, 200]);
    }

    #[test]
    fn decompress_spice_lz4_bit24() {
        // B,G,R: [10, 20, 30] and [40, 50, 60].
        let data = lz4_body(true, 7, &[&[10, 20, 30, 40, 50, 60]]);
        let img = decompress_spice_lz4(&data, 2, 1).unwrap();
        assert_eq!(img.pixels, vec![30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn decompress_spice_lz4_bit16_expands_each_channel() {
        // Little-endian x555. 0x7fff is white and 0x7c00 pure red.
        // 0x8210 is r=0, g=16, b=16, each 16 widening to 0x84, with
        // the unused top bit set, which must be ignored.
        let pixels: Vec<u8> = [0x7fffu16, 0x7c00, 0x8210]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let data = lz4_body(true, 6, &[&pixels]);
        let img = decompress_spice_lz4(&data, 3, 1).unwrap();
        assert_eq!(
            img.pixels,
            vec![255, 255, 255, 255, 255, 0, 0, 255, 0, 0x84, 0x84, 255]
        );
    }

    #[test]
    fn decompress_spice_lz4_bottom_up_reverses_rows() {
        // One pixel wide, two rows. The first row in memory is red and
        // the second green; bottom-up, green is the top row.
        let data = lz4_body(false, 8, &[&[0, 0, 255, 0, 0, 255, 0, 0]]);
        let img = decompress_spice_lz4(&data, 1, 2).unwrap();
        assert_eq!(img.pixels, vec![0, 255, 0, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn decompress_spice_lz4_blocks_need_not_end_on_rows() {
        // Three pixels by two rows, split across blocks mid-pixel: the
        // blocks are one byte stream, not one block for each row.
        let pixels: Vec<u8> = (0..24).collect();
        let data = lz4_body(true, 8, &[&pixels[..5], &pixels[5..19], &pixels[19..]]);
        let img = decompress_spice_lz4(&data, 3, 2).unwrap();
        assert_eq!(img.pixels, bgrx_to_rgba(&pixels));
    }

    // A block may refer back as far as 64 KiB into the output of the
    // blocks before it. Make the first block longer than that, so the
    // dictionary the decoder hands the second block is a window into
    // the middle of the buffer rather than all of it.
    #[test]
    fn decompress_spice_lz4_dependent_block_after_more_than_64k() {
        let width = 128;
        let height = 160;
        let row_bytes = width * 4;
        let pixels: Vec<u8> = (0..height)
            .flat_map(|y| (0..row_bytes).map(move |x| ((x * 7 + y * 3) % 251) as u8))
            .collect();
        let split = 140 * row_bytes;
        assert!(split > LZ4_MAX_DISTANCE);
        let (first, second) = pixels.split_at(split);
        let dependent =
            lz4_flex::block::compress_with_dict(second, &first[split - LZ4_MAX_DISTANCE..]);
        assert!(
            !matches!(lz4_flex::block::decompress(&dependent, second.len()), Ok(d) if d == second),
            "the second block must refer into the first, or this tests nothing"
        );

        let mut data = lz4_body(true, 8, &[first]);
        push_block(&mut data, &dependent);
        let img = decompress_spice_lz4(&data, width, height).unwrap();
        assert!(img.pixels == bgrx_to_rgba(&pixels));
    }

    // ---------------------------------------------------------------
    // Refusals
    // ---------------------------------------------------------------

    #[test]
    fn decompress_spice_lz4_refuses_unknown_formats() {
        // Every format byte but the four LZ4 carries, including the
        // 0, 2, 3 and 4 an earlier decoder accepted.
        let pixels = [0u8; 8];
        for format in (0..=255u8).filter(|f| !(6..=9).contains(f)) {
            let data = lz4_body(true, format, &[&pixels]);
            assert!(
                decompress_spice_lz4(&data, 2, 1).is_none(),
                "format {format} must be refused"
            );
        }
    }

    #[test]
    fn decompress_spice_lz4_refuses_a_body_without_blocks() {
        assert!(decompress_spice_lz4(&[], 1, 1).is_none());
        assert!(decompress_spice_lz4(&[1], 1, 1).is_none());
        assert!(decompress_spice_lz4(&[1, 8], 1, 1).is_none());
    }

    #[test]
    fn decompress_spice_lz4_refuses_a_truncated_body() {
        let pixels: Vec<u8> = (0..32).collect();
        let data = lz4_body(true, 8, &[&pixels[..16], &pixels[16..]]);
        assert!(decompress_spice_lz4(&data, 4, 2).is_some());
        // Every cut: inside the first block's length or data, at the
        // boundary between the blocks, and inside the second block's
        // length or data.
        for len in 2..data.len() {
            assert!(
                decompress_spice_lz4(&data[..len], 4, 2).is_none(),
                "body cut to {len} of {} bytes must be refused",
                data.len()
            );
        }
    }

    #[test]
    fn decompress_spice_lz4_refuses_the_wrong_amount_of_data() {
        let pixels: Vec<u8> = (0..32).collect();
        let data = lz4_body(true, 8, &[&pixels]);
        assert!(decompress_spice_lz4(&data, 4, 2).is_some());
        // Fewer rows than the blocks hold, or more.
        assert!(decompress_spice_lz4(&data, 4, 1).is_none());
        assert!(decompress_spice_lz4(&data, 4, 3).is_none());
        // A further block once the image is complete.
        let mut extra = data.clone();
        push_block(&mut extra, &lz4_flex::block::compress(&[0]));
        assert!(decompress_spice_lz4(&extra, 4, 2).is_none());
        // Stray bytes too short to be a block length.
        let mut stray = data.clone();
        stray.extend_from_slice(&[0, 0]);
        assert!(decompress_spice_lz4(&stray, 4, 2).is_none());
    }

    #[test]
    fn decompress_spice_lz4_refuses_a_block_length_past_the_data() {
        let pixels: Vec<u8> = (0..32).collect();
        let mut data = lz4_body(true, 8, &[&pixels]);
        let len = u32::from_be_bytes(data[2..6].try_into().unwrap());
        data[2..6].copy_from_slice(&(len + 1).to_be_bytes());
        assert!(decompress_spice_lz4(&data, 4, 2).is_none());
        data[2..6].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decompress_spice_lz4(&data, 4, 2).is_none());
    }

    #[test]
    fn decompress_spice_lz4_zero_dimensions_returns_none() {
        let data = lz4_body(true, 8, &[&[0, 0, 0, 0]]);
        assert!(decompress_spice_lz4(&data, 0, 1).is_none());
        assert!(decompress_spice_lz4(&data, 1, 0).is_none());
    }

    // Width and height reach the decoder straight off the wire. The
    // shared `rgba_len` cap refuses all of these before anything is
    // allocated, including 65535 x 65535, whose 17 GiB is perfectly
    // representable on a 64-bit target.
    #[test]
    fn decompress_spice_lz4_absurd_dimensions_returns_none() {
        let data = lz4_body(true, 8, &[&[0, 0, 0, 0]]);
        for (width, height) in [
            (usize::MAX / 4, 8),
            (2, usize::MAX),
            (usize::MAX / 4, 2),
            (usize::MAX, 1),
            (65535, 65535),
            // Just over the total-pixel cap with both sides legal.
            (16384, 16385),
        ] {
            assert!(
                decompress_spice_lz4(&data, width, height).is_none(),
                "{width}x{height} must be refused"
            );
        }
    }

    // ---------------------------------------------------------------
    // Fixtures: tests/fixtures/lz4/README.md describes each file
    // ---------------------------------------------------------------

    /// An image spice-server sent, with its expected pixels from a QMP
    /// screendump.
    struct Capture {
        name: &'static str,
        body: &'static [u8],
        /// RGBA, top row first, compressed with a 4-byte size prefix.
        expected: &'static [u8],
        width: usize,
        height: usize,
    }

    const CAPTURES: &[Capture] = &[
        Capture {
            name: "capture-32x11",
            body: include_bytes!("../tests/fixtures/lz4/capture-32x11.bin"),
            expected: include_bytes!("../tests/fixtures/lz4/capture-32x11.rgba.lz4"),
            width: 32,
            height: 11,
        },
        Capture {
            name: "capture-32x800",
            body: include_bytes!("../tests/fixtures/lz4/capture-32x800.bin"),
            expected: include_bytes!("../tests/fixtures/lz4/capture-32x800.rgba.lz4"),
            width: 32,
            height: 800,
        },
        Capture {
            name: "capture-1280x800",
            body: include_bytes!("../tests/fixtures/lz4/capture-1280x800.bin"),
            expected: include_bytes!("../tests/fixtures/lz4/capture-1280x800.rgba.lz4"),
            width: 1280,
            height: 800,
        },
    ];

    #[test]
    fn decompress_spice_lz4_matches_captured_screendumps() {
        // LZ4 is lossless, so the decode must match exactly.
        for capture in CAPTURES {
            let name = capture.name;
            let expected = lz4_flex::decompress_size_prepended(capture.expected).unwrap();
            let img = decompress_spice_lz4(capture.body, capture.width, capture.height)
                .unwrap_or_else(|| panic!("{name} did not decode"));
            assert_eq!(
                (img.width as usize, img.height as usize),
                (capture.width, capture.height)
            );
            assert!(img.pixels == expected, "{name} differs from its screendump");
        }
    }

    /// The synthetic fixtures are all this size.
    const SYNTHETIC_W: usize = 37;
    const SYNTHETIC_H: usize = 16;

    /// The expected RGBA, top row first, for the synthetic fixtures'
    /// formula. `repeat` is the three-block file's rule: pixels at
    /// `x >= 18` take the formula at row `y % 5`.
    fn synthetic_rgba(format: u8, repeat: bool) -> Vec<u8> {
        // The 16-bit fixture keeps each channel's top five bits, which
        // decode widened as `(v << 3) | (v >> 2)`.
        let widen = |c: u8| (c & 0xf8) | (c >> 5);
        let mut out = Vec::new();
        for y in 0..SYNTHETIC_H {
            for x in 0..SYNTHETIC_W {
                let fy = if repeat && x >= 18 { y % 5 } else { y };
                let r = ((x * 7 + fy) & 0xff) as u8;
                let g = ((fy * 13) & 0xff) as u8;
                let b = ((x ^ fy) & 0xff) as u8;
                let a = ((x + fy * 3) & 0xff) as u8;
                out.extend_from_slice(&match format {
                    bitmap_fmt::BIT16 => [widen(r), widen(g), widen(b), 255],
                    bitmap_fmt::RGBA => [r, g, b, a],
                    _ => [r, g, b, 255],
                });
            }
        }
        out
    }

    const THREE_BLOCKS: &[u8] =
        include_bytes!("../tests/fixtures/lz4/synthetic-32-topdown-3blocks.bin");

    #[test]
    fn decompress_spice_lz4_matches_synthetic_formula() {
        let fixtures: &[(&str, &[u8], u8, bool)] = &[
            ("synthetic-32-topdown-3blocks", THREE_BLOCKS, 8, true),
            (
                "synthetic-32-bottomup",
                include_bytes!("../tests/fixtures/lz4/synthetic-32-bottomup.bin"),
                8,
                false,
            ),
            (
                "synthetic-24",
                include_bytes!("../tests/fixtures/lz4/synthetic-24.bin"),
                7,
                false,
            ),
            (
                "synthetic-16",
                include_bytes!("../tests/fixtures/lz4/synthetic-16.bin"),
                6,
                false,
            ),
            (
                "synthetic-rgba",
                include_bytes!("../tests/fixtures/lz4/synthetic-rgba.bin"),
                9,
                false,
            ),
        ];
        for &(name, body, format, repeat) in fixtures {
            assert_eq!(body[1], format, "{name} format byte");
            let img = decompress_spice_lz4(body, SYNTHETIC_W, SYNTHETIC_H)
                .unwrap_or_else(|| panic!("{name} did not decode"));
            assert!(
                img.pixels == synthetic_rgba(format, repeat),
                "{name} differs from its formula"
            );
        }
    }

    /// The offset of each block's length field in a body, then the end.
    fn block_offsets(body: &[u8]) -> Vec<usize> {
        let mut offsets = Vec::new();
        let mut at = 2;
        while at < body.len() {
            offsets.push(at);
            at += 4 + u32::from_be_bytes(body[at..at + 4].try_into().unwrap()) as usize;
        }
        assert_eq!(at, body.len());
        offsets.push(at);
        offsets
    }

    #[test]
    fn decompress_spice_lz4_three_block_fixture_is_dependent() {
        // Blocks 2 and 3 refer back into the blocks before them, so
        // neither decodes on its own. The formula test above passes
        // for that file only because the decoder hands each block the
        // output before it.
        let offsets = block_offsets(THREE_BLOCKS);
        assert_eq!(offsets.len(), 4);
        for pair in offsets[1..].windows(2) {
            let block = &THREE_BLOCKS[pair[0] + 4..pair[1]];
            assert!(lz4_flex::block::decompress(block, SYNTHETIC_W * SYNTHETIC_H * 4).is_err());
        }
    }

    #[test]
    fn decompress_spice_lz4_refuses_the_three_block_fixture_cut_short() {
        let offsets = block_offsets(THREE_BLOCKS);
        let mut cuts = Vec::new();
        for pair in offsets.windows(2) {
            // At the boundary before each block, inside its length,
            // just after its length, and halfway through its data.
            cuts.extend([
                pair[0],
                pair[0] + 2,
                pair[0] + 4,
                (pair[0] + 4 + pair[1]) / 2,
            ]);
        }
        // And one byte short of the end.
        cuts.push(THREE_BLOCKS.len() - 1);
        for cut in cuts {
            assert!(
                decompress_spice_lz4(&THREE_BLOCKS[..cut], SYNTHETIC_W, SYNTHETIC_H).is_none(),
                "the three-block fixture cut to {cut} bytes must be refused"
            );
        }
    }

    #[test]
    fn decompress_spice_lz4_refuses_a_capture_with_an_unknown_format() {
        let capture = &CAPTURES[0];
        let (width, height) = (capture.width, capture.height);
        assert!(decompress_spice_lz4(capture.body, width, height).is_some());
        for format in [0, 4, 5, 10] {
            let mut body = capture.body.to_vec();
            body[1] = format;
            assert!(decompress_spice_lz4(&body, width, height).is_none());
        }
    }
}
