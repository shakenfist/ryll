# shakenfist-spice-compression

Pure-Rust implementations of the SPICE image-stream
decompression algorithms:

- **QUIC** — the SPICE wavelet/arithmetic codec (not the QUIC
  transport protocol). Feature `quic` (default).
- **GLZ** — dictionary-based cross-frame LZ with a shared
  `GlzDictionary` and notify-based cross-frame reference
  resolution. Feature `glz` (default), pulls in `tokio`.
- **LZ** — single-frame LZ. Feature `lz` (default).
- **LZ4** — SPICE's per-row LZ4 image format (each row is
  independently compressed with `lz4_flex` and a 4-byte length
  prefix). Feature `lz4` (default), pulls in `lz4_flex`.

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
for GLZ images.

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
