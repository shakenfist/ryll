//! Trait surface for recording protocol traffic.
//!
//! Channels record every SPICE message they send and receive
//! through this trait. The host (ryll) implements it on
//! `TrafficBuffers`, which feeds the bug-report ring buffers and
//! the live traffic viewer. A different consumer of the renderer
//! could implement a no-op sink, an eBPF hook, or anything else
//! without affecting the channel hot path.
//!
//! The signatures match the shape of the calls already in the
//! channel files (see `channels/main_channel.rs::send_message`,
//! `channels/display.rs::process_messages`, etc.).

use std::time::Duration;

/// Sink for raw protocol traffic. Channels call into this for
/// every SPICE message they send or receive.
///
/// Implementors must be `Send + Sync` because the trait is held
/// behind `Arc<dyn TrafficSink>` and shared across the per-channel
/// async tasks.
pub trait TrafficSink: Send + Sync {
    /// Record a message sent by the client on `channel`.
    ///
    /// `raw` is the full wire bytes including the 6-byte mini
    /// header. `msg_type` and `msg_name` are derived from the
    /// SPICE protocol's name table.
    fn record_sent(&self, channel: &'static str, msg_type: u16, msg_name: &'static str, raw: &[u8]);

    /// Record a message received from the server on `channel`.
    fn record_received(
        &self,
        channel: &'static str,
        msg_type: u16,
        msg_name: &'static str,
        raw: &[u8],
    );

    /// Time elapsed since the traffic sink was created. Channels
    /// stamp recent-decode entries with this for snapshot
    /// timestamps.
    fn elapsed(&self) -> Duration;
}
