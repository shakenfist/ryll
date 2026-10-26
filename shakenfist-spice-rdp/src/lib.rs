//! RDP frontend for the ryll SPICE client.
//!
//! Serves a SPICE session to stock RDP clients (mstsc, FreeRDP and
//! the platform RDP apps), so a user with no SPICE client can still
//! reach a desktop. It is the library behind `ryll --rdp`, and is
//! kept free of the ryll binary so that a gateway could serve RDP
//! from SPICE sessions without it.
//!
//! The RDP side is [IronRDP](https://github.com/Devolutions/IronRDP)'s
//! `ironrdp-server`, pinned to the 0.13 release. This crate supplies
//! the handlers that server is built with: a display handler that
//! reads damaged rectangles from the session's
//! [`SurfaceMirror`], and an input handler that turns RDP keyboard
//! and mouse events into the renderer's [`InputEvent`]s. The display
//! handler also sends the guest's cursor shape, which
//! [`spawn_cursor_relay`] keeps from the session's event bus. A
//! clipboard backend carries text both ways between the client's
//! clipboard channel and the session's [`MemoryClipboard`], and a
//! sound handler plays the guest's audio on the client from the
//! session's [`AudioRelay`]: Opus passed through unchanged when the
//! client accepts it, decoded PCM when it does not.
//!
//! IronRDP's server type is not `Send`, so it cannot be spawned on a
//! multi-thread tokio runtime. The server therefore runs on an OS
//! thread of its own with a current-thread runtime, and talks to the
//! SPICE session through the `Send` handles in [`SessionHandles`].
//! [`serve`] starts it and returns an [`RdpServerHandle`]. A client
//! that authenticates takes its session over from the client already
//! served it.
//!
//! # Crypto provider
//!
//! Both RDP security modes run inside TLS, and
//! [`TlsIdentity::acceptor`] uses the process-wide rustls
//! `CryptoProvider`. An embedding process must install a default
//! provider before calling [`serve`], e.g.
//! `rustls::crypto::ring::default_provider().install_default()`.
//! This crate does not pick a provider on the embedder's behalf.
//!
//! This is a reachability fallback, not a better path: a transcoded
//! RDP session loses SPICE features such as USB redirection and
//! folder sharing, and costs CPU on the serving side. Users who can
//! run a SPICE client should.

use std::sync::{Arc, Mutex};

use shakenfist_spice_renderer::{DisplaySize, InputEvent, MemoryClipboard, SurfaceMirror};
use tokio::sync::{mpsc, watch};

mod audio;
mod auth;
mod clipboard;
mod cursor;
mod display;
mod input;
mod server;
mod tls;

pub use audio::AudioRelay;
pub use auth::{generate_password, SingleSession};
pub use cursor::{spawn_cursor_relay, CursorState};
pub use display::MirrorDisplay;
pub use input::SpiceInputHandler;
pub use server::{
    serve, RdpConfig, RdpSecurity, RdpServerHandle, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_RDP_PORT,
};
pub use tls::TlsIdentity;

/// The username, password and domain an RDP client presents, as
/// IronRDP carries them. Re-exported so a consumer can fill an
/// [`RdpConfig`] and implement [`SessionProvider`] without depending
/// on IronRDP. Its `Debug` output leaves out the password.
pub use ironrdp_server::Credentials;

