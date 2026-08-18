//! A client-side [`PeerConnection`] for tests that need to drive
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
//! Every call in the setup those four shared changed shape in
//! webrtc-rs 0.20 (`APIBuilder`, `new_peer_connection`,
//! `add_transceiver_from_kind`, `gathering_complete_promise`,
//! `connection_state`). Collapsing them here meant the 0.20 port
//! rewrote this once rather than four times across two crates. See
//! `docs/plans/PLAN-webrtc-0.20-upgrade.md`.
//!
//! ## Availability
//!
//! Compiled for this crate's own unit tests, and for external
//! consumers that enable the `test-support` feature. It is not part
//! of the crate's production surface.
//!
//! ## What is deliberately not here
//!
//! `on_track` packet counting and datachannel echo handler *bodies*
//! live in the tests that need them — only `tests/loopback.rs` does,
//! and generalising a single use would make this type harder to read
//! for no gain. [`TestPeerBuilder::on_track_hook`] and
//! [`TestPeerBuilder::on_data_channel_hook`] accept those bodies and
//! register them at build time, which is where webrtc-rs 0.20 also
//! requires them to be supplied. Reach through [`TestPeer::pc`] for
//! anything neither hook nor this module covers.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use webrtc::data_channel::DataChannel;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCPeerConnectionState, RTCSessionDescription, Registry,
};
use webrtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};

use crate::bind_addrs::host_udp_bind_addrs;
use crate::bridge::register_h264;
use crate::sticky::StickySignal;

