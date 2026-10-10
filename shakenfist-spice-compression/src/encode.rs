//! SPICE image encoders, for servers. The decoders in the other
//! modules are what a client needs; this is the other direction.
//! LZ4 and JPEG are the encoders.
use anyhow::{anyhow, Result};
use jpeg_encoder::{rgb_to_ycbcr, Encoder, ImageBuffer, JpegColorType, SamplingFactor};

use crate::limits;

/// A borrowed BGRX image: 32 bits a pixel, bytes B, G, R and an
/// unused byte, rows `stride` bytes apart.
///
/// This is what an X11 ZPixmap at depth 24 holds in memory, and it
/// is also the layout of `SPICE_BITMAP_FMT_32BIT`. The fields are
/// private so that an instance always satisfies what [`Bgrx::new`]
/// checks, and the encoders can index it without further checks.
#[derive(Debug, Clone, Copy)]
pub struct Bgrx<'a> {
    data: &'a [u8],
    width: u32,
    height: u32,
    stride: usize,
}

impl<'a> Bgrx<'a> {
    /// Wrap `data` as a `width` x `height` image whose rows start
    /// `stride` bytes apart.
    ///
    /// Returns `None` when a side is zero, the size is refused by
    /// [`limits::rgba_len`] (the same caps the decoders apply),
    /// `stride` is less than `width * 4`, or `data` is too short to
    /// hold the last row. The last row needs only `width * 4` bytes,
    /// not a full `stride`, so a buffer with no trailing padding is
    /// accepted.
    pub fn new(data: &'a [u8], width: u32, height: u32, stride: usize) -> Option<Self> {
        limits::rgba_len(width as usize, height as usize)?;
        // `rgba_len` bounded both sides, so this cannot overflow.
        let row_bytes = width as usize * 4;
        if stride < row_bytes {
            return None;
        }
        let needed = stride
            .checked_mul(height as usize - 1)?
            .checked_add(row_bytes)?;
        if data.len() < needed {
            return None;
        }
        Some(Self {
            data,
            width,
            height,
            stride,
        })
    }

    /// The pixel buffer, including any row padding.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes from the start of one row to the start of the next.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// The rows without their padding, each `width * 4` bytes.
    fn rows(&self) -> impl Iterator<Item = &'a [u8]> {
        let row_bytes = self.width as usize * 4;
        self.data
            .chunks(self.stride)
            .take(self.height as usize)
            .map(move |row| &row[..row_bytes])
    }

    /// The pixels packed into `height` rows of `width * 4` bytes
    /// with the stride padding removed.
    fn packed(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width as usize * 4 * self.height as usize);
        for row in self.rows() {
            out.extend_from_slice(row);
        }
        out
    }
}

/// Encode an image as a SPICE LZ4 image.
///
/// Returns the `BinaryData` body, the bytes after the `data_size`
/// field, so the caller wraps it as the protocol crate's
/// `ImagePayload::Lz4(BinaryData)`. The body is a top-down byte of 1
/// (the first row is the top one), a `SPICE_BITMAP_FMT_32BIT` byte
/// of 8, and then the whole image as a single big-endian-length
/// framed LZ4 block. The block does not depend on any other, as in
/// spice-server's common case. Rows are packed with the stride
/// padding removed, because the decoders (and spice-common) assume
/// there is none.
///
/// This does not decide whether compressing was worthwhile. Noisy
/// images can come out longer than the raw pixels, and a caller
/// whose result is longer than `width * height * 4` should send an
/// uncompressed BITMAP instead, as spice-server does.
pub fn encode_spice_lz4(image: &Bgrx) -> Vec<u8> {
    /// `SPICE_BITMAP_FMT_32BIT`.
    const FORMAT_32BIT: u8 = 8;

    let block = lz4_flex::block::compress(&image.packed());
    let mut out = Vec::with_capacity(2 + 4 + block.len());
    out.extend_from_slice(&[1, FORMAT_32BIT]);
    // An LZ4 block of at most MAX_IMAGE_PIXELS * 4 bytes is far
    // below u32::MAX once compressed, so this cannot truncate.
    out.extend_from_slice(&(block.len() as u32).to_be_bytes());
    out.extend_from_slice(&block);
    out
}

