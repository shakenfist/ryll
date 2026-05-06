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
//! Both the reaper and a concurrent `/offer` handler serialise on
//! `bridge_slot`. Whichever acquires the lock first takes the
//! bridge via `slot.take()`; the other observes `None` from
//! `slot.take()` and no-ops. No data loss; a minor cost (encoder
//! stop + fresh restart) is acceptable in the race scenario.

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
pub async fn run_bridge_reaper(state: Arc<WebState>) {
    loop {
        // Snapshot the dead-signal handles from the active
        // bridge without holding the lock across the await.
        // We take both the Notify and the AtomicBool so we can
        // replicate wait_for_dead's late-subscriber fast-path:
        // if the bridge already died before we called
        // notified().await, the flag check returns immediately
        // rather than hanging forever.
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

        // Wait for the bridge to die. Check the flag first
        // (fast-path for already-dead bridges) before awaiting
        // the Notify so we never miss a notification that fired
        // before we subscribed.
        if !dead_flag.load(Ordering::SeqCst) {
            dead_notify.notified().await;
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