/// How often [`TestPeer::wait_until_connected`] re-checks the state.
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The type [`TestPeerBuilder::on_track_hook`] accepts.
///
/// Kept as a named alias so `tests/loopback.rs` names one type rather
/// than spelling out a boxed async closure. It was an alias for
/// webrtc-rs 0.17's own `OnTrackHdlrFn`; 0.20 has no equivalent public
/// type — `on_track` is a `PeerConnectionEventHandler` method taking
/// only the track, with the receiver and transceiver arguments gone —
/// so the alias is now written out here. That is the point of the
/// alias: this line changes, and the hook's callers keep their
/// shape.
pub type OnTrackHook = Box<
    dyn Fn(Arc<dyn TrackRemote>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
        + Send
        + Sync,
>;

/// The type [`TestPeerBuilder::on_data_channel_hook`] accepts. See
/// [`OnTrackHook`] for why this is an alias.
pub type OnDataChannelHook = Box<
    dyn Fn(Arc<dyn DataChannel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
        + Send
        + Sync,
>;

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
    on_track: Option<OnTrackHook>,
    on_data_channel: Option<OnDataChannelHook>,
    narrow_codecs: Option<NarrowCodecs>,
}

/// A deliberately narrow codec set for the offer, replacing
/// `register_default_codecs`. See
/// [`TestPeerBuilder::offer_only_h264_fmtp`].
struct NarrowCodecs {
    h264_fmtp: String,
    h264_payload_type: u8,
    opus_payload_type: u8,
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
    /// this. Found the hard way.
    pub fn seed_data_channel(mut self, label: &str) -> Self {
        self.seed_data_channel = Some(label.to_owned());
        self
    }

    /// Register a callback for incoming remote tracks, at build time.
    ///
    /// webrtc-rs 0.20 hands the event handler to the builder before
    /// the peer connection exists, so any test that needs `on_track`
    /// behaviour has to supply it here; there is no post-construction
    /// registration to reach through [`TestPeer::pc`] for. See
    /// `tests/loopback.rs` for the shape callers want: spawn a
    /// per-track reader rather than looping inline in the hook body,
    /// which would stall the driver.
    pub fn on_track_hook(mut self, f: OnTrackHook) -> Self {
        self.on_track = Some(f);
        self
    }

    /// Register a callback for incoming datachannels, at build time.
    /// See [`Self::on_track_hook`] for why build time rather than a
    /// post-construction reach-through. Note the hook receives an
    /// `Arc<dyn DataChannel>` and must poll it for messages — 0.20 has
    /// no `on_message` callback.
    ///
    /// No test currently uses this, and that is not an oversight: on
    /// 0.20 a peer that created its own datachannel before negotiation
    /// never sees an `on_data_channel` for the other end's, because
    /// both land on the same SCTP stream id. See
    /// [`TestPeer::seed_data_channel`], which is where a test wanting
    /// the remote peer's control messages should look. The hook stays
    /// because a channel created *after* negotiation does still arrive
    /// this way.
    pub fn on_data_channel_hook(mut self, f: OnDataChannelHook) -> Self {
        self.on_data_channel = Some(f);
        self
    }

    /// Offer exactly one H.264 entry, with `fmtp` at
    /// `h264_payload_type`, plus one Opus entry at
    /// `opus_payload_type` — instead of webrtc-rs's default codec set.
    ///
    /// This exists because the default `TestPeer` cannot catch a whole
    /// class of bug. It registers the same codecs as the bridge, so
    /// every payload type the bridge might stamp is negotiated and
    /// anything the bridge sends is accepted. Real browsers are not so
    /// accommodating: Chrome offers H.264 `42001f`, Firefox offers only
    /// `42e01f`, and the negotiated payload type differs accordingly
    /// because the core remaps each match onto whichever of our
    /// MediaEngine entries it matched.
    ///
    /// Offering a narrow set is what makes the remap observable, and
    /// distinct payload types on this side prove the numbers on the
    /// wire came from negotiation rather than from a constant that
    /// happened to agree.
    pub fn offer_only_h264_fmtp(
        mut self,
        fmtp: &str,
        h264_payload_type: u8,
        opus_payload_type: u8,
    ) -> Self {
        self.narrow_codecs = Some(NarrowCodecs {
            h264_fmtp: fmtp.to_owned(),
            h264_payload_type,
            opus_payload_type,
        });
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
        match &self.narrow_codecs {
            None => {
                media_engine.register_default_codecs()?;
                register_h264(&mut media_engine)?;
            }
            Some(narrow) => register_narrow_codecs(&mut media_engine, narrow)?,
        }

        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

        // Shadow the connection state rather than asking the peer
        // connection for it. `RTCPeerConnection::connection_state` was
        // an inherent method in 0.17 and has no replacement in 0.20 —
        // not on the `PeerConnection` trait, not on the sans-io core —
        // whereas the state-change callback survived the port as a
        // handler method.
        //
        // Starts at `New`, the state a fresh peer connection reports
        // before anything happens — asserted in this module's tests,
        // because a shadow that defaulted to `Connected` would make
        // every `wait_until_connected` return instantly and quietly
        // gut the tests that rely on it.
        //
        // A `std::sync::Mutex`, matching `WebrtcBridge`'s shadow of
        // the same state, so `connection_state()` stays synchronous.
        // The guard is only ever held across a single assignment or
        // read of a `Copy` enum, never across an await — which matters
        // because the writer runs inline in the driver event loop.
        let state = Arc::new(Mutex::new(RTCPeerConnectionState::New));

        // ICE gathering completion, on the same [`StickySignal`] the
        // bridge uses, and for the same reason:
        // `gathering_complete_promise` does not exist in webrtc-rs
        // 0.20.
        let gathered = Arc::new(StickySignal::new());

        // Caller-supplied on_track / on_data_channel hooks go into the
        // handler, which the builder needs *before* the peer connection
        // exists. That is not merely idiomatic on 0.20, it is the only
        // option — and it also happens to be the ordering these hooks
        // always needed, since a track or datachannel that arrives
        // before its handler is installed fires nothing.
        let events = Arc::new(TestPeerEvents {
            state: state.clone(),
            gathered: gathered.clone(),
            on_track: self.on_track,
            on_data_channel: self.on_data_channel,
        });

        // Bind the same interface addresses the bridge does; see
        // `crate::bind_addrs` for why not `0.0.0.0`. A host with
        // nothing but loopback cannot run these tests at all, so say so
        // here rather than let it surface as an unexplained handshake
        // timeout twenty seconds later.
        let udp_addrs = host_udp_bind_addrs();
        if udp_addrs.is_empty() {
            return Err(anyhow!(
                "no bindable network interface for the test peer: either enumeration failed \
                 or this host reports only loopback, unspecified or IPv6 link-local addresses \
                 — check for an earlier `host_udp_bind_addrs` warning to tell which"
            ));
        }

        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(RTCConfigurationBuilder::new().build())
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_handler(events)
                .with_udp_addrs(udp_addrs)
                .build()
                .await?,
        );

        for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    ..Default::default()
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
            seed_dc,
            state,
            gathered,
        })
    }
}

