//! WebRTC bridge for the ryll SPICE client.
//!
//! Exposes [`WebrtcBridge`], which owns a
//! [`webrtc::peer_connection::PeerConnection`] together with a
//! video track, an audio track, and a control datachannel. It
//! owns the SDP-answer plumbing, the video pump, the Opus audio
//! pump, and the datachannel send/recv path.
//!
//! [`host_udp_bind_addrs`] chooses which local addresses to bind the
//! WebRTC UDP sockets to (see its module docs for why this needs its
//! own reasoning). [`WebrtcBridge::new`] consumes it and fails
//! construction if it comes back empty.

mod bind_addrs;
mod bridge;
mod sticky;

pub use bind_addrs::host_udp_bind_addrs;
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
