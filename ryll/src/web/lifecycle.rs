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
//!
//! # Why the wait is a `select!`
//!
//! The reaper also wakes on `WebState::bridge_replaced`, and that
//! arm is load-bearing rather than an optimisation. One task watches
//! one bridge at a time, so the only way it advances to the next
//! bridge is for its wait to return. On webrtc-rs 0.20 a bridge
//! closed by `/offer` does not reliably raise `dead`: `close()`
//! usually consumes the driver before it dispatches the `Closed`
//! transition, and a stopped driver cannot deliver an ICE or DTLS
//! event either. Waiting on `dead` alone would therefore park this
//! task forever the first time a viewer reloaded the page. On 0.17
//! this could not happen — `pc.close()` drove the state-change
//! callback to `Closed`, which raised `dead` and released the reaper.
//!
//! # Why waking is not permission to reap
//!
//! A second wake source breaks an assumption the loop previously got
//! for free: that returning from the wait meant *this* bridge had
//! died. It no longer does, so the reap is gated on
//! `StickySignal::is_raised` — the condition itself — rather than on
//! the wait having returned.
//!
//! That gate is load-bearing, not belt and braces.
//! `Notify::notify_one` stores a permit when no task is parked, and
//! `post_offer` empties `bridge_slot` at the *start* of the request,
//! so for the whole of the encoder restart, bridge construction and
//! ICE gathering the reaper is going round its no-bridge sleep rather
//! than sitting in the `select!`. The wake lands as a stored permit;
//! the reaper's next iteration then snapshots the already-bumped
//! generation, picks up the *new* bridge, consumes the permit
//! immediately, sees an unchanged generation and — without the gate —
//! reaps the bridge the viewer just connected on. That is the ordinary
//! first-connection path, not a rare race.
//!
//! The generation check cannot cover this, and no amount of care about
//! when the notification is raised can either. The check discriminates
//! a replacement that landed *during* the wait; this is a replacement
//! that landed *before* the snapshot. The two are symmetric, and a
//! wake carries no evidence of which one it was. Only the bridge's own
//! signal does.

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

        // Wait for the bridge to die, or for `/offer` to replace it.
        //
        // `StickySignal::wait` carries both guards the death case
        // needs: the sticky fast-path for a bridge that died before we
        // subscribed, and interest registration before the flag check
        // so a death landing mid-subscribe is still delivered rather
        // than lost.
        //
        // The replacement arm is not an optimisation. This task is
        // long-lived and watches one bridge at a time, so the *only*
        // way it moves on to the next bridge is for this wait to
        // return — and a bridge that `/offer` closed does not reliably
        // raise its dead signal on webrtc-rs 0.20, because `close()`
        // usually consumes the driver before it dispatches the
        // `Closed` transition, after which no ICE or DTLS event can
        // fire either. Watching only `dead` would park this task on a
        // signal that will never fire, for the rest of the process's
        // life: the encoder would keep running for nobody, the audio
        // tap would keep feeding a dead pump, and no later viewer
        // would ever be reaped.
        tokio::select! {
            () = dead.wait() => {}
            () = state.bridge_replaced.notified() => {}
        }

        // Generation check: if `/offer` replaced the bridge
        // while we were waiting, skip the reap. The new bridge
        // is healthy and its own dead-signal is yet to fire.
        // This is also the replacement arm's normal exit.
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

        // Gate the reap on the bridge actually being dead. Returning
        // from the wait is not evidence of that: a replacement
        // notification raised while this task was on the no-bridge
        // sleep path is stored as a permit and consumed by the very
        // next `select!`, which is the normal first-connection
        // sequence — see "Why waking is not permission to reap" in
        // the module docs.
        //
        // This cannot spin. The permit has been consumed, so the next
        // iteration re-reads the same generation and the same bridge
        // and parks on both arms with nothing pending.
        if !dead.is_raised() {
            tracing::debug!("bridge reaper: woken but bridge is alive; re-parking");
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

    /// A bridge replaced by `/offer` does not strand the reaper on the
    /// old bridge's dead signal.
    ///
    /// This is the path that broke on the webrtc-rs 0.20 port. The
    /// reaper watches one bridge at a time and only advances when its
    /// wait returns; `close()` on 0.20 does not reliably raise `dead`,
    /// so a reaper watching only `dead` parks on the replaced bridge
    /// forever and never reaps any later viewer.
    ///
    /// Both bridges are real, but no peer connects to either — the
    /// dead signal is raised directly through the public
    /// [`shakenfist_spice_webrtc::StickySignal::raise`], which is what
    /// the state-change handler would do, so the test is deterministic
    /// and needs no ICE or DTLS.
    ///
    /// Without the replacement arm in `run_bridge_reaper`, this fails
    /// by timeout with the bridge still sitting in the slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaper_follows_a_replaced_bridge() {
        use shakenfist_spice_renderer::EncoderControl;
        use shakenfist_spice_webrtc::{WebrtcBridge, WebrtcBridgeConfig};
        use tokio::sync::mpsc;

        let state = Arc::new(WebState::new());

        let (enc_tx, _enc_rx) = mpsc::channel::<EncoderControl>(4);
        let first = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: enc_tx.clone(),
        })
        .await
        .expect("first bridge");
        let first_dead = first.dead_signal();
        {
            let mut slot = state.bridge_slot.lock().await;
            *slot = Some(first);
        }

        // The reaper parks on the first bridge's dead signal, which
        // this test never raises.
        let reaper = tokio::spawn(run_bridge_reaper(Arc::clone(&state)));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Replace it, exactly as `post_offer` does: install, bump the
        // generation, then wake the reaper.
        let second = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: enc_tx,
        })
        .await
        .expect("second bridge");
        let second_dead = second.dead_signal();
        {
            let mut slot = state.bridge_slot.lock().await;
            let old = slot.replace(second);
            state.bridge_generation.fetch_add(1, Ordering::SeqCst);
            if let Some(old) = old {
                old.close().await.ok();
            }
        }
        state.bridge_replaced.notify_one();

        // Give the reaper a moment to re-read the slot and park on the
        // new bridge, then kill that one.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !first_dead.is_raised(),
            "the first bridge's dead signal was never raised by this test; if it is raised \
             here the test is no longer proving what it claims to"
        );
        second_dead.raise();

        // The reaper must now reap: the slot ends up empty.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state.bridge_slot.lock().await.is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reaper did not reap the replacement bridge within 5s — it is probably still \
                 waiting on the replaced bridge's dead signal"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        reaper.abort();
        let _ = tokio::time::timeout(Duration::from_millis(200), reaper).await;
    }

    /// A replacement notification raised while no bridge is in the slot
    /// does not cause the reaper to reap the bridge that replacement
    /// installed.
    ///
    /// This is the ordinary first-connection sequence, not a rare race.
    /// `post_offer` empties `bridge_slot` before it restarts the
    /// encoder and builds the new bridge, so the reaper spends that
    /// whole window on its no-bridge sleep path with nothing parked in
    /// the `select!`. `notify_one` therefore stores a permit, and the
    /// reaper's next iteration snapshots the already-bumped generation
    /// before consuming it — so the generation check sees no change and
    /// waves the reap through.
    ///
    /// Without the `dead.is_raised()` gate this fails: the bridge is
    /// gone from the slot, its peer connection closed and the encoder
    /// stopped, moments after the viewer was sent its answer.
    ///
    /// The second half then raises the dead signal for real, so the
    /// test also shows the gate did not simply disable reaping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replacement_wake_does_not_reap_a_live_bridge() {
        use shakenfist_spice_renderer::EncoderControl;
        use shakenfist_spice_webrtc::{WebrtcBridge, WebrtcBridgeConfig};
        use tokio::sync::mpsc;

        let state = Arc::new(WebState::new());
        let (enc_tx, _enc_rx) = mpsc::channel::<EncoderControl>(4);

        // Built up front so the install below is quick enough to land
        // inside a single no-bridge sleep window.
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: enc_tx,
        })
        .await
        .expect("bridge");
        let dead = bridge.dead_signal();

        // Start the reaper with an empty slot and let it reach the
        // no-bridge sleep, which is where `post_offer` finds it.
        let reaper = tokio::spawn(run_bridge_reaper(Arc::clone(&state)));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Install exactly as `post_offer` does, while nothing is parked
        // on `bridge_replaced` — so this stores a permit.
        {
            let mut slot = state.bridge_slot.lock().await;
            *slot = Some(bridge);
        }
        state.bridge_generation.fetch_add(1, Ordering::SeqCst);
        state.bridge_replaced.notify_one();

        // Well past the 500ms no-bridge sleep, so the reaper has had
        // its chance to wake, consume the permit and act on it.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            state.bridge_slot.lock().await.is_some(),
            "the reaper reaped a live bridge on a stale replacement wake — a viewer would \
             lose its peer connection immediately after being sent an answer"
        );
        assert!(
            !dead.is_raised(),
            "the bridge died on its own; this test is no longer proving what it claims to"
        );

        // The gate must not have cost us the actual reap.
        dead.raise();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state.bridge_slot.lock().await.is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reaper did not reap a genuinely dead bridge within 5s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        reaper.abort();
        let _ = tokio::time::timeout(Duration::from_millis(200), reaper).await;
    }
}
