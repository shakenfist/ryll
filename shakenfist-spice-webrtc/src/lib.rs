//! WebRTC bridge for the ryll SPICE client.
//!
//! Exposes [`WebrtcBridge`], which owns an
//! [`webrtc::peer_connection::RTCPeerConnection`] together with a
//! video track, an audio track, and a control datachannel. Phase 3
//! step 3b ships the constructor and SDP-answer plumbing; 3c adds
//! the video pump; 3d adds the synthetic Opus audio pump; the
//! datachannel send/recv (3e) attaches later.

mod bridge;
pub mod sticky;

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
