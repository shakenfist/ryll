# shakenfist-spice-compression

Pure-Rust implementations of the SPICE image-stream
decompression algorithms:

- **QUIC** — the SPICE wavelet/arithmetic codec (not the QUIC
  transport protocol). Feature `quic` (default).
- **GLZ** — dictionary-based cross-frame LZ with a shared
  `GlzDictionary` and notify-based cross-frame reference
  resolution. Feature `glz` (default), pulls in `tokio`.
- **LZ** — single-frame LZ. Feature `lz` (default).
- **LZ4** — SPICE's LZ4 image format (a format byte, then dependent
  raw LZ4 blocks with big-endian lengths, decoded with `lz4_flex`).
  Feature `lz4` (default), pulls in `lz4_flex`.

The crate name covers both directions. The current release
provides decompression only, matching what the ryll client
needs today. Compression may be added in future minor releases
(SPICE proxies and server-side tooling are likely consumers)
without a crate rename.

## Return types

`decompress_glz`, `decompress_lz`, and `decompress_spice_lz4`
return a `DecompressedImage { width, height, pixels: Vec<u8>,
image_id, win_head_dist }` (directly, inside `Option`, or
inside `Result` depending on the codec). `quic_decode` is the
exception: it returns `Option<Vec<u8>>` and leaves the
dimension wrapping to the caller. The struct is
`#[non_exhaustive]`; construct via `DecompressedImage::new(...)`
(sets `win_head_dist` to 0) or `DecompressedImage::new_glz(...)`
for GLZ images. Both return `Option<DecompressedImage>`, and
return `None` unless `pixels.len()` equals
`limits::rgba_len(width, height)`.

## Limits

The `limits` module holds `MAX_IMAGE_DIMENSION`,
`MAX_IMAGE_PIXELS` and `rgba_len(width, height)`. Image
dimensions on the wire are attacker-controlled, so every decoder
checks them against these caps before allocating its RGBA output,
and `rgba_len` is the checked way to size an RGBA buffer. Decoders
that wrap a codec library (H.264, and the WIC and ImageIO JPEG
backends) only learn the dimensions after the library has parsed
the frame, so its intermediate buffers rely on its own limits.

## Usage

```rust,ignore
use shakenfist_spice_compression::{
    decompress_glz, DecompressedImage, GlzDictionary,
};

// Shared GLZ dictionary across all display channels.
let dict = GlzDictionary::new();

// Decompress a GLZ image from wire bytes.
let image: DecompressedImage =
    decompress_glz(glz_bytes, &dict).await?;

// Insert into dictionary for cross-frame references.
// This also notifies any waiters blocked on this image.
dict.insert(image.image_id, image.pixels.clone());
# Ok::<(), anyhow::Error>(())
```

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
Internal consumers within the shakenfist project (ryll and
the planned Rust rewrite of the kerbside SPICE proxy) depend
on this crate via workspace paths; external consumers should
use `cargo add shakenfist-spice-compression`.

## License

Apache-2.0
