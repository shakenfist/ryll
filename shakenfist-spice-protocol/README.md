# shakenfist-spice-protocol

Pure-Rust SPICE protocol primitives:

- **`constants`** — SPICE magic, version, capability flags,
  `ChannelType`, `SpiceError`, `ImageType`,
  `NotifySeverity`, and the message-type opcode constants
  for every channel direction (main / display / inputs /
  cursor / spicevmc).
- **`messages`** — wire-format structs with `read`/`write`
  methods for every SPICE message type ryll knows about,
  including the input event types (`KeyEvent`,
  `MousePosition`, `MouseButton`, `InputsKeyModifiers`).
- **`link`** — SPICE link handshake (`SpiceLinkMess`,
  `SpiceLinkReply`, `perform_link`, `perform_auth`),
  `SpiceStream` (a Plain/TLS wrapper), and the
  `encrypt_password` helper for SPICE password auth
  (RSA-OAEP + SHA1).
- **`logging`** — protocol-traffic logging helpers and a
  `message_names` lookup module for every channel direction.

A high-level `SpiceClient` for actually connecting to a SPICE
server is intentionally not part of this crate; it lives in
[ryll](https://github.com/shakenfist/ryll) for now and will
move into a separate crate once it has been refactored to take
a narrow `ConnectionConfig` struct instead of ryll's broader
application config (see the
[extraction plan](https://github.com/shakenfist/ryll/blob/develop/docs/plans/PLAN-crate-extraction.md)
for context).

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
