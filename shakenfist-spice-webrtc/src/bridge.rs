//! [`WebrtcBridge`] wraps a [`PeerConnection`] together with a
//! video track, an audio track, and a control datachannel.
//!
//! Phase 3 step 3b implements [`WebrtcBridge::new`] and
//! [`WebrtcBridge::accept_offer`]; 3c adds the video pump; 3d adds
//! the synthetic Opus audio pump; 3e adds the datachannel
//! send/recv ([`WebrtcBridge::send_control`],
//! [`WebrtcBridge::control_rx`]); 3f adds the in-process loopback
//! integration test.
//!
//! ## Codec registration
//!
//! webrtc-rs's [`MediaEngine::register_default_codecs`] registers
//! Opus by default, but H.264 registration depends on the build
//! features enabled in the `webrtc` crate. To make H.264
//! advertisement deterministic regardless of feature flags, this
//! module always registers H.264 manually with a profile / level
//! that matches what openh264 emits at the renderer layer
//! (`profile-level-id=42e01f`, baseline level 3.1, packetization
//! mode 1). See RFC 6184 §8.1 for the SDP fmtp line semantics.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use shakenfist_spice_renderer::{EncodedFrame, EncoderControl};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;

use crate::bind_addrs::host_udp_bind_addrs;
use crate::sticky::StickySignal;

// The protocol-level types come from `rtc`, the sans-io core, because
// `webrtc` is only a thin async shim over it and deliberately does not
// re-export it (`webrtc-0.20.2/src/lib.rs:112-125`). Splitting the
// imports this way is not a choice: `write_rtp` takes an
// `rtc::rtp::Packet`, and an `rtp` 0.17 `Packet` is not a different
// version of that type, it is a different type entirely. Note the
// module is `codec`, singular, where the old crate had `codecs`.
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::codec::opus::OpusPayloader;
use rtc::rtp::packetizer::Payloader;
use rtc::rtp::{Header, Packet};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::MediaStreamTrack;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer,
    RTCPeerConnectionState, RTCSessionDescription, Registry,
};

/// Payload type we register H.264 under in the MediaEngine, and the
/// value the pumps stamp until negotiation replaces it.
///
/// This is a starting point, not the wire value. The payload type
/// actually sent is whatever the offer/answer settled on, resolved by
/// [`WebrtcBridge::resolve_negotiated_payload_types`] — see there for
/// why a constant is not good enough.
const H264_PAYLOAD_TYPE: u8 = 102;

/// The media stream both tracks belong to. Surfaces in the answer SDP
/// as the `msid` / `mslabel` value, so it is observable to the browser
/// and must not drift.
const STREAM_ID: &str = "ryll-spice";

/// Track ids, likewise observable in the SDP's `msid` and `label`
/// lines.
const VIDEO_TRACK_ID: &str = "video";
const AUDIO_TRACK_ID: &str = "audio";

/// The H.264 fmtp line we register and select on. Baseline profile,
/// level 3.1, packetization-mode 1 (FU-A fragmentation, RFC 6184 §5.4)
/// — matching what the renderer's openh264 wrapper emits.
const H264_FMTP_LINE: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

/// H.264 RTP clock rate per RFC 6184 / RFC 4566.
const VIDEO_CLOCK_RATE_HZ: u32 = 90_000;

/// Maximum RTP packet size handed to the H.264 payloader. 1200 is
/// a conservative default that fits inside typical browser path
/// MTU minus UDP+IP+SRTP overhead on a 1500-byte ethernet frame.
const VIDEO_MTU: usize = 1200;

/// Payload type Opus is registered under by webrtc-rs's
/// [`MediaEngine::register_default_codecs`] (which the bridge calls in
/// [`WebrtcBridge::new`]), and the value the audio pumps stamp until
/// negotiation replaces it.
///
/// Opus is far safer than H.264 here — there is exactly one Opus entry
/// in the MediaEngine, so any Opus the remote peer offers remaps onto
/// this number whatever it called it. It is still resolved from the
/// negotiated parameters rather than trusted, because "there is only
/// one entry today" is not a property this module controls.
const OPUS_PAYLOAD_TYPE: u8 = 111;

/// Opus RTP clock rate per RFC 7587 §4.1: always 48 kHz, even for
/// narrowband mono streams.
const AUDIO_SAMPLE_RATE_HZ: u32 = 48_000;

/// One Opus frame per 20 ms RTP packet — the WebRTC default and the
/// "standard" Opus frame size per RFC 6716 §2.1.4. 50 packets/s.
const AUDIO_FRAME_DURATION_MS: u64 = 20;

/// 48 kHz × 20 ms = 960 samples per Opus frame, mono.
const AUDIO_SAMPLES_PER_FRAME: usize =
    (AUDIO_SAMPLE_RATE_HZ as usize / 1000) * (AUDIO_FRAME_DURATION_MS as usize);

/// Output buffer size for the Opus encoder. Opus packets at 64 kbps
/// for 20 ms frames are well under 200 bytes, but the libopus API
/// wants a generously-sized scratch buffer; 1500 fits any
/// realistic configuration.
const AUDIO_OPUS_BUF_BYTES: usize = 1500;

/// Synthetic-tone frequency for the Phase 3 audio pump.
const AUDIO_TONE_HZ: f64 = 440.0;

/// Amplitude for the synthetic sine, expressed as a fraction of
/// `i16::MAX`. 0.3 leaves headroom and keeps the test tone at a
/// comfortable listening level.
const AUDIO_TONE_AMPLITUDE: f64 = 0.3;

/// How often a run of dropped messages gets a repeat log line, once
/// the first drop has already been logged. `BridgeEvents` uses
/// `try_send` on both its outgoing channels so a slow consumer can
/// never stall the peer connection's driver loop (see
/// [`BridgeEvents`]); a consumer that stays stalled at input-event
/// rates would otherwise produce one `warn!` per event. Logging the
/// first drop immediately, then every Nth after that, gives a stalled
/// consumer a periodic reminder in the logs without flooding them.
const LOG_EVERY_N_DROPS: u64 = 100;

/// The bridge's reaction to everything the peer connection tells it.
///
/// The shared state behind [`BridgeHandler`], which is what webrtc-rs
/// actually calls. Gathered into one type rather than written inline
/// because the closures this replaced were capturing four pieces of
/// shared state between them and the clone dance obscured what they
/// actually did.
///
/// Two of the three things phase 01 wired up survive as handler
/// methods, renamed: `on_state_change` is dispatched from
/// [`PeerConnectionEventHandler::on_connection_state_change`], and
/// `on_ice_gathering_state_change` now takes an
/// [`RTCIceGatheringState`] rather than an `RTCIceGathererState`. The
/// third moved: datachannel messages stopped being callbacks
/// altogether in 0.20, so [`Self::on_control_message`] survives as a
/// function while its callers are the spawned [`run_dc_pump`] loops.
///
/// # Handler methods must not block — but not every method here is a
/// handler
///
/// 0.20 awaits handler methods inline in the peer connection's driver
/// event loop (`webrtc-0.20.2/src/peer_connection/driver.rs:653-681`),
/// which is the same task that services ICE, DTLS, SCTP and RTP. An
/// await inside one of those stalls the whole connection.
///
/// The rule is about the *dispatch path*, not about this type. Only
/// the methods reached from [`BridgeHandler`] run inline:
/// [`Self::on_state_change`] and
/// [`Self::on_ice_gathering_state_change`]. `on_state_change`
/// therefore uses `try_send` and degrades to a counted, rate-limited
/// drop rather than waiting on a slow encoder — see
/// [`LOG_EVERY_N_DROPS`].
///
/// [`Self::on_control_message`] is not a handler. Datachannel
/// messages stopped being callbacks in 0.20, so it is called from the
/// spawned [`run_dc_pump`] loops, where an await parks one poll loop
/// and nothing else. It awaits, on purpose; see the method for why
/// dropping input events is worse than back-pressure. Getting this
/// distinction wrong is how a keystroke goes missing.
///
/// See `docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md`
/// step 1e, and step 2b of the phase-02 plan for the `try_send`
/// change.
struct BridgeEvents {
    /// Ask the encoder for an IDR whenever a viewer attaches.
    encoder_control: mpsc::Sender<EncoderControl>,
    /// Raised once when the PC reaches a terminal state. Sticky so
    /// waiters that subscribe after the death still return; see
    /// [`StickySignal`].
    dead: Arc<StickySignal>,
    /// Fan-in for control-datachannel messages, from either the DC
    /// this bridge created or one the remote peer opened.
    incoming_tx: mpsc::Sender<Vec<u8>>,
    /// Latest peer connection state. Shadowed here because
    /// `RTCPeerConnection::connection_state` does not survive the
    /// 0.20 port, while this callback does.
    state: Arc<Mutex<RTCPeerConnectionState>>,
    /// Raised once when ICE gathering completes. Sticky for the same
    /// reason as `dead`: a late `accept_offer` would otherwise wait
    /// forever on a notification that already happened.
    gathered: Arc<StickySignal>,
    /// Count of keyframe requests dropped because `encoder_control`
    /// was full — the encoder is not draining it fast enough.
    /// Read/written with `Ordering::Relaxed`: this is a log-cadence
    /// counter, not a synchronisation point, so it needs no ordering
    /// guarantee beyond atomicity. See [`LOG_EVERY_N_DROPS`].
    dropped_keyframe_requests: AtomicU64,
    /// Join handles for the datachannel poll loops, shared with the
    /// [`WebrtcBridge`] that owns them so [`WebrtcBridge::close`] can
    /// abort any that are still running. Written here by
    /// [`PeerConnectionEventHandler::on_data_channel`], one entry per
    /// datachannel the remote peer opens; `new` adds the one for the
    /// control DC we create ourselves.
    ///
    /// A plain `std::sync::Mutex` because the guard is only ever held
    /// across a `push` or a `drain` and never across an await — which
    /// matters here more than usual, since one of the writers runs
    /// inline in the driver event loop.
    dc_pumps: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl BridgeEvents {
    /// Record a dropped message on `counter` and decide whether this
    /// particular drop should be logged.
    ///
    /// Returns the new running total on the first drop and on every
    /// `LOG_EVERY_N_DROPS`th drop after that, `None` otherwise. Callers
    /// use the total to say how many messages have been lost so far,
    /// not just that one more was.
    fn note_drop(counter: &AtomicU64) -> Option<u64> {
        let total = counter.fetch_add(1, Ordering::Relaxed) + 1;
        (total == 1 || total.is_multiple_of(LOG_EVERY_N_DROPS)).then_some(total)
    }

    /// Peer connection state transition.
    ///
    /// Two jobs. On `Connected`, ask the encoder for a fresh IDR so
    /// the viewer can decode immediately rather than waiting for the
    /// next periodic keyframe. On a terminal state, signal `dead` so
    /// the server-side reaper can tear the bridge and encoder down
    /// proactively.
    async fn on_state_change(&self, state: RTCPeerConnectionState) {
        // `into_inner` on poison: the critical section is a single
        // assignment of a `Copy` enum, so a panic can never leave the
        // shadow inconsistent — recovering the value is strictly
        // better than silently dropping the write, which would leave
        // state readers polling a value that stopped updating with no
        // hint as to why.
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;

        match state {
            // `try_send`, not `send().await`: this method is awaited
            // inline in webrtc-rs's peer-connection driver loop (see
            // `BridgeEvents`), so it must never block on a consumer. `encoder_control` is bounded
            // capacity 4 in the test suite, and the production
            // encoder task is expected to drain it promptly, but a
            // slow encoder must degrade the keyframe request, not the
            // whole connection.
            RTCPeerConnectionState::Connected => {
                match self
                    .encoder_control
                    .try_send(EncoderControl::RequestKeyframe)
                {
                    Ok(()) => {
                        tracing::debug!("WebrtcBridge: requested keyframe on Connected");
                    }
                    // The receiver was dropped — same failure and
                    // wording as the pre-`try_send` code path.
                    Err(err @ TrySendError::Closed(_)) => {
                        tracing::warn!(
                            error = %err,
                            "WebrtcBridge: failed to request keyframe on Connected",
                        );
                    }
                    // The channel is full: the encoder is behind and
                    // did not drain the last request in time. Dropping
                    // this one silently would leave a viewer looking
                    // at a stale or corrupt frame until the next
                    // periodic IDR, which nobody would be able to
                    // explain from logs alone, so this is `warn!`
                    // rather than `debug!`. Rate-limited via
                    // `note_drop` so a stuck encoder does not turn
                    // into a log line per `Connected` transition.
                    Err(TrySendError::Full(_)) => {
                        if let Some(dropped) = Self::note_drop(&self.dropped_keyframe_requests) {
                            tracing::warn!(
                                dropped,
                                "WebrtcBridge: encoder_control channel full, dropped keyframe \
                                 request — encoder is not keeping up, viewer may see a stale \
                                 or corrupt frame until the next periodic IDR",
                            );
                        }
                    }
                }
            }
            // The guard has a deliberate side effect: `raise()`
            // raises the dead signal on every terminal transition,
            // and returns true only for the first, so subsequent
            // transitions (e.g. Disconnected → Closed) do not re-log.
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed
                if self.dead.raise() =>
            {
                tracing::info!(
                    ?state,
                    "WebrtcBridge: PC reached terminal state, signalling dead",
                );
            }
            _ => {}
        }
    }

    /// ICE gathering state transition.
    ///
    /// Raises the sticky `gathered` signal on `Complete`, which is
    /// what `accept_offer` waits on before reading the local
    /// description. Replaces `gathering_complete_promise()`, which
    /// does not exist in webrtc-rs 0.20.
    ///
    /// Ordering is safe on 0.20: each candidate is pushed into the core
    /// before the completion sentinel, the sentinel is what moves
    /// gathering state to `Complete`, and `local_description()`
    /// re-renders from the live ICE agent on every call rather than
    /// returning a string frozen at `set_local_description` time. So
    /// "await Complete, then read the description" cannot observe a
    /// short answer.
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state != RTCIceGatheringState::Complete {
            return;
        }
        if self.gathered.raise() {
            tracing::debug!("WebrtcBridge: ICE gathering complete");
        }
    }