/// Register exactly the codecs [`TestPeerBuilder::offer_only_h264_fmtp`]
/// asked for, in place of webrtc-rs's defaults.
///
/// Registered directly rather than by filtering
/// `register_default_codecs`, because the point is to control the
/// payload *numbers* as well as the entries: a browser picks its own,
/// and the bridge has to cope with numbers that are not the ones it
/// registered.
fn register_narrow_codecs(media_engine: &mut MediaEngine, narrow: &NarrowCodecs) -> Result<()> {
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: narrow.h264_fmtp.clone(),
                rtcp_feedback: vec![],
            },
            payload_type: narrow.h264_payload_type,
        },
        RtpCodecKind::Video,
    )?;
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: narrow.opus_payload_type,
        },
        RtpCodecKind::Audio,
    )?;
    Ok(())
}

/// The single event handler [`TestPeerBuilder::build`] hands to
/// webrtc-rs, replacing the four separate callback registrations 0.17
/// wanted.
///
/// Every method here is awaited inline in the peer connection's driver
/// event loop, so none of them may block. The two built-in ones do a
/// mutex-guarded assignment and an atomic flag raise respectively; the
/// two caller-supplied hooks are the test's own business, and
/// `tests/loopback.rs` documents why its bodies spawn rather than loop.
struct TestPeerEvents {
    state: Arc<Mutex<RTCPeerConnectionState>>,
    gathered: Arc<StickySignal>,
    on_track: Option<OnTrackHook>,
    on_data_channel: Option<OnDataChannelHook>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for TestPeerEvents {
    async fn on_connection_state_change(&self, next: RTCPeerConnectionState) {
        // `into_inner` on poison: a single `Copy` assignment cannot
        // leave the value inconsistent, and dropping the write would
        // strand `wait_until_connected` on a stale state with no hint
        // as to why.
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.raise();
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if let Some(hook) = &self.on_track {
            hook(track).await;
        }
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        if let Some(hook) = &self.on_data_channel {
            hook(dc).await;
        }
    }
}

/// The client half of a bridge exchange. See the module docs.
pub struct TestPeer {
    pc: Arc<dyn PeerConnection>,
    /// The seed datachannel, if [`TestPeerBuilder::seed_data_channel`]
    /// asked for one. Held so it outlives the offer that needed it,
    /// and exposed by [`Self::seed_data_channel`] because on 0.20 it
    /// is also where the *remote* peer's control messages arrive; see
    /// that accessor's docs.
    seed_dc: Option<Arc<dyn DataChannel>>,
    /// Latest state seen by the state-change callback. See the
    /// comment where it is registered for why this is shadowed
    /// rather than read from the peer connection.
    state: Arc<Mutex<RTCPeerConnectionState>>,
    /// Raised once when ICE gathering completes; sticky so a late
    /// waiter returns immediately.
    gathered: Arc<StickySignal>,
}

impl TestPeer {
    /// Start building a peer. See [`TestPeerBuilder`].
    pub fn builder() -> TestPeerBuilder {
        TestPeerBuilder::new()
    }

    /// The underlying peer connection, for anything this type does
    /// not wrap — statistics, ICE restarts, and the like.
    ///
    /// Events are not among those things and cannot be reached from
    /// here. webrtc-rs 0.20 takes one
    /// [`PeerConnectionEventHandler`] at build time and offers no way
    /// to add or replace a handler afterwards, so
    /// [`TestPeerBuilder::on_track_hook`] and
    /// [`TestPeerBuilder::on_data_channel_hook`] are the only routes to
    /// on-track and on-data-channel behaviour. (The upside of the
    /// inversion: the last-writer-wins footgun this doc comment used to
    /// warn about no longer exists.)
    pub fn pc(&self) -> &Arc<dyn PeerConnection> {
        &self.pc
    }

