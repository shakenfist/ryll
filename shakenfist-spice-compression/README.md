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

All four decoders return a `DecompressedImage { width, height,
pixels: Vec<u8>, image_id, win_head_dist }` on success (with
the historical exception of `quic_decode`, which returns
`Option<Vec<u8>>` and leaves the wrapping to the caller — this
asymmetry will be smoothed out before the first published
release). The struct is `#[non_exhaustive]`; construct via
`DecompressedImage::new(...)` (sets `win_head_dist` to 0) or
`DecompressedImage::new_glz(...)` for GLZ images.

## Status

This crate is **not yet published to crates.io**. The `0.0.0`
entry there is a Phase 2 name reservation; the real `0.1.0`
release will follow once API polish is complete (see the
[extraction plan](https://github.com/shakenfist/ryll/blob/develop/docs/plans/PLAN-crate-extraction.md)
for the polish list).

The crate name deliberately covers both directions of the
codecs (compression and decompression) so that compression
implementations can be added in future minor releases without
a crate rename. SPICE proxies (such as the planned Rust
rewrite of the shakenfist kerbside proxy) and server-side
tooling are likely to want compression as well as
decompression. The current `0.1.0`-target code is
decompression only, matching what ryll needs today.

Internal consumers (ryll itself and the planned kerbside
rewrite) should depend on this crate via a workspace path or a
git dependency until `0.1.0` ships.

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
