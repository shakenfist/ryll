//! Trait surface for the `--capture` pipeline.
//!
//! Channels call into this trait to record packets and frames.
//! The host (ryll) implements it on `CaptureSession` (pcap +
//! H.264/MP4). Other consumers can drop the sink (no-op) or
//! implement a different sink (e.g. a streaming encoder for the
//! `--web` mode).
//!
//! Signatures intentionally mirror the shape of the calls in
//! `ryll/src/capture.rs::CaptureSession`.

/// Sink for capture-mode packet and frame recording.
///
/// Behind `Arc<dyn CaptureSink>` so it can be cheaply cloned
/// into the per-channel async tasks. Channels typically wrap
/// this in `Option<Arc<dyn CaptureSink>>` and skip the call
/// when capture is disabled.
pub trait CaptureSink: Send + Sync {
    /// Record a packet sent by the client on the given channel.
    fn packet_sent(&self, channel: &str, data: &[u8]);

    /// Record a packet received from the server on the given
    /// channel.
    fn packet_received(&self, channel: &str, data: &[u8]);

    /// Record a display frame after a MARK boundary. `surface_id`
    /// is the SPICE surface id; only surface 0 is currently
    /// recorded by ryll's `CaptureSession`, but the sink decides
    /// the policy.
    fn frame(&self, surface_id: u32, pixels: &[u8], width: u32, height: u32);
}
