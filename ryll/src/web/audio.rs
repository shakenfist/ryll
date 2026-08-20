//! [`OpusPacketSink`] implementation for `--web` mode.
//!
//! Plumbing summary:
//!
//! - The renderer's `PlaybackChannel` calls into this sink from
//!   its async task on every Opus DATA packet, before its
//!   existing decode-to-cpal path. The cpal path is left alone
//!   — its output goes to a (probably nonexistent) audio device
//!   in `--web` mode but does not break anything.
//! - The sink holds an `Arc<Mutex<Option<mpsc::Sender<...>>>>`.
//!   The active `WebrtcBridge`'s audio pump owns the matching
//!   `Receiver`, which is plugged in at `/offer` time by the
//!   signalling handler.
//! - Each fresh `/offer` replaces the bridge and drops the old
//!   `Sender`; the old audio pump's `Receiver` then closes and
//!   the pump exits cleanly.
//!
//! For SPICE servers that negotiated raw PCM rather than Opus
//! (rare; xspice and QEMU default to Opus), the sink logs a
//! warn-once message and discards the samples. PCM-to-Opus
//! encoding is not implemented; the fallback it would
//! implement is specified in
//! `docs/plans/PLAN-web-frontend.md`.

use std::sync::{Arc, Mutex, Once};

use shakenfist_spice_renderer::OpusPacketSink;
use tokio::sync::mpsc;
use tracing::warn;

/// Type alias for the channel the active bridge plugs in.
/// The tuple is `(opus_packet_bytes, samples_in_packet_at_48k)`
/// — matches the signature of
/// `WebrtcBridge::spawn_audio_pump`.
pub type OpusSender = mpsc::Sender<(Vec<u8>, u32)>;

/// Shared slot holding the active bridge's audio sender. The
/// `signalling::post_offer` handler writes into the slot when a
/// new bridge replaces an old one; the playback channel reads
/// from the slot on every Opus packet.
///
/// We use `std::sync::Mutex` rather than `tokio::sync::Mutex`
/// because the playback channel calls into the sink from a
/// regular async tokio task — `tokio::sync::Mutex::blocking_lock`
/// would panic in that context. `std::sync::Mutex` is fine here:
/// the lock is held only for the duration of an `as_ref().clone()`
/// or `*= Some(...)` (microseconds), and contention is rare
/// (one writer per `/offer`, one reader per Opus packet).
pub type ActiveSenderSlot = Arc<Mutex<Option<OpusSender>>>;

/// `OpusPacketSink` implementation that forwards Opus packets to
/// whichever bridge is currently active. PCM is logged once and
/// discarded.
pub struct WebOpusSink {
    inner: ActiveSenderSlot,
}

impl WebOpusSink {
    /// Build a new sink and return both the sink itself (for
    /// passing to `run_connection`) and the matching slot
    /// handle (for the signalling handler to plug each
    /// `/offer`'s sender into).
    pub fn new() -> (Arc<Self>, ActiveSenderSlot) {
        let inner: ActiveSenderSlot = Arc::new(Mutex::new(None));
        let sink = Arc::new(Self {
            inner: inner.clone(),
        });
        (sink, inner)
    }
}

impl OpusPacketSink for WebOpusSink {
    fn on_opus_packet(&self, packet: &[u8], samples_in_packet: u32) {
        // Snapshot the active sender out of the slot so we don't
        // hold the lock across the try_send (which is non-blocking
        // anyway, but principle of least surprise).
        let tx = match self.inner.lock() {
            Ok(guard) => guard.as_ref().cloned(),
            Err(poisoned) => {
                // A panic in another holder poisoned the mutex.
                // We can still recover the inner Option safely;
                // dropping the bad lock would lose state, so
                // recover and continue.
                poisoned.into_inner().as_ref().cloned()
            }
        };
        if let Some(tx) = tx {
            // try_send is non-blocking. On TrySendError::Full we
            // drop the packet (the receiver is slow); on Closed
            // we drop the packet (the bridge is being torn down,
            // the post_offer flow will install a fresh sender).
            match tx.try_send((packet.to_vec(), samples_in_packet)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::debug!("web audio: bridge audio pump full; dropping packet");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::debug!("web audio: bridge audio pump closed; dropping packet");
                }
            }
        }
        // No active bridge: silently drop. Audio is meaningful
        // only when a viewer is attached.
    }

    fn on_pcm_samples(&self, _samples: &[i16], _sample_rate_hz: u32, _channels: u8) {
        static WARNED: Once = Once::new();
        WARNED.call_once(|| {
            warn!(
                "Web mode audio: SPICE server negotiated raw PCM; \
                 audio will be silent (PCM-to-Opus encoding is future work)."
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_opus_packet_when_sender_registered() {
        let (sink, slot) = WebOpusSink::new();
        let (tx, mut rx) = mpsc::channel::<(Vec<u8>, u32)>(8);
        *slot.lock().unwrap() = Some(tx);

        sink.on_opus_packet(b"hello-opus", 960);

        let (bytes, samples) = rx.recv().await.expect("packet should arrive");
        assert_eq!(bytes, b"hello-opus");
        assert_eq!(samples, 960);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drops_silently_when_no_sender_registered() {
        let (sink, _slot) = WebOpusSink::new();
        // No panic, no error, just a no-op.
        sink.on_opus_packet(b"orphan", 960);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drops_silently_when_sender_full() {
        let (sink, slot) = WebOpusSink::new();
        // Capacity 1 + an unconsumed message means the next
        // try_send fails Full; the sink must not panic.
        let (tx, _rx) = mpsc::channel::<(Vec<u8>, u32)>(1);
        tx.try_send((vec![0xaa], 960)).expect("first send");
        *slot.lock().unwrap() = Some(tx);

        // Second send hits Full and is dropped silently.
        sink.on_opus_packet(b"second", 960);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drops_silently_when_sender_closed() {
        let (sink, slot) = WebOpusSink::new();
        let (tx, rx) = mpsc::channel::<(Vec<u8>, u32)>(8);
        *slot.lock().unwrap() = Some(tx);
        drop(rx); // receiver gone -> any send fails Closed.

        sink.on_opus_packet(b"posthumous", 960);
    }

    #[test]
    fn pcm_path_does_not_panic() {
        // The warn-once is impractical to capture without a
        // tracing subscriber dependency in tests; the assertion
        // here is "no panic, no UB". The warn-once invariant
        // holds by construction (std::sync::Once).
        let (sink, _slot) = WebOpusSink::new();
        sink.on_pcm_samples(&[0i16, 0i16], 48_000, 2);
        sink.on_pcm_samples(&[0i16, 0i16], 48_000, 2);
    }
}
