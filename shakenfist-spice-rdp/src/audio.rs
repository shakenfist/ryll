//! The audio output channel: the guest's audio, played on the RDP
//! client.
//!
//! The session's playback channel hands its audio to an
//! [`AudioRelay`], a [`PlaybackSink`] that opens no device and
//! publishes what it is given on a small broadcast channel. IronRDP
//! builds a [`SessionSound`] handler for every connection from the
//! [`SoundFactory`] registered with the server; once the client has
//! agreed a format, the handler subscribes to the session's relay and
//! spawns a forwarder that turns the relay's audio into waves. The
//! relay copies only the audio some forwarder plays: Opus packets while
//! one sends Opus, and PCM while one sends PCM.
//!
//! Each wave goes to the server's event sender, which the connection's
//! loop applies to the channel with `?`: a wave the channel refuses
//! ends the whole RDP connection, not just the audio. The channel
//! refuses waves until a format has been chosen, which is when IronRDP
//! calls [`RdpsndServerHandler::start`], so nothing can send before
//! then: until `start` the handler holds the sender in a form with no
//! way to send on it (see [`gate`]). Subscribing at `start` also means
//! no audio from before the client could hear it is ever queued.
//!
//! The server chooses the format from those the client accepted. It
//! offers Opus and then PCM, both 48 kHz stereo, and takes the first
//! the client accepted. Opus costs nothing to pass through, since
//! SPICE sends Opus, and is a few tens of kbit/s; 16-bit PCM is about
//! 1.5 Mbit/s. Only 48 kHz stereo is offered because decoded Opus is
//! 48 kHz and nothing here resamples, so a SPICE stream that does not
//! fit the chosen format is not played.
//!
//! With Opus, each SPICE packet is one wave, unchanged. With PCM, the
//! decoded samples are gathered into waves of at least 20 ms, because
//! spice-server also sends packets of about 5 ms and IronRDP keeps
//! only the last four waves of a batch that queued up while the
//! connection was busy. A wave's timestamp counts the milliseconds of
//! audio sent before it.
//!
//! Neither volume nor Close is ever sent: SPICE gives ryll no volume
//! to pass on, and a volume change the client did not ask for would
//! end the connection. When the guest stops playing, the client stops
//! receiving waves.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{RdpsndServerHandler, ServerEvent, ServerEventSender, SoundServerFactory};
use shakenfist_spice_renderer::{PlaybackFormat, PlaybackSink};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::SessionSlot;

/// How many items the relay holds for a forwarder that has fallen
/// behind before it drops the oldest. Only the kind of audio the
/// forwarder plays is published, one item for each SPICE packet, so
/// this is about sixteen 20 ms packets: a third of a second. Audio that
/// late is better dropped than played, and the forwarder only turns
/// items into waves, so it falls behind only while the RDP server
/// thread is stalled.
const RELAY_CAPACITY: usize = 16;

/// The sample rate of every format offered, and of decoded Opus.
const SAMPLE_RATE_HZ: u32 = 48_000;

/// The channel count of every format offered.
const CHANNELS: u16 = 2;

/// The fewest frames (samples per channel) a PCM wave carries: 20 ms
/// at 48 kHz.
const PCM_WAVE_FRAMES: usize = 960;

/// One piece of what the session's playback channel played.
#[derive(Debug, Clone)]
pub(crate) enum AudioItem {
    /// A stream started, or a running one changed its data mode.
    Start(PlaybackFormat),
    /// An Opus packet, as SPICE sent it, and the 48 kHz frames it
    /// holds.
    Opus { packet: Arc<[u8]>, frames: u32 },
    /// The interleaved samples one Opus packet decoded to.
    Decoded {
        samples: Arc<[i16]>,
        rate_hz: u32,
        channels: u8,
    },
    /// Interleaved samples from a SPICE server that sends raw PCM.
    /// Published with the decoded samples, to forwarders that send PCM.
    Raw {
        samples: Arc<[i16]>,
        rate_hz: u32,
        channels: u8,
    },
    /// The stream stopped.
    Stop,
}

/// The session's audio, for the RDP client: a [`PlaybackSink`] that
/// publishes what the playback channel plays to whichever connection
/// is listening.
///
/// Give the same relay to the session as its
/// [`PlaybackOutput::Sink`](shakenfist_spice_renderer::PlaybackOutput::Sink)
/// and to [`SessionHandles::audio`](crate::SessionHandles::audio).
/// Every callback is non-blocking, and audio played while no client is
/// listening is dropped. The relay keeps the current stream's format,
/// so a client that connects while the guest is playing knows it.
pub struct AudioRelay {
    items: broadcast::Sender<AudioItem>,
    /// The running stream's format, or `None` between streams. Changed
    /// with its item sent while locked, so a subscriber sees either
    /// the old format and the change, or only the new format.
    format: Mutex<Option<PlaybackFormat>>,
    wanted: Arc<Wanted>,
}

/// How many forwarders send each codec, so that the relay copies only
/// the audio one of them plays.
#[derive(Debug, Default)]
struct Wanted {
    opus: AtomicUsize,
    pcm: AtomicUsize,
}