/// Feeds a [`Bgrx`] to `jpeg-encoder` one row at a time, converting
/// to YCbCr as it goes, so the image is never copied whole.
struct BgrxRows<'a, 'b>(&'b Bgrx<'a>);

impl ImageBuffer for BgrxRows<'_, '_> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        // `encode_spice_jpeg` checked that both sides fit.
        self.0.width as u16
    }

    fn height(&self) -> u16 {
        self.0.height as u16
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let start = y as usize * self.0.stride;
        let row = &self.0.data[start..start + self.0.width as usize * 4];
        for &[b, g, r, _] in row.as_chunks::<4>().0 {
            let (y, cb, cr) = rgb_to_ycbcr(r, g, b);
            buffers[0].push(y);
            buffers[1].push(cb);
            buffers[2].push(cr);
        }
    }
}

/// Encode an image as a SPICE JPEG image.
///
/// Returns the `BinaryData` body, a baseline JFIF stream with 4:2:0
/// chroma subsampling, so the caller wraps it as the protocol
/// crate's `ImagePayload::Jpeg(BinaryData)`. The X byte of each
/// pixel is ignored. `quality` is 1 to 100; spice-server uses 85.
///
/// The JPEG has exactly the image's dimensions, which matters
/// because the client (spice-common) asserts that the JPEG's own
/// dimensions equal the image descriptor's. The pixels are
/// converted a row at a time as they are encoded, with no
/// intermediate copy of the image.
///
/// Fails when `quality` is outside 1 to 100, or when a side exceeds
/// what a JPEG can hold (65535), which [`Bgrx::new`]'s limits
/// already rule out.
pub fn encode_spice_jpeg(image: &Bgrx, quality: u8) -> Result<Vec<u8>> {
    if !(1..=100).contains(&quality) {
        return Err(anyhow!("JPEG quality {quality} is not between 1 and 100"));
    }
    if image.width > u16::MAX as u32 || image.height > u16::MAX as u32 {
        return Err(anyhow!(
            "{}x{} image is too large for JPEG",
            image.width,
            image.height
        ));
    }

    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, quality);
    // The default depends on the quality (4:4:4 from 90 up), so set
    // it rather than rely on it.
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder
        .encode_image(BgrxRows(image))
        .map_err(|e| anyhow!("JPEG encode failed: {e}"))?;
    Ok(out)
}

#[cfg(all(test, feature = "lz4"))]
mod tests {
    use super::*;
    use crate::decompress_spice_lz4;
    use proptest::prelude::*;

    #[test]
    fn encode_spice_lz4_exact_bytes_for_a_small_image() {
        // 2x2 with 4 bytes of padding on each row, filled with 0xEE
        // that must not reach the output.
        #[rustfmt::skip]
        let data = [
            1, 2, 3, 4,  5, 6, 7, 8,  0xEE, 0xEE, 0xEE, 0xEE,
            9, 10, 11, 12,  13, 14, 15, 16,
        ];
        let packed: Vec<u8> = (1..=16).collect();
        let image = Bgrx::new(&data[..], 2, 2, 12).expect("valid image");
        assert_eq!((image.width(), image.height(), image.stride()), (2, 2, 12));
        assert_eq!(image.data().len(), data.len());

        let body = encode_spice_lz4(&image);
        assert_eq!(&body[..2], &[1, 8]);
        let len = u32::from_be_bytes(body[2..6].try_into().unwrap()) as usize;
        assert_eq!(body.len(), 6 + len);
        assert_eq!(
            lz4_flex::block::decompress(&body[6..], packed.len()).unwrap(),
            packed
        );
        assert_eq!(&body[6..], &lz4_flex::block::compress(&packed)[..]);
    }

