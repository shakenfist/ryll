//! POST /offer signalling handler and per-viewer encoder
//! lifecycle.
//!
//! Each new viewer gets a fresh encoder + bridge pair. The
//! existing encoder is stopped (sending [`EncoderControl::Stop`])
//! and a new [`EncoderTask`] is spawned with a fresh
//! `mpsc::channel` for the encoded-frame stream. Single-viewer
//! enforcement: a second offer replaces the existing bridge.
//!
//! # Lock ordering
//!
//! Always close the existing bridge **before** restarting the
//! encoder. The bridge's video-pump task holds a reference to
//! the old encoder's `frame_rx`; closing the bridge drops the
//! pump task, which drops `frame_rx`, which lets the old
//! encoder task exit on its next blocking_send. Holding the
//! encoder lock while constructing a new bridge is safe — the
//! bridge constructor does not take any state lock.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use shakenfist_spice_renderer::{
    even_dimensions, EncodedFrame, EncoderControl, EncoderTask, H264Encoder, RealFrameSource,
    SurfaceMirror,
};
use shakenfist_spice_webrtc::{WebrtcBridge, WebrtcBridgeConfig};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::server::WebState;

/// Encoder FPS cap. 30 fps matches master plan Resolution §4. The
/// encoder dimensions come from the SurfaceMirror's primary surface
/// at restart time rather than a hard-coded constant.
const ENCODER_FPS: u32 = 30;

/// Sentinel error message returned by [`EncoderInfra::restart`]
/// when the SPICE session has not yet produced a primary
/// surface. The HTTP handler maps this to 503 Service Unavailable
/// so the browser can retry once the session has finished
/// initialising. Match-by-string is fragile; we use a const so
/// the producer and consumer agree.
pub const RESTART_ERR_NO_PRIMARY: &str = "primary surface not yet available";

/// JSON body of a `POST /offer` request: a browser SDP offer.
#[derive(Deserialize)]
pub struct OfferReq {
    /// Always `"offer"` in practice. We do not strictly check
    /// this — webrtc-rs's `RTCSessionDescription::offer` will
    /// reject malformed SDP regardless.
    #[serde(rename = "type")]
    pub req_type: String,
    pub sdp: String,
}

/// JSON body of a `POST /offer` response: the server's SDP
/// answer.
#[derive(Serialize)]
pub struct OfferRes {
    #[serde(rename = "type")]
    pub res_type: &'static str,
    pub sdp: String,
}

/// Holds the active encoder pipeline. [`Self::restart`] replaces
/// the encoder + frame channel atomically and returns the new
/// `frame_rx` for the caller to hand to a fresh
/// [`WebrtcBridge`].
pub struct EncoderInfra {
    /// Sender for [`EncoderControl`] messages to the running
    /// task. `None` until the first restart.
    control_tx: Option<mpsc::Sender<EncoderControl>>,
    /// JoinHandle of the running encoder task. `None` until
    /// the first restart.
    handle: Option<JoinHandle<anyhow::Result<()>>>,
}

impl EncoderInfra {
    pub fn new() -> Self {
        Self {
            control_tx: None,
            handle: None,
        }
    }

