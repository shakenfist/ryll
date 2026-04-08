# shakenfist-spice-compression

Pure-Rust implementations of the SPICE image-stream
decompression algorithms:

- **QUIC** — the SPICE wavelet/arithmetic codec (not the QUIC
  transport protocol). Feature `quic` (default).
- **GLZ** — dictionary-based cross-frame LZ with a shared
  previous-images dictionary. Async because of a cross-channel
  retry loop. Feature `glz` (default), pulls in `tokio`.
- **LZ** — single-frame LZ. Feature `lz` (default).
- **LZ4** — SPICE's per-row LZ4 image format (each row is
  independently compressed with `lz4_flex` and a 4-byte length
  prefix). Feature `lz4` (default), pulls in `lz4_flex`.

All four decoders return a `DecompressedImage { width, height,
pixels: Vec<u8>, image_id }` on success (with the historical
exception of `quic_decode`, which returns `Option<Vec<u8>>` and
leaves the wrapping to the caller — this asymmetry will be
smoothed out before the first published release). The struct
is `#[non_exhaustive]`; construct via `DecompressedImage::new(...)`.

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
use shakenfist_spice_compression::{decompress_glz, DecompressedImage};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// Shared GLZ dictionary across all display channels.
let dict: Arc<Mutex<HashMap<u64, Vec<u8>>>> =
    Arc::new(Mutex::new(HashMap::new()));

// Decompress a GLZ image from wire bytes.
let image: DecompressedImage =
    decompress_glz(glz_bytes, &dict).await?;
# Ok::<(), anyhow::Error>(())
```

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