    #[test]
    fn bgrx_new_refuses_bad_geometry() {
        let data = vec![0u8; 1024];
        // Last row is exactly width * 4 bytes: 3 * 20 + 16.
        assert!(Bgrx::new(&data[..76], 4, 4, 20).is_some());
        // One byte short of that.
        assert!(Bgrx::new(&data[..75], 4, 4, 20).is_none());
        // Stride below a row.
        assert!(Bgrx::new(&data, 4, 4, 15).is_none());
        // Zero sides.
        assert!(Bgrx::new(&data, 0, 4, 16).is_none());
        assert!(Bgrx::new(&data, 4, 0, 16).is_none());
        // Over the limits, even with an arithmetic overflow in play.
        let over = limits::MAX_IMAGE_DIMENSION + 1;
        assert!(Bgrx::new(&data, over, 1, over as usize * 4).is_none());
        assert!(Bgrx::new(&data, 1, over, 4).is_none());
        assert!(Bgrx::new(&data, 16384, 16384 + 1, 16384 * 4).is_none());
        assert!(Bgrx::new(&data, 4, 4, usize::MAX).is_none());
    }

    /// An image with `pad` bytes of 0xEE garbage after each row.
    fn padded(width: usize, height: usize, pad: usize, pixels: &[u8]) -> (Vec<u8>, usize) {
        let stride = width * 4 + pad;
        let mut data = Vec::new();
        for row in pixels.chunks(width * 4).take(height) {
            data.extend_from_slice(row);
            data.extend(std::iter::repeat_n(0xEE, pad));
        }
        (data, stride)
    }

    proptest! {
        #[test]
        fn encode_spice_lz4_round_trips_through_the_decoder(
            (width, height, pad, pixels) in (1usize..=64, 1usize..=64, 0usize..=16)
                .prop_flat_map(|(w, h, pad)| {
                    (Just(w), Just(h), Just(pad), prop::collection::vec(any::<u8>(), w * h * 4))
                })
        ) {
            let (data, stride) = padded(width, height, pad, &pixels);
            let image = Bgrx::new(&data, width as u32, height as u32, stride).unwrap();
            let body = encode_spice_lz4(&image);
            let decoded = decompress_spice_lz4(&body, width, height).expect("decodes");
            let expected: Vec<u8> = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[b, g, r, _]| [r, g, b, 255])
                .collect();
            prop_assert_eq!((decoded.width as usize, decoded.height as usize), (width, height));
            prop_assert_eq!(decoded.pixels, expected);
        }

        #[test]
        fn encode_spice_lz4_compressible_images_round_trip(
            width in 1usize..=64, height in 1usize..=64, pad in 0usize..=16, value: u8
        ) {
            // Long runs exercise LZ4 matches, which random bytes
            // almost never produce.
            let pixels = vec![value; width * height * 4];
            let (data, stride) = padded(width, height, pad, &pixels);
            let image = Bgrx::new(&data, width as u32, height as u32, stride).unwrap();
            let body = encode_spice_lz4(&image);
            let decoded = decompress_spice_lz4(&body, width, height).expect("decodes");
            prop_assert!(decoded
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|&px| px == [value, value, value, 255]));
        }
    }
}

#[cfg(all(test, feature = "jpeg"))]
mod jpeg_tests {
    use super::*;
    use crate::{JpegDecoder, JpegDecoderRsDecoder};
    use proptest::prelude::*;

