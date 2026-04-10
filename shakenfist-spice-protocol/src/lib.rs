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
//! A high-level `SpiceClient` for actually connecting to a
//! SPICE server is intentionally not part of this crate; it
//! lives in ryll for now and will move into a separate crate
//! once it has been refactored to take a narrow
//! `ConnectionConfig` struct instead of ryll's broader
//! application config.

pub mod constants;
pub mod link;
pub mod logging;
pub mod messages;

// Re-export the most commonly used items at the crate root
// for convenience.
pub use constants::{
    capabilities, cursor_client, cursor_server, display_client, display_server, inputs_client,
    inputs_server, keyboard_modifiers, main_client, main_server, mouse_buttons, spicevmc_client,
    spicevmc_server, ChannelType, ImageType, NotifySeverity, SpiceError, IMAGE_FLAGS_CACHE_ME,
    SPICE_MAGIC, SPICE_VERSION_MAJOR, SPICE_VERSION_MINOR,
};
