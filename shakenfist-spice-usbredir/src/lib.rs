//! Pure-Rust parser and message types for the SPICE USB
//! redirection (usbredir) protocol, suitable for clients,
//! proxies, and protocol analysis tools.
//!
//! - [`constants`] — message types, capabilities, status
//!   codes, USB speed and endpoint-type enums.
//! - [`messages`] — wire-format struct definitions with
//!   `read` and `write` methods for every usbredir message
//!   type, plus `UsbredirMessage` / `UsbredirPayload` for
//!   parsed message dispatch.
//! - [`parser`] — `UsbredirParser`, a byte-stream parser
//!   that accumulates data and yields complete
//!   `UsbredirMessage` values.
//!
//! Extracted from the
//! [ryll](https://github.com/shakenfist/ryll) SPICE client.

pub mod constants;
pub mod messages;
pub mod parser;
