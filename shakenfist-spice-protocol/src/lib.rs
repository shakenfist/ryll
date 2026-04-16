//! Pure-Rust SPICE protocol primitives extracted from the
//! [ryll](https://github.com/shakenfist/ryll) SPICE client.
//!
//! This crate provides the wire-format types and helpers
//! needed to implement a SPICE client, server, or proxy in
//! Rust:
//!
//! - [`constants`] — protocol magic, version, capability
//!   flags, and message-type opcode constants for every
//!   channel direction.
//! - [`messages`] — wire-format struct definitions with
//!   `read` and `write` methods, including the input message
//!   types (`KeyEvent`, `MousePosition`, etc.).
//! - [`link`] — SPICE link handshake (`SpiceLinkMess`,
//!   `SpiceLinkReply`, `perform_link`, `perform_auth`),
//!   `SpiceStream` (a Plain/TLS wrapper), and the
//!   `encrypt_password` helper for SPICE auth.
//! - [`logging`] — protocol-traffic logging helpers and
//!   message-name lookup tables for every channel direction.
//!
//! - [`ConnectionConfig`] — SPICE server connection
//!   parameters (host, port, TLS, credentials). This is the
//!   narrow configuration type that [`SpiceClient`] accepts.
//! - [`client`] — `SpiceClient` for managing SPICE channel
//!   connections (TLS/TCP, keepalive, link handshake, auth).

pub mod client;
pub mod constants;
pub mod link;
pub mod logging;
pub mod messages;

pub use client::SpiceClient;

// Re-export the most commonly used items at the crate root
// for convenience.
pub use constants::{
    capabilities, cursor_client, cursor_server, display_client, display_server, inputs_client,
    inputs_server, keyboard_modifiers, main_client, main_server, mouse_buttons, playback_server,
    spicevmc_client, spicevmc_server, ChannelType, ImageType, NotifySeverity, SpiceError,
    IMAGE_FLAGS_CACHE_ME, SPICE_MAGIC, SPICE_VERSION_MAJOR, SPICE_VERSION_MINOR,
};

/// SPICE server connection parameters.
///
/// This is the narrow type that [`SpiceClient`] needs to
/// establish a connection; it deliberately excludes
/// application-level concerns like CLI parsing, .vv file
/// handling, and session settings.
///
/// Construct via struct literal syntax:
/// ```
/// # use shakenfist_spice_protocol::ConnectionConfig;
/// let config = ConnectionConfig {
///     host: "spice.example.com".into(),
///     port: 5900,
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone, Default)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub tls_port: Option<u16>,
    pub password: Option<String>,
    /// PEM-encoded CA certificate for TLS. When present,
    /// hostname verification is relaxed (SPICE servers
    /// commonly use self-signed certs without SAN extensions).
    pub ca_cert: Option<String>,
    /// Expected certificate subject. Currently informational
    /// only — SPICE servers commonly omit SAN extensions, so
    /// subject matching is not enforced.
    pub host_subject: Option<String>,
}
