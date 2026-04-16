# shakenfist-spice-usbredir

Pure-Rust parser and message types for the SPICE USB
redirection (usbredir) protocol:

- **`constants`** — message types, capabilities, status codes,
  USB speed and endpoint-type enums.
- **`messages`** — wire-format struct definitions with `read`
  and `write` methods for every usbredir message type, plus
  `UsbredirMessage` / `UsbredirPayload` for parsed message
  dispatch.
- **`parser`** — `UsbredirParser`, a byte-stream parser that
  accumulates data via `feed()` and yields complete
  `UsbredirMessage` values via `next_message()`.

## Status

This crate is **not yet published to crates.io**. The `0.0.0`
entry there is a Phase 2 name reservation; the real `0.1.0`
release will follow once API polish is complete.

Internal consumers (ryll itself and the planned Rust rewrite
of the shakenfist kerbside SPICE proxy) should depend on this
crate via a workspace path or a git dependency until `0.1.0`
ships.

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