impl Wanted {
    fn count(&self, codec: Codec) -> &AtomicUsize {
        match codec {
            Codec::Opus => &self.opus,
            Codec::Pcm => &self.pcm,
        }
    }
}

/// A forwarder's claim on the audio its codec plays, from
/// [`AudioRelay::subscribe`] until this is dropped.
pub(crate) struct Subscription {
    wanted: Arc<Wanted>,
    codec: Codec,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.wanted
            .count(self.codec)
            .fetch_sub(1, Ordering::Release);
    }
}

impl AudioRelay {
    /// A relay with no stream running and no one listening.
    pub fn new() -> Self {
        let (items, _) = broadcast::channel(RELAY_CAPACITY);
        Self {
            items,
            format: Mutex::new(None),
            wanted: Arc::default(),
        }
    }

    /// The running stream's format, or `None` if the guest is not
    /// playing.
    pub fn format(&self) -> Option<PlaybackFormat> {
        *self.lock_format()
    }

    /// Listen from now on for audio to send as `codec`: the running
    /// stream's format, if any, and every item published after it. The
    /// relay publishes that codec's audio until the subscription is
    /// dropped.
    pub(crate) fn subscribe(
        &self,
        codec: Codec,
    ) -> (
        Option<PlaybackFormat>,
        broadcast::Receiver<AudioItem>,
        Subscription,
    ) {
        let format = self.lock_format();
        let items = self.items.subscribe();
        // Counted only once the receiver exists, so audio published by
        // a thread that sees the count reaches it.
        self.wanted.count(codec).fetch_add(1, Ordering::Release);
        let subscription = Subscription {
            wanted: Arc::clone(&self.wanted),
            codec,
        };
        (*format, items, subscription)
    }

    /// Whether some forwarder sends `codec`, so that audio no one will
    /// hear is not copied.
    fn wants(&self, codec: Codec) -> bool {
        self.wanted.count(codec).load(Ordering::Acquire) > 0
    }

    /// Publish `item`. With no one listening it is dropped, which is
    /// not an error.
    fn publish(&self, item: AudioItem) {
        let _ = self.items.send(item);
    }

    /// Lock the format. A panic while the lock was held cannot leave
    /// the `Option` half-written, so a poisoned lock is recovered
    /// rather than propagated.
    fn lock_format(&self) -> std::sync::MutexGuard<'_, Option<PlaybackFormat>> {
        self.format
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for AudioRelay {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AudioRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioRelay")
            .field("format", &self.format())
            .field("wanted", &self.wanted)
            .finish()
    }
}

impl PlaybackSink for AudioRelay {
    fn on_start(&self, format: PlaybackFormat) {
        let mut current = self.lock_format();
        *current = Some(format);
        self.publish(AudioItem::Start(format));
    }

    fn on_opus_packet(&self, packet: &[u8], samples_in_packet: u32) {
        if self.wants(Codec::Opus) {
            self.publish(AudioItem::Opus {
                packet: packet.into(),
                frames: samples_in_packet,
            });
        }
    }

    fn on_decoded_pcm(&self, samples: &[i16], sample_rate_hz: u32, channels: u8) {
        if self.wants(Codec::Pcm) {
            self.publish(AudioItem::Decoded {
                samples: samples.into(),
                rate_hz: sample_rate_hz,
                channels,
            });
        }
    }

    fn on_pcm_samples(&self, samples: &[i16], sample_rate_hz: u32, channels: u8) {
        if self.wants(Codec::Pcm) {
            self.publish(AudioItem::Raw {
                samples: samples.into(),
                rate_hz: sample_rate_hz,
                channels,
            });
        } else if self.wants(Codec::Opus) {
            warn_raw_pcm_for_opus();
        }
    }

    fn on_stop(&self) {
        let mut current = self.lock_format();
        *current = None;
        self.publish(AudioItem::Stop);
    }
}

/// The formats offered to every client, most preferred first.
///
/// Opus has no fixed byte rate or block size, so its two derived
/// fields describe the 16-bit stereo PCM it decodes to; IronRDP
/// matches a client's reply on the other fields, and a client replies
/// with the formats it accepts as they were offered. Neither format
/// carries extra data.
fn offered_formats() -> Vec<AudioFormat> {
    let format = |tag| AudioFormat {
        format: tag,
        n_channels: CHANNELS,
        n_samples_per_sec: SAMPLE_RATE_HZ,
        n_avg_bytes_per_sec: SAMPLE_RATE_HZ * u32::from(CHANNELS) * 2,
        n_block_align: CHANNELS * 2,
        bits_per_sample: 16,
        data: None,
    };
    vec![format(WaveFormat::OPUS), format(WaveFormat::PCM)]
}

/// Raw PCM is not played to a client that chose Opus.
fn warn_raw_pcm_for_opus() {
    shakenfist_spice_protocol::warn_once!(
        "rdp-audio:raw:opus-chosen",
        "rdp audio: the SPICE server sends raw PCM but the client chose Opus, which ryll does not \
         encode; not played"
    );
}

