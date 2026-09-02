//! Integration test for the bridge's "dead" signal,
//! end-to-end.
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
//!   driven by the sticky `StickySignal`.
//!
//! No browser involved; SDP exchange is direct between the two
//! peers. ICE uses host-only candidates (empty `ice_servers`).

use std::time::Duration;

use shakenfist_spice_renderer::EncoderControl;
use tokio::sync::mpsc;

use shakenfist_spice_webrtc::test_client::TestPeer;
use shakenfist_spice_webrtc::{WebrtcBridge, WebrtcBridgeConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pc_close_signals_dead() {
    // ── Server: production WebrtcBridge ──────────────────────────
    let (server_enc_tx, _server_enc_rx) = mpsc::channel::<EncoderControl>(4);
    let server = WebrtcBridge::new(WebrtcBridgeConfig::for_tests(server_enc_tx))
        .await
        .expect("server bridge");

    // ── Client: TestPeer ────────────────────────────────────────
    //
    // The seed datachannel puts an m=application section in the
    // offer so the bridge's control DC negotiates over SCTP.
    // Without it the SCTP association never opens and the handshake
    // stalls — see the loopback test for the full rationale.
    let client = TestPeer::builder()
        .seed_data_channel()
        .build()
        .await
        .expect("client peer");

    // ── SDP exchange: client offers, server answers ─────────────
    let final_offer_sdp = client.offer_and_gather().await.expect("client offer");

    let answer_sdp = server
        .accept_offer(final_offer_sdp)
        .await
        .expect("server accept");
    client
        .set_remote_answer(answer_sdp)
        .await
        .expect("client rsd");

    // ── Wait for the client peer to reach Connected ─────────────
    //
    // ICE + DTLS are symmetric: if the client reports Connected,
    // the server side has already completed (or is about to
    // complete) its half of the handshake. The server's PC state
    // is not exposed on `WebrtcBridge`'s public API outside of
    // `cfg(test)`, so polling the client side is sufficient.
    client
        .wait_until_connected(Duration::from_secs(20))
        .await
        .expect("client PC did not reach Connected");

    // ── Force the server's PC into a terminal state ────────────
    //
    // Closing the client PC tears down DTLS + ICE; the server's
    // peer-connection-state-change callback fires `Disconnected`
    // and/or `Closed`, which raises the sticky dead signal and
    // wakes waiters. The bridge's `wait_for_dead` future resolves once
    // the server-side PC observes the failure. In practice this
    // fires in well under 5 s, but the ICE consent-freshness
    // timer and DTLS close_notify propagation are not strictly
    // bounded — give CI runners a generous 35 s ceiling.
    client.close().await.expect("client close");

    let dead = tokio::time::timeout(Duration::from_secs(35), server.wait_for_dead()).await;
    assert!(
        dead.is_ok(),
        "server bridge did not observe terminal state within 35s",
    );

    // ── Late-subscriber fast-path ─────────────────────────────
    //
    // After the first `wait_for_dead` resolves, the sticky
    // dead signal is raised. A subsequent call must return
    // immediately via `StickySignal`'s flag fast-path; a bare
    // `Notify` would block forever here, because it does not
    // queue notifications for late subscribers. 100 ms is
    // generous: the fast-path is a single atomic load and a
    // synchronous return.
    let late = tokio::time::timeout(Duration::from_millis(100), server.wait_for_dead()).await;
    assert!(
        late.is_ok(),
        "second wait_for_dead should return immediately via the sticky fast-path",
    );

    // ── Cleanup ─────────────────────────────────────────────────
    server.close().await.ok();
}
