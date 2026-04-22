# shakenfist-spice-protocol

Pure-Rust SPICE protocol primitives. The types and helpers
needed to implement a SPICE client, server, or proxy in Rust:

- **`constants`** — SPICE magic, version, capability flags,
  `ChannelType`, `SpiceError`, `ImageType`, `NotifySeverity`,
  and the message-type opcode constants for every channel
  direction (main / display / inputs / cursor / spicevmc).
- **`messages`** — wire-format structs with `read`/`write`
  methods for every SPICE message type ryll knows about,
  including the input event types (`KeyEvent`,
  `MousePosition`, `MouseButton`, `InputsKeyModifiers`).
- **`link`** — SPICE link handshake (`SpiceLinkMess`,
  `SpiceLinkReply`, `perform_link`, `perform_auth`),
  `SpiceStream` (a Plain/TLS wrapper), and the
  `encrypt_password` helper for SPICE password auth
  (RSA-OAEP + SHA1).
- **`client`** — `SpiceClient` for managing SPICE channel
  connections (TLS/TCP, keepalive, link handshake, auth),
  configured via a narrow [`ConnectionConfig`] struct so it
  can be driven from contexts other than ryll's CLI.
- **`logging`** — protocol-traffic logging helpers and a
  `message_names` lookup module for every channel direction.

The crate does not implement per-channel message handling;
decoding display updates, playing audio, or rendering cursors
is the caller's job. This crate gives you the bytes on and
off the wire, plus the connection plumbing, and stops there.

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
Internal consumers within the shakenfist project (ryll and
the planned Rust rewrite of the kerbside SPICE proxy) depend
on this crate via workspace paths; external consumers should
use `cargo add shakenfist-spice-protocol`.

## License

Apache-2.0
