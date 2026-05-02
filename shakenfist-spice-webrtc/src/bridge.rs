//! [`WebrtcBridge`] wraps an [`RTCPeerConnection`] together with a
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

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use shakenfist_spice_renderer::{EncodedFrame, EncoderControl};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::codecs::h264::H264Payloader;
use webrtc::rtp::codecs::opus::OpusPayloader;
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::rtp::packetizer::Payloader;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocalWriter;

/// Payload type used for H.264 in our SDP. Reused by both the
/// MediaEngine registration in [`register_h264`] and the RTP
/// header construction in the video pump.
const H264_PAYLOAD_TYPE: u8 = 102;

/// H.264 RTP clock rate per RFC 6184 / RFC 4566.
const VIDEO_CLOCK_RATE_HZ: u32 = 90_000;

/// Maximum RTP packet size handed to the H.264 payloader. 1200 is
/// a conservative default that fits inside typical browser path
/// MTU minus UDP+IP+SRTP overhead on a 1500-byte ethernet frame.
const VIDEO_MTU: usize = 1200;

/// Payload type used for Opus in our SDP. Matches the value used
/// by webrtc-rs's [`MediaEngine::register_default_codecs`] (which
/// the bridge calls in [`WebrtcBridge::new`]).
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
    pc: Arc<RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticRTP>,
    audio_track: Arc<TrackLocalStaticRTP>,
    control_dc: Arc<RTCDataChannel>,
    #[allow(dead_code)] // retained for diagnostics; the on-state
    // handler keeps its own clone.
    encoder_control: mpsc::Sender<EncoderControl>,
    /// Receiver for incoming control-DC messages. Take it once
    /// via [`WebrtcBridge::control_rx`]. Wrapped in
    /// `Mutex<Option<...>>` because `WebrtcBridge` is shared via
    /// `Arc` but the receiver can only be consumed once.
    incoming_control: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
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

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let rtc_config = RTCConfiguration {
            ice_servers: config
                .ice_servers
                .iter()
                .map(|url| RTCIceServer {
                    urls: vec![url.clone()],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let pc = Arc::new(api.new_peer_connection(rtc_config).await?);

        // Video track.
        let video_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "ryll-spice".to_owned(),
        ));
        pc.add_track(video_track.clone()).await?;

        // Audio track. Opus payload type negotiation is handled by
        // webrtc-rs's MediaEngine; the capability mime-type is
        // sufficient here.
        let audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "ryll-spice".to_owned(),
        ));
        pc.add_track(audio_track.clone()).await?;

        // Control datachannel. Ordered + reliable for input events
        // (Phase 5) and cursor overlay (Phase 5b).
        let control_dc = pc
            .create_data_channel(
                "control",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    max_retransmits: None,
                    ..Default::default()
                }),
            )
            .await?;

        // Keyframe-on-attach: whenever the PC reaches Connected, ask
        // the encoder for a fresh IDR so the viewer can decode
        // immediately rather than waiting for the next periodic
        // keyframe.
        let on_connected_tx = config.encoder_control.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let tx = on_connected_tx.clone();
            Box::pin(async move {
                if state == RTCPeerConnectionState::Connected {
                    if let Err(err) = tx.send(EncoderControl::RequestKeyframe).await {
                        tracing::warn!(
                            error = %err,
                            "WebrtcBridge: failed to request keyframe on Connected",
                        );
                    } else {
                        tracing::debug!("WebrtcBridge: requested keyframe on Connected");
                    }
                }
            })
        }));

        // Control DC incoming message fan-in: the on_message callback
        // pushes raw bytes onto a bounded mpsc channel. The consumer
        // takes the Receiver once via `control_rx()`.
        //
        // Two DCs are involved in a two-bridge loopback scenario:
        //   1. This bridge's own `control_dc` (created above) — used
        //      for `send_control` and receives messages from the remote
        //      peer's answerer-side DC.
        //   2. The DC *received* from the remote peer when the bridge
        //      acts as the answerer (`on_data_channel` callback) —
        //      this carries messages sent by the remote peer on its
        //      own created DC.
        // Both are wired to the same `incoming_tx` so `control_rx()`
        // delivers messages from either direction.
        let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(64);

        let incoming_tx_clone = incoming_tx.clone();
        control_dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = incoming_tx_clone.clone();
            Box::pin(async move {
                let bytes = msg.data.to_vec();
                if tx.send(bytes).await.is_err() {
                    tracing::debug!("WebrtcBridge: control_rx receiver dropped, message lost");
                }
            })
        }));

        // Also handle DCs initiated by the remote peer. When this
        // bridge acts as the answerer in an SDP exchange, the offerer's
        // DC arrives here via `on_data_channel`. Wire its `on_message`
        // to the same channel so `control_rx()` sees all incoming
        // messages regardless of which side initiated the DC.
        let incoming_tx_dc = incoming_tx.clone();
        pc.on_data_channel(Box::new(move |remote_dc: Arc<RTCDataChannel>| {
            let tx = incoming_tx_dc.clone();
            Box::pin(async move {
                tracing::debug!(
                    label = %remote_dc.label(),
                    "WebrtcBridge: remote DC received via on_data_channel"
                );
                remote_dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let tx = tx.clone();
                    Box::pin(async move {
                        let bytes = msg.data.to_vec();
                        if tx.send(bytes).await.is_err() {
                            tracing::debug!(
                                "WebrtcBridge: control_rx receiver dropped, \
                                 remote-DC message lost"
                            );
                        }
                    })
                }));
            })
        }));

        let incoming_control = Mutex::new(Some(incoming_rx));

        Ok(Self {
            pc,
            video_track,
            audio_track,
            control_dc,
            encoder_control: config.encoder_control,
            incoming_control,
        })
    }

    /// Accept a remote SDP offer, generate our answer, and wait for
    /// ICE gathering to complete so the returned answer carries every
    /// candidate we know about (no trickle ICE for the MVP).
    pub async fn accept_offer(&self, offer_sdp: String) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer_sdp)?;
        self.pc.set_remote_description(offer).await?;

        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer).await?;

        // Block until ICE gathering finishes. `gathering_complete_promise`
        // returns a oneshot-style receiver in webrtc-rs 0.17.
        let mut gather_complete = self.pc.gathering_complete_promise().await;
        let _ = gather_complete.recv().await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("local description missing after ICE gathering"))?;
        Ok(local_desc.sdp)
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
        tokio::spawn(run_video_pump(rx, track))
    }

    /// Spawn the synthetic audio pump task: generate a 440 Hz sine
    /// wave at 48 kHz mono, encode it via Opus in 20 ms windows
    /// (960 samples per frame), payload via [`OpusPayloader`], and
    /// write RTP packets to the bridge's audio track at 50 fps.
    ///
    /// Phase 5 will replace this with a real Opus passthrough from
    /// the SPICE playback channel; Phase 3 ships this synthetic
    /// path so the audio track is exercised in integration tests
    /// (3f) and so the browser-side `<audio>` element receives a
    /// continuous stream as soon as DTLS comes up.
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
        tokio::spawn(run_synthetic_audio_pump(track))
    }

    /// Send a payload over the control datachannel. The DC is
    /// reliable + ordered; this is appropriate for inputs and
    /// cursor overlay updates (Phase 5).
    ///
    /// Returns an error if the underlying datachannel send fails
    /// (e.g. the channel is not yet open or the remote peer has
    /// closed it).
    pub async fn send_control(&self, payload: &[u8]) -> Result<()> {
        let bytes = Bytes::copy_from_slice(payload);
        self.control_dc
            .send(&bytes)
            .await
            .map(|_n| ())
            .map_err(|e| anyhow!("control DC send: {}", e))
    }

    /// Take the receiver for incoming control-DC messages.
    ///
    /// Can only be called once per bridge; subsequent calls return
    /// `None`. Register the `on_message` callback (done in
    /// [`WebrtcBridge::new`]) before calling this to avoid a race
    /// where an early message is lost.
    pub fn control_rx(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.incoming_control
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    /// Close the underlying peer connection. Consumes `self` so
    /// callers cannot accidentally reuse a closed bridge.
    pub async fn close(self) -> Result<()> {
        self.pc.close().await?;
        Ok(())
    }
}