/// What the client's waves carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    /// SPICE's Opus packets, unchanged.
    Opus,
    /// 16-bit little-endian interleaved stereo PCM at 48 kHz.
    Pcm,
}

impl Codec {
    /// The codec of an offered format.
    fn of(format: &AudioFormat) -> Option<Self> {
        match format.format {
            WaveFormat::OPUS => Some(Self::Opus),
            WaveFormat::PCM => Some(Self::Pcm),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Pcm => "PCM",
        }
    }
}

/// Builds a [`SessionSound`] for each RDP connection.
///
/// The server gives the factory its event sender once, when the
/// server is built, and each handler gets a clone of it.
pub(crate) struct SoundFactory {
    slot: SessionSlot,
    events: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl SoundFactory {
    /// A factory whose handlers play the audio of the session in
    /// `slot`.
    pub(crate) fn new(slot: SessionSlot) -> Self {
        Self { slot, events: None }
    }
}

impl SoundServerFactory for SoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(SessionSound::new(self.slot.clone(), self.events.clone()))
    }
}

impl ServerEventSender for SoundFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.events = Some(sender);
    }
}

/// The only way to send a wave to the client.
///
/// Kept in a module of its own so that the event sender's fields are
/// private to it: the rest of this file can reach the sender only
/// through [`Started`], and gets a `Started` only by giving up an
/// [`Unstarted`], which [`SessionSound::begin`] alone does, from
/// [`RdpsndServerHandler::start`]. So no code path can send before a
/// format is chosen, rather than each send site checking that one is.
/// `begin` moves the `Started` into the forwarder task, and leaving
/// the streaming state aborts that task; the server's runtime runs on
/// one thread, so the task is never mid-poll when `stop` runs, and
/// once aborted it is never polled again. A `Started` sends only
/// waves.
mod gate {
    use ironrdp_server::{RdpsndServerMessage, ServerEvent};
    use tokio::sync::mpsc;

    /// The server's event sender, held until a format is chosen. It
    /// offers no way to send.
    pub(super) struct Unstarted(mpsc::UnboundedSender<ServerEvent>);

    impl Unstarted {
        pub(super) fn new(events: mpsc::UnboundedSender<ServerEvent>) -> Self {
            Self(events)
        }

        /// A format has been chosen: send waves from now on.
        pub(super) fn start(self) -> Started {
            Started(self.0)
        }
    }

    /// A channel with a chosen format.
    pub(super) struct Started(mpsc::UnboundedSender<ServerEvent>);

    impl Started {
        /// Send a wave of `data`, stamped `timestamp`, to the
        /// connection's loop, which applies it to the channel. Returns
        /// false once the server has gone, when there is no one left
        /// to send to.
        pub(super) fn send_wave(&self, data: Vec<u8>, timestamp: u32) -> bool {
            self.0
                .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                    data, timestamp,
                )))
                .is_ok()
        }
    }
}

/// Where a connection's audio channel is.
enum State {
    /// Before a format is chosen. `None` if the server never gave the
    /// factory a sender, which leaves nothing to send with.
    Waiting(Option<gate::Unstarted>),
    /// A forwarder is sending the session's audio. Held only to be
    /// dropped, which aborts the forwarder and ends its subscription.
    Streaming { _forwarder: Forwarding },
    /// The channel has been torn down, or it could not start.
    Stopped,
}

/// The forwarder task, aborted when this is dropped, and its
/// subscription to the relay, which ends with it then rather than when
/// the runtime next gets to the aborted task.
struct Forwarding {
    task: JoinHandle<()>,
    _subscription: Subscription,
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Why [`RdpsndServerHandler::start`] declined the chosen format.
#[derive(Debug)]
struct StartError(&'static str);

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for StartError {}

/// The audio channel handler for one RDP connection.
pub(crate) struct SessionSound {
    slot: SessionSlot,
    formats: Vec<AudioFormat>,
    state: State,
}

impl SessionSound {
    fn new(slot: SessionSlot, events: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        Self {
            slot,
            formats: offered_formats(),
            state: State::Waiting(events.map(gate::Unstarted::new)),
        }
    }

    /// Start sending the session's audio as `codec`: subscribe to the
    /// relay and spawn the forwarder.
    fn begin(&mut self, codec: Codec) -> Result<(), StartError> {
        let state = std::mem::replace(&mut self.state, State::Stopped);
        let State::Waiting(Some(unstarted)) = state else {
            self.state = state;
            return Err(StartError("rdp audio: no event sender, or already started"));
        };
        let Some(relay) = self
            .slot
            .current()
            .and_then(|session| session.audio.clone())
        else {
            return Err(StartError("rdp audio: the session has no audio"));
        };
        // IronRDP calls the handler from inside the server's runtime.
        // Failing here declines the format cleanly: the channel then
        // takes no waves and none are sent.
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| StartError("rdp audio: no runtime to stream on"))?;
        let (stream, items, subscription) = relay.subscribe(codec);
        debug!(?stream, "rdp audio: listening to the session's audio");
        let forwarder = Forwarder::new(unstarted.start(), codec);
        self.state = State::Streaming {
            _forwarder: Forwarding {
                task: runtime.spawn(forward(forwarder, items)),
                _subscription: subscription,
            },
        };
        info!(
            "rdp audio: streaming {} at {} Hz, {} channels, to the client",
            codec.name(),
            SAMPLE_RATE_HZ,
            CHANNELS
        );
        Ok(())
    }
}

impl std::fmt::Debug for SessionSound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.state {
            State::Waiting(_) => "waiting",
            State::Streaming { .. } => "streaming",
            State::Stopped => "stopped",
        };
        f.debug_struct("SessionSound")
            .field("state", &state)
            .finish()
    }
}

