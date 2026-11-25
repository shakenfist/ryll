//! The two copies of the SPICE video codec-type numbers must agree.
//!
//! `shakenfist-spice-protocol` names the whole wire enum, for any
//! consumer that has to put a codec type on a stream-create or
//! recognise one it is refusing. `shakenfist-spice-compression` names
//! the two it can decode, beside the `for_stream` dispatch that reads
//! them.
//!
//! They are separate copies on purpose. The compression crate cannot
//! depend on the protocol crate without pulling the link handshake's
//! TLS stack -- tokio-rustls, rsa, sha1, webpki-roots, x509-parser --
//! into an encode-only codec build, which is the cost
//! shakenfist/ryll#555 exists to remove rather than relocate. So this
//! test is the only thing standing between the two copies and silent
//! drift, and it lives in the renderer because that is the one crate
//! which depends on both.
//!
//! Its own file rather than a case inside `channels::display`'s tests:
//! that module's file is 6,300 lines and has an open issue to split it
//! (shakenfist/ryll#629), so it is not the place to add to.

use shakenfist_spice_compression::{SPICE_VIDEO_CODEC_TYPE_H264, SPICE_VIDEO_CODEC_TYPE_MJPEG};
use shakenfist_spice_protocol::video_codec_type as codec;

/// The numbers the compression crate decodes by are the numbers the
/// protocol crate names.
#[test]
fn the_two_crates_agree_on_the_codecs_ryll_decodes() {
    assert_eq!(SPICE_VIDEO_CODEC_TYPE_MJPEG, codec::MJPEG);
    assert_eq!(SPICE_VIDEO_CODEC_TYPE_H264, codec::H264);
}

/// And the whole enum matches spice-protocol's `enums.h`, which
/// numbers `SPICE_VIDEO_CODEC_TYPE_*` from 1 in this order. The three
/// ryll cannot decode are still wire values a server may send, and a
/// stream-create carrying one is refused by number rather than
/// guessed at.
#[test]
fn the_wire_enum_matches_the_spice_protocol_header() {
    assert_eq!(
        [
            codec::MJPEG,
            codec::VP8,
            codec::H264,
            codec::VP9,
            codec::H265,
        ],
        [1, 2, 3, 4, 5]
    );
}
