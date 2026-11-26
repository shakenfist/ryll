//! Full in-process loopback integration test.
//!
//! Two peers run in the same process. The "server" peer uses
//! [`WebrtcBridge`] (the production type) and runs the H.264 video
//! pump (driven by a real `H264Encoder` + `EncoderTask` +
//! `SyntheticFrameSource`) and the synthetic Opus audio pump. The
//! "client" peer is a `TestPeer`: the bridge's API is shaped for the
//! *server* role (sending video + audio, owning the control DC), so
//! to verify "incoming RTP packets" and the ping/pong round-trip we
//! drive the client side directly.
//!
//! The first test asserts:
//! * >= 10 video RTP packets received within ~3 seconds.
//! * >= 5 audio RTP packets received within ~3 seconds.
//! * Round-trip on the control DC: server sends "ping", the client
//!   replies "pong", server's `control_rx` delivers it.
//! * `on_data_channel` never fires on the client. That is the
//!   observable signature of out-of-band negotiation — neither end
//!   announces the control channel in band — and it is what tells
//!   the reader *why* the ping/pong above failed when it does.
//!
//! The tests below it narrow the client's codec set: one offers a
//! reduced-but-workable set and still expects media, the other offers
//! no H.264 at all and expects the encoder task to retire rather than
//! spin. The last two stop short of media and assert on the answer
//! SDP: what the bridge advertises, and that both browsers' H.264
//! profiles still negotiate. Each carries its own doc comment; this
//! list is a summary of the first test, not an inventory of the file.
//!
//! No browser involved; SDP exchange is direct between the two
//! peers. ICE uses host-only candidates (empty `ice_servers`).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use shakenfist_spice_renderer::{EncoderControl, EncoderTask, H264Encoder, SyntheticFrameSource};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use shakenfist_spice_webrtc::test_client::TestPeer;
use shakenfist_spice_webrtc::{WebrtcBridge, WebrtcBridgeConfig};

