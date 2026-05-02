//! WebRTC bridge for the ryll SPICE client.
//!
//! Exposes [`WebrtcBridge`], which owns an
//! [`webrtc::peer_connection::RTCPeerConnection`] together with a
//! video track, an audio track, and a control datachannel. Phase 3
//! step 3b ships the constructor and SDP-answer plumbing; the video
//! pump (3c), synthetic-audio pump (3d), and datachannel send/recv
//! (3e) attach later.

mod bridge;

pub use bridge::{WebrtcBridge, WebrtcBridgeConfig};
