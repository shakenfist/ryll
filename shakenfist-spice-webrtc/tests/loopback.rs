//! Phase 3 step 3f: full in-process loopback integration test.
//!
//! Two peers run in the same process. The "server" peer uses
//! [`WebrtcBridge`] (the production type) and runs the H.264 video
//! pump (driven by a real `H264Encoder` + `EncoderTask` +
//! `SyntheticFrameSource`) and the synthetic Opus audio pump. The
//! "client" peer is a hand-rolled `RTCPeerConnection`: the bridge's
//! API is shaped for the *server* role (sending video + audio,
//! owning the control DC), so to verify "incoming RTP packets" and
//! the ping/pong round-trip we drive the client side directly.
//!
//! Asserts:
//! * >= 10 video RTP packets received within ~3 seconds.
//! * >= 5 audio RTP packets received within ~3 seconds.
//! * Round-trip on the control DC: server sends "ping", the client
//!   replies "pong", server's `control_rx` delivers it.
//!
//! No browser involved; SDP exchange is direct between the two
//! peers. ICE uses host-only candidates (empty `ice_servers`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use shakenfist_spice_renderer::{EncoderControl, EncoderTask, H264Encoder, SyntheticFrameSource};
use tokio::sync::mpsc;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_remote::TrackRemote;

use shakenfist_spice_webrtc::test_client::TestPeer;
use shakenfist_spice_webrtc::{WebrtcBridge, WebrtcBridgeConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_video_audio_datachannel() {
    // rustls CryptoProvider: webrtc 0.17.1 pulls both ring and
    // aws-lc-rs into the dependency graph through rustls 0.23, so
    // rustls cannot auto-select. Install ring explicitly. The 3e
    // unit test does the same; `install_default` is idempotent
    // (returns Err if already set; we ignore the result).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // ── Server: production WebrtcBridge ──────────────────────────
    let (server_enc_tx, server_enc_rx) = mpsc::channel::<EncoderControl>(4);
    let server = WebrtcBridge::new(WebrtcBridgeConfig {
        ice_servers: vec![],
        encoder_control: server_enc_tx,
    })
    .await
    .expect("server bridge");

    // ── Client: TestPeer ────────────────────────────────────────
    //
    // The bridge's surface area is "I send video + audio + own a
    // control DC"; the receiving side needs `on_track` and
    // `on_data_channel` callbacks that the bridge does not expose.
    // Driving the client side directly keeps the bridge's public
    // API focused on its production (server) role.
    //
    // `TestPeer` handles the codec registration (default codecs plus
    // the bridge's H.264 PT 102), the recvonly transceivers, and the
    // seed datachannel that puts an m=application section in the
    // offer. The `on_track` / `on_data_channel` wiring below is
    // specific to this test, so it reaches through to the raw PC.
    let client = TestPeer::builder()
        .seed_data_channel("client-seed")
        .build()
        .await
        .expect("client peer");
    let client_pc = client.pc().clone();

    // Counters for incoming RTP packets, by track kind.
    let video_count = Arc::new(AtomicUsize::new(0));
    let audio_count = Arc::new(AtomicUsize::new(0));

    // on_track: spawn a per-track reader loop that increments the
    // appropriate counter for each successfully decoded RTP packet.
    //
    // The read loop runs in a `tokio::spawn`-ed task rather than
    // directly inside the on_track callback. webrtc-rs awaits the
    // returned future before firing on_track for the *next* track,
    // so a long-lived `read_rtp` loop inside the callback would
    // pin the event loop on the first track (audio) and prevent
    // on_track from ever firing for video. Spawning lets the
    // callback return immediately and both kinds receive packets.
    {
        let video_count = video_count.clone();
        let audio_count = audio_count.clone();
        client_pc.on_track(Box::new(
            move |track: Arc<TrackRemote>,
                  _receiver: Arc<RTCRtpReceiver>,
                  _transceiver: Arc<RTCRtpTransceiver>| {
                let video_count = video_count.clone();
                let audio_count = audio_count.clone();
                Box::pin(async move {
                    let kind = track.kind();
                    let counter = match kind {
                        RTPCodecType::Video => video_count,
                        RTPCodecType::Audio => audio_count,
                        _ => return,
                    };
                    tokio::spawn(async move {
                        while track.read_rtp().await.is_ok() {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                })
            },
        ));
    }

    // on_data_channel: when the server's control DC arrives, install
    // an on_message handler that echoes "ping" back as "pong".
    client_pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        Box::pin(async move {
            let dc_for_send = dc.clone();
            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let dc = dc_for_send.clone();
                Box::pin(async move {
                    if msg.data.as_ref() == b"ping" {
                        let _ = dc.send(&Bytes::from_static(b"pong")).await;
                    }
                })
            }));
        })
    }));

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
    // the server side has already completed its half of the
    // handshake and is also Connected (or about to be). The
    // server's PC state is not exposed on `WebrtcBridge`'s public
    // API; polling the client side is sufficient for a loopback
    // test where both peers run in the same process.
    client
        .wait_until_connected(Duration::from_secs(20))
        .await
        .expect("client PC did not reach Connected");

    // ── Server-side encoder pipeline + pumps ────────────────────
    //
    // 64x64 keeps openh264 cheap in debug builds; the test only
    // cares that packets flow, not that the resolution is realistic.
    let encoder = H264Encoder::new(64, 64).expect("encoder init");
    let source = SyntheticFrameSource::new(64, 64);
    let (frame_tx, frame_rx) = mpsc::channel(32);
    let _enc_handle = EncoderTask::spawn(encoder, source, frame_tx, server_enc_rx, 30);
    let _video_pump = server.spawn_video_pump(frame_rx);
    let _audio_pump = server.spawn_synthetic_audio_pump();

    // ── Control-DC ping/pong ────────────────────────────────────
    //
    // The DC's `Open` state lags the PC reaching `Connected` —
    // SCTP association setup happens after DTLS, and the bridge
    // does not expose the DC's ready-state through its API. Poll
    // by retrying `send_control` until it succeeds (or we time
    // out). Each retry sleeps 50 ms; 5 s total is plenty on
    // loopback (the 3e unit test settled in ~200 ms).
    let mut server_rx = server.control_rx().expect("server control_rx");
    let send_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match server.send_control(b"ping").await {
            Ok(()) => break,
            Err(e) => {
                if std::time::Instant::now() >= send_deadline {
                    panic!("server failed to send ping within 5s; last error: {}", e);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    let pong = tokio::time::timeout(Duration::from_secs(3), server_rx.recv())
        .await
        .expect("pong did not arrive in time")
        .expect("server rx closed");
    assert_eq!(pong, b"pong", "expected pong reply");

    // ── Let video and audio flow for ~3 seconds ─────────────────
    //
    // Thresholds err on the side of "the pipeline is alive": at
    // 30 fps video + 50 fps audio we expect ~90 video and ~150
    // audio packets in 3 s, but debug-build openh264 in Docker is
    // slower than wall-clock (Phase 2 step 2d saw ~52 frames in
    // 3 s instead of 90), and the very first packets of an access
    // unit can be dropped before the SRTP context fully arms.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let v = video_count.load(Ordering::Relaxed);
    let a = audio_count.load(Ordering::Relaxed);
    eprintln!(
        "loopback: received {} video RTP packets, {} audio RTP packets",
        v, a
    );
    assert!(v >= 10, "expected >=10 video packets, got {}", v);
    assert!(a >= 5, "expected >=5 audio packets, got {}", a);

    // ── Cleanup ─────────────────────────────────────────────────
    server.close().await.expect("server close");
    client.close().await.expect("client close");
}
