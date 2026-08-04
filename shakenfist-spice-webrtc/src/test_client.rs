//! A client-side [`RTCPeerConnection`] for tests that need to drive
//! the browser half of a [`WebrtcBridge`] exchange.
//!
//! [`WebrtcBridge`]'s API is shaped for the *server* role — it sends
//! video and audio and owns the control datachannel — so every test
//! that wants to observe incoming RTP, answer a datachannel, or watch
//! the handshake reach `Connected` has to hand-roll the other end.
//! Before this module there were four such hand-rolled peers:
//! `bridge.rs`'s own unit tests, `tests/loopback.rs`,
//! `tests/lifecycle.rs`, and `ryll`'s `/offer` signalling test. They
//! agreed on the important parts by convention rather than by
//! construction.
//!
//! Every call in the setup those four shared changes shape in
//! webrtc-rs 0.20 (`APIBuilder`, `Registry`, `new_peer_connection`,
//! `add_transceiver_from_kind`, `gathering_complete_promise`,
//! `connection_state`). Collapsing them here means the 0.20 port
//! rewrites this once rather than four times across two crates. See
//! `docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md` step 1c.
//!
//! ## Availability
//!
//! Compiled for this crate's own unit tests, and for external
//! consumers that enable the `test-support` feature. It is not part
//! of the crate's production surface.
//!
//! ## What is deliberately not here
//!
//! `on_track` packet counting and datachannel echo handlers live in
//! the tests that need them — only `tests/loopback.rs` does, and
//! generalising a single use would make this type harder to read for
//! no gain. Reach through [`TestPeer::pc`] for anything this module
//! does not cover.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

use crate::bridge::register_h264;

/// How often [`TestPeer::wait_until_connected`] re-checks the state.
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Builder for [`TestPeer`].
///
/// The defaults match what a peer answering a [`WebrtcBridge`]
/// usually wants: H.264 registered so the offer/answer converges on
/// the bridge's payload type, recvonly video and audio transceivers
/// so the offer carries `m=video` and `m=audio`, and no seed
/// datachannel.
#[derive(Default)]
pub struct TestPeerBuilder {
    seed_data_channel: Option<String>,
}

impl TestPeerBuilder {
    /// Start from the defaults described on [`TestPeerBuilder`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a datachannel with this label before the offer is
    /// generated, so the SDP carries an `m=application` section.
    ///
    /// This matters more than it looks. You can only answer what was
    /// offered: without an `m=application` section in the offer, the
    /// bridge's control datachannel cannot be negotiated, the SCTP
    /// association never opens, and the bridge's `on_data_channel`
    /// never fires on either side. Tests that exchange control
    /// messages — or that just need the handshake to complete — need
    /// this. Found the hard way in phase 3 step 3f.
    pub fn seed_data_channel(mut self, label: &str) -> Self {
        self.seed_data_channel = Some(label.to_owned());
        self
    }

    /// Build the peer connection and add its transceivers.
    pub async fn build(self) -> Result<TestPeer> {
        // Mirror the bridge's own codec registration exactly, so the
        // offer/answer converges on the same payload type and both
        // ends bind tracks to the same codec entry.
        //
        // In this build `register_h264` is redundant — webrtc-rs's
        // default codecs already cover H.264 and the explicit PT 102
        // registration is dropped as a duplicate payload type. See
        // `register_h264_is_redundant_with_default_codecs` below,
        // which exists to tell us if that ever stops being true.
        // Calling it anyway keeps this peer in lockstep with the
        // bridge rather than relying on the coincidence.
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;
        register_h264(&mut media_engine)?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);

        for kind in [RTPCodecType::Video, RTPCodecType::Audio] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await?;
        }

        // Transceivers before the seed datachannel, matching what
        // tests/loopback.rs and tests/lifecycle.rs did. ryll's
        // signalling test created its datachannel first; the m-line
        // ordering that produces is asserted identical in this
        // module's tests, so the two orderings were interchangeable
        // and the majority spelling wins.
        let seed_dc = match self.seed_data_channel {
            Some(label) => Some(pc.create_data_channel(&label, None).await?),
            None => None,
        };

        Ok(TestPeer {
            pc,
            _seed_dc: seed_dc,
        })
    }
}