    /// Stop any existing encoder, spawn a fresh one, and
    /// return the receiver-end of the encoded-frame stream so
    /// the caller can wire it into a new [`WebrtcBridge`].
    ///
    /// Returns also a fresh `control_tx` that the caller
    /// passes into [`WebrtcBridgeConfig::encoder_control`].
    /// That sender is owned by the bridge for keyframe-on-
    /// attach signalling; the [`EncoderInfra`] holds its own
    /// clone (via the `control_tx` field) for sending
    /// [`EncoderControl::Stop`] on the next restart.
    ///
    /// Encoder dimensions are read from `surface_mirror`'s
    /// primary surface at the moment of restart. If the SPICE
    /// session has not yet produced a primary surface,
    /// returns `Err` with [`RESTART_ERR_NO_PRIMARY`]; the
    /// HTTP handler maps that to 503 so the browser retries
    /// after session-init finishes.
    pub async fn restart(
        &mut self,
        surface_mirror: &Arc<Mutex<SurfaceMirror>>,
    ) -> anyhow::Result<(mpsc::Receiver<EncodedFrame>, mpsc::Sender<EncoderControl>)> {
        // Read primary surface dimensions before tearing the
        // old encoder down — if the mirror has no primary yet
        // we want to return Err without disturbing the existing
        // pipeline. A retry from the browser then has a chance
        // to land after the SPICE session is ready.
        let (width, height) = {
            let guard = surface_mirror.lock().await;
            match guard.primary_surface() {
                Some(s) => s.size(),
                None => return Err(anyhow::anyhow!(RESTART_ERR_NO_PRIMARY)),
            }
        };

        // openh264 requires even dimensions. If SPICE produced
        // an odd-sized primary (rare, but possible at startup
        // before the guest's vdagent settles) round down by
        // one pixel — losing a single column/row is invisible
        // and avoids a hard error from H264Encoder::new. The
        // frame source keeps reporting the odd size, which
        // `H264Encoder::encode_cropped` crops to match.
        let (width, height) = even_dimensions(width, height);
        if width == 0 || height == 0 {
            return Err(anyhow::anyhow!(RESTART_ERR_NO_PRIMARY));
        }

        // Stop existing if any. Order matters: send Stop first
        // so the encoder loop sees the message on its next
        // tick; then await the JoinHandle with a timeout so a
        // wedged encoder cannot block the new viewer's offer.
        if let Some(tx) = self.control_tx.take() {
            // Use try_send rather than await: the task may
            // already have exited (channel closed) which is
            // fine and shouldn't error here.
            let _ = tx.send(EncoderControl::Stop).await;
        }
        if let Some(h) = self.handle.take() {
            let aborted = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
            match aborted {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => warn!("encoder previous run errored: {}", e),
                Ok(Err(e)) => warn!("encoder previous join errored: {}", e),
                Err(_) => warn!("encoder did not stop within 2s; continuing"),
            }
        }

        // Build new pipeline at the SPICE-derived dimensions.
        let encoder = H264Encoder::new(width, height, ENCODER_FPS)
            .map_err(|e| anyhow::anyhow!("H264Encoder::new: {}", e))?;
        let source = RealFrameSource::new(surface_mirror.clone());
        let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(32);
        let (control_tx, control_rx) = mpsc::channel::<EncoderControl>(8);

        let handle = EncoderTask::spawn(encoder, source, frame_tx, control_rx, ENCODER_FPS);

        // Hold our own clone of control_tx so we can send
        // Stop on the next restart. Hand the original to the
        // caller for the bridge config (the bridge clones it
        // again internally; either copy works).
        self.control_tx = Some(control_tx.clone());
        self.handle = Some(handle);

        info!(
            "web: encoder restarted at {}x{}@{}fps",
            width, height, ENCODER_FPS
        );
        Ok((frame_rx, control_tx))
    }

    /// Stop the active encoder task without restarting. Used
    /// by the bridge reaper when the browser
    /// disconnects and no immediate replacement is expected,
    /// and by the shutdown path in `run_web` to release
    /// resources before the runtime drops.
    ///
    /// Sends [`EncoderControl::Stop`] on the control channel
    /// and awaits the task [`JoinHandle`] with a 2-second
    /// ceiling. If the task does not exit within the timeout
    /// it is abandoned — the task will exit naturally on the
    /// next send error once all receivers are dropped.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.control_tx.take() {
            let _ = tx.send(EncoderControl::Stop).await;
        }
        if let Some(h) = self.handle.take() {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
            match result {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => tracing::warn!("encoder task errored on stop: {}", e),
                Ok(Err(e)) => tracing::warn!("encoder join errored on stop: {}", e),
                Err(_) => {
                    tracing::warn!("encoder did not stop within 2s on stop; abandoning")
                }
            }
        }
    }
}

impl Default for EncoderInfra {
    fn default() -> Self {
        Self::new()
    }
}

