//! Shared size limits for every decoded image.
//!
//! Image dimensions reach the decoders from the wire, and the
//! server controls them. A hostile SPICE server, or anyone in the
//! middle of a non-TLS connection, can claim a 65535x65535 image
//! and have a decoder allocate 17 GiB of RGBA for it before a
//! single pixel of compressed data has been read. Checked
//! multiplication does not help: it only stops the product
//! wrapping, and 17 GiB is a perfectly representable `usize`.
//! Most of issues #171 to #177 are variations on that theme.
//!
//! Before this module each decoder picked its own limit, or had
//! none. Decoders in this crate and in the renderer size their
//! output buffers with [`rgba_len`], which applies both caps below
//! and so bounds any one decoded image to `MAX_IMAGE_PIXELS * 4`
//! bytes. [`crate::DecompressedImage::new`] applies the same check
//! to the buffer it is handed, so an image that escaped a decoder's
//! own check still cannot be constructed.

/// Upper bound on either side of a decoded image, in pixels.
///
/// Leaves headroom above an 8K display (7680x4320) on either side
/// while refusing the 65535-pixel sides the wire formats can
/// express. On its own it still admits a 16384x16384 image (1 GiB
/// of RGBA), which is why [`MAX_IMAGE_PIXELS`] also applies.
pub const MAX_IMAGE_DIMENSION: u32 = 16384;

/// Upper bound on the total pixel count of a decoded image.
///
/// 64 Mi pixels is 256 MiB of RGBA. An 8K display is about 33 Mi
/// pixels, so a real SPICE server has roughly 2x headroom; a
/// larger image means the server is malformed or adversarial.
pub const MAX_IMAGE_PIXELS: usize = 64 * 1024 * 1024;

/// Byte length of a `width` x `height` RGBA buffer, or `None` if
/// the image must be refused.
///
/// Refuses a zero side (there is nothing to decode, and the
/// decoders and consumers all assume at least one pixel), either
/// side above [`MAX_IMAGE_DIMENSION`], and a pixel count above
/// [`MAX_IMAGE_PIXELS`]. Every decoder sizes its output with this
/// rather than hand-rolling `width * height * 4`, so the limit is
/// applied in one place and before the allocation, not after.
///
/// The arguments are `usize` so callers can pass wire values
/// widened from `u32` (or `u16`) without first narrowing them.
/// Once both caps hold the multiplication cannot overflow, even
/// on a 32-bit target, so no checked arithmetic is needed here or
/// at the call site.
pub fn rgba_len(width: usize, height: usize) -> Option<usize> {
    let max_side = MAX_IMAGE_DIMENSION as usize;
    if width == 0 || height == 0 || width > max_side || height > max_side {
        return None;
    }
    let pixels = width * height;
    if pixels > MAX_IMAGE_PIXELS {
        return None;
    }
    Some(pixels * 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_SIDE: usize = MAX_IMAGE_DIMENSION as usize;

    #[test]
    fn accepts_a_plausible_image() {
        assert_eq!(rgba_len(1, 1), Some(4));
        assert_eq!(rgba_len(1920, 1080), Some(1920 * 1080 * 4));
        assert_eq!(rgba_len(7680, 4320), Some(7680 * 4320 * 4));
    }

    #[test]
    fn refuses_a_zero_side() {
        assert_eq!(rgba_len(0, 16), None);
        assert_eq!(rgba_len(16, 0), None);
        assert_eq!(rgba_len(0, 0), None);
    }

    #[test]
    fn accepts_exactly_the_dimension_cap() {
        assert_eq!(rgba_len(MAX_SIDE, 1), Some(MAX_SIDE * 4));
        assert_eq!(rgba_len(1, MAX_SIDE), Some(MAX_SIDE * 4));
    }

    #[test]
    fn refuses_one_over_the_dimension_cap() {
        assert_eq!(rgba_len(MAX_SIDE + 1, 1), None);
        assert_eq!(rgba_len(1, MAX_SIDE + 1), None);
    }

    #[test]
    fn accepts_exactly_the_pixel_cap() {
        // Both shapes are at the cap: the square one, and one with
        // a side at the dimension cap.
        assert_eq!(rgba_len(8192, 8192), Some(MAX_IMAGE_PIXELS * 4));
        let other = MAX_IMAGE_PIXELS / MAX_SIDE;
        assert_eq!(rgba_len(MAX_SIDE, other), Some(MAX_IMAGE_PIXELS * 4));
        assert_eq!(rgba_len(other, MAX_SIDE), Some(MAX_IMAGE_PIXELS * 4));
    }

    #[test]
    fn refuses_one_row_over_the_pixel_cap() {
        // Each side is within the dimension cap, so only the pixel
        // cap can refuse these.
        assert_eq!(rgba_len(8192, 8193), None);
        let other = MAX_IMAGE_PIXELS / MAX_SIDE + 1;
        assert_eq!(rgba_len(MAX_SIDE, other), None);
        assert_eq!(rgba_len(MAX_SIDE, MAX_SIDE), None);
    }

    #[test]
    fn refuses_u32_max_sides() {
        let max = u32::MAX as usize;
        assert_eq!(rgba_len(max, max), None);
        assert_eq!(rgba_len(max, 1), None);
        assert_eq!(rgba_len(1, max), None);
        assert_eq!(rgba_len(usize::MAX, usize::MAX), None);
    }
}