/// The client half of a bridge exchange. See the module docs.
pub struct TestPeer {
    pc: Arc<RTCPeerConnection>,
    /// Held only to keep the seed datachannel alive for the life of
    /// the peer. Never read — its job was done when the offer was
    /// generated with an `m=application` section.
    _seed_dc: Option<Arc<RTCDataChannel>>,
}

impl TestPeer {
    /// Start building a peer. See [`TestPeerBuilder`].
    pub fn builder() -> TestPeerBuilder {
        TestPeerBuilder::new()
    }

    /// The underlying peer connection, for anything this type does
    /// not wrap — `on_track`, `on_data_channel`, statistics.
    pub fn pc(&self) -> &Arc<RTCPeerConnection> {
        &self.pc
    }

    /// Create an offer and set it as the local description, without
    /// waiting for ICE gathering.
    ///
    /// The returned SDP therefore carries no candidates. That is
    /// enough for tests that only inspect the *answer* the bridge
    /// produces, and it avoids paying for a gathering round-trip
    /// they do not need. Tests that go on to complete a handshake
    /// want [`Self::offer_and_gather`] instead.
    pub async fn create_offer(&self) -> Result<String> {
        let offer = self.pc.create_offer(None).await?;
        let sdp = offer.sdp.clone();
        self.pc.set_local_description(offer).await?;
        Ok(sdp)
    }

