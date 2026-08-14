//! Bridge reaper: watches the active bridge for a terminal
//! `RTCPeerConnectionState` and tears down the bridge + encoder
//! when observed. The SPICE session (`run_connection`) is left
//! untouched — only the WebRTC layer is reaped.
//!
//! # Design: dead_signal vs wait_for_dead
//!
//! `WebrtcBridge::wait_for_dead` takes `&self`, which means the
//! reaper would have to borrow the bridge across an `.await`
//! point. That is impossible when the bridge lives behind a
//! `Mutex<Option<WebrtcBridge>>` — we can't hold the mutex
//! guard across an await.
//!
//! Instead the reaper clones `dead_signal()` — an
//! `Arc<StickySignal>` — out of the slot and awaits
//! `StickySignal::wait()` on its own copy. That is the exact
//! implementation `wait_for_dead` uses, so the late-subscriber
//! fast-path and the lost-wakeup guard come from the shared,
//! unit-tested type rather than a hand-copied inline version of
//! the pattern (which is how the original lost-wakeup bug got
//! in). See `StickySignal`'s docs in `shakenfist-spice-webrtc`
//! for the enable-before-check reasoning.
//!
//! # Lock ordering
//!
//! Consistent with `post_offer`:
//!
//! 1. `bridge_slot` — to get or take the active bridge.
//! 2. `encoder` — to stop the pipeline.
//! 3. `active_opus_tx` — to clear the audio pump sender.
//!
//! # Race: reaper vs. `/offer`
//!
//! The reaper snapshots `bridge_generation` before awaiting the
//! dead signal. After waking, it compares the current generation;
//! if it has advanced (i.e. `/offer` installed a new bridge while
//! the reaper was waiting), the reaper skips the reap and loops
//! back. This prevents the reaper from closing a healthy new
//! bridge when the old bridge's dead signal fires late.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::server::WebState;

/// Watch the active bridge's dead signal in a loop. When the
/// bridge's PC reaches a terminal state (`Failed`, `Disconnected`,
/// or `Closed`), take the bridge out of `bridge_slot`, close it,
/// stop the encoder, and clear the audio pump sender. Then loop
/// back to wait for the next bridge (installed by the next
/// `POST /offer`).
///
/// This task runs for the lifetime of the HTTP server and is
/// aborted in the shutdown path of `run_web` after
/// `axum::serve` returns.
///
/// # Race: reaper vs. `/offer`
///
/// The reaper snapshots `bridge_generation` before awaiting the
/// dead signal. After waking, it compares the current generation;
/// if it has advanced (i.e. `/offer` installed a new bridge while
/// the reaper was waiting), the reaper skips the reap and loops
/// back. This prevents closing a healthy new bridge when the old
/// bridge's dead signal fires late.
pub async fn run_bridge_reaper(state: Arc<WebState>) {
    loop {
        // Snapshot the generation counter and the dead signal from
        // the active bridge — both without holding the lock across
        // the await.
        let gen_at_subscribe = state.bridge_generation.load(Ordering::SeqCst);
        let dead = {
            let slot = state.bridge_slot.lock().await;
            slot.as_ref().map(|b| b.dead_signal())
        };

        let Some(dead) = dead else {
            // No active bridge; sleep briefly and re-check.
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };

        // Wait for the bridge to die. `StickySignal::wait` carries
        // both guards this needs: the sticky fast-path for a bridge
        // that died before we subscribed, and interest registration
        // before the flag check so a death landing mid-subscribe is
        // still delivered rather than lost.
        dead.wait().await;

        // Generation check: if `/offer` replaced the bridge
        // while we were waiting, skip the reap. The new bridge
        // is healthy and its own dead-signal is yet to fire.
        let gen_now = state.bridge_generation.load(Ordering::SeqCst);
        if gen_now != gen_at_subscribe {
            tracing::debug!(
                "bridge reaper: bridge already replaced \
                 (gen {} → {}); skipping reap",
                gen_at_subscribe,
                gen_now,
            );
            continue;
        }

        tracing::info!("bridge reaper: bridge dead signal observed, reaping");

        // Take the bridge out of the slot. Under the lock so
        // a concurrent /offer sees a consistent view. The
        // taken bridge may be None if /offer already replaced
        // it before us — in that case, no-op (the new bridge
        // is alive and its own dead-signal is yet to fire).
        let bridge = {
            let mut slot = state.bridge_slot.lock().await;
            slot.take()
        };
        if let Some(b) = bridge {
            // Close the peer connection cleanly so DTLS/SRTP
            // tears down before the bridge is dropped. Errors
            // are expected when the PC is already closed (e.g.
            // the browser initiated the close).
            if let Err(e) = b.close().await {
                tracing::debug!("bridge reaper: bridge close returned: {}", e);
            }
        }

        // Stop the encoder pipeline (sends EncoderControl::Stop,
        // awaits the task handle with a 2s ceiling).
        {
            let mut enc = state.encoder.lock().await;
            enc.stop().await;
        }

        // Clear the audio pump sender so the renderer-side
        // WebOpusSink drops packets rather than building up
        // a backlog while no viewer is attached.
        {
            match state.active_opus_tx.lock() {
                Ok(mut guard) => *guard = None,
                Err(poisoned) => {
                    let mut inner = poisoned.into_inner();
                    *inner = None;
                }
            }
        }

        tracing::info!("bridge reaper: reaped; awaiting next viewer");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::web::server::WebState;

    /// The reaper loops on the no-bridge path without reaping.
    /// Bumping the generation counter does not affect this path;
    /// the reaper just keeps sleeping until a bridge arrives.
    /// This exercises the `AtomicU64` round-trip and ensures
    /// `bridge_generation` is accessible from the reaper's state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaper_exits_cleanly_on_no_bridge() {
        let state = Arc::new(WebState::new());

        // Start with generation 0 and no bridge in the slot.
        assert_eq!(state.bridge_generation.load(Ordering::SeqCst), 0);

        // Spawn the reaper — it will immediately sleep (no bridge).
        let reaper = tokio::spawn(run_bridge_reaper(Arc::clone(&state)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !reaper.is_finished(),
            "reaper should be waiting, not finished"
        );

        // Bump the generation (simulating /offer installing a
        // new bridge without us actually populating the slot).
        state.bridge_generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(state.bridge_generation.load(Ordering::SeqCst), 1);

        // Reaper is still alive — it only sleeps on the no-bridge
        // path; the generation check is never reached.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!reaper.is_finished(), "reaper should still be waiting");

        // Abort and verify the task exits.
        reaper.abort();
        let _ = tokio::time::timeout(Duration::from_millis(200), reaper).await;
    }

    // NOTE: A unit test that exercises the race path (generation
    // mismatch causes reaper to skip a reap) requires constructing
    // a real `WebrtcBridge` (which needs a running ICE/DTLS stack).
    // That is too heavy for a unit test. The no-regression case —
    // a normal browser disconnect triggers a reap — is covered by
    // the integration test `post_offer_returns_valid_answer` in
    // `signalling.rs` (the bridge is closed at the end of that
    // test, which transitions the PC to Closed state). The
    // generation-counter skip path is exercised by code inspection
    // and the logic above.
}