impl RdpsndServerHandler for SessionSound {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    /// The first format the client accepted, in the order offered, or
    /// none if the session has no audio.
    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        let has_audio = self
            .slot
            .current()
            .is_some_and(|session| session.audio.is_some());
        if !has_audio {
            info!("rdp audio: the session has no audio; declining every format");
            return None;
        }
        common
            .iter()
            .find(|negotiated| Codec::of(negotiated.format()).is_some())
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let started = match Codec::of(format.format()) {
            Some(codec) => self.begin(codec),
            None => Err(StartError("rdp audio: a format that was not offered")),
        };
        started.map_err(|error| Box::new(error) as Box<dyn RdpsndError>)
    }

    /// The channel is torn down with its connection: the forwarder
    /// stops, and nothing more is sent.
    fn stop(&mut self) {
        self.state = State::Stopped;
        debug!("rdp audio: stopped");
    }
}

/// The audio that is ever sent, from the session's relay, as waves in
/// the chosen codec.
struct Forwarder {
    channel: gate::Started,
    codec: Codec,
    /// Interleaved stereo PCM gathered for the next wave.
    pcm: Vec<i16>,
    /// 48 kHz frames sent since the forwarder started, from which each
    /// wave's timestamp is derived.
    frames_sent: u64,
}

impl Forwarder {
    fn new(channel: gate::Started, codec: Codec) -> Self {
        Self {
            channel,
            codec,
            pcm: Vec::with_capacity(PCM_WAVE_FRAMES * 4),
            frames_sent: 0,
        }
    }

    /// Turn `item` into waves, if it fits the chosen codec. Returns
    /// false once the server has gone.
    fn handle(&mut self, item: AudioItem) -> bool {
        match (item, self.codec) {
            (AudioItem::Start(format), _) => {
                debug!(?format, "rdp audio: the guest's stream started");
                true
            }
            (AudioItem::Stop, _) => {
                debug!("rdp audio: the guest's stream stopped");
                self.flush_pcm()
            }
            // An Opus packet is one wave, unchanged. A packet from a
            // mono stream needs no conversion: an Opus decoder set up
            // for stereo decodes mono packets.
            (AudioItem::Opus { packet, frames }, Codec::Opus) => {
                self.send(packet.to_vec(), u64::from(frames))
            }
            // Published only while another forwarder sends the other
            // codec. A PCM forwarder plays the packet's decoded samples
            // instead, and an Opus one has sent the packet they came
            // from.
            (AudioItem::Opus { .. }, Codec::Pcm) => true,
            (AudioItem::Decoded { .. }, Codec::Opus) => true,
            (
                AudioItem::Decoded {
                    samples,
                    rate_hz,
                    channels,
                },
                Codec::Pcm,
            ) => {
                if fits_pcm(rate_hz, channels) {
                    self.gather_pcm(&samples)
                } else {
                    shakenfist_spice_protocol::warn_once!(
                        "rdp-audio:decoded:shape",
                        "rdp audio: the guest's decoded audio is {} Hz, {} channels, not {} Hz stereo; not played",
                        rate_hz,
                        channels,
                        SAMPLE_RATE_HZ
                    );
                    true
                }
            }
            // Published only while another forwarder sends PCM.
            (AudioItem::Raw { .. }, Codec::Opus) => {
                warn_raw_pcm_for_opus();
                true
            }
            (
                AudioItem::Raw {
                    samples,
                    rate_hz,
                    channels,
                },
                Codec::Pcm,
            ) => {
                if fits_pcm(rate_hz, channels) {
                    self.gather_pcm(&samples)
                } else {
                    shakenfist_spice_protocol::warn_once!(
                        "rdp-audio:raw:shape",
                        "rdp audio: the SPICE server's raw PCM is {} Hz, {} channels, not {} Hz stereo, \
                         and ryll does not resample; not played",
                        rate_hz,
                        channels,
                        SAMPLE_RATE_HZ
                    );
                    true
                }
            }
        }
    }

    /// Add interleaved stereo `samples` to the next PCM wave, and send
    /// it once it holds at least [`PCM_WAVE_FRAMES`].
    fn gather_pcm(&mut self, samples: &[i16]) -> bool {
        self.pcm.extend_from_slice(samples);
        if self.pcm.len() >= PCM_WAVE_FRAMES * usize::from(CHANNELS) {
            self.flush_pcm()
        } else {
            true
        }
    }

