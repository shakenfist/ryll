# shakenfist-spice-usbredir

Pure-Rust parser and message types for the SPICE USB
redirection (usbredir) protocol, suitable for clients,
proxies, and protocol analysis tools:

- **`constants`** — message types, capabilities, status codes,
  USB speed and endpoint-type enums.
- **`messages`** — wire-format struct definitions with `read`
  and `write` methods for every usbredir message type, plus
  `UsbredirMessage` / `UsbredirPayload` for parsed message
  dispatch.
- **`parser`** — `UsbredirParser`, a byte-stream parser that
  accumulates data via `feed()` and yields complete
  `UsbredirMessage` values via `next_message()`.

The crate is transport-agnostic: it parses the byte stream
that arrives over a SPICE `spicevmc` channel (or any other
transport that delivers the same framing), but does not open
sockets, speak to `usbdevfs`, or interact with physical
hardware. Ryll pairs this crate with its own USB backends
(`nusb` for physical devices on Linux, and a virtual
mass-storage backend for RAW disk images) to provide end-to-end
USB redirection.

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
Internal consumers within the shakenfist project (ryll and
the planned Rust rewrite of the kerbside SPICE proxy) depend
on this crate via workspace paths; external consumers should
use `cargo add shakenfist-spice-usbredir`.

## License

Apache-2.0