    /// A message arrived on a control datachannel — either the one
    /// this bridge created or one the remote peer opened. Both fan in
    /// here so `control_rx()` sees messages from either direction.
    ///
    /// `send().await`, not `try_send`, and deliberately unlike
    /// `on_state_change`.
    ///
    /// The two are not in the same position despite living on the
    /// same type. `on_state_change` really is dispatched inline from
    /// [`BridgeHandler::on_connection_state_change`], so an await
    /// there stalls the driver loop. This method is not: its only
    /// caller is [`run_dc_pump`], which both
    /// [`WebrtcBridge::new`] and [`BridgeHandler::on_data_channel`]
    /// `tokio::spawn`. Awaiting here parks that one datachannel's
    /// poll loop and nothing else.
    ///
    /// Which makes back-pressure the right answer rather than a
    /// luxury. This channel carries the browser's keyboard, mouse and
    /// resize events over an ordered, reliable datachannel; dropping
    /// one is not a lost frame that the next frame supersedes. A
    /// dropped key-up leaves a modifier stuck down in the guest, a
    /// symptom a viewer can only report as "my keyboard went weird".
    /// Parking the pump instead lets SCTP flow control push back on
    /// the browser, which is what an ordered reliable channel is for.
    async fn on_control_message(&self, data: Vec<u8>, source: &'static str) {
        // The only failure left is a dropped receiver, which is not a
        // back-pressure condition and cannot be waited out.
        if self.incoming_tx.send(data).await.is_err() {
            tracing::debug!(
                source,
                "WebrtcBridge: control_rx receiver dropped, message lost"
            );
        }
    }
}

/// What webrtc-rs 0.20 is actually handed: one event handler, supplied
/// to the builder before the peer connection exists, replacing the four
/// separate callback registrations 0.17 wanted.
///
/// A newtype over `Arc<BridgeEvents>` rather than an `impl` on
/// `BridgeEvents` itself, for one concrete reason:
/// [`PeerConnectionEventHandler::on_data_channel`] takes `&self`, but it
/// has to hand an *owned* `Arc<BridgeEvents>` to the poll task it spawns,
/// and there is no way to recover an `Arc` from a `&self`. Wrapping the
/// `Arc` is the smallest thing that supplies one, and it leaves
/// `BridgeEvents` a plain state struct that [`WebrtcBridge::new`] can
/// share directly with the control-DC pump it spawns itself.
struct BridgeHandler(Arc<BridgeEvents>);

#[async_trait::async_trait]
impl PeerConnectionEventHandler for BridgeHandler {
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        self.0.on_state_change(state).await;
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        self.0.on_ice_gathering_state_change(state).await;
    }

    /// A datachannel opened by the remote peer.
    ///
    /// This fires less often than it reads like it should. webrtc-rs
    /// 0.20 assigns a datachannel's SCTP stream id when the channel is
    /// created, from the DTLS role
    /// (`rtc-0.20.2/src/peer_connection/internal.rs:936-954`) — and
    /// before the handshake there is no role, so every channel created
    /// ahead of negotiation lands on stream 1. Our control DC is one of
    /// those and so is the browser's `control-seed`
    /// (`ryll/src/web/assets/app.js`), which makes them the same
    /// stream: the peer's channel is already in our id map when its
    /// DCEP open arrives, so the driver does not announce it
    /// (`webrtc-0.20.2/src/peer_connection/driver.rs:84-101`) and the
    /// remote's messages surface on *our* control channel instead,
    /// pumped as `local-dc`.
    ///
    /// What is left for this path is a channel the peer opens *after*
    /// negotiation, where the ids no longer collide. Keeping it costs
    /// one small handler; dropping it would silently discard those.
    ///
    /// Spawns a pump rather than polling inline: this method is awaited
    /// in the driver event loop, so looping on `poll()` here would wedge
    /// the connection permanently. The `Arc<dyn DataChannel>` is the only
    /// handle on the channel, so the spawned task owns it, and the join
    /// handle goes into the shared `dc_pumps` list for
    /// [`WebrtcBridge::close`] to abort.
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let events = self.0.clone();
        let handle = tokio::spawn(run_dc_pump(dc, events, "remote-dc"));
        // `unwrap_or_else(into_inner)` on poison for the same reason as
        // the state shadow: the guarded value is a plain Vec that a
        // panicking pusher cannot leave half-written, and dropping the
        // handle here would silently opt this pump out of cancellation.
        let mut pumps = self
            .0
            .dc_pumps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Drop the handles of pumps that have already exited. Nothing
        // else removes them — `close()` drains the list once, at the
        // end — so a peer that repeatedly opens and closes
        // post-negotiation channels would otherwise grow this vector
        // for the life of the bridge. Bounded in practice, unbounded
        // in principle.
        pumps.retain(|pump| !pump.is_finished());
        pumps.push(handle);
    }
}

/// Forward one datachannel's incoming messages to
/// [`BridgeEvents::on_control_message`] until the channel closes.
///
/// 0.20 has no `on_message` callback and no user-facing datachannel
/// type at all: a channel is an `Arc<dyn DataChannel>` whose events you
/// pull with `poll()`. One spawned task per channel is the shape every
/// shipped example uses
/// (`webrtc-0.20.2/examples/data-channels/data-channels.rs:65-110`), and
/// it is the only shape available given that a poll loop cannot run
/// inside a handler method without stalling the driver.
///
/// `source` distinguishes the datachannel this bridge created from one
/// the remote peer opened, purely for log attribution — both fan into
/// the same `control_rx()` channel.
///
/// Returns when `poll()` yields `None` (the peer connection closed and
/// dropped the event sender) or on `OnClose`. `tokio::spawn`, not the
/// webrtc `Runtime` handle the examples use, because we need the
/// `JoinHandle` for the close path and the handler already runs on
/// tokio under the default `runtime-tokio` feature.
async fn run_dc_pump(dc: Arc<dyn DataChannel>, events: Arc<BridgeEvents>, source: &'static str) {
    while let Some(event) = dc.poll().await {
        match event {
            DataChannelEvent::OnMessage(msg) => {
                events.on_control_message(msg.data.to_vec(), source).await;
            }
            DataChannelEvent::OnOpen => {
                tracing::debug!(source, "WebrtcBridge: control datachannel open");
            }
            DataChannelEvent::OnClose => {
                tracing::debug!(source, "WebrtcBridge: control datachannel closed");
                break;
            }
            // OnError / OnClosing / the buffered-amount thresholds. We
            // set no thresholds and do not implement send back-pressure,
            // so none of them is actionable here.
            _ => {}
        }
    }
}

/// Configuration for [`WebrtcBridge::new`].
pub struct WebrtcBridgeConfig {
    /// ICE servers (STUN). Empty by default; the LAN-only assumption
    /// (master plan Resolution §3) means STUN is often unnecessary,
    /// but operators that need it can populate this list with
    /// `stun:host:port` URLs.
    pub ice_servers: Vec<String>,
    /// Sender for [`EncoderControl`] so the bridge can ask the
    /// encoder for an IDR keyframe whenever a viewer reaches
    /// `Connected`.
    pub encoder_control: mpsc::Sender<EncoderControl>,
}

impl WebrtcBridgeConfig {
    /// Build a config with no ICE servers and the given encoder
    /// control channel. Equivalent to setting `ice_servers = vec![]`.
    pub fn new(encoder_control: mpsc::Sender<EncoderControl>) -> Self {
        Self {
            ice_servers: Vec::new(),
            encoder_control,
        }
    }
}

/// One-PC, one-viewer WebRTC bridge between the SPICE-side encoder
/// pipeline and a browser-side `RTCPeerConnection`.
///
/// Phase 3 step 3b ships [`WebrtcBridge::new`],
/// [`WebrtcBridge::accept_offer`], and [`WebrtcBridge::close`]; 3c
/// adds [`WebrtcBridge::spawn_video_pump`]; 3d adds
/// [`WebrtcBridge::spawn_synthetic_audio_pump`]; 3e adds
/// [`WebrtcBridge::send_control`] and [`WebrtcBridge::control_rx`].
pub struct WebrtcBridge {
    /// `Arc<dyn PeerConnection>` rather than a concrete type because
    /// `PeerConnectionBuilder::build` hands back an unnameable
    /// `impl PeerConnection` — the interceptor chain's type parameter
    /// stays on the builder and never escapes it, which is what lets us
    /// keep the registry type out of this struct.
    pc: Arc<dyn PeerConnection>,
    video_track: Arc<TrackLocalStaticRTP>,
    audio_track: Arc<TrackLocalStaticRTP>,
    control_dc: Arc<dyn DataChannel>,
    // Retained so the Sender is kept alive for diagnostics; the
    // on-state handler keeps its own clone. Prefixed with `_` to
    // signal intentional non-use without suppressing via attribute.
    _encoder_control: mpsc::Sender<EncoderControl>,
    /// Receiver for incoming control-DC messages. Take it once
    /// via [`WebrtcBridge::control_rx`]. Wrapped in
    /// `Mutex<Option<...>>` because `WebrtcBridge` is shared via
    /// `Arc` but the receiver can only be consumed once.
    incoming_control: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    /// Raised once when the underlying `RTCPeerConnection` reaches a
    /// terminal state (`Failed`, `Disconnected`, or `Closed`).
    /// External waiters can clone this via
    /// [`WebrtcBridge::dead_signal`] or await it directly via
    /// [`WebrtcBridge::wait_for_dead`]. Phase 6a wires this up so
    /// the server-side reaper (Phase 6b) can tear down the bridge
    /// and encoder when the browser disconnects. Sticky
    /// ([`StickySignal`]) so a waiter that subscribes after the
    /// bridge already died still returns.
    dead: Arc<StickySignal>,
    /// Latest peer connection state, shadowed by [`BridgeEvents`].
    /// Read by the `#[cfg(test)]` `connection_state` accessor rather
    /// than asking the peer connection, because
    /// `RTCPeerConnection::connection_state` does not survive the
    /// webrtc-rs 0.20 port.
    ///
    /// Only that accessor reads it, so outside a `cfg(test)` build of
    /// this crate the field is genuinely unused. It is kept
    /// unconditionally anyway: `BridgeEvents` writes to it on every
    /// transition regardless, and making the field itself conditional
    /// would mean two shapes of `WebrtcBridge` to keep in step.
    #[cfg_attr(not(test), allow(dead_code))]
    state: Arc<Mutex<RTCPeerConnectionState>>,
    /// Raised once when ICE gathering completes; sticky so a late
    /// waiter returns immediately. Awaited by
    /// [`WebrtcBridge::wait_for_gathering`].
    gathered: Arc<StickySignal>,
    /// Join handles for the datachannel poll loops, shared with
    /// [`BridgeEvents`] so that both the control DC created here and
    /// every DC the remote peer opens land in one list.
    ///
    /// This crate owns its own task hygiene on 0.20: dropping a peer
    /// connection *detaches* its driver task rather than stopping it,
    /// so a bridge that is dropped without `close()` would leak the
    /// driver plus one task per datachannel. [`WebrtcBridge::close`]
    /// aborts these; see there for why the abort is a backstop rather
    /// than the primary mechanism.
    dc_pumps: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// The SSRCs advertised for the two tracks, handed to the pumps so
    /// the RTP headers they stamp match. Not cosmetic: 0.20's
    /// `write_rtp` rejects any packet whose SSRC is not one of the
    /// track's. See where they are chosen in [`WebrtcBridge::new`].
    video_ssrc: u32,
    audio_ssrc: u32,
    /// The payload types the pumps stamp, published here by
    /// [`WebrtcBridge::resolve_negotiated_payload_types`] once the
    /// offer/answer has settled.
    ///
    /// Shared rather than passed by value because of ordering: the
    /// pumps are spawned before `accept_offer` runs (see
    /// `ryll/src/web/signalling.rs`), so the value does not exist yet
    /// when they start. They read it per packet.
    video_payload_type: Arc<AtomicU8>,
    audio_payload_type: Arc<AtomicU8>,
    /// Set by [`WebrtcBridge::close`] so the `Drop` backstop below
    /// knows the teardown already happened and stays out of the way.
    closed: AtomicBool,
}

