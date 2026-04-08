# shakenfist-spice-protocol

**This crate is a functionally empty crate that exists to
reserve the crate name for an upcoming pure-Rust SPICE
protocol library extracted from the
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

When released, this crate will provide pure-Rust SPICE
protocol primitives: protocol constants and opcodes,
wire-format message structs with serialisation, the SPICE link
handshake (including TLS and RSA-OAEP authentication), and a
`SpiceClient` for connecting to a SPICE server.

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