    /// Send whatever PCM has been gathered as one wave of
    /// little-endian samples.
    fn flush_pcm(&mut self) -> bool {
        if self.pcm.is_empty() {
            return true;
        }
        let frames = self.pcm.len() / usize::from(CHANNELS);
        let data = self
            .pcm
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        self.pcm.clear();
        self.send(data, frames as u64)
    }

    /// Send a wave of `data` holding `frames` frames, stamped with the
    /// audio sent before it.
    fn send(&mut self, data: Vec<u8>, frames: u64) -> bool {
        let timestamp = wave_timestamp(self.frames_sent);
        self.frames_sent = self.frames_sent.saturating_add(frames);
        self.channel.send_wave(data, timestamp)
    }
}

/// Whether PCM of this shape can go out as the offered PCM format.
fn fits_pcm(rate_hz: u32, channels: u8) -> bool {
    rate_hz == SAMPLE_RATE_HZ && u16::from(channels) == CHANNELS
}

/// A wave's timestamp: the milliseconds of 48 kHz audio in
/// `frames_sent`, wrapping at `u32::MAX`.
fn wave_timestamp(frames_sent: u64) -> u32 {
    let millis = frames_sent.saturating_mul(1000) / u64::from(SAMPLE_RATE_HZ);
    // Keeping the low 32 bits is the wrap.
    (millis & u64::from(u32::MAX)) as u32
}

