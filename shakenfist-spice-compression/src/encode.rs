//! SPICE image encoders, for servers. The decoders in the other
//! modules are what a client needs; this is the other direction.
//! LZ4 is the first encoder.
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