/// Best-effort cleanup for a bridge that is dropped without
/// [`WebrtcBridge::close`].
///
/// On 0.20 dropping the peer connection detaches its driver task
/// rather than stopping it, so a forgotten `close()` leaks the driver,
/// the UDP sockets bound for ICE, and one task per datachannel — and
/// leaks them silently, which is what makes this worth a destructor
/// rather than a rule to remember. `close()` remains the real path: it
/// is awaited, it propagates errors, and it is what the tests
/// exercise. This only catches the paths that forgot.
///
/// `Drop` cannot await, so the close is spawned. If there is no
/// runtime to spawn on there is nothing useful left to do, and the
/// leak is reported rather than hidden.
impl Drop for WebrtcBridge {
    fn drop(&mut self) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }

        let pumps = {
            let mut guard = self
                .dc_pumps
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *guard)
        };
        for pump in pumps {
            pump.abort();
        }

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let pc = self.pc.clone();
                handle.spawn(async move {
                    if let Err(e) = pc.close().await {
                        tracing::debug!(
                            "webrtc: background close of a dropped-but-not-closed bridge \
                             errored: {}",
                            e
                        );
                    }
                });
            }
            Err(_) => {
                tracing::warn!(
                    "webrtc: bridge dropped without close() and outside a tokio runtime; \
                     its driver task and UDP sockets leak for the life of the process"
                );
            }
        }
    }
}

impl WebrtcBridge {
    /// Build the peer connection, register H.264 + Opus codecs,
    /// create the video, audio, and control transports, and arm
    /// the on-connected handler that requests a keyframe whenever
    /// a viewer attaches.
    pub async fn new(config: WebrtcBridgeConfig) -> Result<Self> {
        let mut media_engine = MediaEngine::default();
        // register_default_codecs is conditional on webrtc's H.264
        // feature being enabled. Register Opus + H.264 explicitly
        // so the answer SDP advertises both regardless.
        media_engine.register_default_codecs()?;
        register_h264(&mut media_engine)?;

        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

        // Bridge lifecycle signal, raised once when the PC reaches a
        // terminal state (`Failed` / `Disconnected` / `Closed`). The
        // reaper task in Phase 6b waits on this to tear down the
        // bridge and encoder when the browser disconnects. Sticky
        // (see `StickySignal`) so late subscribers — callers that
        // begin awaiting after the PC already died — return
        // immediately.
        let dead = Arc::new(StickySignal::new());

        // Control DC incoming message fan-in: both the DC this bridge
        // created and any DC the remote peer opens push raw bytes
        // onto one bounded mpsc channel. The consumer takes the
        // Receiver once via `control_rx()`.
        //
        // Two sources fan in here:
        //   1. This bridge's own `control_dc` (created below) — used
        //      for `send_control`, and in practice also where the
        //      remote peer's messages arrive, because its channel and
        //      ours share an SCTP stream id (see
        //      `BridgeHandler::on_data_channel`).
        //   2. A DC the remote peer opens after negotiation, delivered
        //      through `on_data_channel`.
        let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(64);

        let state = Arc::new(Mutex::new(RTCPeerConnectionState::New));
        let gathered = Arc::new(StickySignal::new());
        let dc_pumps = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(BridgeEvents {
            encoder_control: config.encoder_control.clone(),
            dead: dead.clone(),
            incoming_tx,
            state: state.clone(),
            gathered: gathered.clone(),
            dropped_keyframe_requests: AtomicU64::new(0),
            dc_pumps: dc_pumps.clone(),
        });

        // Which local addresses to bind the ICE sockets to. 0.20 makes
        // this the caller's problem — the bound addresses are the only
        // input to host-candidate generation and nothing downstream
        // filters them — so an empty list is not a degraded bridge, it
        // is a bridge that can only ever advertise candidates no
        // browser will use. Fail here rather than hand back something
        // that passes every test we have and reaches nobody. See
        // `crate::bind_addrs` and Decision 4 of the phase-02 plan.
        let udp_addrs = host_udp_bind_addrs();
        if udp_addrs.is_empty() {
            return Err(anyhow!(
                "no bindable network interface: either enumeration failed or every address \
                 this host reports is loopback, unspecified, or IPv6 link-local — check for an \
                 earlier `host_udp_bind_addrs` warning to tell which. Either way the peer \
                 connection could only offer ICE candidates no remote peer can reach"
            ));
        }

        // One handler replaces 0.17's four separate callback
        // registrations, and it has to be supplied *before* the peer
        // connection exists — hence all the state above being built
        // first. `.with_handler` is the one mandatory builder call:
        // `build()` returns an error without it.
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(
                    RTCConfigurationBuilder::new()
                        .with_ice_servers(
                            config
                                .ice_servers
                                .iter()
                                .map(|url| RTCIceServer {
                                    urls: vec![url.clone()],
                                    ..Default::default()
                                })
                                .collect(),
                        )
                        .build(),
                )
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_handler(Arc::new(BridgeHandler(events.clone())))
                .with_udp_addrs(udp_addrs)
                .build()
                .await?,
        );

        // Video and audio tracks. 0.20 builds a track from a whole
        // `MediaStreamTrack`, whose `codings` carry the SSRC and the
        // codec that 0.17 chose on our behalf.
        //
        // The codings are a *selector, not an override*. Matching is
        // mime_type plus fmtp, falling back to mime_type alone
        // (`rtc-0.20.2/src/rtp_transceiver/rtp_sender/rtp_codec.rs:139-163`),
        // and the codec that ends up negotiated is cloned from the
        // MediaEngine's registered entry — so the payload type and fmtp
        // in the SDP come from `register_h264` and
        // `register_default_codecs`, never from the values below. Do
        // not try to fix an fmtp mismatch by editing this side; it is
        // the side that is ignored.
        //
        // The SSRCs are chosen here rather than left as `None` for the
        // core to fill in. `write_rtp` on 0.20 does not rewrite the
        // packet header the way 0.17's did — it *validates* it, and
        // rejects any packet whose SSRC is not one of the track's
        // (`rtc-0.20.2/src/rtp_transceiver/rtp_sender/mod.rs:368-374`).
        // So the pumps have to stamp the same value the SDP
        // advertises, which means something has to know it. Choosing
        // it at construction, and handing it to the pumps, is what
        // every shipped example does ("rewrite SSRC to match what we
        // advertised in the SDP",
        // `examples/rtp-to-webrtc/rtp-to-webrtc.rs:243`). This is not a
        // new generator: the pumps each called `rand::random()` for
        // their own SSRC before, and the core would have done the same
        // — the change is only that both ends now agree on the answer.
        //
        // Getting it wrong is silent. Every packet is dropped at the
        // sender with a rejection the pumps log at debug, so the
        // symptom is a connected viewer watching nothing.
        // Distinct and non-zero. Both tracks are BUNDLE-ed onto one
        // transport, and RFC 8843 §9.2 requires SSRCs to be unique
        // across a BUNDLE group — a receiver demultiplexing by SSRC
        // would misroute or drop one of the two streams. Zero is
        // excluded because it reads as "unset" in enough tooling to be
        // worth never emitting. Before the port a collision was
        // invisible, because the core rewrote the header; now the
        // value is what the SDP advertises and what `write_rtp`
        // validates against, so it is load-bearing. The odds are
        // ~2^-32 per bridge, which is precisely why this is a guard
        // rather than something anyone would ever reproduce.
        let video_ssrc = nonzero_random_ssrc();
        let mut audio_ssrc = nonzero_random_ssrc();
        while audio_ssrc == video_ssrc {
            audio_ssrc = nonzero_random_ssrc();
        }

        // Attaching the tracks and the control datachannel is
        // fallible, and the peer connection already exists with its
        // driver task running and its UDP sockets bound. A bare `?`
        // here would drop that `Arc` and — on 0.20, where dropping
        // detaches the driver rather than stopping it — leak the
        // driver and one socket per interface, with `WebrtcBridge`
        // never constructed so its `Drop` backstop cannot help. So
        // the fallible part is one call with one error path.
        let (video_track, audio_track, control_dc) =
            match attach_tracks_and_control_dc(&pc, video_ssrc, audio_ssrc).await {
                Ok(attached) => attached,
                Err(e) => {
                    if let Err(ce) = pc.close().await {
                        tracing::debug!(
                            "webrtc: closing a half-built peer connection errored: {}",
                            ce
                        );
                    }
                    return Err(e);
                }
            };

        // The DC we created gets the same pump as any remote one; see
        // `run_dc_pump` for why a task rather than a callback. Its
        // handle joins the same list `on_data_channel` writes to, so
        // `close` has one place to look.
        {
            let handle = tokio::spawn(run_dc_pump(control_dc.clone(), events.clone(), "local-dc"));
            dc_pumps
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(handle);
        }

        let incoming_control = Mutex::new(Some(incoming_rx));