/// HTTP status for an `accept_offer` failure.
///
/// Everything `accept_offer` rejects is the browser's offer being
/// unusable — malformed SDP, no codec in common — except a gathering
/// timeout, which happened entirely on this side. Reporting that as
/// `400 Bad Request` tells the operator the browser sent rubbish and
/// tells the page to stop retrying; `504 Gateway Timeout` says what
/// happened and reads as worth another go.
///
/// Matched by the sentinel the webrtc crate puts in the message, for
/// the same reason [`RESTART_ERR_NO_PRIMARY`] is a const: the
/// producer and the consumer have to agree on the string.
fn accept_offer_status(msg: &str) -> StatusCode {
    if msg.contains(shakenfist_spice_webrtc::GATHERING_TIMEOUT_ERR) {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// Whether the request that entered `post_offer` at
/// `generation_at_entry` is still the latest offer, and so still owns
/// the shared encoder and audio slot.
///
/// `bridge_generation` is bumped once per bridge installed, so an
/// unchanged counter means no later offer has reached step 7. The
/// reaper reads the same counter for the same reason; see
/// `crate::web::lifecycle::run_bridge_reaper`.
fn still_the_current_offer(state: &WebState, generation_at_entry: u64) -> bool {
    state.bridge_generation.load(Ordering::SeqCst) == generation_at_entry
}

/// `POST /offer` handler.
///
/// Steps:
///
/// 1. Take `bridge_slot` lock; pull out any existing bridge.
/// 2. Drop the lock and close the old bridge (lets its
///    video-pump task drop the old `frame_rx`).
/// 3. Take `encoder` lock; `restart()` to get a fresh
///    `frame_rx` and a fresh `control_tx`.
/// 4. Build the new [`WebrtcBridge`] with `control_tx` as
///    `encoder_control`.
/// 5. `spawn_video_pump(frame_rx)` and
///    `spawn_synthetic_audio_pump`.
/// 6. `accept_offer(offer.sdp)` -> answer SDP.
/// 7. Take `bridge_slot` lock; store the new bridge.
/// 8. Return JSON answer.
pub async fn post_offer(
    State(state): State<Arc<WebState>>,
    Json(offer): Json<OfferReq>,
) -> Result<Json<OfferRes>, (StatusCode, String)> {
    // Rate limit: at most one accepted offer per second. Uses a
    // std::sync::Mutex because the lock hold time is microseconds
    // and no .await is held while locked.
    {
        let now = Instant::now();
        match state.last_offer_at.lock() {
            Ok(mut last) => {
                if now.duration_since(*last) < Duration::from_secs(1) {
                    return Err((
                        StatusCode::TOO_MANY_REQUESTS,
                        "Too many requests; wait 1 s between offers".to_string(),
                    ));
                }
                *last = now;
            }
            Err(poisoned) => {
                // Recover from a poisoned mutex rather than
                // panic; reset the timestamp and proceed.
                let mut inner = poisoned.into_inner();
                *inner = now;
            }
        }
    }

    info!(
        "web: /offer received (type={}, sdp_len={})",
        offer.req_type,
        offer.sdp.len()
    );

    // The generation this request starts from. Only the failure path
    // below reads it, to tell "nothing has happened since" from "a
    // later offer has installed its own bridge", which decides
    // whether the shared encoder and audio slot are still ours to
    // stop. The cooldown makes that interleaving rare, not
    // impossible: it bounds how often an offer may *start*, and this
    // request can sit in `accept_offer` for longer than a second
    // waiting for ICE gathering.
    let generation_at_entry = state.bridge_generation.load(Ordering::SeqCst);

    // Step 1+2: replace any existing bridge.
    let old_bridge = {
        let mut slot = state.bridge_slot.lock().await;
        slot.take()
    };
    if let Some(old) = old_bridge {
        if let Err(e) = old.close().await {
            warn!("web: closing previous bridge errored: {}", e);
        }
    }

    // Step 3: restart the encoder. The mirror is read here to
    // pick up the primary surface dimensions at restart time.
    // If no primary surface exists yet (browser connected
    // before SPICE finished session-init), surface-the
    // sentinel as a 503 so the browser retries.
    let (frame_rx, encoder_control) = {
        let mut enc = state.encoder.lock().await;
        match enc.restart(&state.surface_mirror).await {
            Ok(pair) => pair,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains(RESTART_ERR_NO_PRIMARY) {
                    return Err((StatusCode::SERVICE_UNAVAILABLE, msg));
                }
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("encoder restart: {}", msg),
                ));
            }
        }
    };

    // Step 4: build the new bridge.
    let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
        ice_servers: state.ice_servers.clone(),
        encoder_control,
        udp_bind: state.udp_bind.clone(),
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("bridge new: {}", e),
        )
    })?;

    // Step 5: spawn pumps. The handles are detached — the
    // pumps live for as long as the bridge does. Closing the
    // bridge drops the tracks, which causes write_rtp to
    // fail; the video pump exits when its frame_rx closes
    // (driven by step 1+2 of the next offer); the audio pump
    // exits when its rx closes (driven by the next /offer
    // overwriting `active_opus_tx`, dropping this Sender).
    let _video_handle = bridge.spawn_video_pump(frame_rx);
    // Real Opus passthrough from the SPICE playback channel. Build a
    // fresh per-viewer mpsc and plug the Sender into the shared slot
    // the renderer-side `WebPlaybackSink` reads from. The previous
    // bridge's Sender is replaced atomically; that drops the
    // previous audio pump's `Receiver` and causes that pump to exit
    // cleanly.
    let (opus_tx, opus_rx) = mpsc::channel::<(Vec<u8>, u32)>(64);
    {
        // std::sync::Mutex on the slot — see the rationale in
        // ryll/src/web/audio.rs. lock() may panic if poisoned;
        // we recover the inner Option in that case.
        match state.active_opus_tx.lock() {
            Ok(mut guard) => *guard = Some(opus_tx),
            Err(poisoned) => {
                let mut inner = poisoned.into_inner();
                *inner = Some(opus_tx);
            }
        }
    }
    let _audio_handle = bridge.spawn_audio_pump(opus_rx);

    // Spawn the browser → renderer input relay. Each
    // bridge owns exactly one control DC; `control_rx()` is
    // single-shot, so taking it here is correct. We only spawn
    // the relay when the renderer-side senders are populated
    // (i.e. `run_web` wired a real SPICE session); the unit
    // tests construct `WebState::new()` without senders and
    // skip this branch silently.
    if let (Some(input_tx), Some(resize_tx)) = (state.input_tx.clone(), state.resize_tx.clone()) {
        if let Some(control_rx) = bridge.control_rx() {
            let mirror = state.surface_mirror.clone();
            tokio::spawn(crate::web::inputs::run_input_relay(
                control_rx,
                input_tx,
                resize_tx,
                mirror,
                state.mouse_mode.clone(),
                state.control_tx.clone(),
                state.no_video_codec.clone(),
            ));
        } else {
            warn!("web: bridge.control_rx() returned None; input relay not spawned");
        }
    }

    // Step 6: SDP exchange.
    //
    // Close the bridge explicitly on the error path, and stop the work
    // started for it. It is not in `bridge_slot` yet, so nothing else
    // will ever reap it, and on 0.20 dropping it detaches the driver
    // task rather than stopping it — the peer connection, its UDP
    // sockets (one per non-loopback interface), the control-DC pump,
    // and the input relay spawned above would all outlive the failed
    // request. A client posting malformed SDP in a loop would
    // accumulate them for the life of the process; the offer cooldown
    // bounds the rate, not the total. `WebrtcBridge`'s `Drop` is a
    // backstop for paths that forget, but here the close is awaited,
    // so teardown is finished before the 400 goes back.
    //
    // The encoder restarted at step 3 and the audio sender installed
    // at step 5 are not the bridge's to drop, and a failed offer
    // leaves them running for a viewer that does not exist — burning
    // a core on H.264 nobody receives until the next offer restarts
    // it. They are stopped here through the same helper the reaper
    // uses, but only while this request is still the latest one: a
    // later offer that has already installed its bridge owns the
    // encoder and the audio slot now, and its session must survive
    // this one's failure.
    let answer_sdp = match bridge.accept_offer(offer.sdp).await {
        Ok(sdp) => sdp,
        Err(e) => {
            if let Err(ce) = bridge.close().await {
                warn!("web: closing an abandoned bridge errored: {}", ce);
            }
            if still_the_current_offer(&state, generation_at_entry) {
                crate::web::lifecycle::stop_viewer_pipeline(&state).await;
            } else {
                debug!(
                    "web: a later offer installed its own bridge while this one failed;                      leaving the encoder and audio slot alone"
                );
            }
            let msg = format!("accept_offer: {}", e);
            return Err((accept_offer_status(&msg), msg));
        }
    };

    // Negotiation has settled, so the answer to "will this viewer
    // ever see a picture" is now known. Record it for the input
    // relay to hand back when the browser says hello — this is the
    // earliest the fact exists and the latest it can be captured,
    // since the bridge moves into `bridge_slot` on the next
    // statement. A viewer with no common video codec otherwise gets
    // a session where everything except the picture works, which
    // reads as a rendering bug rather than a negotiation one.
    let no_video = !bridge.video_negotiated();
    state.no_video_codec.store(no_video, Ordering::SeqCst);
    if no_video {
        warn!(
            "web: this viewer offered no H.264; video is disabled for the session and the \
             browser has been told why"
        );
    }

    // Step 7: store the new bridge and bump the generation
    // counter so the reaper knows not to act on the dead
    // signal from the OLD bridge (which may fire after the
    // new bridge is already in the slot).
    {
        let mut slot = state.bridge_slot.lock().await;
        *slot = Some(bridge);
        state.bridge_generation.fetch_add(1, Ordering::SeqCst);
    }

    // Wake the reaper so it stops watching the bridge we just
    // replaced. Strictly after the generation bump above: the reaper
    // compares generations on waking to decide whether the bridge it
    // was watching is still current, and waking it any earlier would
    // have it see an unchanged counter and reap the bridge installed
    // a line ago.
    //
    // Needed because closing the old bridge does not reliably raise
    // its dead signal on webrtc-rs 0.20 — without this the reaper
    // parks on a signal that will never fire and never observes any
    // later bridge. See `crate::web::lifecycle::run_bridge_reaper`.
    state.bridge_replaced.notify_one();

    // The browser is caught up on the mouse mode by its own `hello`
    // over the control datachannel, not from here. Sending it at this
    // point would be strictly too early: the answer below has not
    // been returned yet, so ICE, DTLS and SCTP have not run and the
    // datachannel cannot accept anything. See `BrowserMsg::Hello`.

    info!("web: /offer answered (answer_sdp_len={})", answer_sdp.len());
    Ok(Json(OfferRes {
        res_type: "answer",
        sdp: answer_sdp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::server::{build_router, WebState};
    use axum::body::Body;
    use axum::http::{header, Method, Request as HttpRequest, StatusCode};
    use shakenfist_spice_webrtc::test_client::TestPeer;
    use tower::ServiceExt;

    /// Helper struct that mirrors [`OfferRes`] for
    /// deserialisation in tests. [`OfferRes`] only derives
    /// `Serialize` (it's a response shape), so deserialising
    /// the response body needs a parallel struct.
    #[derive(serde::Deserialize)]
    struct OfferResJson {
        #[serde(rename = "type")]
        res_type: String,
        sdp: String,
    }

    /// Build a real client-side `RTCPeerConnection`, generate
    /// an offer, and POST it to the `/offer` endpoint of an
    /// in-process axum app. Assert the response is 200 and
    /// carries an SDP answer that advertises H.264.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn post_offer_returns_valid_answer() {
        // Install the rustls ring provider, mirroring the
        // production install in `main()`. Not for the DTLS
        // handshake: `shakenfist-spice-webrtc` has no rustls
        // dependency since the webrtc-0.20 port, and rtc-dtls
        // selects its crypto provider from its own cargo
        // features without consulting the process default. It
        // is the SPICE TLS and axum-server paths that need one.
        // `install_default` is idempotent across concurrent
        // tests (it returns Err if already set, which we
        // ignore).
        let _ = rustls::crypto::ring::default_provider().install_default();

        let state = Arc::new(WebState::new());
        // Seed the surface mirror with a primary so the encoder restart
        // path doesn't return the 503 sentinel. 1280x720 is an arbitrary
        // but realistic primary size.
        {
            let mut m = state.surface_mirror.lock().await;
            m.apply_event(&shakenfist_spice_renderer::ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 1280,
                height: 720,
            });
        }
        let token = state.token.clone();
        // Keep a handle on the state: the encoder `post_offer` starts
        // has to be stopped below.
        let router = build_router(state.clone());

        // Build a client-side PC to generate a real SDP
        // offer.
        //
        // The offer must carry an m=application section, or
        // the bridge's data-channel
        // expectations don't match the answer side. That is
        // what the seed datachannel is for.
        //
        // This test previously built its client without the
        // bridge's explicit H.264 registration. That turns out
        // to be indistinguishable from registering it --
        // webrtc-rs's default codecs already advertise H.264 --
        // so `TestPeer`'s single spelling is used here too. See
        // `register_h264_is_redundant_with_default_codecs` in
        // shakenfist-spice-webrtc.
        let client = TestPeer::builder()
            .seed_control_channel()
            .build()
            .await
            .expect("client peer");

        // Gathering completes before the offer is returned, so
        // it carries every candidate.
        let final_offer_sdp = client.offer_and_gather().await.expect("offer");

        // POST the offer.
        let body = serde_json::json!({
            "type": "offer",
            "sdp": final_offer_sdp,
        })
        .to_string();
        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri(format!("/offer?token={}", token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();

        let resp = router.oneshot(req).await.expect("router");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "expected 200, got {}",
            resp.status()
        );

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let answer: OfferResJson = serde_json::from_slice(&body_bytes).expect("json");
        assert_eq!(answer.res_type, "answer");
        assert!(
            answer.sdp.contains("v=0"),
            "answer SDP should start with v=0:\n{}",
            answer.sdp
        );
        let lower = answer.sdp.to_ascii_lowercase();
        assert!(
            lower.contains("h264"),
            "answer SDP should advertise H264:\n{}",
            answer.sdp
        );

        // Cleanup: feed the answer back to the client PC so
        // it can close cleanly.
        client
            .set_remote_answer(answer.sdp)
            .await
            .expect("client rsd");
        client.close().await.expect("client close");

        // Stop the encoder `post_offer` started, as production does on
        // the next offer or when the viewer goes. It runs on the
        // blocking pool, and dropping this test's runtime waits for
        // every blocking task, so an encoder left running hangs the
        // test after its last assertion has passed. That is how this
        // test spent six hours at a time in CI (#330); the encoder now
        // also stops once its frame receiver is dropped, but a test
        // should not lean on teardown order to clean up after itself.
        state.encoder.lock().await.stop().await;
    }

    /// `post_offer` must not try to push anything to the browser.
    ///
    /// It used to end by sending the current mouse mode, to catch a
    /// newly-connected browser up on state the SPICE server announced
    /// at session-init — long before any browser existed, and never
    /// re-broadcast, since the mode does not change again on a healthy
    /// session. The send was strictly too early: it ran before the
    /// answer below had even been returned, so ICE, DTLS and SCTP had
    /// not run and the datachannel could not accept anything.
    /// `send_control` has no buffering and no open-state tracking, and
    /// the error was swallowed at debug, so it failed in silence.
    ///
    /// The browser now asks instead (`BrowserMsg::Hello`). This test
    /// pins the negative half of that: nothing is queued for the
    /// browser during signalling. It fails on the old code — the
    /// mouse-mode message is sitting in the queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn post_offer_queues_nothing_for_a_browser_that_cannot_receive_yet() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let state = Arc::new(WebState::new());
        {
            let mut m = state.surface_mirror.lock().await;
            m.apply_event(&shakenfist_spice_renderer::ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 1280,
                height: 720,
            });
        }
        let token = state.token.clone();
        let router = build_router(state.clone());

        let client = TestPeer::builder()
            .seed_control_channel()
            .build()
            .await
            .expect("client peer");
        let final_offer_sdp = client.offer_and_gather().await.expect("offer");

        let body = serde_json::json!({ "type": "offer", "sdp": final_offer_sdp }).to_string();
        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri(format!("/offer?token={}", token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = router.oneshot(req).await.expect("router");
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let answer: OfferResJson = serde_json::from_slice(&body_bytes).expect("json");

        let queued = state
            .control_rx
            .lock()
            .await
            .as_mut()
            .expect("no writer has taken the receiver in tests")
            .try_recv();
        assert!(
            queued.is_err(),
            "post_offer queued {:?} for a datachannel that cannot be open yet",
            queued.map(|b| String::from_utf8_lossy(&b).into_owned())
        );

        client
            .set_remote_answer(answer.sdp)
            .await
            .expect("client rsd");
        client.close().await.expect("client close");
        state.encoder.lock().await.stop().await;
    }

    /// Without a token, `POST /offer` is rejected by the
    /// middleware before the handler runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_offer_without_token_is_unauthorized() {
        let state = Arc::new(WebState::new());
        let router = build_router(state);

        let body = serde_json::json!({
            "type": "offer",
            "sdp": "v=0\r\n",
        })
        .to_string();
        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/offer")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = router.oneshot(req).await.expect("router");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// `EncoderInfra::restart` returns the
    /// [`RESTART_ERR_NO_PRIMARY`] sentinel when the surface
    /// mirror has no primary surface. The HTTP handler turns
    /// this into 503 so the browser can retry once SPICE has
    /// finished session-init.
    #[tokio::test(flavor = "current_thread")]
    async fn restart_errs_when_mirror_empty() {
        let mirror = Arc::new(Mutex::new(SurfaceMirror::new()));
        let mut infra = EncoderInfra::new();
        let err = infra
            .restart(&mirror)
            .await
            .expect_err("restart should error on empty mirror");
        let msg = err.to_string();
        assert!(
            msg.contains(RESTART_ERR_NO_PRIMARY),
            "error should carry the no-primary sentinel: {}",
            msg
        );
    }

    /// Once the mirror has a primary surface,
    /// `EncoderInfra::restart` succeeds and produces a fresh
    /// frame_rx + control_tx pair.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_succeeds_with_primary_surface() {
        let mirror = Arc::new(Mutex::new(SurfaceMirror::new()));
        {
            let mut m = mirror.lock().await;
            m.apply_event(&shakenfist_spice_renderer::ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 640,
                height: 480,
            });
        }
        let mut infra = EncoderInfra::new();
        let (frame_rx, control_tx) = infra
            .restart(&mirror)
            .await
            .expect("restart should succeed");
        // Sanity: the channel pair is alive.
        assert!(!control_tx.is_closed());
        drop(frame_rx);
        // Stop the spawned encoder so the test exits cleanly.
        let _ = control_tx
            .send(shakenfist_spice_renderer::EncoderControl::Stop)
            .await;
    }

    /// `EncoderInfra::stop` sends `Stop` and awaits the task
    /// handle. After `stop()` returns, `control_tx` and
    /// `handle` are both `None` and the encoder task has exited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_releases_encoder_task() {
        let mirror = Arc::new(Mutex::new(SurfaceMirror::new()));
        {
            let mut m = mirror.lock().await;
            m.apply_event(&shakenfist_spice_renderer::ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 640,
                height: 480,
            });
        }
        let mut infra = EncoderInfra::new();
        let (_frame_rx, _control_tx) = infra
            .restart(&mirror)
            .await
            .expect("restart should succeed");
        // Encoder is running. Now stop it via stop().
        infra.stop().await;
        // After stop(), the infra fields are cleared.
        assert!(
            infra.control_tx.is_none(),
            "control_tx should be None after stop()"
        );
        assert!(infra.handle.is_none(), "handle should be None after stop()");
    }

    /// Seed a `WebState`'s surface mirror with a primary surface so
    /// `EncoderInfra::restart` does not return the no-primary
    /// sentinel. 1280x720 is arbitrary but realistic.
    async fn seed_primary_surface(state: &WebState) {
        let mut m = state.surface_mirror.lock().await;
        m.apply_event(&shakenfist_spice_renderer::ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: 1280,
            height: 720,
        });
    }

    /// Install a sender in the audio slot, as a bridge's `/offer`
    /// does, and return the matching receiver so the channel stays
    /// open for the assertions.
    fn install_opus_sender(state: &WebState) -> mpsc::Receiver<(Vec<u8>, u32)> {
        let (tx, rx) = mpsc::channel::<(Vec<u8>, u32)>(4);
        *state.active_opus_tx.lock().expect("opus slot") = Some(tx);
        rx
    }

    /// An offer that fails `accept_offer` must not leave the encoder
    /// and the audio pump it started running for a viewer that never
    /// arrived. Before this was fixed they ran until the next
    /// successful offer, encoding H.264 for nobody.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_offer_stops_the_work_it_started() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let state = Arc::new(WebState::new());
        seed_primary_surface(&state).await;
        let _opus_rx = install_opus_sender(&state);

        // "v=0" alone is a syntactically valid start and a useless
        // session description: the bridge gets past JSON decoding and
        // fails in `accept_offer`, which is the path under test.
        let res = post_offer(
            State(Arc::clone(&state)),
            Json(OfferReq {
                req_type: "offer".to_string(),
                sdp: "v=0\r\n".to_string(),
            }),
        )
        .await;
        // `OfferRes` is a response shape and derives no Debug, so the
        // Ok arm is named rather than unwrapped.
        match res {
            Ok(_) => panic!("a malformed offer should be rejected"),
            Err((status, msg)) => assert_eq!(status, StatusCode::BAD_REQUEST, "{}", msg),
        }

        assert!(
            state.encoder.lock().await.control_tx.is_none(),
            "the encoder restarted for the failed offer should have been stopped"
        );
        assert!(
            state.active_opus_tx.lock().expect("opus slot").is_none(),
            "the audio pump sender should have been cleared"
        );
    }

    /// A gathering timeout is this side's failure, not the browser's,
    /// and the handler has to say so: the page shows the status, and a
    /// 400 would have the operator looking at the browser.
    #[test]
    fn a_gathering_timeout_is_a_gateway_timeout_not_a_bad_request() {
        let timed_out = format!(
            "accept_offer: {} within 20s: no local description at all",
            shakenfist_spice_webrtc::GATHERING_TIMEOUT_ERR
        );
        assert_eq!(
            accept_offer_status(&timed_out),
            StatusCode::GATEWAY_TIMEOUT,
            "a gathering timeout must not be reported as a bad offer"
        );
        assert_eq!(
            accept_offer_status("accept_offer: SdpInvalidSyntax"),
            StatusCode::BAD_REQUEST,
            "an unusable offer is still the browser's fault"
        );
    }

    /// The teardown above must not reach across a newer viewer. If a
    /// later offer has installed its bridge — which bumps
    /// `bridge_generation` — the encoder and the audio slot belong to
    /// that session, and a failure finishing afterwards must leave
    /// them alone.
    ///
    /// Tested on the predicate rather than through `post_offer`,
    /// because the interleaving it guards against cannot be staged
    /// deterministically from outside: the bump has to land after the
    /// handler has read the counter and before its `accept_offer`
    /// returns, and a malformed offer fails that fast with no await
    /// point in between. The predicate is the whole of the decision.
    #[tokio::test(flavor = "current_thread")]
    async fn a_later_offer_takes_ownership_of_the_shared_pipeline() {
        let state = WebState::new();
        let generation_at_entry = state.bridge_generation.load(Ordering::SeqCst);
        assert!(
            still_the_current_offer(&state, generation_at_entry),
            "with no later bridge installed, the failing offer still owns the pipeline"
        );

        // Step 7 of a later offer: install a bridge, bump the counter.
        state.bridge_generation.fetch_add(1, Ordering::SeqCst);
        assert!(
            !still_the_current_offer(&state, generation_at_entry),
            "once a later offer has installed its bridge, the pipeline is no longer ours"
        );
    }

    /// Two `POST /offer` requests in rapid succession: the second
    /// must return 429 Too Many Requests. This exercises the
    /// `last_offer_at` cooldown added in the wave-2d security fix.
    ///
    /// We call `post_offer` directly (bypassing the axum router)
    /// to avoid building a real WebRTC offer, which would be too
    /// slow for an in-series burst test. We rely on the rate-limit
    /// guard returning early before any bridge construction so a
    /// dummy SDP is fine.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_offer_within_cooldown_returns_429() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let state = Arc::new(WebState::new());
        // Seed the surface mirror so the encoder restart does not
        // return 503 for the first offer.
        {
            let mut m = state.surface_mirror.lock().await;
            m.apply_event(&shakenfist_spice_renderer::ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 1280,
                height: 720,
            });
        }
        let token = state.token.clone();
        let router = build_router(Arc::clone(&state));

        // Helper: build a minimal (deliberately malformed) offer
        // body. The SDP will fail `accept_offer` inside the
        // handler, but a 429 fires before that point, so the
        // second request never reaches `accept_offer`.
        let offer_body = serde_json::json!({
            "type": "offer",
            "sdp": "v=0\r\n",
        })
        .to_string();

        // First request — should NOT be rate-limited (not 429).
        // It may return any other status (200 if accept_offer
        // somehow succeeds, or 400 on bad SDP — both are fine).
        let req1 = HttpRequest::builder()
            .method(Method::POST)
            .uri(format!("/offer?token={}", token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(offer_body.clone()))
            .unwrap();

        // Second request — must be 429. Send both requests with
        // no sleep between them so the cooldown window cannot
        // expire between the two.
        let req2 = HttpRequest::builder()
            .method(Method::POST)
            .uri(format!("/offer?token={}", token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(offer_body.clone()))
            .unwrap();

        // Use a cloned router for each oneshot call.
        let router2 = router.clone();
        let resp1 = router.oneshot(req1).await.expect("router req1");
        let resp2 = router2.oneshot(req2).await.expect("router req2");

        assert_ne!(
            resp1.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "first offer should not be rate-limited"
        );
        assert_eq!(
            resp2.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "second offer within cooldown should be 429"
        );
    }
}
