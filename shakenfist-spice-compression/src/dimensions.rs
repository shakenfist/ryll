/// Round a size down to the even dimensions H.264 requires.
///
/// 4:2:0 chroma subsampling halves both axes, so both must be even.
/// `H264Encoder::encode` accepts an image of an odd size whose
/// rounded size is the encoder's, and drops its last row or column,
/// so a producer of odd frames rounds with this when it picks the
/// encoder's size, and compares sizes after rounding.
pub fn even_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}
