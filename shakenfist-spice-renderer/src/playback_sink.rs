//! Where the playback channel's audio goes.
//!
//! A session chooses with [`PlaybackOutput`]. `Local` plays the guest's
//! audio through this host's default output device, on a cpal thread
//! that exists only in a build with the `audio` feature. `Sink` hands
//! the audio to a [`PlaybackSink`] instead and opens no device, in every
//! build: the web frontend uses it to forward Opus packets straight to a
//! WebRTC audio track without re-encoding, and a frontend that wants
//! PCM takes the decoded samples.
//!
//! The playback channel decodes Opus in both modes, so its decode
//! counters mean the same whichever is chosen.
//!
//! When the SPICE server negotiated Opus (the common case), the sink
//! sees each raw packet through [`PlaybackSink::on_opus_packet`] before
//! decode, then the decoded samples through
//! [`PlaybackSink::on_decoded_pcm`]. For servers that negotiated raw PCM
//! (rare; xspice and QEMU default to Opus), it sees the samples through
//! [`PlaybackSink::on_pcm_samples`] instead.

use std::sync::Arc;

/// The shape of an audio stream, as the server announced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackFormat {
    /// The data mode of the stream's packets, an
    /// `audio_data_mode::*` value from `PLAYBACK_MODE`. It is
    /// `audio_data_mode::INVALID` until the server has sent a mode,
    /// and may name a codec ryll cannot play.
    pub mode: u16,
    /// Samples a second per channel, from `PLAYBACK_START`. Decoded
    /// Opus is always 48 kHz, whatever this says; see
    /// [`PlaybackSink::on_decoded_pcm`].
    pub sample_rate_hz: u32,
    /// The channel count from `PLAYBACK_START`, saturated at
    /// `u8::MAX`.
    pub channels: u8,
}

/// Sink for the audio the playback channel receives.
///
/// Implementations are called from inside the playback channel's
/// async task on the renderer's tokio runtime. They must be
/// non-blocking and cheap; in practice they should `try_send`
/// onto a downstream channel and drop on overflow rather than
/// awaiting.
pub trait PlaybackSink: Send + Sync {
    /// An audio stream started (`PLAYBACK_START`), or the server
    /// changed the data mode of a running one (`PLAYBACK_MODE`), so
    /// this can arrive more than once per stream. Called before any
    /// of the stream's data. The default implementation is a no-op.
    fn on_start(&self, _format: PlaybackFormat) {}

    /// Forward one Opus packet, before it is decoded.
    /// `samples_in_packet` is the number of 48 kHz samples
    /// represented by this packet (typically 960 for 20 ms frames;
    /// smaller values like 256 are also seen from spice-server).
    /// Consumers use it to advance their RTP timestamps.
    fn on_opus_packet(&self, packet: &[u8], samples_in_packet: u32);

    /// Forward raw PCM samples, interleaved. Called when the SPICE
    /// server negotiated PCM rather than Opus. The default
    /// implementation is a no-op; the web sink overrides this to
    /// log a warn-once message and discard.
    fn on_pcm_samples(&self, _samples: &[i16], _sample_rate_hz: u32, _channels: u8) {}

    /// Forward the PCM decoded from one Opus packet, interleaved,
    /// straight after that packet's
    /// [`on_opus_packet`](PlaybackSink::on_opus_packet). Opus
    /// decodes at 48 kHz, so `sample_rate_hz` is always 48000. A
    /// packet that fails to decode produces no call. The default
    /// implementation is a no-op.
    fn on_decoded_pcm(&self, _samples: &[i16], _sample_rate_hz: u32, _channels: u8) {}

    /// The audio stream stopped (`PLAYBACK_STOP`). Only a STOP that
    /// ends a started stream is passed on, so this always follows an
    /// [`on_start`](PlaybackSink::on_start). The default
    /// implementation is a no-op.
    fn on_stop(&self) {}
}

/// Where a session's playback channel sends the guest's audio.
#[derive(Clone)]
pub enum PlaybackOutput {
    /// Play it through this host's default output device. A build
    /// without the `audio` feature has no local output, so it does
    /// not link the playback channel at all.
    Local,
    /// Hand it to the sink and open no output device. The playback
    /// channel is linked in every build.
    Sink(Arc<dyn PlaybackSink>),
}

impl std::fmt::Debug for PlaybackOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaybackOutput::Local => f.write_str("Local"),
            PlaybackOutput::Sink(_) => f.write_str("Sink(..)"),
        }
    }
}
