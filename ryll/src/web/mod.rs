//! HTTP server, signalling endpoint, and embedded browser
//! shell for the `--web` mode.
//!
//! The server binds an ephemeral TCP port, generates a
//! per-launch random 32-byte token, and serves a token-gated
//! axum app: the embedded browser shell's static assets and
//! the POST /offer SDP exchange handler.

pub(crate) mod assets;
pub(crate) mod audio;
pub(crate) mod control;
pub(crate) mod cursor;
pub(crate) mod inputs;
pub(crate) mod lifecycle;
pub(crate) mod notifications;
pub(crate) mod server;
pub(crate) mod signalling;

pub use server::{run, WebState};
