//! Pre-decode tap on the SPICE playback channel.
//!
//! When the SPICE server negotiated Opus (the common case),
//! the renderer-side playback channel forwards each raw Opus
//! packet to the registered sink BEFORE its existing
//! decode-to-PCM-into-cpal path. The web frontend uses this
//! to forward Opus packets directly to a WebRTC audio track
//! without re-encoding.
//!
//! For SPICE servers that negotiated raw PCM (rare; xspice and
//! QEMU default to Opus), the [`OpusPacketSink::on_pcm_samples`]
//! method is called instead. Phase 5 ships only a warn-and-
//! silence implementation for this fallback in `--web` mode;
//! full PCM-to-Opus encoding is tracked as a future-work item.
//!
//! The GUI and headless modes pass `None` for the optional
//! sink: their existing decode-to-cpal path is unchanged, and
//! the tap point in
//! [`crate::channels::PlaybackChannel`] is a no-op when no
//! sink is registered.

/// Sink for raw audio data observed by the playback channel.
///
/// Implementations are called from inside the playback channel's
/// async task on the renderer's tokio runtime. They must be
/// non-blocking and cheap; in practice they should `try_send`
/// onto a downstream channel and drop on overflow rather than
/// awaiting.
pub trait OpusPacketSink: Send + Sync {
    /// Forward one Opus packet. `samples_in_packet` is the
    /// number of 48 kHz samples represented by this packet
    /// (typically 960 for 20 ms frames; smaller values like
    /// 256 are also seen from spice-server). Consumers use it
    /// to advance their RTP timestamps.
    fn on_opus_packet(&self, packet: &[u8], samples_in_packet: u32);

    /// Forward raw PCM samples. Called when the SPICE server
    /// negotiated PCM rather than Opus. The default
    /// implementation is a no-op; the web sink overrides this
    /// to log a warn-once message and discard.
    fn on_pcm_samples(&self, _samples: &[i16], _sample_rate_hz: u32, _channels: u8) {}
}
