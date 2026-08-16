//! WebRTC bridge for the ryll SPICE client.
//!
//! Exposes [`WebrtcBridge`], which owns a
//! [`webrtc::peer_connection::PeerConnection`] together with a
//! video track, an audio track, and a control datachannel. Phase 3
//! step 3b ships the constructor and SDP-answer plumbing; 3c adds
//! the video pump; 3d adds the synthetic Opus audio pump; the
//! datachannel send/recv (3e) attaches later.
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
