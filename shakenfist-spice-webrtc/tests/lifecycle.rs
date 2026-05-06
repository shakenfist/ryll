//! Phase 6 step 6a integration test: exercises the bridge's
//! "dead" signal end-to-end.
//!
//! Two peers run in the same process. The "server" peer is the
//! production [`WebrtcBridge`]; the "client" peer is a
//! hand-rolled `RTCPeerConnection` (mirroring the loopback
//! test's pattern) so we can call `client_pc.close()` and force
//! the server's PC into a terminal state.
//!
//! Asserts:
//! * After `client_pc.close()`, the server's
//!   [`WebrtcBridge::wait_for_dead`] resolves within 35 seconds
//!   (the PC observes `Disconnected` or `Closed`). The ceiling
//!   accommodates ICE consent-freshness and DTLS close_notify
//!   propagation under load; in practice the signal fires in
//!   well under 5 s but CI runners can be slow.
//! * A subsequent call to `wait_for_dead()` returns
//!   immediately, exercising the late-subscriber fast-path
//!   driven by the sticky `dead_flag`.
//!
//! No browser involved; SDP exchange is direct between the two
//! peers. ICE uses host-only candidates (empty `ice_servers`).

use std::sync::Arc;
use std::time::Duration;

use shakenfist_spice_renderer::EncoderControl;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

use shakenfist_spice_webrtc::{WebrtcBridge, WebrtcBridgeConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pc_close_signals_dead() {
    // rustls CryptoProvider: webrtc 0.17.1 pulls both ring and
    // aws-lc-rs into the dependency graph through rustls 0.23, so
    // rustls cannot auto-select. Install ring explicitly.
    // `install_default` is idempotent across concurrent tests
    // (it returns Err if already set, which we ignore).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // ── Server: production WebrtcBridge ──────────────────────────
    let (server_enc_tx, _server_enc_rx) = mpsc::channel::<EncoderControl>(4);
    let server = WebrtcBridge::new(WebrtcBridgeConfig {
        ice_servers: vec![],
        encoder_control: server_enc_tx,
    })
    .await
    .expect("server bridge");

    // ── Client: hand-rolled RTCPeerConnection ───────────────────
    //
    // Codec registration mirrors the server bridge: default codecs
    // (Opus, VP8, several H.264 profiles, ...) plus an explicit
    // H.264 PT 102 with the same fmtp line as the bridge so the
    // SDP offer/answer converges.
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .expect("default codecs");
    let h264 = RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    };
    media_engine
        .register_codec(h264, RTPCodecType::Video)
        .expect("client h264");

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine).expect("interceptors");
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();
    let client_pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .expect("client pc"),
    );

    // Add recv-only video and audio transceivers so the offer
    // carries m=video and m=audio sections.
    let _ = client_pc
        .add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .expect("video transceiver");
    let _ = client_pc
        .add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .expect("audio transceiver");

    // Seed an m=application section so the bridge's control DC
    // negotiates over SCTP. Without this the SCTP association
    // never opens and the loopback handshake stalls — see the
    // loopback test for the full rationale.
    let _client_seed_dc = client_pc
        .create_data_channel("client-seed", None)
        .await
        .expect("client seed dc");

    // ── SDP exchange: client offers, server answers ─────────────
    let offer = client_pc.create_offer(None).await.expect("offer");
    client_pc
        .set_local_description(offer)
        .await
        .expect("client lsd");
    let mut gather = client_pc.gathering_complete_promise().await;
    let _ = gather.recv().await;
    let final_offer_sdp = client_pc
        .local_description()
        .await
        .expect("client local description")
        .sdp;

    let answer_sdp = server
        .accept_offer(final_offer_sdp)
        .await
        .expect("server accept");
    let answer = RTCSessionDescription::answer(answer_sdp).expect("answer");
    client_pc
        .set_remote_description(answer)
        .await
        .expect("client rsd");

    // ── Wait for the client peer to reach Connected ─────────────
    //
    // ICE + DTLS are symmetric: if the client reports Connected,
    // the server side has already completed (or is about to
    // complete) its half of the handshake. The server's PC state
    // is not exposed on `WebrtcBridge`'s public API outside of
    // `cfg(test)`, so polling the client side is sufficient.
    let connected = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if client_pc.connection_state() == RTCPeerConnectionState::Connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        connected.is_ok(),
        "client PC did not reach Connected within 20s (state={:?})",
        client_pc.connection_state(),
    );

    // ── Force the server's PC into a terminal state ────────────
    //
    // Closing the client PC tears down DTLS + ICE; the server's
    // peer-connection-state-change callback fires `Disconnected`
    // and/or `Closed`, which sets `dead_flag` and notifies
    // waiters. The bridge's `wait_for_dead` future resolves once
    // the server-side PC observes the failure. In practice this
    // fires in well under 5 s, but the ICE consent-freshness
    // timer and DTLS close_notify propagation are not strictly
    // bounded — give CI runners a generous 35 s ceiling.
    client_pc.close().await.expect("client close");

    let dead = tokio::time::timeout(Duration::from_secs(35), server.wait_for_dead()).await;
    assert!(
        dead.is_ok(),
        "server bridge did not observe terminal state within 35s",
    );

    // ── Late-subscriber fast-path ─────────────────────────────
    //
    // After the first `wait_for_dead` resolves, the sticky
    // `dead_flag` is set. A subsequent call must return
    // immediately via the flag check; otherwise we'd block on
    // `notify.notified().await`, which does not queue
    // notifications for late subscribers and would wait forever.
    // 100 ms is generous: the fast-path is a single atomic load
    // and a synchronous return.
    let late = tokio::time::timeout(Duration::from_millis(100), server.wait_for_dead()).await;
    assert!(
        late.is_ok(),
        "second wait_for_dead should return immediately via the dead_flag fast-path",
    );

    // ── Cleanup ─────────────────────────────────────────────────
    server.close().await.ok();
}
