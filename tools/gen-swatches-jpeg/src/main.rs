//! One-shot tool used to generate the `swatches.jpg` fixture for
//! `shakenfist-spice-compression`'s `ImageIoDecoder` test. Produces
//! a 32x32 RGBA image with R/G/B/yellow swatches in four quadrants,
//! encodes it as JPEG quality 85, and writes to the path given on
//! argv. Re-run only when the fixture needs to change.

use std::env;
use std::fs;
use std::path::PathBuf;

const W: usize = 32;
const H: usize = 32;

fn main() {
    let out_path: PathBuf = env::args()
        .nth(1)
        .expect("usage: gen-swatches-jpeg OUTPATH")
        .into();

    // Build a 32x32 RGBA image: four 16x16 quadrants:
    //   TL = red    (255,  0,  0)
    //   TR = green  (  0,255,  0)
    //   BL = blue   (  0,  0,255)
    //   BR = yellow (255,255,  0)
    let mut src = Vec::<u8>::with_capacity(W * H * 4);
    for y in 0..H {
        for x in 0..W {
            let (r, g, b) = match (x < W / 2, y < H / 2) {
                (true, true) => (255u8, 0u8, 0u8),    // TL red
                (false, true) => (0u8, 255u8, 0u8),   // TR green
                (true, false) => (0u8, 0u8, 255u8),   // BL blue
                (false, false) => (255u8, 255u8, 0u8), // BR yellow
            };
            src.push(r);
            src.push(g);
            src.push(b);
            src.push(255);
        }
    }

    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_EXT_RGBA);
    comp.set_size(W, H);
    comp.set_quality(85.0);
    let mut started = comp.start_compress(Vec::new()).expect("start_compress");
    started.write_scanlines(&src).expect("write_scanlines");
    let jpeg_bytes = started.finish().expect("finish");

    fs::write(&out_path, &jpeg_bytes).expect("write output");
    eprintln!(
        "wrote {} bytes to {}",
        jpeg_bytes.len(),
        out_path.display()
    );
}