        Ok(Self {
            pc,
            video_track,
            audio_track,
            control_dc,
            _encoder_control: config.encoder_control,
            incoming_control,
            dead,
            state,
            gathered,
            dc_pumps,
            video_ssrc,
            audio_ssrc,
            video_payload_type: Arc::new(AtomicU8::new(H264_PAYLOAD_TYPE)),
            audio_payload_type: Arc::new(AtomicU8::new(OPUS_PAYLOAD_TYPE)),
            closed: AtomicBool::new(false),
        })
    }

    /// Wait until the bridge's underlying `RTCPeerConnection`
    /// reaches a terminal state (`Failed`, `Disconnected`, or
    /// `Closed`).
    ///
    /// The signal is sticky: a caller that invokes this after the
    /// PC has already died returns immediately, and any number of
    /// waiters — concurrent or sequential — all resolve. See
    /// [`StickySignal`] for the semantics and the lost-wakeup
    /// reasoning.
    pub async fn wait_for_dead(&self) {
        self.dead.wait().await;
    }

    /// Return a clone of the [`StickySignal`] that is raised once
    /// when the bridge's PC reaches a terminal state. Used by the
    /// server-side reaper (Phase 6b) so it can wait on the signal
    /// without holding the `bridge_slot` lock or borrowing `&self`
    /// across an `.await`. `handle.wait().await` is equivalent to
    /// [`WebrtcBridge::wait_for_dead`], including the
    /// late-subscriber fast-path.
    pub fn dead_signal(&self) -> Arc<StickySignal> {
        self.dead.clone()
    }

    /// Accept a remote SDP offer, generate our answer, and wait for
    /// ICE gathering to complete so the returned answer carries every
    /// candidate we know about (no trickle ICE for the MVP).
    ///
    /// A `WebrtcBridge` handles exactly one offer/answer exchange;
    /// renegotiation (including ICE restart) requires a new bridge.
    /// The gathering signal this waits on is sticky and never resets,
    /// so a second call would read the local description immediately,
    /// before a re-gathering round had repopulated its candidates.
    /// Production honours this by constructing a fresh bridge per
    /// `POST /offer`.
    pub async fn accept_offer(&self, offer_sdp: String) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer_sdp)?;
        self.pc.set_remote_description(offer).await?;

        // Must be after `set_remote_description` and cannot be any
        // earlier: that call is what intersects the offer with our
        // MediaEngine and fixes the payload types.
        self.resolve_negotiated_payload_types().await;

        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer).await?;

        self.wait_for_gathering().await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("local description missing after ICE gathering"))?;
        Ok(local_desc.sdp)
    }

    /// Read the payload types the offer/answer settled on out of the
    /// senders, and publish them where the pumps will see them.
    ///
    /// A payload type is a *negotiated* value, not a constant, and 0.20
    /// is the release where that stops being a technicality. `write_rtp`
    /// validates the header's payload type against the sender's
    /// negotiated codec list and rejects anything else with
    /// `ErrRTPTransceiverCodecUnsupported`
    /// (`rtc-0.20.2/src/rtp_transceiver/rtp_sender/mod.rs:388-409`);
    /// 0.17 silently overwrote the field instead, which is why stamping
    /// a constant worked for as long as it did.
    ///
    /// That list is the offer intersected with our MediaEngine, with
    /// each match remapped onto *our* payload type
    /// (`rtc-0.20.2/src/rtp_transceiver/internal.rs:299-383`), so which
    /// number survives depends on what the browser offered.
    /// `register_default_codecs` registers five H.264 entries at
    /// different profile-level-ids, and browsers do not all offer the
    /// same ones: Chrome offers `42001f`, which matches our PT 102,
    /// while Firefox offers only `42e01f`, which matches PT 125. Stamp
    /// 102 for a Firefox viewer and every video packet is rejected at
    /// the sender — a connected viewer looking at a black screen, with
    /// the explanation only at `trace` level inside the library.
    ///
    /// Audio has one Opus entry and so cannot drift today, but it is
    /// resolved the same way rather than trusted.
    ///
    /// Failure to resolve leaves the registered default in place and
    /// warns. It does not fail the offer: a bridge that answers and
    /// sends nothing is worth more diagnostically than one that refuses
    /// to answer, and the warning names the symptom.
    async fn resolve_negotiated_payload_types(&self) {
        let mut video: Option<u8> = None;
        let mut audio: Option<u8> = None;

        for sender in self.pc.get_senders().await {
            let params = match sender.get_parameters().await {
                Ok(params) => params,
                Err(e) => {
                    tracing::warn!("webrtc: reading sender parameters failed: {}", e);
                    continue;
                }
            };
            let codecs = &params.rtp_parameters.codecs;
            video = video.or_else(|| negotiated_h264_payload_type(codecs));
            audio = audio.or_else(|| negotiated_payload_type(codecs, MIME_TYPE_OPUS));
        }

        match video {
            Some(pt) => {
                self.video_payload_type.store(pt, Ordering::Relaxed);
                tracing::debug!("webrtc: negotiated H.264 payload type {}", pt);
            }
            None => tracing::warn!(
                "webrtc: no H.264 payload type negotiated; the video pump will keep stamping {} \
                 and the sender will reject every packet — the viewer will see no video",
                H264_PAYLOAD_TYPE
            ),
        }

        match audio {
            Some(pt) => {
                self.audio_payload_type.store(pt, Ordering::Relaxed);
                tracing::debug!("webrtc: negotiated Opus payload type {}", pt);
            }
            None => tracing::warn!(
                "webrtc: no Opus payload type negotiated; the audio pump will keep stamping {} \
                 and the sender will reject every packet — the viewer will hear nothing",
                OPUS_PAYLOAD_TYPE
            ),
        }
    }

    /// Wait until ICE gathering has completed.
    ///
    /// Backed by [`BridgeEvents::on_ice_gathering_state_change`]
    /// rather than `RTCPeerConnection::gathering_complete_promise`,
    /// which does not exist in webrtc-rs 0.20. The sticky signal
    /// gives a late caller — one that arrives after gathering
    /// already finished — a fast-path return, and [`StickySignal`]
    /// closes the lost-wakeup window that 0.17's oneshot
    /// `gathering_complete_promise` never had; see its docs for the
    /// reasoning.
    async fn wait_for_gathering(&self) {
        self.gathered.wait().await;
    }

    /// Spawn the video pump task: consume [`EncodedFrame`]s from
    /// `rx`, payload each NAL via [`H264Payloader`], and write
    /// RTP packets to the bridge's video track. The marker bit
    /// is set on the last RTP packet of each access unit per
    /// RFC 6184 §5.1. The returned [`JoinHandle`] resolves when
    /// `rx` is closed (i.e. all senders dropped).
    ///
    /// `write_rtp` errors do not stop the loop — the remote
    /// peer may not have completed ICE/DTLS when the first
    /// frames arrive, so transient drops are expected and are
    /// logged at debug level.
    pub fn spawn_video_pump(&self, rx: mpsc::Receiver<EncodedFrame>) -> JoinHandle<Result<()>> {
        let track = self.video_track.clone();
        tokio::spawn(run_video_pump(
            rx,
            track,
            self.video_ssrc,
            self.video_payload_type.clone(),
        ))
    }

    /// Spawn the synthetic audio pump task: generate a 440 Hz sine
    /// wave at 48 kHz mono, encode it via Opus in 20 ms windows
    /// (960 samples per frame), payload via [`OpusPayloader`], and
    /// write RTP packets to the bridge's audio track at 50 fps.
    ///
    /// Phase 3 ships this synthetic path so the audio track is
    /// exercised in integration tests (3f); Phase 5e replaces it
    /// in production with [`Self::spawn_audio_pump`] which forwards
    /// real SPICE Opus packets. The synthetic pump is retained for
    /// tests and as a debugging aid.
    ///
    /// The pump runs forever — there is no natural stop condition
    /// because the synthetic source has no end-of-stream. Callers
    /// bound its lifetime by either dropping the
    /// [`JoinHandle`] / aborting it, or by tearing down the
    /// surrounding tokio runtime. `track.write_rtp` errors before
    /// the remote peer completes ICE/DTLS are logged at debug and
    /// do not stop the loop, mirroring the video pump's behaviour.
    pub fn spawn_synthetic_audio_pump(&self) -> JoinHandle<Result<()>> {
        let track = self.audio_track.clone();
        tokio::spawn(run_synthetic_audio_pump(
            track,
            self.audio_ssrc,
            self.audio_payload_type.clone(),
        ))
    }

    /// Spawn the real Opus passthrough pump (Phase 5e).
    ///
    /// Consumes `(opus_packet, samples_in_packet)` tuples from
    /// `rx`, where `opus_packet` is a single Opus packet as
    /// emitted by the SPICE playback channel and
    /// `samples_in_packet` is its 48 kHz sample duration (used
    /// to advance the RTP timestamp). Payloads with
    /// [`OpusPayloader`] (which is a one-to-one passthrough for
    /// the standard one-Opus-packet-per-RTP-packet framing per
    /// RFC 7587 §4.2), and writes the resulting RTP packet to
    /// the bridge's audio track.
    ///
    /// The caller is responsible for plugging the corresponding
    /// `mpsc::Sender` into the playback channel's
    /// `OpusPacketSink`. The pump exits cleanly when the channel
    /// closes (every sender dropped, e.g. when the active bridge
    /// is replaced by a fresh `/offer` and the previous viewer's
    /// sender is dropped).
    ///
    /// `track.write_rtp` errors before the remote peer completes
    /// ICE/DTLS are logged at debug and do not stop the loop,
    /// mirroring the video pump's behaviour.
    pub fn spawn_audio_pump(&self, rx: mpsc::Receiver<(Vec<u8>, u32)>) -> JoinHandle<Result<()>> {
        let track = self.audio_track.clone();
        tokio::spawn(run_audio_pump(
            rx,
            track,
            self.audio_ssrc,
            self.audio_payload_type.clone(),
        ))
    }

    /// Send a payload over the control datachannel. The DC is
    /// reliable + ordered; this is appropriate for inputs and
    /// cursor overlay updates (Phase 5).
    ///
    /// Returns an error if the underlying datachannel send fails
    /// (e.g. the channel is not yet open or the remote peer has
    /// closed it).
    pub async fn send_control(&self, payload: &[u8]) -> Result<()> {
        // `send` takes a `BytesMut` by value in 0.20 and returns
        // `Result<()>` rather than a byte count, so there is nothing
        // left to discard.
        self.control_dc
            .send(BytesMut::from(payload))
            .await
            .map_err(|e| anyhow!("control DC send: {}", e))
    }

    /// Take the receiver for incoming control-DC messages.
    ///
    /// Can only be called once per bridge; subsequent calls return
    /// `None`. The datachannel poll loops that feed this are spawned by
    /// [`WebrtcBridge::new`], so messages can start arriving before the
    /// first call; the channel is buffered (64 slots) precisely so an
    /// early message is not lost while a caller gets around to taking
    /// the receiver.
    pub fn control_rx(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.incoming_control
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    /// Close the underlying peer connection and stop the datachannel
    /// poll loops. Consumes `self` so callers cannot accidentally reuse
    /// a closed bridge.
    ///
    /// Closing must be explicit on 0.20: dropping a peer connection
    /// detaches its driver task rather than stopping it.
    ///
    /// Note that closing here does not reliably raise the `dead`
    /// signal. `close()` sets the driver's shutdown flag, and the
    /// driver checks that at the top of every loop iteration, so it
    /// usually exits before dispatching the `Closed` transition the
    /// core queued. That is fine for every caller we have — the reaper
    /// waits on `dead` to decide *whether* to close, and a bridge that
    /// has been closed has no one left to notify — but it means `dead`
    /// should be read as "the peer went away", not "the bridge is
    /// finished".
    ///
    /// Order matters. `pc.close()` first, because that is what makes
    /// each pump's `poll()` return `None` and lets it exit on its own —
    /// the abort below is a backstop for a pump that is wedged
    /// somewhere else, not the normal exit path. Aborting first would
    /// cancel a pump mid-`on_control_message` and drop a message that
    /// had already been received.
    ///
    /// Ordering and unconditionality are separable, though. The
    /// aborts run whether or not `pc.close()` succeeded, and the close
    /// result is propagated afterwards: returning early on a close
    /// error would leak exactly the tasks this method exists to reap.
    /// Every caller already treats that error as something to log and
    /// carry on from — `post_offer` and `run_bridge_reaper` both warn
    /// and continue — so the error branch was anticipated everywhere
    /// except in here.
    pub async fn close(self) -> Result<()> {
        let closed = self.pc.close().await;

        let pumps = {
            let mut guard = self
                .dc_pumps
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *guard)
        };
        for pump in pumps {
            pump.abort();
        }

        // Disarm the destructor only now that the work is done. This
        // is a cancellation point, not a formality: `close()` is
        // awaited inside an axum handler whose future is dropped when
        // the client disconnects, and inside `run_bridge_reaper`,
        // which `run_web` aborts at shutdown. Storing the flag first
        // would mean a `close()` cancelled anywhere above leaves a
        // bridge that `Drop` then declines to touch — the one path
        // where the backstop is guaranteed not to fire, which is the
        // opposite of what it exists for.
        //
        // Falling through to `Drop` on cancellation is safe because
        // both steps are idempotent: aborting a finished or
        // already-aborted handle is a no-op, and a second
        // `pc.close()` re-sets a shutdown flag that is already set.
        //
        // Not covered by a test, and deliberately not faked with one.
        // Reaching the window needs `pc.close()` to return `Pending`
        // at least once, and in-process — connected to a `TestPeer`
        // or not — it completes on the first poll, so the test passes
        // whichever order these two statements are in. A test that
        // cannot fail reads as coverage without being any. Keep the
        // store last on the strength of the reasoning above.
        self.closed.store(true, Ordering::SeqCst);

        Ok(closed?)
    }
}

/// Attach the two media tracks and the control datachannel to a
/// freshly built peer connection.
///
/// Split out of [`WebrtcBridge::new`] so that everything which can
/// fail after `PeerConnectionBuilder::build()` shares a single error
/// path. The caller closes the peer connection on `Err`; see there
/// for why dropping it instead would leak.
///
/// `add_track` fails with `ErrRTPTransceiverCodecUnsupported` if a
/// coding names a codec the MediaEngine does not carry — the drift
/// `h264_codec()` and `opus_codec()` exist to prevent, and one that
/// would be deterministic rather than intermittent if it ever
/// happened.
async fn attach_tracks_and_control_dc(
    pc: &Arc<dyn PeerConnection>,
    video_ssrc: u32,
    audio_ssrc: u32,
) -> Result<(
    Arc<TrackLocalStaticRTP>,
    Arc<TrackLocalStaticRTP>,
    Arc<dyn DataChannel>,
)> {
    let video_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        STREAM_ID.to_owned(),
        VIDEO_TRACK_ID.to_owned(),
        VIDEO_TRACK_ID.to_owned(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(video_ssrc),
                ..Default::default()
            },
            codec: h264_codec(),
            ..Default::default()
        }],
    )));
    pc.add_track(video_track.clone() as Arc<dyn TrackLocal>)
        .await?;

    let audio_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        STREAM_ID.to_owned(),
        AUDIO_TRACK_ID.to_owned(),
        AUDIO_TRACK_ID.to_owned(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(audio_ssrc),
                ..Default::default()
            },
            codec: opus_codec(),
            ..Default::default()
        }],
    )));
    pc.add_track(audio_track.clone() as Arc<dyn TrackLocal>)
        .await?;

    // Control datachannel. Ordered + reliable for input events
    // (Phase 5) and cursor overlay (Phase 5b).
    let control_dc = pc
        .create_data_channel(
            "control",
            // `ordered` is a plain `bool` in 0.20 (it was an
            // `Option<bool>`), and its `Default` is `true` rather
            // than the derived `false`. Stated explicitly anyway:
            // reliable + ordered is a property inputs depend on,
            // not something to inherit silently.
            Some(RTCDataChannelInit {
                ordered: true,
                max_retransmits: None,
                ..Default::default()
            }),
        )
        .await?;

    Ok((video_track, audio_track, control_dc))
}

