//! Manual GLZ dictionary-reference benchmark; no server or timing assertions.
//!
//! Run from the repository root:
//! cargo run -p shakenfist-spice-compression --release --no-default-features --features glz --example glz_references
//!
//! Use the same command/profile on both revisions. Stream construction,
//! dictionary insertion, pixel validation and warmup are outside the timings.

use anyhow::{ensure, Result};
use shakenfist_spice_compression::{decompress_glz, GlzDictionary};
use std::{hint::black_box, time::Instant};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const ITERATIONS: u32 = 20;

/// Encode RGB32 cross-image references to consecutive regions of image 0.
/// The GLZ match format is defined by spice-server's glz-encode-match.tmpl.c.
fn stream(references: u32) -> Vec<u8> {
    let pixels = WIDTH * HEIGHT;
    assert_eq!(pixels % references, 0);
    let length = pixels / references;
    let mut data = b"  ZL\x00\x01\x00\x01\x18".to_vec(); // GLZ 1.1, top-down RGB32
    for value in [WIDTH, HEIGHT, WIDTH * 4] {
        data.extend_from_slice(&value.to_be_bytes());
    }
    data.extend_from_slice(&1u64.to_be_bytes()); // current image id
    data.extend_from_slice(&1u32.to_be_bytes()); // dictionary window includes id 0

    for fragment in 0..references {
        let offset = fragment * length;
        let extended_offset = offset >= 4096;
        data.push(
            ((length.min(7) << 5) as u8)
                | if extended_offset { 0x10 } else { 0 }
                | (offset & 15) as u8,
        );
        if length >= 7 {
            let mut remainder = length - 7;
            while remainder >= 255 {
                data.push(255);
                remainder -= 255;
            }
            data.push(remainder as u8);
        }
        data.push((offset >> 4) as u8);
        if extended_offset {
            let long_offset = offset >= (1 << 17);
            // One image-distance byte; optionally one more pixel-offset byte.
            data.push(0x40 | if long_offset { 0x20 } else { 0 } | ((offset >> 12) & 31) as u8);
            data.push(1); // previous image
            if long_offset {
                data.push((offset >> 17) as u8);
            }
        } else {
            data.push(1); // short pixel offset, image distance 1
        }
    }
    data
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    // Non-uniform RGBA pixels expose incorrect reference offsets.
    let expected: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|pixel| [pixel as u8, (pixel >> 8) as u8, (pixel % 251) as u8, 255])
        .collect();
    let dictionary = GlzDictionary::with_cap(expected.len() * 2);
    dictionary.insert(0, expected.clone());

    for references in [1, 128] {
        let data = stream(references);
        let decoded = runtime.block_on(decompress_glz(&data, &dictionary))?;
        ensure!(
            (decoded.width, decoded.height) == (WIDTH, HEIGHT) && decoded.pixels == expected,
            "incorrect pixels for {references} references"
        );
        drop(decoded);
        for _ in 0..3 {
            black_box(runtime.block_on(decompress_glz(black_box(&data), black_box(&dictionary)))?);
        }

        let start = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(runtime.block_on(decompress_glz(black_box(&data), black_box(&dictionary)))?);
        }
        let milliseconds = start.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
        println!(
            "{WIDTH}x{HEIGHT}, {references:3} reference(s): {milliseconds:.3} ms/decode ({ITERATIONS} iterations)"
        );
    }
    Ok(())
}
