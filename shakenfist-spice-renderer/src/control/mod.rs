//! Unix-socket control interface for headless mode.
//!
//! This module provides the `--control-socket` feature: an external
//! driver can connect to the socket and drive the headless SPICE
//! session via NDJSON requests.  Protocol version 1.0 is documented
//! in `ryll/docs/control-socket-protocol.md`.
//!
//! ## Module layout
//!
//! - `protocol` — wire types (request/response/event envelopes,
//!   error codes, verb-specific params/results, version helpers).
//! - `server` — the `Server` task and the `StatusProvider` trait.

pub mod protocol;
pub mod server;

pub use server::{Server, StatusProvider};