/// Draw a random SSRC that is never zero.
///
/// See the SSRC discussion in [`WebrtcBridge::new`] for why the value
/// matters on 0.20 and why the caller also rejects a collision between
/// the two tracks.
fn nonzero_random_ssrc() -> u32 {
    loop {
        let candidate: u32 = rand::random();
        if candidate != 0 {
            return candidate;
        }
    }
}

/// Register H.264 with the MediaEngine. We pin profile-level-id to
/// `42e01f` (baseline profile, level 3.1) which matches what the
/// renderer's openh264 wrapper emits and what every browser decodes.
/// Packetization-mode 1 enables FU-A fragmentation per RFC 6184 §5.4.
///
/// This call is redundant on 0.20 and is kept deliberately.
/// `register_default_codecs` registers five H.264 entries
/// unconditionally (PT 102, 127, 125, 108, 123) and webrtc-rs drops
/// this one as a duplicate payload type — phase 01 asserted exactly
/// that in `register_h264_is_redundant_with_default_codecs`, and the
/// answer SDP is byte-identical with or without it. It stays because
/// it states our profile-level preference in one place: if the
/// defaults ever stop carrying H.264, or carry it at a profile the
/// renderer cannot emit, this is what turns that into a changed SDP
/// rather than a silent renegotiation onto VP8.
///
/// It is also why PT 102 negotiates `profile-level-id=42001f` while
/// the encoder emits `42e01f`: the default entry wins, and 0.20's
/// codec matching degrades an fmtp mismatch to a mime-type-only match
/// rather than an error
/// (`rtc-0.20.2/src/rtp_transceiver/rtp_sender/rtp_codec.rs:139-163`).
/// Browsers tolerate the constraint-set difference. The discrepancy
/// predates the port and is deliberately left alone.
pub(crate) fn register_h264(media_engine: &mut MediaEngine) -> Result<()> {
    let h264 = RTCRtpCodecParameters {
        rtp_codec: h264_codec(),
        payload_type: H264_PAYLOAD_TYPE,
    };
    media_engine.register_codec(h264, RtpCodecKind::Video)?;
    Ok(())
}

/// The payload type negotiated for `mime_type`, if the remote peer
/// accepted it at all.
///
/// Case-insensitive because the mime type arrives from the remote SDP,
/// where `video/H264` and `video/h264` are the same codec — the core
/// compares it the same way
/// (`rtc-0.20.2/src/rtp_transceiver/internal.rs:307`).
fn negotiated_payload_type(codecs: &[RTCRtpCodecParameters], mime_type: &str) -> Option<u8> {
    codecs
        .iter()
        .find(|codec| codec.rtp_codec.mime_type.eq_ignore_ascii_case(mime_type))
        .map(|codec| codec.payload_type)
}

/// The payload type to stamp on H.264 packets, preferring an entry
/// that negotiated packetization-mode 1 and, among those, one whose
/// profile matches what the encoder actually emits.
///
/// The packetization-mode preference is not cosmetic.
/// [`H264Payloader`] fragments NALs larger than [`VIDEO_MTU`] into
/// FU-A units, which is exactly what packetization-mode 1 permits and
/// mode 0 forbids (RFC 6184 §5.4, §6.2). If only a mode-0 entry
/// survives negotiation there is nothing useful to do about it here —
/// refusing to send guarantees a blank screen, whereas sending gets
/// the un-fragmented frames through — so this warns and uses it.
///
/// The profile preference breaks ties between mode-1 entries.
/// `register_default_codecs` registers baseline, constrained-baseline
/// and high-profile entries, and a browser may negotiate several, so
/// without a tie-break the choice falls to intersection order and can
/// name a profile we never produce. The renderer pins none, which
/// leaves openh264 at its own defaults: `iEntropyCodingModeFlag` is 0
/// (CAVLC) in `param_svc.h:164` and nothing in `EncoderConfig` or
/// `H264Encoder::new` overrides it, so `encoder_ext.cpp:662` resolves
/// `uiProfileIdc` to `PRO_BASELINE` — profile_idc `0x42`. Preferring a
/// `profile-level-id` that starts `42` therefore makes the payload
/// type we stamp agree with the bitstream we send.
///
/// This is a preference, not a filter. Browsers decode from the SPS
/// rather than the SDP profile, so a mismatch is unlikely to break
/// playback on its own; there is no case in which having no `42` entry
/// is a reason to send nothing.
fn negotiated_h264_payload_type(codecs: &[RTCRtpCodecParameters]) -> Option<u8> {
    let h264 = codecs.iter().filter(|codec| {
        codec
            .rtp_codec
            .mime_type
            .eq_ignore_ascii_case(MIME_TYPE_H264)
    });

    let mut fallback: Option<u8> = None;
    let mut off_profile: Option<u8> = None;
    for codec in h264 {
        if is_packetization_mode_1(&codec.rtp_codec.sdp_fmtp_line) {
            if is_baseline_profile(&codec.rtp_codec.sdp_fmtp_line) {
                return Some(codec.payload_type);
            }
            off_profile.get_or_insert(codec.payload_type);
        }
        fallback.get_or_insert(codec.payload_type);
    }

    if let Some(pt) = off_profile {
        tracing::debug!(
            "webrtc: no baseline-profile H.264 entry negotiated; using packetization-mode 1 \
             payload type {}, whose profile does not match the encoder's baseline output",
            pt
        );
        return Some(pt);
    }

    if let Some(pt) = fallback {
        tracing::warn!(
            "webrtc: negotiated H.264 payload type {} is packetization-mode 0; frames larger \
             than {} bytes need FU-A fragmentation and will not decode",
            pt,
            VIDEO_MTU
        );
    }
    fallback
}

/// Whether an SDP fmtp line declares packetization-mode 1.
///
/// RFC 6184 §8.1: the parameter is optional and defaults to 0, so an
/// absent or unparseable line is mode 0, not "unknown".
fn is_packetization_mode_1(sdp_fmtp_line: &str) -> bool {
    sdp_fmtp_line.split(';').any(|param| {
        let mut kv = param.splitn(2, '=');
        matches!(
            (kv.next().map(str::trim), kv.next().map(str::trim)),
            (Some("packetization-mode"), Some("1"))
        )
    })
}

/// Whether an SDP fmtp line declares a baseline-profile
/// `profile-level-id`, i.e. one whose profile_idc byte is `0x42`.
///
/// RFC 6184 §8.1 defines `profile-level-id` as exactly six hex digits:
/// profile_idc, profile_iop, level_idc. Only the first byte is
/// compared, so both plain baseline (`42001f`) and constrained
/// baseline (`42e01f`) match — they differ only in the constraint
/// flags of the second byte, and openh264's `PRO_BASELINE` output is
/// decodable by anything that offered either.
///
/// An absent, short or non-hex value is not baseline. Per §8.1 an
/// absent `profile-level-id` means `42000a` — which *is* baseline —
/// but that default only applies to a codec the remote actually
/// offered without the parameter, and treating a malformed line as a
/// match would let a garbled high-profile entry win the tie-break.
/// Returning false costs nothing: the caller falls back to the first
/// packetization-mode 1 entry either way.
fn is_baseline_profile(sdp_fmtp_line: &str) -> bool {
    sdp_fmtp_line.split(';').any(|param| {
        let mut kv = param.splitn(2, '=');
        match (kv.next().map(str::trim), kv.next().map(str::trim)) {
            (Some("profile-level-id"), Some(id)) => {
                id.len() == 6 && id.is_char_boundary(2) && id[..2].eq_ignore_ascii_case("42")
            }
            _ => false,
        }
    })
}

/// The H.264 codec description, shared by the MediaEngine registration
/// in [`register_h264`] and the video track's codings.
///
/// One function rather than two literals so the two cannot drift: if
/// the coding names a codec the MediaEngine does not have, `add_track`
/// fails outright with `ErrRTPTransceiverCodecUnsupported`.
fn h264_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: VIDEO_CLOCK_RATE_HZ,
        // Zero, not one: `channels` is an audio concept and RFC 4566's
        // rtpmap encoding parameter is omitted entirely for video.
        channels: 0,
        sdp_fmtp_line: H264_FMTP_LINE.to_owned(),
        rtcp_feedback: vec![],
    }
}

/// The Opus codec description for the audio track's codings.
///
/// Matches the entry `MediaEngine::register_default_codecs` installs at
/// payload type 111 — 48 kHz stereo per RFC 7587 §4.1, which is the
/// signalled rate regardless of the mono content we actually send. The
/// fmtp line is left empty on purpose: matching falls back to mime_type
/// alone, and the negotiated fmtp comes from the registered entry, so
/// repeating `minptime=10;useinbandfec=1` here would only create a
/// second copy to keep in step.
fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: AUDIO_SAMPLE_RATE_HZ,
        channels: 2,
        sdp_fmtp_line: String::new(),
        rtcp_feedback: vec![],
    }
}

/// Spawned by [`WebrtcBridge::spawn_video_pump`]. Owns the
/// receiver side of the encoder's [`EncodedFrame`] channel and a
/// clone of the bridge's video track. Iterates encoded frames,
/// strips Annex-B start codes, payloads each NAL via
/// [`H264Payloader`], and writes RTP packets to the track. The
/// marker bit is set on the last RTP packet of each access unit
/// per RFC 6184 §5.1.
///
/// `ssrc` must be the SSRC the track advertised (see
/// [`WebrtcBridge::new`]): 0.20's `write_rtp` validates the header it
/// is given rather than rewriting it, so a mismatch drops every packet.
/// `payload_type` is the same story for the other validated header
/// field, and is read per packet rather than captured because
/// negotiation resolves it after this task is already running — see
/// [`WebrtcBridge::resolve_negotiated_payload_types`].
///
/// Errors from `track.write_rtp` are logged at debug and the
/// loop continues — the receiver may not have negotiated DTLS
/// yet when the first frames arrive, so dropped packets early on
/// are normal.
async fn run_video_pump(
    mut rx: mpsc::Receiver<EncodedFrame>,
    track: Arc<TrackLocalStaticRTP>,
    ssrc: u32,
    payload_type: Arc<AtomicU8>,
) -> Result<()> {
    let mut payloader = H264Payloader::default();
    let mut sequence: u16 = rand::random();

    while let Some(frame) = rx.recv().await {
        // EncodedFrame::timestamp_us is microseconds; convert to
        // a 32-bit RTP timestamp at 90 kHz. Use u128 arithmetic
        // to avoid overflow during the multiply, then truncate.
        let rtp_ts = ((frame.timestamp_us as u128).saturating_mul(VIDEO_CLOCK_RATE_HZ as u128)
            / 1_000_000u128) as u32;

        // Read the negotiated payload type once per access unit, not
        // once per packet. `resolve_negotiated_payload_types` can
        // store a new value while this frame is being fragmented, and
        // an AU that went out under two payload types reads as two
        // streams to a receiver, which then reassembles neither. The
        // window is small — the pumps start at step 5 and the store
        // happens inside `accept_offer` at step 6, before DTLS is up,
        // so affected packets are dropped at the sender anyway — but
        // one load is no more work than several.
        let frame_payload_type = payload_type.load(Ordering::Relaxed);

        // Collect every RTP packet for this access unit so we can
        // set the marker bit on the last one only.
        let mut packets: Vec<Packet> = Vec::new();

        for annex_b_nal in &frame.nal_units {
            // Defensive: every NAL produced by H264Encoder is
            // 4-byte-start-code framed (Phase 2 step 2b); skip
            // anything too short to be a real NAL body.
            if annex_b_nal.len() < 5 {
                continue;
            }
            // Strip the 4-byte Annex-B start code.
            let raw_nal = &annex_b_nal[4..];

            let payloads = payloader
                .payload(VIDEO_MTU, &Bytes::copy_from_slice(raw_nal))
                .map_err(|e| anyhow!("H264Payloader failed: {}", e))?;

            // SPS (NAL type 7) and PPS (NAL type 8) produce empty
            // payload sets — they're cached and bundled as a
            // STAP-A on the next non-parameter NAL per RFC 6184.
            // Skip empty entries cleanly without special-casing.
            for payload in payloads {
                if payload.is_empty() {
                    continue;
                }
                let header = Header {
                    version: 2,
                    payload_type: frame_payload_type,
                    sequence_number: sequence,
                    timestamp: rtp_ts,
                    ssrc,
                    marker: false, // updated below for the last packet
                    ..Default::default()
                };
                packets.push(Packet { header, payload });
                sequence = sequence.wrapping_add(1);
            }
        }

        // Mark the last RTP packet of this access unit.
        if let Some(last) = packets.last_mut() {
            last.header.marker = true;
        }

        // Write to the track. Errors here are logged but do not
        // stop the pump — the remote side may not have completed
        // ICE/DTLS yet when the first frames arrive.
        for pkt in packets {
            if let Err(e) = track.write_rtp(pkt).await {
                tracing::debug!("video pump: write_rtp dropped packet: {}", e);
            }
        }
    }

    tracing::debug!("video pump: receiver closed, exiting");
    Ok(())
}

