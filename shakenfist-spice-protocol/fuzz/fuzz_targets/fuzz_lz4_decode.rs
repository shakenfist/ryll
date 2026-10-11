#![no_main]

use libfuzzer_sys::fuzz_target;

// decompress_spice_lz4 faces the display channel: an LZ4 image's BinaryData
// body comes from the server, which may be hostile. The first two input bytes
// are the claimed width and height (each plus one, so 1 to 256) and the rest
// is the body. Besides "never panics", whatever the decoder accepts must be
// exactly the requested size, because the renderer reads the pixel buffer as
// if it were complete. A zero side must always be refused.
fuzz_target!(|data: &[u8]| {
    let Some((dims, body)) = data.split_first_chunk::<2>() else {
        return;
    };
    let width = dims[0] as usize + 1;
    let height = dims[1] as usize + 1;

    if let Some(image) = shakenfist_spice_compression::decompress_spice_lz4(body, width, height) {
        assert_eq!(image.width as usize, width);
        assert_eq!(image.height as usize, height);
        assert_eq!(image.pixels.len(), width * height * 4);
    }

    // A zero side is never a valid image.
    assert!(shakenfist_spice_compression::decompress_spice_lz4(body, 0, height).is_none());
    assert!(shakenfist_spice_compression::decompress_spice_lz4(body, width, 0).is_none());
});