    /// Create an offer, set it as the local description, wait for ICE
    /// gathering to complete, and return the fully-resolved SDP.
    ///
    /// Mirrors what a browser does before POSTing its offer. The
    /// gathering wait is what makes the offer carry every candidate,
    /// which the non-trickle exchange the bridge implements depends
    /// on.
    pub async fn offer_and_gather(&self) -> Result<String> {
        self.create_offer().await?;

        let mut gather_complete = self.pc.gathering_complete_promise().await;
        let _ = gather_complete.recv().await;

        let local = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("local description missing after ICE gathering"))?;
        Ok(local.sdp)
    }

    /// Apply an SDP answer, completing the exchange on the client
    /// side.
    pub async fn set_remote_answer(&self, answer_sdp: String) -> Result<()> {
        let answer = RTCSessionDescription::answer(answer_sdp)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// The current peer connection state.
    pub fn connection_state(&self) -> RTCPeerConnectionState {
        self.pc.connection_state()
    }

    /// Poll until the peer reaches `Connected`, or `timeout` elapses.
    ///
    /// ICE and DTLS are symmetric, so a client reporting `Connected`
    /// means the server side has already completed its half of the
    /// handshake. Polling one end is sufficient for an in-process
    /// loopback.
    ///
    /// Returns the state observed on timeout in the error, because
    /// "did not connect" and "connected then failed" want different
    /// debugging.
    pub async fn wait_until_connected(&self, timeout: Duration) -> Result<()> {
        let outcome = tokio::time::timeout(timeout, async {
            loop {
                if self.connection_state() == RTCPeerConnectionState::Connected {
                    return;
                }
                tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
            }
        })
        .await;

        match outcome {
            Ok(()) => Ok(()),
            Err(_) => Err(anyhow!(
                "peer did not reach Connected within {:?} (state={:?})",
                timeout,
                self.connection_state(),
            )),
        }
    }

    /// Close the peer connection.
    pub async fn close(&self) -> Result<()> {
        self.pc.close().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the `m=` line kinds from an SDP, in order.
    fn m_line_kinds(sdp: &str) -> Vec<String> {
        sdp.lines()
            .filter_map(|l| l.strip_prefix("m="))
            .filter_map(|l| l.split_whitespace().next())
            .map(|s| s.to_owned())
            .collect()
    }

    /// Before this module existed, three of the four hand-rolled test
    /// peers added their transceivers before creating the seed
    /// datachannel and one (ryll's signalling test) did the reverse.
    /// [`TestPeerBuilder::build`] had to pick one, so this asserts the
    /// choice is not observable: the offer's m-line ordering is the
    /// same either way, with `m=application` last regardless.
    ///
    /// If this ever fails, the orderings are *not* interchangeable and
    /// the builder needs to preserve each call site's original
    /// sequence rather than normalising it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seed_dc_ordering_does_not_change_m_line_order() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Transceivers first, then the datachannel — what the builder
        // does.
        let transceivers_first = TestPeer::builder()
            .seed_data_channel("seed")
            .build()
            .await
            .expect("transceivers-first peer");
        let a = transceivers_first.create_offer().await.expect("offer a");

        // Datachannel first, then transceivers — hand-rolled here
        // because the builder deliberately does not expose the
        // choice.
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .expect("default codecs");
        register_h264(&mut media_engine).expect("h264");
        let mut registry = Registry::new();
        registry =
            register_default_interceptors(registry, &mut media_engine).expect("interceptors");
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("pc"),
        );
        let _dc = pc.create_data_channel("seed", None).await.expect("seed dc");
        for kind in [RTPCodecType::Video, RTPCodecType::Audio] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await
            .expect("transceiver");
        }
        let offer = pc.create_offer(None).await.expect("offer b");
        let b = offer.sdp.clone();

        assert_eq!(
            m_line_kinds(&a),
            m_line_kinds(&b),
            "seed-datachannel ordering changed the m-line sequence\na:\n{}\nb:\n{}",
            a,
            b,
        );

        transceivers_first.close().await.expect("close a");
        pc.close().await.expect("close b");
    }

    /// Codec lines from an SDP, in order.
    fn codec_lines(sdp: &str) -> Vec<String> {
        sdp.lines()
            .filter(|l| l.starts_with("a=rtpmap:") || l.starts_with("a=fmtp:"))
            .map(|s| s.to_owned())
            .collect()
    }

    /// [`register_h264`] currently has no observable effect once
    /// `register_default_codecs` has run: the defaults already
    /// register H.264 at PT 102, and webrtc-rs drops the explicit
    /// re-registration as a duplicate payload type. The bridge and
    /// this module both call it anyway, on the theory that the
    /// default codec set is feature-dependent.
    ///
    /// This test pins the current reality so a change is noticed.
    ///
    /// If it fails, `register_h264` has started to matter, and two
    /// things need checking: that every `TestPeer` call site still
    /// wants the bridge's exact H.264 registration (ryll's signalling
    /// test asserts on the *bridge's* advertised codecs, so a client
    /// that suddenly advertises different H.264 could mask a
    /// regression), and that the payload type the video pump stamps
    /// into RTP headers still matches what the SDP negotiated.
    ///
    /// Related, and deliberately not fixed here: the default PT 102
    /// entry carries `profile-level-id=42001f`, while
    /// `register_h264` asks for `42e01f`. Since the explicit
    /// registration is dropped, the bridge negotiates 42001f while
    /// its encoder emits 42e01f. Browsers tolerate the constraint-set
    /// difference, and untangling it is not this phase's job.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_h264_is_redundant_with_default_codecs() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        // What TestPeer builds: defaults plus the explicit H.264.
        let with = TestPeer::builder().build().await.expect("with h264");
        let with_sdp = with.create_offer().await.expect("offer with");

        // Defaults only, hand-rolled because the builder deliberately
        // does not expose the choice.
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .expect("default codecs");
        let mut registry = Registry::new();
        registry =
            register_default_interceptors(registry, &mut media_engine).expect("interceptors");
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("pc"),
        );
        for kind in [RTPCodecType::Video, RTPCodecType::Audio] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await
            .expect("transceiver");
        }
        let without_sdp = pc.create_offer(None).await.expect("offer without").sdp;

        assert_eq!(
            codec_lines(&with_sdp),
            codec_lines(&without_sdp),
            "register_h264 changed the advertised codecs; read this test's \
             doc comment before adjusting it",
        );

        with.close().await.expect("close with");
        pc.close().await.expect("close without");
    }
}