/// Forward the session's audio from `items` until the relay or the
/// server goes away, or the forwarder is aborted.
async fn forward(mut forwarder: Forwarder, mut items: broadcast::Receiver<AudioItem>) {
    loop {
        match items.recv().await {
            Ok(item) => {
                if !forwarder.handle(item) {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                shakenfist_spice_protocol::warn_once!(
                    "rdp-audio:lagged",
                    "rdp audio: dropped {} pieces of the guest's audio that arrived while the RDP \
                     server was busy",
                    missed
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ironrdp_core::encode_vec;
    use ironrdp_rdpsnd::pdu::{
        AudioFormatFlags, ClientAudioFormatPdu, ClientAudioOutputPdu, QualityMode, QualityModePdu,
        TrainingConfirmPdu, Version,
    };
    use ironrdp_rdpsnd::server::RdpsndServer;
    use ironrdp_server::RdpsndServerMessage;
    use ironrdp_svc::SvcProcessor;
    use shakenfist_spice_protocol::constants::audio_data_mode;
    use shakenfist_spice_renderer::SurfaceMirror;
    use tokio::sync::watch;

    use super::*;
    use crate::{CursorState, SessionHandles};

    /// A slot holding a session with `audio`.
    fn session_slot(audio: Option<Arc<AudioRelay>>) -> SessionSlot {
        let (input_tx, _) = mpsc::channel(1);
        let (_, mouse_mode) = watch::channel(0);
        let (_, cursor) = watch::channel(CursorState::default());
        let (resize_tx, _) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 16, 16, &[]);
        let slot = SessionSlot::new();
        slot.set(Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            cursor,
            clipboard: None,
            audio,
            label: "test".to_string(),
        }));
        slot
    }

    /// IronRDP's own audio channel, driven through the handshake with
    /// a handler built by the factory, and the receiving end of the
    /// server's event sender.
    struct Harness {
        server: RdpsndServer,
        events: mpsc::UnboundedReceiver<ServerEvent>,
        relay: Arc<AudioRelay>,
    }

    impl Harness {
        /// A channel that has offered its formats, for a session with
        /// or without audio.
        fn new(with_audio: bool) -> Self {
            let relay = Arc::new(AudioRelay::new());
            let slot = session_slot(with_audio.then(|| relay.clone()));
            let (events_tx, events) = mpsc::unbounded_channel();
            let mut factory = SoundFactory::new(slot);
            factory.set_sender(events_tx);
            let mut server = RdpsndServer::new(factory.build_backend());
            server.start().unwrap();
            Self {
                server,
                events,
                relay,
            }
        }

        /// The client's reply, up to the Training Confirm that makes
        /// the server choose a format and start.
        fn accept_formats(&mut self, formats: Vec<AudioFormat>) {
            self.client_pdu(ClientAudioOutputPdu::AudioFormat(ClientAudioFormatPdu {
                version: Version::V8,
                flags: AudioFormatFlags::ALIVE,
                formats,
                volume_left: 0xFFFF,
                volume_right: 0xFFFF,
                pitch: 0x0001_0000,
                dgram_port: 0,
            }));
            self.client_pdu(ClientAudioOutputPdu::QualityMode(QualityModePdu {
                quality_mode: QualityMode::High,
            }));
        }

        fn confirm_training(&mut self) {
            self.client_pdu(ClientAudioOutputPdu::TrainingConfirm(TrainingConfirmPdu {
                timestamp: 4231,
                pack_size: 0,
            }));
        }

        /// A channel that has started with whichever of `formats` the
        /// server prefers.
        fn started(with_audio: bool, formats: Vec<AudioFormat>) -> Self {
            let mut h = Self::new(with_audio);
            h.accept_formats(formats);
            h.confirm_training();
            h
        }

        fn client_pdu(&mut self, pdu: ClientAudioOutputPdu) {
            self.server.process(&encode_vec(&pdu).unwrap()).unwrap();
        }
    }

    fn opus() -> AudioFormat {
        offered_formats().remove(0)
    }

    fn pcm() -> AudioFormat {
        offered_formats().remove(1)
    }

    fn opus_stream() -> PlaybackFormat {
        PlaybackFormat {
            mode: audio_data_mode::OPUS,
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: 2,
        }
    }

    /// The next wave sent, as its data and timestamp, or `None` if
    /// nothing was sent. Time is paused, so the wait costs nothing and
    /// ends as soon as every task is idle.
    async fn next_wave(
        events: &mut mpsc::UnboundedReceiver<ServerEvent>,
    ) -> Option<(Vec<u8>, u32)> {
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Some(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)))) => {
                Some((data, timestamp))
            }
            Ok(Some(other)) => panic!("unexpected server event {other:?}"),
            Ok(None) | Err(_) => None,
        }
    }

    /// `frames` stereo frames, each sample a distinct value counting
    /// from `first`.
    fn stereo(first: i16, frames: usize) -> Vec<i16> {
        (0..frames * 2)
            .map(|i| first.wrapping_add(i as i16))
            .collect()
    }

    fn le_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[tokio::test]
    async fn the_relay_delivers_each_kind_of_item_in_order() {
        let relay = AudioRelay::new();
        let (format, mut items, _opus) = relay.subscribe(Codec::Opus);
        // With PCM wanted too, every kind is published.
        let (_, _pcm_items, _pcm) = relay.subscribe(Codec::Pcm);
        assert_eq!(format, None);
        relay.on_start(opus_stream());
        relay.on_opus_packet(&[1, 2, 3], 960);
        relay.on_decoded_pcm(&[4, -5], 48_000, 2);
        relay.on_pcm_samples(&[6, 7], 44_100, 1);
        relay.on_stop();
        assert!(matches!(items.recv().await, Ok(AudioItem::Start(f)) if f == opus_stream()));
        match items.recv().await {
            Ok(AudioItem::Opus { packet, frames }) => {
                assert_eq!(&packet[..], [1, 2, 3]);
                assert_eq!(frames, 960);
            }
            other => panic!("expected an Opus packet, got {other:?}"),
        }
        match items.recv().await {
            Ok(AudioItem::Decoded {
                samples,
                rate_hz,
                channels,
            }) => assert_eq!((&samples[..], rate_hz, channels), (&[4, -5][..], 48_000, 2)),
            other => panic!("expected decoded PCM, got {other:?}"),
        }
        match items.recv().await {
            Ok(AudioItem::Raw {
                samples,
                rate_hz,
                channels,
            }) => assert_eq!((&samples[..], rate_hz, channels), (&[6, 7][..], 44_100, 1)),
            other => panic!("expected raw PCM, got {other:?}"),
        }
        assert!(matches!(items.recv().await, Ok(AudioItem::Stop)));
    }

    #[test]
    fn a_late_subscriber_learns_the_running_streams_format() {
        let relay = AudioRelay::new();
        // No one is listening, which is not an error.
        relay.on_start(opus_stream());
        relay.on_opus_packet(&[1], 960);
        let (format, mut items, _subscription) = relay.subscribe(Codec::Opus);
        assert_eq!(format, Some(opus_stream()));
        assert_eq!(relay.format(), Some(opus_stream()));
        // Nothing from before the subscription is queued.
        assert!(items.try_recv().is_err());
        relay.on_stop();
        assert_eq!(relay.subscribe(Codec::Opus).0, None);
    }

    /// The items `items` has been sent so far.
    fn received(items: &mut broadcast::Receiver<AudioItem>) -> Vec<AudioItem> {
        std::iter::from_fn(|| items.try_recv().ok()).collect()
    }

    #[test]
    fn with_only_opus_wanted_no_pcm_is_published() {
        let relay = AudioRelay::new();
        let (_, mut items, _subscription) = relay.subscribe(Codec::Opus);
        relay.on_start(opus_stream());
        relay.on_decoded_pcm(&stereo(0, 960), 48_000, 2);
        relay.on_pcm_samples(&stereo(0, 960), 48_000, 2);
        relay.on_opus_packet(&[1, 2], 960);
        relay.on_stop();
        let items = received(&mut items);
        assert!(
            matches!(
                items[..],
                [AudioItem::Start(_), AudioItem::Opus { .. }, AudioItem::Stop]
            ),
            "{items:?}"
        );
    }

    #[test]
    fn with_only_pcm_wanted_no_opus_is_published() {
        let relay = AudioRelay::new();
        let (_, mut items, _subscription) = relay.subscribe(Codec::Pcm);
        relay.on_start(opus_stream());
        relay.on_opus_packet(&[1, 2], 960);
        relay.on_decoded_pcm(&stereo(0, 960), 48_000, 2);
        relay.on_pcm_samples(&stereo(0, 960), 48_000, 2);
        relay.on_stop();
        let items = received(&mut items);
        assert!(
            matches!(
                items[..],
                [
                    AudioItem::Start(_),
                    AudioItem::Decoded { .. },
                    AudioItem::Raw { .. },
                    AudioItem::Stop
                ]
            ),
            "{items:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_codec_is_wanted_only_while_its_forwarder_runs() {
        let relay = Arc::new(AudioRelay::new());
        let handler = |relay: &Arc<AudioRelay>| {
            let (events_tx, _) = mpsc::unbounded_channel();
            SessionSound::new(session_slot(Some(relay.clone())), Some(events_tx))
        };
        let mut stopped = handler(&relay);
        stopped.begin(Codec::Pcm).unwrap();
        assert!(relay.wants(Codec::Pcm) && !relay.wants(Codec::Opus));
        stopped.stop();
        assert!(!relay.wants(Codec::Pcm));

        let mut dropped = handler(&relay);
        dropped.begin(Codec::Opus).unwrap();
        assert!(relay.wants(Codec::Opus) && !relay.wants(Codec::Pcm));
        drop(dropped);
        assert!(!relay.wants(Codec::Opus));

        // Torn down by IronRDP with its connection.
        let mut h = Harness::started(true, vec![opus()]);
        assert!(h.relay.wants(Codec::Opus));
        drop(h.server);
        assert!(!h.relay.wants(Codec::Opus));
        h.relay.on_opus_packet(&[1], 960);
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_before_start() {
        let mut h = Harness::new(true);
        h.accept_formats(vec![opus(), pcm()]);
        h.relay.on_start(opus_stream());
        h.relay.on_opus_packet(&[1, 2, 3], 960);
        h.relay.on_decoded_pcm(&stereo(0, 960), 48_000, 2);
        h.relay.on_stop();
        assert!(next_wave(&mut h.events).await.is_none());

        h.confirm_training();
        assert!(next_wave(&mut h.events).await.is_none());
        h.relay.on_opus_packet(&[4, 5], 960);
        assert_eq!(next_wave(&mut h.events).await, Some((vec![4, 5], 0)));
    }

    #[tokio::test(start_paused = true)]
    async fn opus_is_chosen_when_the_client_accepts_both() {
        // The client lists PCM first; the server's order decides.
        let mut h = Harness::started(true, vec![pcm(), opus()]);
        h.relay.on_opus_packet(&[9, 8, 7], 960);
        h.relay.on_decoded_pcm(&stereo(0, 960), 48_000, 2);
        assert_eq!(next_wave(&mut h.events).await, Some((vec![9, 8, 7], 0)));
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn pcm_is_chosen_when_the_client_accepts_only_pcm() {
        let mut h = Harness::started(true, vec![pcm()]);
        let samples = stereo(0, 960);
        h.relay.on_opus_packet(&[9, 8, 7], 960);
        h.relay.on_decoded_pcm(&samples, 48_000, 2);
        assert_eq!(
            next_wave(&mut h.events).await,
            Some((le_bytes(&samples), 0))
        );
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_without_audio_declines_every_format() {
        let mut h = Harness::started(false, vec![opus(), pcm()]);
        // With no format chosen the channel refuses waves.
        assert!(h.server.wave(vec![0; 4], 0).is_err());
        h.relay.on_opus_packet(&[1], 960);
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_after_the_channel_is_torn_down() {
        let mut h = Harness::started(true, vec![opus()]);
        drop(h.server);
        h.relay.on_opus_packet(&[1], 960);
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_after_stop() {
        let relay = Arc::new(AudioRelay::new());
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let mut handler = SessionSound::new(session_slot(Some(relay.clone())), Some(events_tx));
        handler.begin(Codec::Opus).unwrap();
        relay.on_opus_packet(&[1], 960);
        assert_eq!(next_wave(&mut events).await, Some((vec![1], 0)));
        handler.stop();
        relay.on_opus_packet(&[2], 960);
        assert!(next_wave(&mut events).await.is_none());
        // Stopped for good: a second start has no sender to start with.
        assert!(handler.begin(Codec::Opus).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn opus_passes_through_byte_for_byte_one_packet_per_wave() {
        let mut h = Harness::started(true, vec![opus(), pcm()]);
        h.relay.on_start(opus_stream());
        h.relay.on_opus_packet(&[0xFC, 0xFF, 0xFE], 960);
        h.relay.on_opus_packet(&[0x00], 480);
        h.relay.on_opus_packet(&[0x7F, 0x80], 960);
        h.relay.on_stop();
        assert_eq!(
            next_wave(&mut h.events).await,
            Some((vec![0xFC, 0xFF, 0xFE], 0))
        );
        assert_eq!(next_wave(&mut h.events).await, Some((vec![0x00], 20)));
        assert_eq!(next_wave(&mut h.events).await, Some((vec![0x7F, 0x80], 30)));
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn pcm_is_gathered_into_20ms_waves_in_order_and_flushed_on_stop() {
        let mut h = Harness::started(true, vec![pcm()]);
        // spice-server's short packets: 256 frames, about 5 ms each.
        let packets: Vec<Vec<i16>> = (0..6).map(|n| stereo(-600 + n * 512, 256)).collect();
        h.relay.on_start(opus_stream());
        for packet in &packets[..3] {
            h.relay.on_decoded_pcm(packet, 48_000, 2);
        }
        assert!(
            next_wave(&mut h.events).await.is_none(),
            "768 frames is not a wave yet"
        );

        h.relay.on_decoded_pcm(&packets[3], 48_000, 2);
        let (data, timestamp) = next_wave(&mut h.events).await.unwrap();
        assert_eq!(data, le_bytes(&packets[..4].concat()));
        assert_eq!(timestamp, 0);

        h.relay.on_decoded_pcm(&packets[4], 48_000, 2);
        h.relay.on_decoded_pcm(&packets[5], 48_000, 2);
        assert!(next_wave(&mut h.events).await.is_none());
        h.relay.on_stop();
        let (data, timestamp) = next_wave(&mut h.events).await.unwrap();
        assert_eq!(data, le_bytes(&packets[4..].concat()));
        // 1024 frames sent before it: 21.3 ms.
        assert_eq!(timestamp, 21);
        // A second stop has nothing left to flush.
        h.relay.on_start(opus_stream());
        h.relay.on_stop();
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_20ms_packet_is_a_wave_of_its_own() {
        let mut h = Harness::started(true, vec![pcm()]);
        let first = stereo(0, 960);
        let second = stereo(5000, 960);
        h.relay.on_decoded_pcm(&first, 48_000, 2);
        h.relay.on_decoded_pcm(&second, 48_000, 2);
        assert_eq!(next_wave(&mut h.events).await, Some((le_bytes(&first), 0)));
        assert_eq!(
            next_wave(&mut h.events).await,
            Some((le_bytes(&second), 20))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pcm_samples_go_out_little_endian() {
        let mut h = Harness::started(true, vec![pcm()]);
        let mut samples = vec![0x0102_i16, -2];
        samples.resize(960 * 2, 0);
        h.relay.on_decoded_pcm(&samples, 48_000, 2);
        let (data, _) = next_wave(&mut h.events).await.unwrap();
        assert_eq!(data[..4], [0x02, 0x01, 0xFE, 0xFF]);
        assert_eq!(data.len(), 960 * 4);
    }

    #[tokio::test(start_paused = true)]
    async fn only_48khz_stereo_raw_pcm_is_played() {
        let mut h = Harness::started(true, vec![pcm()]);
        let samples = stereo(0, 960);
        h.relay.on_pcm_samples(&samples, 44_100, 2);
        h.relay.on_pcm_samples(&samples[..960], 48_000, 1);
        h.relay.on_decoded_pcm(&samples[..960], 48_000, 1);
        assert!(next_wave(&mut h.events).await.is_none());
        h.relay.on_pcm_samples(&samples, 48_000, 2);
        assert_eq!(
            next_wave(&mut h.events).await,
            Some((le_bytes(&samples), 0))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn raw_pcm_is_not_played_when_the_client_chose_opus() {
        let mut h = Harness::started(true, vec![opus()]);
        h.relay.on_pcm_samples(&stereo(0, 960), 48_000, 2);
        h.relay.on_stop();
        assert!(next_wave(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn timestamps_count_the_audio_sent_not_the_waves() {
        let mut h = Harness::started(true, vec![pcm()]);
        for n in 0..4 {
            h.relay.on_decoded_pcm(&stereo(n, 1024), 48_000, 2);
        }
        let mut timestamps = Vec::new();
        while let Some((_, timestamp)) = next_wave(&mut h.events).await {
            timestamps.push(timestamp);
        }
        // 1024 frames is 21.33 ms; rounding each wave would give 63.
        assert_eq!(timestamps, [0, 21, 42, 64]);
    }

    #[test]
    fn timestamps_wrap_at_u32_max() {
        let wrap = 48 * (u64::from(u32::MAX) + 1);
        assert_eq!(wave_timestamp(0), 0);
        assert_eq!(wave_timestamp(48_000), 1000);
        assert_eq!(wave_timestamp(wrap - 48), u32::MAX);
        assert_eq!(wave_timestamp(wrap), 0);
        assert_eq!(wave_timestamp(wrap + 5 * 48), 5);
    }

    #[test]
    fn opus_is_offered_before_pcm_at_48khz_stereo() {
        let formats = offered_formats();
        let tags: Vec<_> = formats.iter().map(|f| f.format).collect();
        assert_eq!(tags, [WaveFormat::OPUS, WaveFormat::PCM]);
        for format in &formats {
            assert_eq!((format.n_channels, format.n_samples_per_sec), (2, 48_000));
            assert_eq!(format.bits_per_sample, 16);
            assert_eq!(format.data, None);
        }
    }
}