/// Spawned by [`WebrtcBridge::spawn_synthetic_audio_pump`]. Owns
/// a clone of the bridge's audio track. Generates a 440 Hz sine
/// wave at 48 kHz mono in 20 ms (960-sample) windows, encodes
/// each window via libopus (through the `opus` crate), payloads
/// with [`OpusPayloader`] (which is a passthrough for one
/// Opus-packet-per-RTP-packet framing per RFC 7587 §4.2), and
/// writes the resulting RTP packet to the track.
///
/// RTP timestamp arithmetic: Opus's RTP clock is the audio
/// sample rate (48 kHz, RFC 7587 §4.1) regardless of channel
/// count, so each 20 ms frame advances the timestamp by exactly
/// `AUDIO_SAMPLES_PER_FRAME` (960). The starting timestamp is
/// random per RFC 3550 §5.1 (we use 0 here for simplicity since
/// the SSRC is randomised; the receiver tracks deltas anyway).
///
/// `ssrc` and `payload_type` must match what the track advertised and
/// what negotiation settled on; see [`run_video_pump`].
///
/// `track.write_rtp` errors are logged at debug and the loop
/// continues — the receiver may not have negotiated DTLS yet
/// when the pump starts, so dropped packets early on are normal.
/// The loop never returns `Ok(())` on its own; it exits only
/// when the spawning task is aborted or the runtime shuts down.
async fn run_synthetic_audio_pump(
    track: Arc<TrackLocalStaticRTP>,
    ssrc: u32,
    payload_type: Arc<AtomicU8>,
) -> Result<()> {
    let mut encoder = opus::Encoder::new(
        AUDIO_SAMPLE_RATE_HZ,
        opus::Channels::Mono,
        // 440 Hz pure tone is "music"-like content; Audio is
        // the right application class. Voip would also work
        // but optimises for narrowband speech.
        opus::Application::Audio,
    )
    .map_err(|e| anyhow!("Opus encoder init failed: {}", e))?;
    // 64 kbps is generous for a mono test tone; keeps encoder
    // CPU low and packets compact (~160 bytes per 20 ms frame).
    encoder
        .set_bitrate(opus::Bitrate::Bits(64_000))
        .map_err(|e| anyhow!("Opus set_bitrate failed: {}", e))?;

    let mut payloader = OpusPayloader;
    let mut sequence: u16 = rand::random();
    let mut rtp_timestamp: u32 = 0;
    // Monotonic sample counter drives the sine-wave phase;
    // separate from rtp_timestamp because the latter wraps every
    // ~24 h while this is just a phase clock.
    let mut sample_clock: u64 = 0;

    let mut pcm: Vec<i16> = vec![0i16; AUDIO_SAMPLES_PER_FRAME];
    let mut opus_buf: Vec<u8> = vec![0u8; AUDIO_OPUS_BUF_BYTES];

    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(AUDIO_FRAME_DURATION_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let two_pi_f_over_sr =
        2.0 * std::f64::consts::PI * AUDIO_TONE_HZ / (AUDIO_SAMPLE_RATE_HZ as f64);
    let scale = i16::MAX as f64 * AUDIO_TONE_AMPLITUDE;

    loop {
        interval.tick().await;

        // Fill one frame's worth of 440 Hz sine at the current
        // phase. sample_clock is monotonic across frames so the
        // tone is phase-continuous between packets.
        for (i, slot) in pcm.iter_mut().enumerate() {
            let n = sample_clock + i as u64;
            let v = (two_pi_f_over_sr * n as f64).sin();
            *slot = (v * scale) as i16;
        }
        sample_clock = sample_clock.wrapping_add(AUDIO_SAMPLES_PER_FRAME as u64);

        // Encode 960 PCM samples to one Opus packet.
        let bytes_written = match encoder.encode(&pcm, &mut opus_buf) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("synthetic audio pump: Opus encode failed: {}", e);
                continue;
            }
        };
        if bytes_written == 0 {
            continue;
        }

        // Payload the Opus packet. OpusPayloader returns a single
        // Bytes equal to the input for any non-empty payload (it
        // does not fragment), so the inner loop runs exactly once
        // per encoded frame.
        let opus_packet = Bytes::copy_from_slice(&opus_buf[..bytes_written]);
        let payloads = payloader
            .payload(AUDIO_OPUS_BUF_BYTES, &opus_packet)
            .map_err(|e| anyhow!("OpusPayloader failed: {}", e))?;

        // One load per input packet rather than per output packet,
        // for the reason given in `run_video_pump`.
        let packet_payload_type = payload_type.load(Ordering::Relaxed);

        for payload in payloads {
            if payload.is_empty() {
                continue;
            }
            let header = Header {
                version: 2,
                payload_type: packet_payload_type,
                sequence_number: sequence,
                timestamp: rtp_timestamp,
                ssrc,
                marker: false,
                ..Default::default()
            };
            let pkt = Packet { header, payload };
            sequence = sequence.wrapping_add(1);
            if let Err(e) = track.write_rtp(pkt).await {
                tracing::debug!("synthetic audio pump: write_rtp dropped packet: {}", e);
            }
        }

        rtp_timestamp = rtp_timestamp.wrapping_add(AUDIO_SAMPLES_PER_FRAME as u32);
    }
}

/// Spawned by [`WebrtcBridge::spawn_audio_pump`]. Owns the
/// receiver side of the SPICE playback channel's pre-decode
/// tap and a clone of the bridge's audio track. Iterates
/// incoming `(opus_packet, samples_in_packet)` tuples,
/// payloads via [`OpusPayloader`] (a passthrough), and writes
/// RTP packets to the track. The RTP timestamp advances by
/// `samples_in_packet` per packet to match RFC 7587 §4.1's
/// 48 kHz audio clock — even when the SPICE server sends
/// shorter Opus frames (5.33 ms / 256 samples), the
/// timestamp delta tracks the actual content duration.
///
/// `ssrc` and `payload_type` must match what the track advertised and
/// what negotiation settled on; see [`run_video_pump`].
///
/// `track.write_rtp` errors are logged at debug and the loop
/// continues — the receiver may not have negotiated DTLS
/// yet when the first packets arrive, so dropped packets
/// early on are normal.
async fn run_audio_pump(
    mut rx: mpsc::Receiver<(Vec<u8>, u32)>,
    track: Arc<TrackLocalStaticRTP>,
    ssrc: u32,
    payload_type: Arc<AtomicU8>,
) -> Result<()> {
    let mut payloader = OpusPayloader;
    let mut sequence: u16 = rand::random();
    let mut rtp_timestamp: u32 = 0;

    while let Some((opus_packet, samples_in_packet)) = rx.recv().await {
        if opus_packet.is_empty() {
            continue;
        }
        let payload = Bytes::from(opus_packet);
        let payloads = payloader
            .payload(AUDIO_OPUS_BUF_BYTES, &payload)
            .map_err(|e| anyhow!("OpusPayloader failed: {}", e))?;

        // One load per input packet rather than per output packet,
        // for the reason given in `run_video_pump`.
        let packet_payload_type = payload_type.load(Ordering::Relaxed);

        for payload in payloads {
            if payload.is_empty() {
                continue;
            }
            let header = Header {
                version: 2,
                payload_type: packet_payload_type,
                sequence_number: sequence,
                timestamp: rtp_timestamp,
                ssrc,
                marker: false,
                ..Default::default()
            };
            let pkt = Packet { header, payload };
            sequence = sequence.wrapping_add(1);
            if let Err(e) = track.write_rtp(pkt).await {
                tracing::debug!("audio pump: write_rtp dropped packet: {}", e);
            }
        }
        // Advance the RTP timestamp by the actual sample duration
        // of this packet so the receiver's jitter buffer tracks
        // wall-clock correctly even for non-20 ms framings.
        rtp_timestamp = rtp_timestamp.wrapping_add(samples_in_packet);
    }

    tracing::debug!("audio pump: receiver closed, exiting");
    Ok(())
}