/// What the RDP adapters need from one SPICE session.
///
/// Every field is `Send`, and the channels work across tokio
/// runtimes, so the SPICE session can run on the caller's runtime
/// while the RDP server runs on its own thread.
pub struct SessionHandles {
    /// The session's mirror of its display surfaces, from which the
    /// display handler reads pixels and takes damage.
    pub surface_mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
    /// Sender for keyboard and mouse input to the SPICE inputs
    /// channel.
    pub input_tx: mpsc::Sender<InputEvent>,
    /// The session's current SPICE mouse mode (server or client), as
    /// announced by the main channel.
    pub mouse_mode: watch::Receiver<u32>,
    /// Sender for a requested guest display size.
    pub resize_tx: mpsc::Sender<DisplaySize>,
    /// The session's current cursor shape and visibility, as kept by
    /// [`spawn_cursor_relay`]. Subscribe the relay to the session's
    /// event bus before the session is spawned, or the first cursor
    /// shape is lost.
    pub cursor: watch::Receiver<CursorState>,
    /// Whether the SPICE session is up. While it is `false` the client
    /// is shown the last frame at half brightness, and once it is
    /// `true` again it is sent the whole surface, so a caller that
    /// reconnects SPICE behind the same handles should publish `false`
    /// when the session is lost and `true` once the new session's
    /// primary surface is in the mirror. A caller that never
    /// reconnects passes a receiver that is always `true`, such as one
    /// whose sender has been dropped: a closed watch keeps its last
    /// value.
    pub spice_up: watch::Receiver<bool>,
    /// The session's clipboard, which the main channel ferries to and
    /// from the guest's agent, or `None` to carry no clipboard. Text
    /// the RDP client copies is stored in it as host text, and text the
    /// guest copies is offered to the client from it. Every client is
    /// offered the clipboard channel either way; with `None` the
    /// client's copies are ignored and it is offered nothing.
    pub clipboard: Option<Arc<MemoryClipboard>>,
    /// The session's audio, from the relay the session was given as
    /// its playback sink, or `None` to play no audio. Every client is
    /// offered the audio channel either way; with `None` every format
    /// it accepts is declined, so it is sent nothing.
    pub audio: Option<Arc<AudioRelay>>,
    /// A human-readable name for the session, for logs.
    pub label: String,
}

/// The session the RDP adapters currently serve, if any.
///
/// The display and input handlers are fixed when a connection's
/// server is built, before its client has presented any credentials,
/// so they hold a slot rather than a session. Each connection has a
/// slot of its own. Its credential check fills the slot with the
/// session a [`SessionProvider`] chose, and it is emptied when
/// another client takes that session over and when the connection
/// ends; the handlers read it on each use, so an emptied slot stops
/// them at once. Clones share the same slot.
#[derive(Clone, Default)]
pub struct SessionSlot(Arc<Mutex<Option<Arc<SessionHandles>>>>);

impl SessionSlot {
    /// Create an empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make `handles` the session the adapters serve, replacing any
    /// previous one.
    pub fn set(&self, handles: Arc<SessionHandles>) {
        *self.lock() = Some(handles);
    }

    /// Empty the slot, so the adapters serve no session.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// The session the adapters currently serve, if any.
    pub fn current(&self) -> Option<Arc<SessionHandles>> {
        self.lock().clone()
    }

    /// Lock the slot. A panic while the lock was held cannot leave
    /// the `Option` half-written, so a poisoned lock is recovered
    /// rather than propagated.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<SessionHandles>>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Chooses the SPICE session an authenticated RDP client is served.
///
/// Called with the credentials the client presented, by the
/// credential check of a TLS-mode server; returning [`Rejected`]
/// refuses the connection. An NLA server calls it once instead, with
/// the configured pair, when it starts: CredSSP checks the password
/// itself and IronRDP passes nothing on from that check, so every NLA
/// client is served the session chosen then, and a rejection stops
/// the server from starting.
///
/// The server does learn when a client has authenticated, in either
/// mode, because IronRDP first asks the display handler for its
/// update stream then. That is when the client takes its session
/// over from any client already served it; it carries no credentials
/// to choose a session by.
///
/// # Limits of this seam
///
/// IronRDP 0.13 first asks the display handler for the desktop size
/// before the security handshake, so before any credentials are seen. A
/// provider can therefore choose only between sessions whose primary
/// surfaces are the same size as the one the server announced. A
/// provider holding sessions of different sizes must either have the
/// display handler resize the connection once a session is chosen,
/// or run one server process per session.
pub trait SessionProvider: Send + Sync {
    /// Return the session to serve for `credentials`, or [`Rejected`]
    /// to refuse the connection.
    fn session(
        &self,
        credentials: &ironrdp_server::Credentials,
    ) -> Result<Arc<SessionHandles>, Rejected>;
}

/// A [`SessionProvider`]'s refusal of a client's credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejected;