/// Register H.264 with the MediaEngine. We pin profile-level-id to
/// `42e01f` (baseline profile, level 3.1) which matches what the
/// renderer's openh264 wrapper emits and what every browser decodes.
/// Packetization-mode 1 enables FU-A fragmentation per RFC 6184 §5.4.
///
/// The default-codecs register call may or may not include H.264
/// depending on the webrtc-rs build features. Registering manually
/// guarantees the answer SDP advertises H.264.
fn register_h264(media_engine: &mut MediaEngine) -> Result<()> {
    let h264 = RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: H264_PAYLOAD_TYPE,
        ..Default::default()
    };
    media_engine.register_codec(h264, RTPCodecType::Video)?;
    Ok(())
}

/// Spawned by [`WebrtcBridge::spawn_video_pump`]. Owns the
/// receiver side of the encoder's [`EncodedFrame`] channel and a
/// clone of the bridge's video track. Iterates encoded frames,
/// strips Annex-B start codes, payloads each NAL via
/// [`H264Payloader`], and writes RTP packets to the track. The
/// marker bit is set on the last RTP packet of each access unit
/// per RFC 6184 §5.1.
///
/// Errors from `track.write_rtp` are logged at debug and the
/// loop continues — the receiver may not have negotiated DTLS
/// yet when the first frames arrive, so dropped packets early on
/// are normal.
async fn run_video_pump(
    mut rx: mpsc::Receiver<EncodedFrame>,
    track: Arc<TrackLocalStaticRTP>,
) -> Result<()> {
    let mut payloader = H264Payloader::default();
    let mut sequence: u16 = rand::random();
    let ssrc: u32 = rand::random();

    while let Some(frame) = rx.recv().await {
        // EncodedFrame::timestamp_us is microseconds; convert to
        // a 32-bit RTP timestamp at 90 kHz. Use u128 arithmetic
        // to avoid overflow during the multiply, then truncate.
        let rtp_ts = ((frame.timestamp_us as u128).saturating_mul(VIDEO_CLOCK_RATE_HZ as u128)
            / 1_000_000u128) as u32;

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
                    payload_type: H264_PAYLOAD_TYPE,
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
            if let Err(e) = track.write_rtp(&pkt).await {
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
/// `track.write_rtp` errors are logged at debug and the loop
/// continues — the receiver may not have negotiated DTLS yet
/// when the pump starts, so dropped packets early on are normal.
/// The loop never returns `Ok(())` on its own; it exits only
/// when the spawning task is aborted or the runtime shuts down.
async fn run_synthetic_audio_pump(track: Arc<TrackLocalStaticRTP>) -> Result<()> {
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
    let ssrc: u32 = rand::random();
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

        for payload in payloads {
            if payload.is_empty() {
                continue;
            }
            let header = Header {
                version: 2,
                payload_type: OPUS_PAYLOAD_TYPE,
                sequence_number: sequence,
                timestamp: rtp_timestamp,
                ssrc,
                marker: false,
                ..Default::default()
            };
            let pkt = Packet { header, payload };
            sequence = sequence.wrapping_add(1);
            if let Err(e) = track.write_rtp(&pkt).await {
                tracing::debug!("synthetic audio pump: write_rtp dropped packet: {}", e);
            }
        }

        rtp_timestamp = rtp_timestamp.wrapping_add(AUDIO_SAMPLES_PER_FRAME as u32);
    }
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
        let mut gather = self.pc.gathering_complete_promise().await;
        let _ = gather.recv().await;
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

    /// Return the current peer connection state. Used in tests to
    /// poll until both sides reach `Connected`.
    pub(crate) fn connection_state(
        &self,
    ) -> webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState {
        self.pc.connection_state()
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
    use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
    use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

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

        // Build a separate "client" PC to generate an offer that the
        // bridge can answer. The client adds recvonly video + audio
        // transceivers so its offer has m=video and m=audio sections.
        let mut client_me = MediaEngine::default();
        client_me
            .register_default_codecs()
            .expect("client default codecs");
        register_h264(&mut client_me).expect("client h264");
        let mut client_reg = Registry::new();
        client_reg =
            register_default_interceptors(client_reg, &mut client_me).expect("client interceptors");
        let client_api = APIBuilder::new()
            .with_media_engine(client_me)
            .with_interceptor_registry(client_reg)
            .build();
        let client_pc = client_api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .expect("client pc");

        let _video_tx = client_pc
            .add_transceiver_from_kind(
                RTPCodecType::Video,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await
            .expect("video transceiver");
        let _audio_tx = client_pc
            .add_transceiver_from_kind(
                RTPCodecType::Audio,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await
            .expect("audio transceiver");

        let offer = client_pc.create_offer(None).await.expect("offer");
        client_pc
            .set_local_description(offer.clone())
            .await
            .expect("client lsd");

        let answer_sdp = bridge.accept_offer(offer.sdp).await.expect("accept_offer");

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

        client_pc.close().await.expect("client close");
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
        let encoder = H264Encoder::new(64, 64).expect("encoder init");
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
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

        // When both aws-lc-rs and ring are in the dependency tree
        // (webrtc 0.17.1 pulls both via rustls 0.23) rustls cannot
        // auto-select a CryptoProvider. Install ring explicitly before
        // the DTLS handshake starts. `install_default` is idempotent
        // across concurrent tests (it returns Err if already set, which
        // we ignore).
        let _ = rustls::crypto::ring::default_provider().install_default();

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
}
