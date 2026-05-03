//! HTTP server, signalling endpoint, and embedded browser
//! shell for the `--web` mode.
//!
//! The server binds an ephemeral TCP port, generates a
//! per-launch random 32-byte token, and serves a token-gated
//! axum app. Subsequent steps fill in the static-asset
//! serving (4b), the POST /offer SDP exchange handler (4c),
//! and the browser-shell wiring (4d).

mod server;

// These re-exports become used in step 4e when run_web
// is wired into main. Suppressed until then.
#[allow(unused_imports)]
pub use server::{run, WebState};