/// Appended to a control-channel failure message when the client saw
/// an in-band datachannel announcement.
///
/// Out-of-band negotiation sends no DCEP open, so `on_data_channel`
/// firing means the pairing fell back to in band and the two ends are
/// on different streams. That is a specific, actionable cause for what
/// otherwise reads as an unexplained timeout.
fn in_band_hint(in_band_channel: &AtomicBool) -> &'static str {
    if in_band_channel.load(Ordering::Relaxed) {
        " -- and on_data_channel fired, so the control channel was announced in band \
         rather than negotiated out of band, putting the two ends on different streams"
    } else {
        ""
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_video_audio_datachannel() {
    // ── Server: production WebrtcBridge ──────────────────────────
    let (server_enc_tx, server_enc_rx) = mpsc::channel::<EncoderControl>(4);
    let server = WebrtcBridge::new(WebrtcBridgeConfig::for_tests(server_enc_tx))
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
    // offer. The `on_track` wiring below is specific to this test, so
    // it is supplied to the builder: 0.20 hands the event handler to
    // the builder before the peer connection exists, so there is no
    // post-construction registration to reach through `TestPeer::pc()`
    // for. That is also the ordering an on-track hook always needed —
    // a track that arrives before its handler is installed fires
    // nothing. The datachannel side is handled after `build()`
    // instead; see the echo below for why that is both possible and
    // necessary now.
    //
    // Counters for incoming RTP packets, by track kind. Created
    // before the peer so they can be cloned into the builder hooks
    // below.
    let video_count = Arc::new(AtomicUsize::new(0));
    let audio_count = Arc::new(AtomicUsize::new(0));

    // Set if `on_data_channel` ever fires; asserted still false at the
    // end. See the hook below.
    let in_band_channel = Arc::new(AtomicBool::new(false));

    let client = TestPeer::builder()
        .seed_control_channel()
        .on_data_channel_hook({
            // This is a *diagnostic*, not the detection, and the
            // distinction is worth stating because it is easy to
            // mistake for a stronger guard than it is.
            //
            // The detection is the ping/pong below. A webrtc-rs
            // regression that stopped honouring `negotiated` puts the
            // two ends on different streams -- RFC 8832 section 6
            // gives the DTLS client even ids and the server odd ones
            // -- and the pong never arrives. Verified by mutation:
            // dropping `negotiated` from both `attach_tracks_and_
            // control_dc` and `TestPeerBuilder::build` fails this test
            // on the pong timeout.
            //
            // What it fails *with*, though, is "pong did not arrive in
            // time", which says nothing about why. The stream id
            // cannot be asserted directly -- `webrtc`'s `DataChannel`
            // trait exposes `id()`, an internal handle, and never
            // gained the `stream_id()` that `rtc::RTCDataChannel` has
            // -- but an in-band channel does announce itself with a
            // DCEP open, and that announcement arrives here. So the
            // flag turns an opaque timeout into a named cause, and is
            // asserted on its own account at the end for the case
            // where a channel is announced in band and the round-trip
            // somehow survives it.
            let in_band_channel = in_band_channel.clone();
            Box::new(move |dc: Arc<dyn DataChannel>| {
                let in_band_channel = in_band_channel.clone();
                Box::pin(async move {
                    in_band_channel.store(true, Ordering::Relaxed);
                    eprintln!(
                        "loopback: unexpected in-band datachannel {:?}",
                        dc.label().await
                    );
                })
            })
        })
        .on_track_hook({
            let video_count = video_count.clone();
            let audio_count = audio_count.clone();
            // on_track: spawn a per-track reader loop that increments
            // the appropriate counter for each RTP packet the track
            // yields.
            //
            // The read loop runs in a `tokio::spawn`-ed task rather
            // than directly inside the hook body. webrtc-rs awaits
            // the returned future before firing on_track for the
            // *next* track, so a long-lived poll loop inside the hook
            // would pin the driver's event loop on the first track
            // (audio) and prevent on_track from ever firing for
            // video. Spawning lets the hook return immediately and
            // both kinds receive packets. This reasoning is unchanged
            // from 0.17 — 0.20's driver loop awaits handler methods
            // inline too — but the mechanics are not: `read_rtp()` is
            // gone, and a remote track is now polled for events of
            // which RTP packets are one variant.
            Box::new(move |track: Arc<dyn TrackRemote>| {
                let video_count = video_count.clone();
                let audio_count = audio_count.clone();
                Box::pin(async move {
                    // `kind()` is async in 0.20. Awaiting it here is
                    // fine: once per track, not once per packet.
                    let counter = match track.kind().await {
                        RtpCodecKind::Video => video_count,
                        RtpCodecKind::Audio => audio_count,
                        // `RtpCodecKind` is `#[non_exhaustive]` from 0.21.
                        _ => return,
                    };
                    tokio::spawn(async move {
                        while let Some(event) = track.poll().await {
                            if matches!(event, TrackRemoteEvent::OnRtpPacket(_)) {
                                counter.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                })
            })
        })
        .build()
        .await
        .expect("client peer");

    // ── Client-side control echo ────────────────────────────────
    //
    // The echo runs on the client's *own* control channel — the one
    // `seed_control_channel()` created — not on one delivered by
    // `on_data_channel`. Both ends create their end out of band on
    // the same fixed stream id (`CONTROL_DC_STREAM_ID`), so the two
    // are one channel: no DCEP open is sent for it and
    // `on_data_channel` never fires here — which the hook installed
    // above turns into an assertion rather than a claim. See
    // `TestPeer::control_channel`.
    //
    // This is also how the real browser client behaves:
    // `ryll/src/web/assets/app.js` creates one negotiated `control`
    // channel on that same id, hangs `onmessage` off it, and registers
    // no `ondatachannel` handler at all. So the echo below exercises
    // the production data path rather than a test-only one.
    //
    // Polling only starts here, after `build()` returned — which on
    // 0.20 is safe in a way a callback registration would not be:
    // events queue in the channel's buffer until something polls them,
    // so nothing is lost by attaching late.
    let client_dc = client
        .control_channel()
        .expect("control channel requested above")
        .clone();
    let _echo = tokio::spawn(async move {
        while let Some(event) = client_dc.poll().await {
            match event {
                DataChannelEvent::OnMessage(msg) if msg.data.as_ref() == b"ping" => {
                    let _ = client_dc.send(BytesMut::from(&b"pong"[..])).await;
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
    });

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
    let encoder = H264Encoder::new(64, 64, 30).expect("encoder init");
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
    // loopback (the datachannel unit test settled in ~200 ms).
    let mut server_rx = server.control_rx().expect("server control_rx");
    let send_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match server.send_control(b"ping").await {
            Ok(()) => break,
            Err(e) => {
                if std::time::Instant::now() >= send_deadline {
                    panic!(
                        "server failed to send ping within 5s; last error: {}{}",
                        e,
                        in_band_hint(&in_band_channel)
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    let pong = match tokio::time::timeout(Duration::from_secs(3), server_rx.recv()).await {
        Ok(Some(pong)) => pong,
        Ok(None) => panic!("server rx closed{}", in_band_hint(&in_band_channel)),
        Err(_) => panic!(
            "pong did not arrive in time{}",
            in_band_hint(&in_band_channel)
        ),
    };
    assert_eq!(pong, b"pong", "expected pong reply");

    // ── Let video and audio flow for ~3 seconds ─────────────────
    //
    // Thresholds err on the side of "the pipeline is alive": at
    // 30 fps video + 50 fps audio we expect ~90 video and ~150
    // audio packets in 3 s, but debug-build openh264 in Docker is
    // slower than wall-clock (~52 frames in 3 s instead of 90
    // when measured), and the very first packets of an access
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

    // Both ends negotiate the control channel out of band, so nothing
    // should have announced itself in band -- even in the case where
    // the round-trip above succeeded anyway. Checked here rather than
    // earlier because the ping/pong proves the SCTP association is up
    // and carrying the channel, which is what makes "no DCEP open
    // arrived" mean "none was sent" rather than "not yet".
    assert!(
        !in_band_channel.load(Ordering::Relaxed),
        "on_data_channel fired: the control channel was announced in \
         band, so it is not the out-of-band channel this test claims \
         to exercise"
    );

    // ── Cleanup ─────────────────────────────────────────────────
    server.close().await.expect("server close");
    client.close().await.expect("client close");
}

/// Media still flows when the client offers a codec set that does not
/// include the payload types the bridge registered.
///
/// `loopback_video_audio_datachannel` above cannot catch this. Its
/// `TestPeer` offers webrtc-rs's default codecs, numbered the way the
/// bridge's own entries are, so the payload type the bridge stamps is
/// negotiated and accepted no matter where the number came from. Real
/// browsers number their codecs however they like. The core remaps
/// each offered codec onto the bridge's entry it matched, and 0.20's
/// `write_rtp` then rejects any packet whose payload type is not on
/// the resulting list.
///
/// So this offers what Firefox offers, and numbers it the way Firefox
/// does — 126 for H.264 and 109 for Opus, neither of which is a number
/// the bridge registers — and asserts packets still arrive. While the
/// bridge registered several H.264 entries, stamping a constant failed
/// this test with zero video packets while every other test in the
/// suite stayed green, which is exactly the failure mode worth a
/// dedicated test; with one entry it holds the remapping down instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_media_flows_when_client_offers_a_narrow_codec_set() {
    let (server_enc_tx, server_enc_rx) = mpsc::channel::<EncoderControl>(4);
    let server = WebrtcBridge::new(WebrtcBridgeConfig::for_tests(server_enc_tx))
        .await
        .expect("server bridge");

    let video_count = Arc::new(AtomicUsize::new(0));
    let audio_count = Arc::new(AtomicUsize::new(0));

    let client = TestPeer::builder()
        // The seed channel is still required: without an m=application
        // section in the offer the SCTP association never opens and the
        // handshake stalls. See the test above.
        .seed_control_channel()
        .offer_only_h264_fmtp(
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
            126,
            109,
        )
        .on_track_hook({
            let video_count = video_count.clone();
            let audio_count = audio_count.clone();
            // Spawns rather than looping inline, for the reason given
            // in the test above: the hook is awaited in the driver loop.
            Box::new(move |track: Arc<dyn TrackRemote>| {
                let video_count = video_count.clone();
                let audio_count = audio_count.clone();
                Box::pin(async move {
                    let counter = match track.kind().await {
                        RtpCodecKind::Video => video_count,
                        RtpCodecKind::Audio => audio_count,
                        // `RtpCodecKind` is `#[non_exhaustive]` from 0.21.
                        _ => return,
                    };
                    tokio::spawn(async move {
                        while let Some(event) = track.poll().await {
                            if matches!(event, TrackRemoteEvent::OnRtpPacket(_)) {
                                counter.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                })
            })
        })
        .build()
        .await
        .expect("client peer");

    let final_offer_sdp = client.offer_and_gather().await.expect("client offer");
    let answer_sdp = server
        .accept_offer(final_offer_sdp)
        .await
        .expect("server accept");
    client
        .set_remote_answer(answer_sdp)
        .await
        .expect("client rsd");

    client
        .wait_until_connected(Duration::from_secs(20))
        .await
        .expect("client PC did not reach Connected");

    let encoder = H264Encoder::new(64, 64, 30).expect("encoder init");
    let source = SyntheticFrameSource::new(64, 64);
    let (frame_tx, frame_rx) = mpsc::channel(32);
    let _enc_handle = EncoderTask::spawn(encoder, source, frame_tx, server_enc_rx, 30);
    let _video_pump = server.spawn_video_pump(frame_rx);
    let _audio_pump = server.spawn_synthetic_audio_pump();

    tokio::time::sleep(Duration::from_secs(3)).await;

    let v = video_count.load(Ordering::Relaxed);
    let a = audio_count.load(Ordering::Relaxed);
    eprintln!(
        "narrow-codec loopback: received {} video RTP packets, {} audio RTP packets",
        v, a
    );
    assert!(
        v >= 10,
        "expected >=10 video packets with a narrow codec offer, got {} — the pump is probably \
         stamping a payload type that was not negotiated",
        v
    );
    assert!(a >= 5, "expected >=5 audio packets, got {}", a);

    server.close().await.expect("server close");
    client.close().await.expect("client close");
}

/// A viewer that offers no H.264 gets no video RTP at all — and still
/// gets its audio.
///
/// This is issue #290's regression test, and the negative twin of
/// `loopback_media_flows_when_client_offers_a_narrow_codec_set` above.
/// Before the fix the pump kept encoding and writing at the frame
/// rate for output the sender discarded, logging an unthrottled
/// `ERROR` per packet from inside webrtc-rs; the log became unusable
/// and the CPU was spent producing nothing. The trap this used to walk
/// into was that VP8 *is* in webrtc-rs's default codec set, so a video
/// codec negotiated successfully and only ryll's own encoder — H.264
/// and nothing else — made it unusable. The bridge no longer registers
/// the default set, so a VP8-only offer now settles on no video codec
/// at all; either way the question is "did *H.264* survive", not "was
/// a codec negotiated".
///
/// The load-bearing assertion is that the *encoder* stops. Counting
/// received video packets does not discriminate: before the fix the
/// pump wrote at the frame rate and webrtc-rs rejected every packet
/// at the sender, so the far end saw nothing either way — which is
/// exactly why #290 was invisible to the test suite. What changed is
/// the work: the bridge now asks the encoder to stop, so the task
/// retires instead of encoding into a discard. Awaiting its handle
/// is that, observed.
///
/// Audio still flowing is the guard against over-fixing it: the
/// failure is video-specific, and a session that went silent too
/// would be a worse bug than the one being fixed.
///
/// The viewer-facing half of #289 cannot be asserted here — the
/// message is ryll's, not this crate's, because the control
/// datachannel's protocol lives in `ryll/src/web/control.rs`. See
/// `hello_is_answered_with_the_no_video_notice` there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_video_stops_when_the_client_offers_no_h264() {
    let (server_enc_tx, server_enc_rx) = mpsc::channel::<EncoderControl>(4);
    let server = WebrtcBridge::new(WebrtcBridgeConfig::for_tests(server_enc_tx))
        .await
        .expect("server bridge");

    let video_count = Arc::new(AtomicUsize::new(0));
    let audio_count = Arc::new(AtomicUsize::new(0));

    let client = TestPeer::builder()
        .seed_control_channel()
        // Firefox's payload numbers for VP8 and Opus.
        .offer_no_h264(120, 109)
        .on_track_hook({
            let video_count = video_count.clone();
            let audio_count = audio_count.clone();
            Box::new(move |track: Arc<dyn TrackRemote>| {
                let video_count = video_count.clone();
                let audio_count = audio_count.clone();
                Box::pin(async move {
                    let counter = match track.kind().await {
                        RtpCodecKind::Video => video_count,
                        RtpCodecKind::Audio => audio_count,
                        // `RtpCodecKind` is `#[non_exhaustive]` from 0.21.
                        _ => return,
                    };
                    tokio::spawn(async move {
                        while let Some(event) = track.poll().await {
                            if matches!(event, TrackRemoteEvent::OnRtpPacket(_)) {
                                counter.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                })
            })
        })
        .build()
        .await
        .expect("client peer");

    let final_offer_sdp = client.offer_and_gather().await.expect("client offer");
    let answer_sdp = server
        .accept_offer(final_offer_sdp)
        .await
        .expect("server accept");

    // The answer says there is no video, rather than naming a codec
    // the bridge cannot produce. The bridge registers H.264 and Opus
    // only, so this offer leaves its video sender with no codec at
    // all; `accept_offer` withdraws the track, which is what lets the
    // media section be rejected with port 0 (RFC 9429 §5.3.1) instead
    // of failing the answer outright. Getting this wrong is not a
    // cosmetic SDP difference: it is the whole session, since a
    // browser with no H.264 would get a 400 from `POST /offer` and no
    // audio, input or explanation.
    assert!(
        answer_sdp.lines().any(|l| l.starts_with("m=video 0 ")),
        "the answer should reject the video m-line:\n{answer_sdp}"
    );

    client
        .set_remote_answer(answer_sdp)
        .await
        .expect("client rsd");

    client
        .wait_until_connected(Duration::from_secs(20))
        .await
        .expect("client PC did not reach Connected");

    // The bridge knows immediately, and this is the fact ryll reads
    // to decide whether to tell the viewer.
    assert!(
        !server.video_negotiated(),
        "the bridge should have reported no usable video codec after an offer with no H.264"
    );

    let encoder = H264Encoder::new(64, 64, 30).expect("encoder init");
    let source = SyntheticFrameSource::new(64, 64);
    let (frame_tx, frame_rx) = mpsc::channel(32);
    let enc_handle = EncoderTask::spawn(encoder, source, frame_tx, server_enc_rx, 30);
    let _video_pump = server.spawn_video_pump(frame_rx);
    let _audio_pump = server.spawn_synthetic_audio_pump();

    // The `EncoderControl::Stop` was queued during `accept_offer`,
    // before this task existed; the task checks its control channel
    // at the start of every tick, so it reads it on the first one and
    // returns. Generous timeout because a debug-build openh264 in
    // Docker is slower than wall-clock, and the task finishes any
    // encode already in flight before it stops.
    let stopped = tokio::time::timeout(Duration::from_secs(5), enc_handle).await;
    assert!(
        stopped.is_ok(),
        "the encoder was still running 5 s after a negotiation that produced no usable video \
         codec — it is encoding frames that can only be discarded"
    );

    // Same window the two tests above use to accumulate packets, so
    // "none arrived" means the same thing here as ">= 10 arrived"
    // does there.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let v = video_count.load(Ordering::Relaxed);
    let a = audio_count.load(Ordering::Relaxed);
    eprintln!(
        "no-h264 loopback: received {} video RTP packets, {} audio RTP packets",
        v, a
    );
    // Weaker than it looks -- see the note above -- but it is the
    // user-visible outcome, and a future change that made the far end
    // start receiving something here would be worth knowing about.
    assert_eq!(
        v, 0,
        "expected no video packets when no H.264 was negotiated, got {}",
        v
    );
    assert!(
        a >= 5,
        "expected >=5 audio packets, got {} — stopping video must not stop audio",
        a
    );

    server.close().await.expect("server close");
    client.close().await.expect("client close");
}

/// `a=rtpmap` lines from an SDP, lowercased.
fn rtpmap_lines(sdp: &str) -> Vec<String> {
    sdp.lines()
        .filter(|l| l.starts_with("a=rtpmap:"))
        .map(|l| l.to_ascii_lowercase())
        .collect()
}

/// The answer advertises only what the pumps can send.
///
/// The client offers the broad default set a browser offers, so
/// anything in the answer beyond H.264 and Opus is something this
/// bridge chose to advertise and will never send: VP8, VP9, AV1
/// and HEVC have no encoder behind them, an RTX codec promises
/// retransmission the bridge does not do (a browser that NACKs
/// gets silence), and G.722, PCMU and PCMA are not what the SPICE
/// playback channel hands over.
///
/// The RTCP feedback types stay, and are asserted here so that
/// narrowing the codec set does not quietly take them with it:
/// `nack pli` is how a browser asks for a keyframe, which is the
/// one feedback request this bridge is in a position to answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_answer_advertises_only_what_the_bridge_can_send() {
    let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
    let bridge = WebrtcBridge::new(WebrtcBridgeConfig::for_tests(tx))
        .await
        .expect("bridge");
    let client = TestPeer::builder()
        .seed_control_channel()
        .build()
        .await
        .expect("client peer");
    let offer_sdp = client.offer_and_gather().await.expect("client offer");

    // Guard the premise: a client that offered a narrow set could
    // not tell a narrow answer from an echo of its own offer.
    let offered = rtpmap_lines(&offer_sdp);
    assert!(
        offered.iter().any(|l| l.contains("vp8")),
        "the test client should offer the broad set a browser does:\n{offer_sdp}"
    );

    let answer_sdp = bridge.accept_offer(offer_sdp).await.expect("accept offer");
    let answered = rtpmap_lines(&answer_sdp);

    assert!(
        answered.iter().any(|l| l.contains("h264")),
        "the answer must still advertise H.264:\n{answer_sdp}"
    );
    assert!(
        answered.iter().any(|l| l.contains("opus")),
        "the answer must still advertise Opus:\n{answer_sdp}"
    );
    for unsendable in ["vp8", "vp9", "av1", "h265", "rtx", "g722", "pcmu", "pcma"] {
        assert!(
            !answered.iter().any(|l| l.contains(unsendable)),
            "the answer advertises {unsendable}, which the bridge never sends:\n{answer_sdp}"
        );
    }
    // RTX also shows up as an `apt=` fmtp and an FID ssrc-group,
    // which is what a browser keys retransmission off.
    assert!(
        !answer_sdp.contains("apt="),
        "the answer still carries an RTX association:\n{answer_sdp}"
    );
    assert!(
        !answer_sdp.contains("ssrc-group:FID"),
        "the answer still carries an RTX ssrc group:\n{answer_sdp}"
    );
    // The profile in the answer is the one the encoder emits.
    // Before the MediaEngine was narrowed, the default PT 102
    // entry won and the answer said `42001f` while openh264 sent
    // baseline `42e01f`.
    assert!(
        answer_sdp.contains("profile-level-id=42e01f"),
        "the answer should name the profile the encoder emits:\n{answer_sdp}"
    );
    // `nack`, `nack pli` and `transport-cc` are still advertised,
    // by the interceptor chain rather than by the codec
    // registration — see `register_sendable_codecs`.
    assert!(
        answer_sdp.contains("nack pli"),
        "the interceptors' feedback should survive the narrower codec set:\n{answer_sdp}"
    );

    client.close().await.expect("client close");
    bridge.close().await.expect("bridge close");
}

/// Both profile-level-ids browsers actually offer still negotiate
/// a video codec against the single entry the bridge registers.
///
/// This is what the narrowed MediaEngine rests on. Chrome offers
/// `42001f` and Firefox `42e01f`; the bridge registers `42e01f`
/// alone, and `rtc` matches the other by mime type because the
/// fmtp lines differ. Were that not so, one of the two browsers
/// would get a session where everything except the picture works
/// — so this is the test that says whether one entry is enough.
///
/// The payload types are the browsers' own numbers, neither of
/// which the bridge registers for both cases, so a match here is
/// negotiation rather than coincidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_browser_h264_profiles_negotiate_video() {
    for (profile, client_pt) in [("42001f", 102u8), ("42e01f", 126u8)] {
        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig::for_tests(tx))
            .await
            .expect("bridge");
        let client = TestPeer::builder()
            .seed_control_channel()
            .offer_only_h264_fmtp(
                &format!(
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}"
                ),
                client_pt,
                109,
            )
            .build()
            .await
            .expect("client peer");
        let offer_sdp = client.offer_and_gather().await.expect("client offer");
        let answer_sdp = bridge.accept_offer(offer_sdp).await.expect("accept offer");

        assert!(
            bridge.video_negotiated(),
            "a viewer offering only {profile} must still get a video codec:\n{answer_sdp}"
        );
        assert!(
            rtpmap_lines(&answer_sdp).iter().any(|l| l.contains("h264")),
            "the answer to a {profile}-only offer must advertise H.264:\n{answer_sdp}"
        );

        client.close().await.expect("client close");
        bridge.close().await.expect("bridge close");
    }
}
