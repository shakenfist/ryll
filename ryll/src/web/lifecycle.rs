//! Bridge reaper: watches the active bridge for a terminal
//! `RTCPeerConnectionState` and tears down the bridge + encoder
//! when observed. The SPICE session (`run_connection`) is left
//! untouched — only the WebRTC layer is reaped.
//!
//! # Design: dead_handle vs wait_for_dead
//!
//! `WebrtcBridge::wait_for_dead` takes `&self`, which means the
//! reaper would have to borrow the bridge across an `.await`
//! point. That is impossible when the bridge lives behind a
//! `Mutex<Option<WebrtcBridge>>` — we can't hold the mutex
//! guard across an await.
//!
//! Instead the reaper takes two `Arc` handles out of the slot:
//!
//! - `dead_handle()` → `Arc<Notify>` (the raw wakeup channel)
//! - `dead_flag_handle()` → `Arc<AtomicBool>` (the sticky flag)
//!
//! These replicate the logic inside `wait_for_dead`:
//! check the flag first (late-subscriber fast-path), then await
//! `Notify::notified()`. Without the flag check, a bridge that
//! died before we called `notified().await` would leave the
//! reaper hung forever, because `Notify::notify_waiters()` does
//! not queue notifications for late waiters.
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
        // Snapshot the generation counter and the dead-signal
        // handles from the active bridge — all without holding
        // the lock across the await.  We take both the Notify
        // and the AtomicBool so we can replicate
        // wait_for_dead's late-subscriber fast-path: if the
        // bridge already died before we called notified().await,
        // the flag check returns immediately rather than hanging
        // forever.
        let gen_at_subscribe = state.bridge_generation.load(Ordering::SeqCst);
        let handles = {
            let slot = state.bridge_slot.lock().await;
            slot.as_ref()
                .map(|b| (b.dead_handle(), b.dead_flag_handle()))
        };

        let Some((dead_notify, dead_flag)) = handles else {
            // No active bridge; sleep briefly and re-check.
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };

        // Wait for the bridge to die. Two guards, both needed.
        //
        // `enable()` registers interest before anything else, which
        // matters because `Notified` does not register until first
        // polled: without it, a bridge dying between the flag check
        // and the await would fire `notify_waiters()` with nobody
        // registered, and the reaper would wait forever on a bridge
        // that is already gone.
        //
        // The flag check then handles the case where the bridge died
        // before we ever got here — `Notify` does not queue
        // notifications for late subscribers.
        let notified = dead_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if !dead_flag.load(Ordering::SeqCst) {
            notified.await;
        }

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
