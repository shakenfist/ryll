# shakenfist-spice-compression

**This crate is a functionally empty crate that exists to
reserve the crate name for an upcoming pure-Rust SPICE image
compression and decompression library extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client. It
should not be used.**

## Status

Reserved by the [shakenfist](https://github.com/shakenfist)
project. The real `0.1.0` release will be published when the
extraction work in
[ryll](https://github.com/shakenfist/ryll/blob/develop/docs/plans/PLAN-crate-extraction.md)
lands. Until then, this `0.0.0` release is intentionally
empty.

## What this crate will contain

When released, this crate will provide pure-Rust
implementations of the SPICE image-stream codecs: QUIC (the
SPICE wavelet/arithmetic codec, not the QUIC transport
protocol), GLZ (dictionary-based cross-frame LZ), LZ
(single-frame LZ), and LZ4. Each algorithm will be gated
behind a Cargo feature for dependency-minimisation.

The initial `0.1.0` release will contain decompression only,
matching what the ryll client needs today. The crate name
deliberately covers both directions so that compression
implementations of the same codecs can be added in future
minor releases without a crate rename. SPICE proxies (such as
the planned Rust rewrite of the kerbside proxy) and
server-side tooling are likely to want compression as well as
decompression, and a single crate that covers both is more
convenient than splitting them.

## Why a placeholder?

crates.io has a flat, immutable, first-come namespace. We are
publishing this empty crate now to prevent the name from being
claimed by an unrelated party (typosquatter, AI-generated junk
crate, well-meaning third party) before the real
implementation is ready. This is consistent with the
[Rust Forge crate ownership policy](https://forge.rust-lang.org/policies/crate-ownership.html)'s
guidance on reservation crates: the README clearly states the
intent, and substantive publishing will follow.

## Project links

- Source repository:
  <https://github.com/shakenfist/ryll>
- Extraction plan:
  <https://github.com/shakenfist/ryll/blob/develop/docs/plans/PLAN-crate-extraction.md>
- Issues / contact: file via the ryll repository