    /// Every decoder built, each with a name for failure messages.
    fn decoders() -> Vec<(&'static str, Box<dyn JpegDecoder>)> {
        let mut decoders: Vec<(&'static str, Box<dyn JpegDecoder>)> =
            vec![("jpeg-decoder", Box::new(JpegDecoderRsDecoder::new()))];
        #[cfg(feature = "mozjpeg")]
        decoders.push(("mozjpeg", Box::new(crate::MozJpegDecoder::new())));
        decoders
    }

    /// BGRX pixels from `f(x, y) -> [r, g, b]`, rows followed by
    /// `pad` bytes of 0xEE garbage. Returns the data and the stride.
    fn image_from(
        width: usize,
        height: usize,
        pad: usize,
        f: impl Fn(usize, usize) -> [u8; 3],
    ) -> (Vec<u8>, usize) {
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let [r, g, b] = f(x, y);
                data.extend_from_slice(&[b, g, r, 0x99]);
            }
            data.extend(std::iter::repeat_n(0xEE, pad));
        }
        (data, width * 4 + pad)
    }

    /// Mean absolute error per colour channel of `rgba` against `f`.
    fn mean_error(rgba: &[u8], width: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> f64 {
        let mut total = 0u64;
        for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
            let want = f(i % width, i / width);
            for c in 0..3 {
                total += (px[c] as i32 - want[c] as i32).unsigned_abs() as u64;
            }
        }
        total as f64 / (rgba.len() / 4 * 3) as f64
    }

    fn check(
        width: usize,
        height: usize,
        pad: usize,
        f: impl Fn(usize, usize) -> [u8; 3],
        max: f64,
    ) {
        let (data, stride) = image_from(width, height, pad, &f);
        let image = Bgrx::new(&data, width as u32, height as u32, stride).expect("valid image");
        let jpeg = encode_spice_jpeg(&image, 85).expect("encodes");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        for (name, decoder) in decoders() {
            let decoded = decoder
                .decode(&jpeg)
                .unwrap_or_else(|| panic!("{name} decodes"));
            assert_eq!(
                (decoded.width as usize, decoded.height as usize),
                (width, height)
            );
            let err = mean_error(&decoded.rgba, width, &f);
            println!("{name}: {width}x{height} pad {pad}: mean error {err:.3}");
            assert!(err < max, "{name}: mean error {err} >= {max}");
        }
    }

    #[test]
    fn encode_spice_jpeg_gradient_is_close() {
        // Not a multiple of the 16 pixel MCU size, with padding.
        check(
            97,
            61,
            12,
            |x, y| {
                [
                    (x * 255 / 96) as u8,
                    (y * 255 / 60) as u8,
                    ((x + y) * 255 / 156) as u8,
                ]
            },
            3.0,
        );
    }

    #[test]
    fn encode_spice_jpeg_solid_colour_is_close() {
        check(40, 30, 0, |_, _| [200, 30, 90], 1.0);
        check(33, 17, 8, |_, _| [10, 250, 128], 1.0);
    }

    #[test]
    fn encode_spice_jpeg_extreme_qualities_decode() {
        let f = |x: usize, y: usize| [(x * 4) as u8, (y * 4) as u8, (x * y) as u8];
        let (data, stride) = image_from(50, 40, 4, f);
        let image = Bgrx::new(&data, 50, 40, stride).expect("valid image");
        for quality in [1, 100] {
            let jpeg = encode_spice_jpeg(&image, quality).expect("encodes");
            for (name, decoder) in decoders() {
                let decoded = decoder
                    .decode(&jpeg)
                    .unwrap_or_else(|| panic!("{name} decodes"));
                assert_eq!((decoded.width, decoded.height), (50, 40));
            }
        }
    }

    #[test]
    fn encode_spice_jpeg_refuses_bad_quality() {
        let data = [0u8; 4];
        let image = Bgrx::new(&data, 1, 1, 4).expect("valid image");
        assert!(encode_spice_jpeg(&image, 0).is_err());
        assert!(encode_spice_jpeg(&image, 101).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn encode_spice_jpeg_keeps_the_dimensions(
            (width, height, pad, pixels) in (1usize..=64, 1usize..=64, 0usize..=16)
                .prop_flat_map(|(w, h, pad)| {
                    (Just(w), Just(h), Just(pad), prop::collection::vec(any::<u8>(), w * h * 4))
                })
        ) {
            let mut data = Vec::new();
            for row in pixels.chunks(width * 4) {
                data.extend_from_slice(row);
                data.extend(std::iter::repeat_n(0xEE, pad));
            }
            let image = Bgrx::new(&data, width as u32, height as u32, width * 4 + pad).expect("valid image");
            let jpeg = encode_spice_jpeg(&image, 85).expect("encodes");
            for (name, decoder) in decoders() {
                let decoded = decoder.decode(&jpeg);
                prop_assert!(decoded.is_some(), "{} decodes", name);
                let decoded = decoded.expect("valid image");
                prop_assert_eq!((decoded.width as usize, decoded.height as usize), (width, height));
            }
        }
    }
}