/// Test-only helpers that expose internals needed for driving the
/// client side of a two-bridge SDP exchange in unit tests.
#[cfg(test)]
impl WebrtcBridge {
    /// Create an SDP offer, set it as the local description, wait
    /// for ICE gathering to complete, and return the fully-resolved
    /// SDP string. Mirrors what a browser would do before sending
    /// its offer to the server.
    pub(crate) async fn create_offer_and_gather(&self) -> Result<String> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer).await?;
        self.wait_for_gathering().await;
        let local = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("local description missing after ICE gathering"))?;
        Ok(local.sdp)
    }

    /// Set the remote description from an SDP answer string,
    /// completing the SDP exchange on the client side.
    pub(crate) async fn set_remote_answer(&self, answer_sdp: String) -> Result<()> {
        let answer = RTCSessionDescription::answer(answer_sdp)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Return the most recent peer connection state observed by
    /// [`BridgeEvents::on_state_change`]. Used in tests to poll until
    /// both sides reach `Connected`.
    ///
    /// Reads the shadow rather than the peer connection: the
    /// inherent `RTCPeerConnection::connection_state` does not
    /// survive the webrtc-rs 0.20 port, whereas the state-change
    /// callback that feeds this does.
    pub(crate) fn connection_state(&self) -> RTCPeerConnectionState {
        // `into_inner` on poison for the same reason as the writer in
        // `BridgeEvents::on_state_change`: the guarded value is a
        // `Copy` enum that can never be left inconsistent, and
        // returning a made-up `Unspecified` would hide the real state
        // from a test polling on it.
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakenfist_spice_renderer::{
        EncoderControl, EncoderTask, H264Encoder, SyntheticFrameSource,
    };
    use std::time::Duration;
    use tokio::sync::mpsc;

    use crate::test_client::TestPeer;

    /// Build a codec-parameters entry the way negotiation hands it to
    /// us: mime type, fmtp line and the payload type our MediaEngine
    /// remapped the remote's onto.
    fn codec_params(
        mime_type: &str,
        sdp_fmtp_line: &str,
        payload_type: u8,
    ) -> RTCRtpCodecParameters {
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: mime_type.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: sdp_fmtp_line.to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type,
        }
    }

    #[test]
    fn note_drop_logs_the_first_then_every_nth() {
        let counter = AtomicU64::new(0);

        // The first drop always reports, so a single lost message is
        // never silent.
        assert_eq!(BridgeEvents::note_drop(&counter), Some(1));

        // Then nothing until the threshold, and what it reports is the
        // running total rather than "one more".
        for _ in 2..LOG_EVERY_N_DROPS {
            assert_eq!(BridgeEvents::note_drop(&counter), None);
        }
        assert_eq!(
            BridgeEvents::note_drop(&counter),
            Some(LOG_EVERY_N_DROPS),
            "the Nth drop should report"
        );

        for _ in 1..LOG_EVERY_N_DROPS {
            assert_eq!(BridgeEvents::note_drop(&counter), None);
        }
        assert_eq!(
            BridgeEvents::note_drop(&counter),
            Some(LOG_EVERY_N_DROPS * 2),
            "and every Nth after that"
        );
    }

    /// Build a [`BridgeEvents`] whose two outbound channels are
    /// already full, so both handlers take their drop path.
    ///
    /// The receivers are returned rather than dropped: dropping them
    /// closes the channels, and `try_send` would then report `Closed`
    /// instead of `Full` — a different arm, with different logging and
    /// no counter increment.
    #[allow(clippy::type_complexity)]
    fn events_with_full_channels() -> (
        BridgeEvents,
        mpsc::Receiver<EncoderControl>,
        mpsc::Receiver<Vec<u8>>,
    ) {
        let (encoder_control, enc_rx) = mpsc::channel::<EncoderControl>(1);
        let (incoming_tx, inc_rx) = mpsc::channel::<Vec<u8>>(1);
        encoder_control
            .try_send(EncoderControl::RequestKeyframe)
            .expect("fill the encoder channel to capacity");
        incoming_tx.try_send(vec![0]).expect("fill the DC channel");

        let events = BridgeEvents {
            encoder_control,
            dead: Arc::new(StickySignal::new()),
            incoming_tx,
            state: Arc::new(Mutex::new(RTCPeerConnectionState::New)),
            gathered: Arc::new(StickySignal::new()),
            dropped_keyframe_requests: AtomicU64::new(0),
            dc_pumps: Arc::new(Mutex::new(Vec::new())),
        };
        (events, enc_rx, inc_rx)
    }

    #[tokio::test]
    async fn a_full_encoder_channel_drops_the_keyframe_request_and_counts_it() {
        let (events, _enc_rx, _inc_rx) = events_with_full_channels();

        // Must return rather than block: this runs inline in the
        // driver event loop, so a slow encoder has to cost a keyframe
        // rather than the whole connection.
        events
            .on_state_change(RTCPeerConnectionState::Connected)
            .await;

        assert_eq!(
            events.dropped_keyframe_requests.load(Ordering::Relaxed),
            1,
            "a full encoder channel should count a dropped keyframe request"
        );
        // Dropping the request must not cost the state transition too.
        assert_eq!(
            *events
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            RTCPeerConnectionState::Connected
        );
    }

    /// A full control channel must park the caller, not discard the
    /// message.
    ///
    /// The opposite of `a_full_encoder_channel_drops_...` above, and
    /// deliberately so: that path runs inline in the driver loop,
    /// this one runs in a spawned `run_dc_pump`. Input events are
    /// ordered and reliable — a dropped key-up sticks a modifier down
    /// in the guest — so waiting for a slot, and letting SCTP push
    /// back on the browser, is the correct behaviour.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_control_channel_applies_back_pressure() {
        use std::time::Duration;

        let (events, _enc_rx, mut inc_rx) = events_with_full_channels();
        let events = Arc::new(events);

        let send = tokio::spawn({
            let events = events.clone();
            async move { events.on_control_message(vec![1, 2, 3], "test").await }
        });

        // Every slot is taken, so the send must still be parked.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !send.is_finished(),
            "a full control channel dropped the message instead of waiting — an input \
             event would be lost, and a lost key-up sticks a modifier down in the guest"
        );

        // Free a slot; the parked send must then complete and deliver.
        let first = inc_rx.recv().await.expect("the pre-filled message");
        assert_eq!(first, vec![0]);

        tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the send should complete once a slot frees")
            .expect("send task panicked");

        assert_eq!(
            inc_rx.recv().await.expect("the back-pressured message"),
            vec![1, 2, 3],
            "the message that waited must still arrive, in order"
        );
    }

    #[test]
    fn packetization_mode_1_is_recognised_and_defaults_to_0() {
        assert!(is_packetization_mode_1(H264_FMTP_LINE));
        assert!(is_packetization_mode_1("packetization-mode=1"));
        // Whitespace around the separators is legal in an fmtp line.
        assert!(is_packetization_mode_1(
            "level-asymmetry-allowed=1; packetization-mode=1 ;profile-level-id=42e01f"
        ));
        assert!(!is_packetization_mode_1(
            "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"
        ));
        // RFC 6184 §8.1: absent means mode 0, not "unknown".
        assert!(!is_packetization_mode_1(""));
        assert!(!is_packetization_mode_1("profile-level-id=42e01f"));
        // A value we do not understand is not mode 1.
        assert!(!is_packetization_mode_1("packetization-mode=2"));
    }

    #[test]
    fn negotiated_h264_payload_type_prefers_packetization_mode_1() {
        // What a Firefox offer leaves us with: only the 42e01f pair,
        // remapped onto the MediaEngine's PT 125 (mode 1) and 108
        // (mode 0). The mode-0 entry comes first to prove ordering is
        // not what makes this pass.
        let codecs = vec![
            codec_params(
                MIME_TYPE_H264,
                "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f",
                108,
            ),
            codec_params(
                MIME_TYPE_H264,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
                125,
            ),
        ];
        assert_eq!(negotiated_h264_payload_type(&codecs), Some(125));
    }

    #[test]
    fn negotiated_h264_payload_type_falls_back_to_mode_0() {
        let codecs = vec![codec_params(
            MIME_TYPE_H264,
            "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f",
            108,
        )];
        // Nothing better on offer: send it rather than nothing, and
        // let the warning explain the fragmentation problem.
        assert_eq!(negotiated_h264_payload_type(&codecs), Some(108));
    }

    #[test]
    fn negotiated_h264_payload_type_prefers_baseline_among_mode_1_entries() {
        // A high-profile mode-1 entry first, so ordering is not what
        // makes this pass. The encoder emits PRO_BASELINE, so 125 is
        // the entry that describes what we actually send.
        let codecs = vec![
            codec_params(
                MIME_TYPE_H264,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032",
                123,
            ),
            codec_params(
                MIME_TYPE_H264,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
                125,
            ),
        ];
        assert_eq!(negotiated_h264_payload_type(&codecs), Some(125));
    }

    #[test]
    fn negotiated_h264_payload_type_takes_an_off_profile_mode_1_entry() {
        // No baseline entry survived. Packetization mode still wins
        // over profile: mode 0 cannot carry a fragmented NAL at all,
        // whereas a profile mismatch is decoded from the SPS anyway.
        let codecs = vec![
            codec_params(
                MIME_TYPE_H264,
                "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f",
                108,
            ),
            codec_params(
                MIME_TYPE_H264,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032",
                123,
            ),
        ];
        assert_eq!(negotiated_h264_payload_type(&codecs), Some(123));
    }

    #[test]
    fn baseline_profile_is_recognised_only_from_a_well_formed_id() {
        assert!(is_baseline_profile(
            "packetization-mode=1;profile-level-id=42e01f"
        ));
        assert!(is_baseline_profile("profile-level-id=42001f"));
        // Case-insensitive: SDP hex digits are not case-significant.
        assert!(is_baseline_profile("profile-level-id=42E01F"));

        assert!(!is_baseline_profile("profile-level-id=640032"));
        assert!(!is_baseline_profile("profile-level-id=4d001f"));
        // Absent, truncated, over-long, or not a parameter at all.
        assert!(!is_baseline_profile("packetization-mode=1"));
        assert!(!is_baseline_profile(""));
        assert!(!is_baseline_profile("profile-level-id=42"));
        assert!(!is_baseline_profile("profile-level-id=42e01f00"));
        // Six *bytes* whose second byte is mid-character: the length
        // check passes and the `[..2]` slice would panic without the
        // char-boundary guard. "€" is three bytes.
        assert!(!is_baseline_profile("profile-level-id=€abc"));
        // Six bytes, boundary at 2, but not "42".
        assert!(!is_baseline_profile("profile-level-id=é2e01"));
    }

    #[test]
    fn negotiated_h264_payload_type_is_none_when_h264_was_rejected() {
        let codecs = vec![codec_params("video/VP8", "", 96)];
        assert_eq!(negotiated_h264_payload_type(&codecs), None);
    }

    #[test]
    fn negotiated_payload_type_matches_mime_type_case_insensitively() {
        let codecs = vec![
            codec_params("video/vp8", "", 96),
            codec_params("audio/OPUS", "minptime=10;useinbandfec=1", 111),
        ];
        assert_eq!(negotiated_payload_type(&codecs, MIME_TYPE_OPUS), Some(111));
        assert_eq!(negotiated_payload_type(&codecs, MIME_TYPE_H264), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_constructs_with_empty_ice_servers() {
        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let config = WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: tx,
        };
        let bridge = WebrtcBridge::new(config).await.expect("bridge constructs");
        bridge.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_accept_offer_returns_answer_with_h264_and_opus() {
        // Build the bridge under test.
        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: tx,
        })
        .await
        .expect("bridge");

        // A separate "client" peer generates an offer for the bridge
        // to answer. No seed datachannel and no gathering wait: this
        // test only inspects the answer's codec advertisement, so it
        // never completes a handshake.
        let client = TestPeer::builder().build().await.expect("client peer");
        let offer_sdp = client.create_offer().await.expect("offer");

        let answer_sdp = bridge.accept_offer(offer_sdp).await.expect("accept_offer");

        // The answer SDP should advertise both H.264 and Opus. Match
        // case-insensitively because SDP capitalisation is not
        // standardised across implementations.
        let lower = answer_sdp.to_ascii_lowercase();
        assert!(
            lower.contains("h264"),
            "answer SDP should advertise H.264:\n{}",
            answer_sdp
        );
        assert!(
            lower.contains("opus"),
            "answer SDP should advertise Opus:\n{}",
            answer_sdp
        );

        client.close().await.expect("client close");
        bridge.close().await.expect("bridge close");
    }

    /// Smoke test: drive the video pump end-to-end against a real
    /// `H264Encoder` + `EncoderTask` fed by `SyntheticFrameSource`.
    /// We don't need a peer connection — `TrackLocalStaticRTP::write_rtp`
    /// accepts packets even without a connected peer (they're
    /// buffered/dropped at the transport).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn video_pump_runs_without_errors() {
        let (control_tx, _control_rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: control_tx,
        })
        .await
        .expect("bridge");

        // Encoder pipeline driven by a synthetic source.
        let encoder = H264Encoder::new(64, 64, 30).expect("encoder init");
        let source = SyntheticFrameSource::new(64, 64);
        let (frame_tx, frame_rx) = mpsc::channel(32);
        let (enc_ctl_tx, enc_ctl_rx) = mpsc::channel(4);
        let _enc_handle = EncoderTask::spawn(encoder, source, frame_tx, enc_ctl_rx, 30);

        // Spawn the video pump.
        let pump_handle = bridge.spawn_video_pump(frame_rx);

        // Let it run briefly. ~25 frames at 30 fps in 800 ms is
        // plenty for a smoke check; debug-build openh264 is
        // slower than wall-clock but we only need the pipeline
        // to make progress without errors.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // Stop the encoder. When the encoder task drops its
        // `frame_tx`, `frame_rx.recv()` will return None and
        // the pump exits cleanly.
        let _ = enc_ctl_tx.send(EncoderControl::Stop).await;

        // The pump exits when frame_rx closes. Give it a few
        // seconds to drain. The triple unwrap unwraps the
        // timeout, the JoinHandle, and the pump's Result<()>.
        tokio::time::timeout(Duration::from_secs(3), pump_handle)
            .await
            .expect("pump didn't stop in time")
            .expect("join")
            .expect("pump task error");

        bridge.close().await.expect("close");
    }

    /// Smoke test: spawn the synthetic audio pump and let it run
    /// for a few hundred ms. The pump runs forever, so we abort
    /// after a brief sleep. We don't assert exact packet counts —
    /// `TrackLocalStaticRTP::write_rtp` accepts packets even
    /// without a connected peer (they're buffered/dropped at the
    /// transport), and the in-process inspect path is added in
    /// 3f's loopback test. The Phase 3 success criterion here is
    /// "no panics, no encoder errors, the track accepts writes".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn synthetic_audio_pump_emits_packets() {
        let (control_tx, _control_rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: control_tx,
        })
        .await
        .expect("bridge");

        let pump = bridge.spawn_synthetic_audio_pump();

        // ~25 packets at 50 fps in 500 ms; plenty for a smoke
        // check that the encode + payload + write_rtp loop runs.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The pump runs forever, so abort it. The JoinHandle's
        // result will be JoinError::Cancelled, which we don't
        // need to inspect — the assertions are "didn't panic"
        // and "the bridge can still close cleanly".
        pump.abort();

        bridge.close().await.expect("close");
    }

    /// Smoke test: spawn the real Opus passthrough pump and feed
    /// it a handful of synthetic Opus packets. As with the
    /// synthetic pump, we don't assert exact packet counts —
    /// `TrackLocalStaticRTP::write_rtp` accepts packets even
    /// without a connected peer (they're buffered/dropped at the
    /// transport). The Phase 5e success criterion here is "no
    /// panics, payloader accepts the bytes, the track accepts
    /// writes, the pump exits cleanly when the channel closes".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn audio_pump_forwards_real_opus_packets() {
        let (control_tx, _control_rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: control_tx,
        })
        .await
        .expect("bridge");

        // Encode a handful of synthetic Opus packets so the
        // payloader sees real Opus content (not random bytes).
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio)
                .expect("opus encoder");
        encoder
            .set_bitrate(opus::Bitrate::Bits(32_000))
            .expect("set bitrate");

        let (tx, rx) = mpsc::channel::<(Vec<u8>, u32)>(8);
        let pump = bridge.spawn_audio_pump(rx);

        // Feed five 20 ms frames of silence-encoded Opus.
        let pcm = vec![0i16; 960];
        for _ in 0..5 {
            let mut buf = vec![0u8; 1500];
            let n = encoder.encode(&pcm, &mut buf).expect("encode");
            buf.truncate(n);
            tx.send((buf, 960)).await.expect("send");
        }
        drop(tx);

        // The pump exits when its rx closes; give it a moment.
        let res = tokio::time::timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
        res.expect("join").expect("pump task error");

        bridge.close().await.expect("close");
    }

    /// Round-trip test for the control datachannel. Two in-process
    /// `WebrtcBridge` instances exchange SDP directly (no signalling
    /// server), wait for ICE/DTLS to establish, then round-trip
    /// "ping" and "pong" messages.
    ///
    /// The "client" bridge uses the test-only
    /// `create_offer_and_gather` + `set_remote_answer` helpers to
    /// drive the SDP exchange from the client side without adding
    /// any public API.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn control_datachannel_roundtrips_messages() {
        // Server bridge (the answerer).
        let (server_enc_tx, _) = mpsc::channel::<EncoderControl>(4);
        let server = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: server_enc_tx,
        })
        .await
        .expect("server bridge");

        // Client bridge (the offerer).
        let (client_enc_tx, _) = mpsc::channel::<EncoderControl>(4);
        let client = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: client_enc_tx,
        })
        .await
        .expect("client bridge");

        // Take receivers early — before any messages can arrive —
        // so the on_message handlers have a live channel to push
        // into and nothing is dropped due to a closed receiver.
        let mut server_rx = server.control_rx().expect("server rx (first call)");
        let mut client_rx = client.control_rx().expect("client rx (first call)");

        // A second call must return None (the option is exhausted).
        assert!(
            server.control_rx().is_none(),
            "second control_rx should be None"
        );

        // Drive SDP exchange: client offers, server answers.
        let offer_sdp = client
            .create_offer_and_gather()
            .await
            .expect("client offer");
        let answer_sdp = server.accept_offer(offer_sdp).await.expect("server accept");
        client
            .set_remote_answer(answer_sdp)
            .await
            .expect("client set answer");

        // Wait for both PCs to reach Connected (ICE + DTLS).
        // With two in-process PCs and host-only candidates
        // (no STUN) this usually completes within 2 s on loopback.
        let connected = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if server.connection_state() == RTCPeerConnectionState::Connected
                    && client.connection_state() == RTCPeerConnectionState::Connected
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            connected.is_ok(),
            "PCs did not reach Connected within timeout"
        );

        // Give the datachannel a moment to open after PC Connected.
        // The DC open event is asynchronous and slightly lags the
        // connection state change.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Client → server "ping".
        client
            .send_control(b"ping")
            .await
            .expect("client send ping");
        let msg = tokio::time::timeout(Duration::from_secs(2), server_rx.recv())
            .await
            .expect("server did not receive ping in time")
            .expect("server rx closed");
        assert_eq!(msg, b"ping", "server should receive 'ping'");

        // Server → client "pong".
        server
            .send_control(b"pong")
            .await
            .expect("server send pong");
        let msg = tokio::time::timeout(Duration::from_secs(2), client_rx.recv())
            .await
            .expect("client did not receive pong in time")
            .expect("client rx closed");
        assert_eq!(msg, b"pong", "client should receive 'pong'");

        server.close().await.expect("server close");
        client.close().await.expect("client close");
    }

    /// `accept_offer` must return an answer carrying every gathered
    /// candidate, run after run.
    ///
    /// This is the acceptance test for replacing
    /// `gathering_complete_promise()` with the sticky
    /// `on_ice_gathering_state_change` signal. The failure mode being
    /// guarded against is subtle and nasty: a signal that fires
    /// slightly too early yields an answer that is missing
    /// candidates, which still parses, still completes a handshake on
    /// loopback, and only fails on networks where the dropped
    /// candidate was the one that mattered.
    ///
    /// The partial-gathering check is *intra-run*: after
    /// `accept_offer` returns, wait 500 ms and re-read the local
    /// description — if the gathering signal fired early, the
    /// candidates that were still in flight land during that window
    /// and the count grows past what the answer carried. This
    /// detects the real failure mode without depending on host
    /// network stability, which the original cross-run
    /// equal-count assertion did: on the self-hosted runners,
    /// docker/veth interfaces come and go and IPv6 temporary
    /// addresses rotate, so exact equality across twenty sequential
    /// runs is a flake waiting to happen. That stricter cross-run
    /// invariance check still exists, gated behind
    /// `RYLL_GATHERING_SOAK=1` (`make test` passes the variable
    /// through to the devcontainer) for deliberate runs on a quiet
    /// host — see the testing section of `docs/development.md`.
    ///
    /// This test also carries the *only* automated guard on the
    /// bind-address choice: that every candidate in the answer names a
    /// routable address. See the assertion below for why nothing else
    /// in the suite can catch that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn accept_offer_answer_carries_all_candidates() {
        let soak = std::env::var("RYLL_GATHERING_SOAK").is_ok_and(|v| v == "1");
        let iterations = if soak { 20 } else { 3 };

        let mut counts = Vec::new();
        for i in 0..iterations {
            let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
            let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
                ice_servers: vec![],
                encoder_control: tx,
            })
            .await
            .expect("bridge");

            let client = TestPeer::builder()
                .seed_data_channel("client-seed")
                .build()
                .await
                .expect("client peer");
            let offer_sdp = client.offer_and_gather().await.expect("client offer");
            let answer_sdp = bridge.accept_offer(offer_sdp).await.expect("accept_offer");

            let candidate_lines: Vec<&str> = answer_sdp
                .lines()
                .filter(|l| l.starts_with("a=candidate:"))
                .collect();
            let candidates = candidate_lines.len();
            assert!(
                candidates > 0,
                "iteration {i}: answer carried no ICE candidates, so the \
                 gathering signal fired before any were gathered:\n{answer_sdp}"
            );

            // Every candidate must name an address a remote peer could
            // actually dial. webrtc-rs 0.20 hands host-candidate
            // generation the socket addresses we bound and filters
            // nothing, so a `0.0.0.0` / `::` bind yields a literal
            // `a=candidate:... 0.0.0.0 <port> typ host` that every
            // browser discards. This assertion is the only automated
            // check on that: two Rust peers on one host agree about an
            // unspecified address and connect happily, so the loopback
            // and lifecycle tests stay green on a build no browser can
            // reach. See `crate::bind_addrs` and Decision 4 of the
            // phase-02 plan.
            //
            // The address is the fifth space-separated field of an ICE
            // candidate line (RFC 5245 §15.1: foundation, component,
            // transport, priority, connection-address, ...).
            for line in &candidate_lines {
                let addr = line
                    .split_whitespace()
                    .nth(4)
                    .unwrap_or_else(|| panic!("iteration {i}: malformed candidate line: {line}"));
                assert!(
                    addr != "0.0.0.0" && addr != "::",
                    "iteration {i}: answer advertises an unspecified candidate address, \
                     which no browser will use — the UDP sockets were bound to a wildcard \
                     address rather than a real interface:\n{line}"
                );
            }

            counts.push(candidates);

            // Gathering was Complete when accept_offer returned, so
            // no further candidates may appear. If some do, the
            // signal fired after *some* candidates but before the
            // rest — the exact failure mode that yields an answer
            // which parses, handshakes on loopback, and fails only
            // where the missing candidate mattered.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let late = bridge
                .pc
                .local_description()
                .await
                .expect("local description must still exist")
                .sdp
                .lines()
                .filter(|l| l.starts_with("a=candidate:"))
                .count();
            assert_eq!(
                late, candidates,
                "iteration {i}: candidates kept arriving after the gathering \
                 signal fired — the answer went out short"
            );

            client.close().await.expect("client close");
            bridge.close().await.expect("bridge close");
        }

        if soak {
            let first = counts[0];
            assert!(
                counts.iter().all(|&c| c == first),
                "answer candidate count varied across runs, which means the \
                 gathering signal is racing (or the host's interfaces churned \
                 mid-test): {counts:?}"
            );
        }
    }

    /// The gathering signal must not fire before the local
    /// description actually carries the candidates.
    ///
    /// `accept_offer` reads `local_description()` immediately after
    /// waiting, so if `on_ice_gathering_state_change(Complete)` can
    /// be observed before the description is updated, the answer goes
    /// out short. Asserting the description is populated the instant
    /// the wait returns pins that ordering.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gathering_signal_fires_after_local_description_is_populated() {
        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: tx,
        })
        .await
        .expect("bridge");

        let client = TestPeer::builder()
            .seed_data_channel("client-seed")
            .build()
            .await
            .expect("client peer");
        let offer_sdp = client.offer_and_gather().await.expect("client offer");

        let offer = RTCSessionDescription::offer(offer_sdp).expect("offer");
        bridge.pc.set_remote_description(offer).await.expect("srd");
        let answer = bridge.pc.create_answer(None).await.expect("answer");
        bridge.pc.set_local_description(answer).await.expect("sld");

        bridge.wait_for_gathering().await;

        let local = bridge
            .pc
            .local_description()
            .await
            .expect("local description must exist once gathering completed");
        assert!(
            local.sdp.contains("a=candidate:"),
            "gathering reported complete but the local description \
             carries no candidates yet:\n{}",
            local.sdp
        );

        // Gathering has definitely completed by now, so this second
        // call must take the sticky-flag fast path. Whether the first
        // call above blocked on the notification or already found the
        // flag set depends on timing, so this is what guarantees the
        // late-subscriber path is covered at all — without it a
        // regression that made `wait_for_gathering` hang for late
        // callers could pass the suite.
        tokio::time::timeout(Duration::from_millis(100), bridge.wait_for_gathering())
            .await
            .expect("a late wait_for_gathering must return via the sticky flag");

        client.close().await.expect("client close");
        bridge.close().await.expect("bridge close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accept_offer_rejects_malformed_sdp() {
        use shakenfist_spice_renderer::EncoderControl;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: tx,
        })
        .await
        .expect("bridge constructs");

        let result = bridge.accept_offer("not actually sdp".to_owned()).await;
        assert!(
            result.is_err(),
            "accept_offer must reject malformed SDP, got Ok: {:?}",
            result.ok()
        );

        bridge.close().await.expect("close");
    }

    /// `close()` must leave nothing behind for the caller to reap.
    ///
    /// The handles are drained by `close()` itself, so an empty list
    /// is the observable: every handle that was in it has been taken
    /// and aborted. This is the happy path — see
    /// `dropping_a_bridge_without_close_still_stops_its_pumps` for the
    /// path that forgets.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_drains_the_datachannel_pump_handles() {
        use shakenfist_spice_renderer::EncoderControl;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: tx,
        })
        .await
        .expect("bridge constructs");

        let pumps = bridge.dc_pumps.clone();
        assert!(
            !pumps.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "the control datachannel pump should be running before close"
        );

        bridge.close().await.expect("close");

        assert!(
            pumps.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "close() left datachannel pump handles behind"
        );
    }

    /// A bridge dropped without `close()` must still stop its pumps.
    ///
    /// On 0.20 dropping the peer connection detaches its driver rather
    /// than stopping it, so "forgot to close" is a silent leak of the
    /// driver, the ICE sockets and every datachannel pump. `post_offer`
    /// closes explicitly on its error path; this covers the paths that
    /// do not, which is what the `Drop` impl exists for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_bridge_without_close_still_stops_its_pumps() {
        use shakenfist_spice_renderer::EncoderControl;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: tx,
        })
        .await
        .expect("bridge constructs");

        let pumps = bridge.dc_pumps.clone();
        assert!(
            !pumps.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "the control datachannel pump should be running before the drop"
        );

        drop(bridge);

        assert!(
            pumps.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "a bridge dropped without close() leaked its datachannel pumps — on 0.20 that \
             leaks the driver task and its UDP sockets for the life of the process"
        );
    }

    /// The two tracks are BUNDLE-ed, so their SSRCs must differ, and
    /// neither may be zero.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn track_ssrcs_are_distinct_and_non_zero() {
        use shakenfist_spice_renderer::EncoderControl;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::channel::<EncoderControl>(4);
        let bridge = WebrtcBridge::new(WebrtcBridgeConfig {
            ice_servers: vec![],
            encoder_control: tx,
        })
        .await
        .expect("bridge constructs");

        assert_ne!(bridge.video_ssrc, 0, "video SSRC must not be zero");
        assert_ne!(bridge.audio_ssrc, 0, "audio SSRC must not be zero");
        assert_ne!(
            bridge.video_ssrc, bridge.audio_ssrc,
            "BUNDLE-ed tracks must not share an SSRC (RFC 8843 §9.2)"
        );

        bridge.close().await.expect("close");
    }

    #[test]
    fn nonzero_random_ssrc_never_returns_zero() {
        for _ in 0..1_000 {
            assert_ne!(nonzero_random_ssrc(), 0);
        }
    }
}