    /// The seed datachannel, if one was requested.
    ///
    /// Poll this to receive what the *remote* peer sends, and send on
    /// it to reach the remote peer. That is not obvious, and it is
    /// worth understanding before writing a test against it.
    ///
    /// webrtc-rs 0.20 assigns a datachannel's SCTP stream id at
    /// creation time from the DTLS role
    /// (`rtc-0.20.2/src/peer_connection/internal.rs:936-954`), and
    /// before the handshake there is no role yet, so every channel
    /// created ahead of negotiation lands on stream 1. Both ends of an
    /// exchange do exactly that — this peer's seed channel and the
    /// bridge's control channel are the same stream — so each side's
    /// channel is already present in its own id map when the peer's
    /// DCEP open arrives, and
    /// [`PeerConnectionEventHandler::on_data_channel`] is never
    /// announced for it
    /// (`webrtc-0.20.2/src/peer_connection/driver.rs:84-101`).
    ///
    /// This is what the browser sees too: `ryll/src/web/assets/app.js`
    /// creates one `control-seed` channel, registers `onmessage` on it,
    /// and has no `ondatachannel` handler at all. Polling the seed
    /// channel is therefore the *more* faithful test of production, not
    /// a workaround. [`TestPeerBuilder::on_data_channel_hook`] remains
    /// for a genuinely new remote channel — one created after
    /// negotiation, when the ids no longer collide.
    pub fn seed_data_channel(&self) -> Option<&Arc<dyn DataChannel>> {
        self.seed_dc.as_ref()
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
    ///
    /// One exchange per peer: the gathering signal is sticky and
    /// never resets, so a second call on the same `TestPeer` would
    /// not wait for a re-gathering round. Renegotiation needs a fresh
    /// peer — the same constraint as `WebrtcBridge::accept_offer`.
    pub async fn offer_and_gather(&self) -> Result<String> {
        self.create_offer().await?;

        // Sticky wait; see `StickySignal` for the lost-wakeup
        // reasoning this encapsulates.
        self.gathered.wait().await;

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

    /// The most recent peer connection state observed by the
    /// state-change callback.
    pub fn connection_state(&self) -> RTCPeerConnectionState {
        // `into_inner` on poison for the same reason as the writer:
        // a `Copy` enum can never be left inconsistent, and the real
        // value beats a made-up one.
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Poll until the peer reaches `Connected`, or `timeout` elapses.
    ///
    /// ICE and DTLS are symmetric, so a client reporting `Connected`
    /// means the server side has already completed its half of the
    /// handshake. Polling one end is sufficient for an in-process
    /// loopback.
    ///
    /// A peer observed at `Failed` or `Closed` before `Connected`
    /// errors immediately rather than burning the full timeout — in
    /// CI that turns a 20-second dead wait into a sub-second failure
    /// with the observed state in the message. (The message says
    /// "while waiting for", not "without ever connecting": a 50 ms
    /// poll cadence cannot rule out a connect-then-close inside one
    /// interval.) The timeout path also reports the state it saw,
    /// because "did not connect" and "connected then failed" want
    /// different debugging.
    pub async fn wait_until_connected(&self, timeout: Duration) -> Result<()> {
        let outcome = tokio::time::timeout(timeout, async {
            loop {
                match self.connection_state() {
                    RTCPeerConnectionState::Connected => return Ok(()),
                    state @ (RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) => {
                        return Err(anyhow!(
                            "peer reached {state:?} while waiting for Connected"
                        ));
                    }
                    _ => {}
                }
                tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
            }
        })
        .await;

        match outcome {
            Ok(result) => result,
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

    /// A handler that ignores everything.
    ///
    /// `with_handler` is mandatory — `PeerConnectionBuilder::build`
    /// errors without one — so the comparison peers below, which exist
    /// only to produce an offer SDP and are never connected, still need
    /// something to pass.
    struct IgnoreEvents;

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for IgnoreEvents {}

    /// Build a bare peer connection with the given MediaEngine.
    ///
    /// Shared by the two tests that deliberately hand-roll a peer to
    /// compare against [`TestPeerBuilder`]'s output; the transceivers
    /// and datachannels each one wants differ, so only the plumbing is
    /// factored out.
    async fn raw_peer(mut media_engine: MediaEngine) -> Arc<dyn PeerConnection> {
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .expect("interceptors");
        // Same guard as `TestPeerBuilder::build` and `WebrtcBridge::new`.
        // Without it a loopback-only host fails these two tests with an
        // opaque builder error while every other test in the file
        // explains itself.
        let udp_addrs = host_udp_bind_addrs();
        assert!(
            !udp_addrs.is_empty(),
            "no bindable network interface for the test peer: either enumeration failed or this \
             host reports only loopback, unspecified or IPv6 link-local addresses — check for an \
             earlier `host_udp_bind_addrs` warning to tell which"
        );
        Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(RTCConfigurationBuilder::new().build())
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_handler(Arc::new(IgnoreEvents))
                .with_udp_addrs(udp_addrs)
                .build()
                .await
                .expect("pc"),
        )
    }

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
        let pc = raw_peer(media_engine).await;
        let _dc = pc.create_data_channel("seed", None).await.expect("seed dc");
        for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    ..Default::default()
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

    /// The shadowed state has to start at `New`, not `Connected`.
    ///
    /// If it defaulted to `Connected`, every `wait_until_connected`
    /// in the suite would return instantly and the loopback and
    /// lifecycle tests would pass without ever completing a
    /// handshake — the worst kind of green.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shadowed_state_starts_at_new() {
        let peer = TestPeer::builder().build().await.expect("peer");
        assert_eq!(
            peer.connection_state(),
            RTCPeerConnectionState::New,
            "a freshly built peer must report New",
        );

        // And a peer that never connects must actually time out
        // rather than sailing through.
        let err = peer
            .wait_until_connected(Duration::from_millis(200))
            .await
            .expect_err("an unconnected peer must not report Connected");
        assert!(
            err.to_string().contains("did not reach Connected"),
            "unexpected error: {}",
            err
        );

        peer.close().await.expect("close");
    }

    /// A peer at a terminal state fails `wait_until_connected` fast,
    /// with the observed state in the message, instead of burning
    /// the full timeout. Pins the early-exit arm added after the
    /// PR #272 review noted it had no coverage.
    ///
    /// The shadow is set directly rather than by closing the peer and
    /// waiting for the state-change handler to report `Closed`. On
    /// webrtc-rs 0.20 that no longer reliably happens to the side that
    /// initiated the close: `close()` raises the driver's shutdown flag
    /// (`webrtc-0.20.2/src/peer_connection/mod.rs:868-885`), and the
    /// driver checks it at the top of every loop iteration
    /// (`driver.rs:313-323`), so it can exit before dispatching the
    /// `Closed` transition the core queued. The old spelling of this
    /// test was therefore a race — it passed roughly half the time.
    ///
    /// Writing the shadow is legitimate here rather than a cheat: the
    /// arm under test is a pure function of the shadow, and the *other*
    /// direction (a peer observing its remote's teardown, which does
    /// deliver the transition) is what `tests/lifecycle.rs` covers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_until_connected_fails_fast_on_terminal_state() {
        let peer = TestPeer::builder().build().await.expect("peer");
        *peer
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = RTCPeerConnectionState::Closed;

        let started = std::time::Instant::now();
        let err = peer
            .wait_until_connected(Duration::from_secs(20))
            .await
            .expect_err("a closed peer must not report Connected");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "terminal state should fail fast, not burn the timeout",
        );
        assert!(
            err.to_string()
                .contains("reached Closed while waiting for Connected"),
            "unexpected error: {}",
            err
        );

        peer.close().await.expect("close");
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
        // What TestPeer builds: defaults plus the explicit H.264.
        let with = TestPeer::builder().build().await.expect("with h264");
        let with_sdp = with.create_offer().await.expect("offer with");

        // Defaults only, hand-rolled because the builder deliberately
        // does not expose the choice.
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .expect("default codecs");
        let pc = raw_peer(media_engine).await;
        for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    ..Default::default()
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
