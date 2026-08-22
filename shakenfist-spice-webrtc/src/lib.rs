//! WebRTC bridge for the ryll SPICE client.
//!
//! Exposes [`WebrtcBridge`], which owns a
//! [`webrtc::peer_connection::PeerConnection`] together with a
//! video track, an audio track, and a control datachannel. It
//! owns the SDP-answer plumbing, the video pump, the Opus audio
//! pump, and the datachannel send/recv path.
//!
//! [`UdpBindPolicy`] chooses which local addresses to bind the
//! WebRTC UDP sockets to, and on which port (see its module docs for
//! why this needs its own reasoning, and for which of its filters an
//! operator may override). [`WebrtcBridge::new`] resolves it on every
//! call and fails construction if it comes back empty.
//! [`host_udp_bind_addrs`] is the default policy as a function, for
//! external consumers of this crate with no configuration surface of
//! their own; nothing in this workspace calls it.

mod bind_addrs;
mod bridge;
mod sticky;

#[cfg(any(test, feature = "test-support"))]
pub use bind_addrs::{bind_addrs_for_tests, bind_policy_for_tests};
pub use bind_addrs::{host_udp_bind_addrs, BindSelector, UdpBindPolicy};
pub use bridge::{WebrtcBridge, WebrtcBridgeConfig};
pub use sticky::StickySignal;

/// Client-side peer connection for tests that drive the browser half
/// of a bridge exchange.
///
/// Available to this crate's own tests unconditionally, and to
/// external consumers that enable the `test-support` feature. Not
/// part of the production surface.
#[cfg(any(test, feature = "test-support"))]
pub mod test_client;
