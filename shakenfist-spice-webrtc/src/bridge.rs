//! [`WebrtcBridge`] wraps an [`RTCPeerConnection`] together with a
//! video track, an audio track, and a control datachannel.
//!
//! Phase 3 step 3b implements [`WebrtcBridge::new`] and
//! [`WebrtcBridge::accept_offer`]. Phase 3 steps 3c–3e add the video
//! pump, synthetic audio pump, and datachannel send/recv. Phase 3f
//! adds the in-process loopback integration test.
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

use std::sync::Arc;

use anyhow::{anyhow, Result};
use shakenfist_spice_renderer::EncoderControl;
use tokio::sync::mpsc;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

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
/// Phase 3 step 3b ships only [`WebrtcBridge::new`],
/// [`WebrtcBridge::accept_offer`], and [`WebrtcBridge::close`]. The
/// remaining surface (`spawn_video_pump`, `spawn_synthetic_audio_pump`,
/// `send_control`, `control_rx`) is added in 3c–3e.
pub struct WebrtcBridge {
    pc: Arc<RTCPeerConnection>,
    #[allow(dead_code)] // populated for 3c (video pump).
    video_track: Arc<TrackLocalStaticRTP>,
    #[allow(dead_code)] // populated for 3d (audio pump).
    audio_track: Arc<TrackLocalStaticRTP>,
    #[allow(dead_code)] // populated for 3e (datachannel send/recv).
    control_dc: Arc<RTCDataChannel>,
    #[allow(dead_code)] // retained for diagnostics; the on-state
    // handler keeps its own clone.
    encoder_control: mpsc::Sender<EncoderControl>,
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

        Ok(Self {
            pc,
            video_track,
            audio_track,
            control_dc,
            encoder_control: config.encoder_control,
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
        payload_type: 102,
        ..Default::default()
    };
    media_engine.register_codec(h264, RTPCodecType::Video)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakenfist_spice_renderer::EncoderControl;
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
}
