use anyhow::Result;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use super::volume::VolumeControl;
use crate::opcode_counters::OpcodeCounters;
use crate::{
    ByteCounter, LogConfig, NotificationEntry, NotificationSource, OpusPacketSink, TrafficSink,
};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, take_message, MessageHeader, Notify as NotifyMessage, Ping, SetAck,
};
use shakenfist_spice_protocol::{
    main_client, playback_server, warn_once, ChannelType, NotifySeverity,
};

use super::{ChannelEvent, EventSink, MAX_MESSAGE_BODY};

const AUDIO_DATA_MODE_RAW: u16 = 1;
const AUDIO_DATA_MODE_OPUS: u16 = 3;

/// Maximum ring buffer capacity in samples. At 48kHz stereo this is
/// ~2 seconds. Prevents unbounded memory growth if audio data
/// arrives faster than it is consumed.
const MAX_AUDIO_BUFFER_SAMPLES: usize = 48000 * 2 * 2;

/// Cap for the recent-decode-duration ring published into the
/// playback snapshot. Mirrors display.rs's `recent_decodes`
/// pattern: a bounded window of recent measurements so a bug
/// report has fresh latency data without growing unbounded.
const MAX_RECENT_PLAYBACK_DECODES: usize = 64;

/// Lock-free counters owned by the audio-thread side of the
/// playback pipeline. The cpal callback writes to the
/// `device_callbacks_total`, `device_underrun_count`, and
/// `samples_consumed_total` atomics with `Relaxed` ordering on
/// every callback; the producer-push path writes
/// `ring_overflow_count` when the ring is full. The tokio side
/// reads them via `load(Relaxed)` from `update_snapshot` —
/// single load per counter, no contention with the audio
/// callback. Counters are cumulative across audio-session
/// restarts (the Arcs survive STOP → START cycles).
struct AudioCounters {
    device_callbacks_total: AtomicU64,
    device_underrun_count: AtomicU64,
    ring_overflow_count: AtomicU64,
    samples_consumed_total: AtomicU64,
    /// Platform-reported xruns. Written from the cpal error
    /// callback, which for xruns runs on the device's real-time
    /// thread — hence an atomic rather than a log line.
    device_xrun_count: AtomicU64,
}

impl AudioCounters {
    fn new() -> Arc<Self> {
        Arc::new(AudioCounters {
            device_callbacks_total: AtomicU64::new(0),
            device_underrun_count: AtomicU64::new(0),
            ring_overflow_count: AtomicU64::new(0),
            samples_consumed_total: AtomicU64::new(0),
            device_xrun_count: AtomicU64::new(0),
        })
    }
}

/// First delay before retrying a failed output stream.
const OUTPUT_RETRY_INITIAL: Duration = Duration::from_secs(1);

/// Cap on the doubling retry delay. Long enough that a machine
/// with no output device at all is not busy-looping, short
/// enough that plugging headphones back in is noticed promptly.
const OUTPUT_RETRY_MAX: Duration = Duration::from_secs(30);

/// How often the audio thread wakes to check for shutdown while
/// it is watching a stream or waiting to retry.
const AUDIO_THREAD_POLL: Duration = Duration::from_millis(50);

/// What the audio thread reports about the output stream. It
/// has no async context to emit channel events from, so it
/// sends these to the playback channel, which folds them into
/// the snapshot and raises notifications (see [`OutputStatus`]).
#[derive(Debug)]
enum AudioOutputEvent {
    /// An output stream is playing.
    Started(crate::snapshots::PlaybackOutputInfo),
    /// No stream is playing: opening one failed, or a running
    /// one died. The thread retries with backoff until STOP.
    Failed(String),
    /// The platform moved a default-device stream to another
    /// device without a rebuild (cpal `ErrorKind::DeviceChanged`).
    Rerouted(String),
}

/// The playback channel's view of the output stream, built from
/// [`AudioOutputEvent`]s. Kept apart from `PlaybackChannel` so
/// the state machine can be unit-tested without a SPICE stream.
#[derive(Debug, Default)]
struct OutputStatus {
    output: Option<crate::snapshots::PlaybackOutputInfo>,
    error: Option<String>,
    /// True from the first failure until a stream starts again;
    /// a failure streak raises one notification, not one per
    /// retry.
    failing: bool,
    streams_started: u64,
    failure_count: u64,
    reroute_count: u64,
}

impl OutputStatus {
    /// Fold one event into the status. Returns the notification
    /// the operator should see, if any: the first failure of a
    /// streak, the recovery that ends it, and reroutes. A
    /// routine start on PLAYBACK_START is silent.
    fn apply(&mut self, event: AudioOutputEvent) -> Option<NotificationEntry> {
        match event {
            AudioOutputEvent::Started(output) => {
                self.streams_started = self.streams_started.saturating_add(1);
                self.error = None;
                let note = self.failing.then(|| {
                    NotificationEntry::new(
                        NotifySeverity::Info,
                        NotificationSource::Internal,
                        format!("Audio output restored on {}", output.device),
                    )
                });
                self.failing = false;
                self.output = Some(output);
                note
            }
            AudioOutputEvent::Failed(reason) => {
                self.failure_count = self.failure_count.saturating_add(1);
                self.output = None;
                let note = (!self.failing).then(|| {
                    NotificationEntry::new(
                        NotifySeverity::Warn,
                        NotificationSource::Internal,
                        format!("Audio output unavailable ({}); retrying", reason),
                    )
                });
                self.failing = true;
                self.error = Some(reason);
                note
            }
            AudioOutputEvent::Rerouted(device) => {
                self.reroute_count = self.reroute_count.saturating_add(1);
                if let Some(ref mut output) = self.output {
                    output.device = device.clone();
                }
                Some(NotificationEntry::new(
                    NotifySeverity::Info,
                    NotificationSource::Internal,
                    format!("Audio output moved to {}", device),
                ))
            }
        }
    }

    /// The audio session ended (STOP or disconnect). The last
    /// error is kept for bug reports; the cumulative counters
    /// survive.
    fn session_ended(&mut self) {
        self.output = None;
        self.failing = false;
    }
}

pub(crate) struct Resampler {
    ratio: f64,
    pos: f64,
    channels: usize,
}

impl Resampler {
    pub(crate) fn new(from_rate: u32, to_rate: u32, channels: u32) -> Self {
        Resampler {
            ratio: from_rate as f64 / to_rate as f64,
            pos: 0.0,
            channels: channels.max(1) as usize,
        }
    }

    /// Produce one output frame (one sample per channel) by
    /// linearly interpolating between adjacent input frames.
    /// Returns silence without modifying the buffer on underrun.
    pub(crate) fn next_frame(&mut self, buffer: &mut VecDeque<i16>, out: &mut [i16]) {
        let ch = self.channels;
        let idx = self.pos as usize;
        let frac = self.pos - idx as f64;

        // Need two full frames at positions idx and idx+1.
        let needed = (idx + 2) * ch;
        if buffer.len() < needed {
            // Underrun: return silence without polluting the buffer.
            for s in out.iter_mut().take(ch) {
                *s = 0;
            }
            return;
        }

        // Interpolate each channel independently.
        for c in 0..ch {
            let a = buffer[idx * ch + c] as f64;
            let b = buffer[(idx + 1) * ch + c] as f64;
            out[c] = (a + (b - a) * frac) as i16;
        }

        // Advance position and consume whole frames.
        self.pos += self.ratio;
        let consume_frames = self.pos as usize;
        let consume_samples = consume_frames * ch;
        for _ in 0..consume_samples {
            buffer.pop_front();
        }
        self.pos -= consume_frames as f64;
    }
}

/// Fill the output buffer with resampled i16 samples.
fn write_samples_i16(
    data: &mut [i16],
    local_buf: &mut VecDeque<i16>,
    vol: &Arc<VolumeControl>,
    resampler: &mut Resampler,
) {
    let v = vol.effective_volume();
    let ch = resampler.channels;
    let mut frame = vec![0i16; ch];
    for chunk in data.chunks_mut(ch) {
        resampler.next_frame(local_buf, &mut frame);
        for (out, &s) in chunk.iter_mut().zip(frame.iter()) {
            *out = (s as f32 * v) as i16;
        }
    }
}

/// Fill the output buffer with resampled f32 samples.
fn write_samples_f32(
    data: &mut [f32],
    local_buf: &mut VecDeque<i16>,
    vol: &Arc<VolumeControl>,
    resampler: &mut Resampler,
) {
    let v = vol.effective_volume();
    let ch = resampler.channels;
    let mut frame = vec![0i16; ch];
    for chunk in data.chunks_mut(ch) {
        resampler.next_frame(local_buf, &mut frame);
        for (out, &s) in chunk.iter_mut().zip(frame.iter()) {
            *out = s as f32 / 32768.0 * v;
        }
    }
}

/// Convert a little-endian PCM byte stream to a fresh
/// `Vec<i16>`. Used by the pre-decode tap to hand raw PCM
/// samples to an [`OpusPacketSink`] without disturbing the
/// existing cpal path.
fn pcm_bytes_to_i16(bytes: &[u8]) -> Vec<i16> {
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.as_chunks::<2>().0 {
        out.push(i16::from_le_bytes(*chunk));
    }
    out
}

/// Compute the number of 48 kHz samples represented by one
/// Opus packet by inspecting its TOC byte and frame count.
///
/// This is a tiny port of `opus_packet_get_nb_samples()`
/// specialised to Fs=48000 (which is what RFC 7587 §4.1
/// pins for RTP). The TOC byte's bottom two bits give the
/// "code" (frame-count format); for code 3, the frame
/// count is encoded in the next byte's low 6 bits. Frame
/// duration comes from the upper bits of the TOC.
///
/// Returns 960 (the WebRTC default of 20 ms at 48 kHz) for
/// empty or malformed packets so the caller always has a
/// usable timestamp delta. The decoder downstream still
/// validates the packet on its own; this helper is only
/// used for RTP timestamp arithmetic.
fn opus_packet_samples_48k(packet: &[u8]) -> u32 {
    if packet.is_empty() {
        return 960;
    }
    let toc = packet[0];
    let samples_per_frame = samples_per_frame_48k(toc);
    let frame_count = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        3 => {
            // Code 3: the next byte's low 6 bits hold M.
            if packet.len() < 2 {
                return 960;
            }
            (packet[1] & 0x3F) as usize
        }
        _ => 1,
    };
    (samples_per_frame.saturating_mul(frame_count)).min(u32::MAX as usize) as u32
}

/// Mirror of `opus_packet_get_samples_per_frame()` from libopus,
/// specialised to Fs=48000. See RFC 6716 §3.1 for the TOC byte
/// layout.
fn samples_per_frame_48k(toc: u8) -> usize {
    if (toc & 0x80) != 0 {
        let audiosize = ((toc >> 3) & 0x03) as usize;
        (48_000usize << audiosize) / 400
    } else if (toc & 0x60) == 0x60 {
        if (toc & 0x08) != 0 {
            48_000usize / 50
        } else {
            48_000usize / 100
        }
    } else {
        let audiosize = ((toc >> 3) & 0x03) as usize;
        if audiosize == 3 {
            (48_000usize * 60) / 1000
        } else {
            (48_000usize << audiosize) / 100
        }
    }
}

/// Consumer-side state that has to outlive any one cpal stream,
/// so a stream rebuilt after a failure carries on from the same
/// ring buffer.
struct CallbackState {
    consumer: rtrb::Consumer<i16>,
    local_buf: VecDeque<i16>,
}

impl CallbackState {
    /// Drain available samples from the ring buffer into the
    /// local VecDeque so the resampler can use random access.
    fn drain_ring(&mut self) {
        let available = self.consumer.slots();
        if available > 0 {
            let chunk = self
                .consumer
                .read_chunk(available)
                .expect("read_chunk of slots() cannot fail");
            let (first, second) = chunk.as_slices();
            self.local_buf.extend(first.iter().copied());
            self.local_buf.extend(second.iter().copied());
            chunk.commit_all();
        }
    }

    /// Throw away everything queued. While no stream is playing
    /// the producer fills the ring and then drops the *newest*
    /// samples, so a rebuilt stream would otherwise open with up
    /// to two seconds of stale audio.
    fn discard(&mut self) {
        self.drain_ring();
        self.local_buf.clear();
    }

    /// Take the lock from the device callback, which must never
    /// block. Only one stream exists at a time, so the lock is
    /// only contended if a dying stream's last callback overlaps
    /// its replacement; that callback gets `None`, hands the
    /// device silence, and is counted as an underrun. A poisoned
    /// lock is recovered rather than refused: the ring and buffer
    /// are still valid, and refusing would play silence for the
    /// rest of the session with nothing recorded.
    fn lock_for_callback<'a>(
        state: &'a Mutex<CallbackState>,
        counters: &AudioCounters,
    ) -> Option<MutexGuard<'a, CallbackState>> {
        match state.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => {
                counters
                    .device_underrun_count
                    .fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
}

/// Human-readable name for an output device, for logs,
/// notifications and the snapshot.
fn device_name(device: &cpal::Device) -> String {
    use cpal::traits::DeviceTrait;
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown device".to_string())
}

/// State for the dedicated audio output thread.
struct AudioThread {
    handle: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
    events: mpsc::Receiver<AudioOutputEvent>,
}

impl AudioThread {
    /// Spawn a dedicated OS thread that owns the cpal stream.
    /// Samples are read from `consumer` via a lock-free ring buffer.
    fn spawn(
        consumer: rtrb::Consumer<i16>,
        vol: Arc<VolumeControl>,
        source_rate: u32,
        source_channels: u32,
        counters: Arc<AudioCounters>,
    ) -> Option<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown.clone();
        let (events_tx, events) = mpsc::channel();
        let state = Arc::new(Mutex::new(CallbackState {
            consumer,
            local_buf: VecDeque::with_capacity(8192),
        }));

        let handle = std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                Self::run_audio(
                    state,
                    vol,
                    source_rate,
                    source_channels,
                    shutdown_flag,
                    counters,
                    events_tx,
                );
            })
            .ok()?;

        Some(AudioThread {
            handle,
            shutdown,
            events,
        })
    }

    /// Keep an output stream playing until shutdown. A stream
    /// that cannot be opened, or that dies (device unplugged,
    /// sample rate changed under it, audio server restarted), is
    /// rebuilt with a doubling backoff rather than leaving the
    /// session silent until the next PLAYBACK_START.
    fn run_audio(
        state: Arc<Mutex<CallbackState>>,
        vol: Arc<VolumeControl>,
        source_rate: u32,
        source_channels: u32,
        shutdown: Arc<AtomicBool>,
        counters: Arc<AudioCounters>,
        events: mpsc::Sender<AudioOutputEvent>,
    ) {
        // Held here as well as in each stream's error callback,
        // so `err_rx` never reports a disconnect.
        let (err_tx, err_rx) = mpsc::channel::<cpal::Error>();
        let mut backoff = OUTPUT_RETRY_INITIAL;
        let mut failed = false;

        while !shutdown.load(Ordering::Relaxed) {
            if failed {
                state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .discard();
            }
            // Errors queued by a stream that has since been
            // dropped say nothing about the next one.
            while err_rx.try_recv().is_ok() {}

            match Self::open_stream(
                &state,
                &vol,
                source_rate,
                source_channels,
                &counters,
                &err_tx,
            ) {
                Ok((stream, output)) => {
                    info!(
                        "playback: audio output started on {} ({}Hz {} ch {})",
                        output.device, output.sample_rate_hz, output.channels, output.sample_format
                    );
                    let _ = events.send(AudioOutputEvent::Started(output));
                    backoff = OUTPUT_RETRY_INITIAL;

                    let died = Self::watch_stream(&err_rx, &shutdown, &events);
                    // Dropping the stream releases the device.
                    drop(stream);
                    match died {
                        Some(reason) => {
                            warn!("playback: audio output stream failed: {}", reason);
                            let _ = events.send(AudioOutputEvent::Failed(reason));
                            failed = true;
                        }
                        None => break,
                    }
                }
                Err(reason) => {
                    // Retries repeat the same failure; log the
                    // first loudly and the rest quietly.
                    if failed {
                        debug!("playback: audio output still unavailable: {}", reason);
                    } else {
                        warn!("playback: audio output unavailable: {}", reason);
                    }
                    let _ = events.send(AudioOutputEvent::Failed(reason));
                    failed = true;
                }
            }

            Self::sleep_unless_shutdown(backoff, &shutdown);
            backoff = (backoff * 2).min(OUTPUT_RETRY_MAX);
        }

        info!("playback: audio thread shutting down");
    }

    /// Open and start an output stream on the current default
    /// device. Returns the stream and a description of it, or a
    /// human-readable reason it could not be opened.
    fn open_stream(
        state: &Arc<Mutex<CallbackState>>,
        vol: &Arc<VolumeControl>,
        source_rate: u32,
        source_channels: u32,
        counters: &Arc<AudioCounters>,
        err_tx: &mpsc::Sender<cpal::Error>,
    ) -> Result<(cpal::Stream, crate::snapshots::PlaybackOutputInfo), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no audio output device found".to_string())?;
        let name = device_name(&device);
        let default_config = device
            .default_output_config()
            .map_err(|e| format!("cannot read output config of {}: {}", name, e))?;
        debug!(
            "playback: device config: {}Hz, {} ch, {:?}",
            default_config.sample_rate(),
            default_config.channels(),
            default_config.sample_format()
        );
        let config = cpal::StreamConfig {
            channels: source_channels as u16,
            sample_rate: default_config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let device_rate = config.sample_rate;

        let stream = match default_config.sample_format() {
            cpal::SampleFormat::I16 => Self::build_stream::<i16>(
                &device,
                config,
                state,
                vol,
                Resampler::new(source_rate, device_rate, source_channels),
                counters,
                err_tx,
                write_samples_i16,
            ),
            cpal::SampleFormat::F32 => Self::build_stream::<f32>(
                &device,
                config,
                state,
                vol,
                Resampler::new(source_rate, device_rate, source_channels),
                counters,
                err_tx,
                write_samples_f32,
            ),
            fmt => {
                return Err(format!(
                    "{} wants unsupported sample format {:?}",
                    name, fmt
                ))
            }
        }
        .map_err(|e| format!("cannot open stream on {}: {}", name, e))?;
        stream
            .play()
            .map_err(|e| format!("cannot start stream on {}: {}", name, e))?;

        Ok((
            stream,
            crate::snapshots::PlaybackOutputInfo {
                device: name,
                sample_rate_hz: device_rate,
                channels: source_channels as u16,
                sample_format: default_config.sample_format().to_string(),
            },
        ))
    }

    /// Build a cpal output stream for sample type `T`, with
    /// `write` converting the resampled i16 frames into `T`.
    #[allow(clippy::too_many_arguments)]
    fn build_stream<T: cpal::SizedSample + 'static>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        state: &Arc<Mutex<CallbackState>>,
        vol: &Arc<VolumeControl>,
        mut resampler: Resampler,
        counters: &Arc<AudioCounters>,
        err_tx: &mpsc::Sender<cpal::Error>,
        write: fn(&mut [T], &mut VecDeque<i16>, &Arc<VolumeControl>, &mut Resampler),
    ) -> Result<cpal::Stream, cpal::Error> {
        use cpal::traits::DeviceTrait;

        let state = state.clone();
        let vol = vol.clone();
        let data_counters = counters.clone();
        let err_counters = counters.clone();
        let err_tx = err_tx.clone();
        device.build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                data_counters
                    .device_callbacks_total
                    .fetch_add(1, Ordering::Relaxed);
                let Some(mut guard) = CallbackState::lock_for_callback(&state, &data_counters)
                else {
                    data.fill(T::EQUILIBRIUM);
                    return;
                };
                let st = &mut *guard;
                // The authoritative underrun signal is an empty
                // ring at the top of the callback, before draining.
                if st.consumer.slots() == 0 {
                    data_counters
                        .device_underrun_count
                        .fetch_add(1, Ordering::Relaxed);
                }
                st.drain_ring();
                write(data, &mut st.local_buf, &vol, &mut resampler);
                data_counters
                    .samples_consumed_total
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            },
            move |err: cpal::Error| {
                if err.kind() == cpal::ErrorKind::Xrun {
                    // Xruns are reported from the real-time
                    // thread: count them, never log or allocate.
                    err_counters
                        .device_xrun_count
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    let _ = err_tx.send(err);
                }
            },
            None,
        )
    }

    /// Block until the stream dies or shutdown is requested.
    /// Returns why the stream died, or `None` on shutdown.
    fn watch_stream(
        err_rx: &mpsc::Receiver<cpal::Error>,
        shutdown: &AtomicBool,
        events: &mpsc::Sender<AudioOutputEvent>,
    ) -> Option<String> {
        use cpal::traits::HostTrait;

        let mut realtime_denied_logged = false;
        while !shutdown.load(Ordering::Relaxed) {
            let err = match err_rx.recv_timeout(AUDIO_THREAD_POLL) {
                Ok(err) => err,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Some("audio error channel closed".to_string());
                }
            };
            match err.kind() {
                // The stream followed the new default device by
                // itself; record where the sound is going now.
                cpal::ErrorKind::DeviceChanged => {
                    let device = cpal::default_host()
                        .default_output_device()
                        .map(|d| device_name(&d))
                        .unwrap_or_else(|| "unknown device".to_string());
                    info!("playback: audio output rerouted to {}", device);
                    let _ = events.send(AudioOutputEvent::Rerouted(device));
                }
                // Audio still plays, just without real-time
                // scheduling.
                cpal::ErrorKind::RealtimeDenied => {
                    if !realtime_denied_logged {
                        info!("playback: real-time scheduling denied for audio output");
                        realtime_denied_logged = true;
                    }
                }
                // Anything else means the stream is no longer
                // producing sound, or may not be: rebuild it.
                _ => return Some(err.to_string()),
            }
        }
        None
    }

    /// Sleep for `delay`, waking early if shutdown is requested.
    fn sleep_unless_shutdown(delay: Duration, shutdown: &AtomicBool) {
        let deadline = Instant::now() + delay;
        while !shutdown.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(AUDIO_THREAD_POLL.min(deadline - now));
        }
    }

    /// Signal the audio thread to stop and wait for it to finish.
    /// Returns any output events it sent that were not yet
    /// drained, so the final state still reaches the snapshot.
    fn stop(self) -> Vec<AudioOutputEvent> {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
        self.events.try_iter().collect()
    }
}

pub struct PlaybackChannel {
    stream: SpiceStream,
    events: EventSink,
    buffer: Vec<u8>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    log_config: LogConfig,
    snapshot: Arc<Mutex<crate::snapshots::PlaybackSnapshot>>,
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,
    bytes_in: u64,
    bytes_out: u64,
    /// Local cache of disconnect-cause diagnostic fields.
    last_recv_ts_secs: Option<f64>,
    last_send_ts_secs: Option<f64>,
    ping_recv_count: u32,
    pong_send_count: u32,
    last_ping_recv_ts_secs: Option<f64>,
    audio_mode: u16,
    sample_rate: u32,
    channels: u32,
    audio_producer: Option<rtrb::Producer<i16>>,
    audio_thread: Option<AudioThread>,
    opus_decoder: Option<opus_decoder::OpusDecoder>,
    volume_control: Arc<VolumeControl>,
    /// Optional pre-decode tap. When set, every Opus DATA
    /// packet is forwarded to the sink before the decode-to-
    /// cpal path runs. The web frontend uses this to forward
    /// Opus packets straight to a WebRTC audio track without
    /// re-encoding; GUI / headless modes pass `None` and see
    /// the existing decode path unchanged.
    opus_sink: Option<Arc<dyn OpusPacketSink>>,
    /// Per-connection cancel flag. The 100 ms select branch in
    /// the read loop polls this so the channel exits cleanly when
    /// the orchestrator's cancel flag flips (Ctrl+C bridge in
    /// the host, or a fresh Reconnect superseding this attempt).
    cancel: Arc<AtomicBool>,
    /// Lock-free counters shared with the cpal audio callback.
    /// See [`AudioCounters`]. Created once at channel init and
    /// reused across audio-session restarts so the counters are
    /// cumulative over the channel's lifetime.
    audio_counters: Arc<AudioCounters>,
    /// Bounded per-opcode message counters; flushed to the
    /// snapshot by `update_snapshot`. See `OpcodeCounters`.
    opcodes: OpcodeCounters,
    /// Per-session metadata for the currently-active audio
    /// session (Some between START and STOP).
    current_session: Option<crate::snapshots::PlaybackSessionInfo>,
    /// Cumulative START message count.
    start_count: u64,
    /// Cumulative STOP message count.
    stop_count: u64,
    /// Cumulative DATA packet receive count.
    data_packets_received: u64,
    /// DATA packets successfully decoded.
    data_packets_decoded: u64,
    /// DATA packets that failed to decode.
    data_packets_decode_failed: u64,
    /// Sum of DATA payload bytes received.
    data_bytes_received: u64,
    /// Sum of decoded PCM bytes pushed at the ring buffer.
    pcm_bytes_produced: u64,
    /// Bounded ring of recent successful decode durations
    /// (microseconds). Cap [`MAX_RECENT_PLAYBACK_DECODES`].
    recent_decode_durations_us: VecDeque<u32>,
    /// Most recent VOLUME message per-channel vector.
    last_volume_per_channel: Vec<u16>,
    /// Most recent MUTE flag.
    last_mute: Option<bool>,
    /// Most recent LATENCY value in milliseconds.
    last_latency_ms: Option<u32>,
    /// DATA packets dropped because MODE named a codec we
    /// cannot play.
    data_packets_unsupported_codec: u64,
    /// The unsupported mode most recently warned about, so the
    /// per-packet path only reaches `warn_once!` when the mode
    /// changes.
    unsupported_mode_warned: Option<u16>,
    /// Whether this audio session has already logged an Opus
    /// decode failure at warn; later ones go to debug.
    decode_failure_logged: bool,
    /// Output-stream state reported by the audio thread.
    output_status: OutputStatus,
}

impl PlaybackChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        events: EventSink,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<dyn TrafficSink>,
        snapshot: Arc<Mutex<crate::snapshots::PlaybackSnapshot>>,
        volume_control: Arc<VolumeControl>,
        log_config: LogConfig,
        cancel: Arc<AtomicBool>,
        opus_sink: Option<Arc<dyn OpusPacketSink>>,
    ) -> Self {
        PlaybackChannel {
            stream,
            events,
            buffer: Vec::with_capacity(65536),
            byte_counter,
            traffic,
            log_config,
            snapshot,
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
            last_recv_ts_secs: None,
            last_send_ts_secs: None,
            ping_recv_count: 0,
            pong_send_count: 0,
            last_ping_recv_ts_secs: None,
            audio_mode: 0,
            sample_rate: 0,
            channels: 0,
            audio_producer: None,
            audio_thread: None,
            opus_decoder: None,
            volume_control,
            opus_sink,
            cancel,
            audio_counters: AudioCounters::new(),
            opcodes: OpcodeCounters::new(
                message_names::playback_server,
                message_names::playback_client,
            ),
            current_session: None,
            start_count: 0,
            stop_count: 0,
            data_packets_received: 0,
            data_packets_decoded: 0,
            data_packets_decode_failed: 0,
            data_bytes_received: 0,
            pcm_bytes_produced: 0,
            recent_decode_durations_us: VecDeque::with_capacity(MAX_RECENT_PLAYBACK_DECODES),
            last_volume_per_channel: Vec::new(),
            last_mute: None,
            last_latency_ms: None,
            data_packets_unsupported_codec: 0,
            unsupported_mode_warned: None,
            decode_failure_logged: false,
            output_status: OutputStatus::default(),
        }
    }

    /// Public entry point. Wraps `run_loop` so errors
    /// propagating out of the inner select! arms are logged
    /// before the task ends — see `MainChannel::run` for the
    /// rationale (including the `Box::pin` reason).
    pub async fn run(&mut self) -> Result<()> {
        let result = Box::pin(self.run_loop()).await;
        match &result {
            Ok(()) => info!("playback: run loop exited cleanly"),
            Err(e) => error!("playback: run loop exited with error: {:#}", e),
        }
        result
    }

    async fn run_loop(&mut self) -> Result<()> {
        info!("playback: channel started");
        loop {
            self.drain_output_events().await;
            let mut chunk = [0u8; 65536];
            let stream = &mut self.stream;
            let read_result = tokio::select! {
                result = async {
                    match stream {
                        SpiceStream::Plain(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                        SpiceStream::Tls(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                        SpiceStream::TlsServer(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                    }
                } => Some(result),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if self.cancel.load(Ordering::Relaxed) {
                        info!("playback: cancelled");
                        break;
                    }
                    None
                }
            };

            let n = match read_result {
                Some(Ok(0)) => {
                    info!("playback: channel disconnected");
                    self.events
                        .emit(ChannelEvent::Disconnected(ChannelType::Playback))
                        .await;
                    break;
                }
                Some(Ok(n)) => n,
                Some(Err(e)) => return Err(e.into()),
                None => {
                    // Timeout, no data yet. Refresh the snapshot
                    // anyway: the device-side counters keep moving
                    // while the server is silent, and "the device is
                    // pulling but no DATA arrives" is exactly what a
                    // silent-audio report needs to show.
                    self.update_snapshot();
                    continue;
                }
            };

            self.byte_counter.add(n as u64);
            self.buffer.extend_from_slice(&chunk[..n]);
            self.bytes_in += n as u64;
            self.last_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
            self.process_messages().await?;
            self.update_snapshot();
        }

        // Clean shutdown: stop the audio thread.
        self.stop_audio();
        Ok(())
    }

    async fn process_messages(&mut self) -> Result<()> {
        while let Some(message) = take_message(&mut self.buffer, MAX_MESSAGE_BODY)? {
            let header = &message.header;
            let payload = message.payload();
            let msg_type = header.message_type;

            // Counted before dispatch so both known and unknown
            // opcodes reach the counters. Opcodes with no protocol
            // name fold into the unknown-opcode fields rather than
            // growing the map; see `OpcodeCounters`.
            self.opcodes.record_recv(msg_type);

            if self.log_config.verbose {
                logging::log_message(
                    "received",
                    "playback",
                    msg_type,
                    message_names::playback_server(msg_type),
                    header.message_size,
                );
            }

            self.message_count += 1;
            if self.ack_window > 0 && self.message_count - self.last_ack >= self.ack_window {
                self.last_ack = self.message_count;
                let ack = make_message(main_client::ACK, &[]);
                self.send_with_log(main_client::ACK, &ack).await?;
            }

            self.traffic.record_received(
                "playback",
                msg_type,
                message_names::playback_server(msg_type),
                payload,
            );

            match msg_type {
                playback_server::SET_ACK => {
                    let set_ack = SetAck::read(payload)?;
                    self.ack_generation = set_ack.generation;
                    self.ack_window = set_ack.window;
                    self.message_count = 0;
                    self.last_ack = 0;
                    let mut ack_payload = Vec::new();
                    SetAck::write_ack_sync(set_ack.generation, &mut ack_payload)?;
                    let response = make_message(main_client::ACK_SYNC, &ack_payload);
                    self.send_with_log(main_client::ACK_SYNC, &response).await?;
                }
                playback_server::PING => {
                    self.ping_recv_count = self.ping_recv_count.saturating_add(1);
                    self.last_ping_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

                    let ping = Ping::read(payload)?;
                    let mut pong_payload = Vec::new();
                    ping.write_pong(&mut pong_payload)?;
                    let response = make_message(main_client::PONG, &pong_payload);
                    self.send_with_log(main_client::PONG, &response).await?;
                    self.pong_send_count = self.pong_send_count.saturating_add(1);
                }
                playback_server::NOTIFY => {
                    let notify = NotifyMessage::read(payload)?;
                    if self.log_config.verbose {
                        logging::log_detail(&format!(
                            "severity={:?}, visibility={:?}, what={}, message=\"{}\"",
                            notify.severity, notify.visibility, notify.what, notify.message,
                        ));
                    }
                    match notify.severity {
                        NotifySeverity::Error => {
                            warn!("playback: server notify (error): {}", notify.message)
                        }
                        NotifySeverity::Warn => {
                            warn!("playback: server notify (warn): {}", notify.message)
                        }
                        NotifySeverity::Info => {
                            info!("playback: server notify: {}", notify.message)
                        }
                    }
                    let mut entry = NotificationEntry::new(
                        notify.severity,
                        NotificationSource::Spice {
                            channel: ChannelType::Playback,
                            what: notify.what,
                        },
                        notify.message.clone(),
                    );
                    if let Some(v) = notify.visibility {
                        entry = entry.with_visibility(v);
                    }
                    self.events.emit(ChannelEvent::Notification(entry)).await;
                }
                playback_server::START => {
                    if payload.len() >= 14 {
                        self.channels =
                            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                        let format = u16::from_le_bytes([payload[4], payload[5]]);
                        self.sample_rate =
                            u32::from_le_bytes([payload[6], payload[7], payload[8], payload[9]]);
                        let time = u32::from_le_bytes([
                            payload[10],
                            payload[11],
                            payload[12],
                            payload[13],
                        ]);
                        info!(
                            "playback: START: {}Hz, {} channels, format={}, time={}",
                            self.sample_rate, self.channels, format, time
                        );
                        self.start_count = self.start_count.saturating_add(1);
                        self.decode_failure_logged = false;
                        self.current_session = Some(crate::snapshots::PlaybackSessionInfo {
                            started_at_secs: self.traffic.elapsed().as_secs_f64(),
                            mm_time_at_start: time,
                            sample_rate_hz: self.sample_rate,
                            channels: self.channels.min(u8::MAX as u32) as u8,
                            codec: codec_from_mode(self.audio_mode),
                        });
                        self.start_audio_output();
                        // Opus always operates at 48kHz internally; the
                        // channel count comes from the SPICE START message.
                        self.opus_decoder =
                            match opus_decoder::OpusDecoder::new(48000, self.channels as usize) {
                                Ok(d) => {
                                    info!("playback: Opus decoder initialized");
                                    Some(d)
                                }
                                Err(e) => {
                                    warn!("playback: failed to create Opus decoder: {}", e);
                                    None
                                }
                            };
                    }
                }
                playback_server::MODE => {
                    // SpiceMsgPlaybackMode: time(u32) + mode(u16).
                    // Skip the 4-byte multimedia timestamp.
                    if payload.len() >= 6 {
                        self.audio_mode = u16::from_le_bytes([payload[4], payload[5]]);
                        info!("playback: MODE: {}", self.audio_mode);
                        // If a session is active, refresh its codec
                        // to match the new MODE — the server can
                        // change codec mid-session in principle.
                        if let Some(ref mut session) = self.current_session {
                            session.codec = codec_from_mode(self.audio_mode);
                        }
                    }
                }
                playback_server::DATA => {
                    // SpiceMsgPlaybackPacket: time(u32) + data.
                    // Skip the 4-byte multimedia timestamp.
                    self.data_packets_received = self.data_packets_received.saturating_add(1);
                    if payload.len() > 4 {
                        let audio_data = &payload[4..];
                        self.data_bytes_received = self
                            .data_bytes_received
                            .saturating_add(audio_data.len() as u64);
                        if self.audio_mode == AUDIO_DATA_MODE_RAW {
                            // Pre-decode tap: forward to the optional
                            // sink before the decode-to-cpal path.
                            // Web mode uses this; GUI / headless pass
                            // None and the call is a no-op.
                            if let Some(ref sink) = self.opus_sink {
                                sink.on_pcm_samples(
                                    &pcm_bytes_to_i16(audio_data),
                                    self.sample_rate,
                                    self.channels as u8,
                                );
                            }
                            let start = Instant::now();
                            self.push_samples_raw(audio_data);
                            let dur_us = start.elapsed().as_micros().min(u32::MAX as u128) as u32;
                            self.record_decode_success(dur_us, audio_data.len() as u64);
                        } else if self.audio_mode == AUDIO_DATA_MODE_OPUS {
                            // Pre-decode tap: forward the raw Opus
                            // packet to the optional sink before the
                            // libopus decode + cpal path runs.
                            if let Some(ref sink) = self.opus_sink {
                                let samples = opus_packet_samples_48k(audio_data);
                                sink.on_opus_packet(audio_data, samples);
                            }
                            let start = Instant::now();
                            let pcm_bytes = self.push_samples_opus(audio_data);
                            let dur_us = start.elapsed().as_micros().min(u32::MAX as u128) as u32;
                            match pcm_bytes {
                                Some(bytes) => self.record_decode_success(dur_us, bytes),
                                None => {
                                    self.data_packets_decode_failed =
                                        self.data_packets_decode_failed.saturating_add(1);
                                }
                            }
                        } else {
                            // The guest is producing sound we cannot
                            // play. Registering it as a gap raises a
                            // notification (and a --pedantic report)
                            // instead of dropping it silently.
                            self.data_packets_unsupported_codec =
                                self.data_packets_unsupported_codec.saturating_add(1);
                            if self.unsupported_mode_warned != Some(self.audio_mode) {
                                self.unsupported_mode_warned = Some(self.audio_mode);
                                warn_once!(
                                    logging::intern_key(format!(
                                        "playback:unsupported_mode:{}",
                                        self.audio_mode
                                    )),
                                    "playback: dropping audio: server negotiated mode {}, \
                                     which ryll cannot play (only raw PCM and Opus)",
                                    self.audio_mode
                                );
                            }
                        }
                    }
                }
                playback_server::STOP => {
                    info!("playback: STOP");
                    self.stop_count = self.stop_count.saturating_add(1);
                    self.current_session = None;
                    self.stop_audio();
                    self.opus_decoder = None;
                }
                playback_server::VOLUME => {
                    // SPICE_MSG_PLAYBACK_VOLUME wraps a SpiceMsgAudioVolume
                    // payload. Per spice-common/spice.proto:
                    //   message AudioVolume {
                    //       uint8 nchannels;
                    //       uint16 volume[nchannels] @end;
                    //   }
                    // SPICE wire format is little-endian, so each u16
                    // volume is decoded as u16::from_le_bytes.
                    if !payload.is_empty() {
                        let nch = payload[0] as usize;
                        let needed = 1 + nch * 2;
                        if payload.len() >= needed {
                            let mut vols = Vec::with_capacity(nch);
                            for i in 0..nch {
                                let off = 1 + i * 2;
                                vols.push(u16::from_le_bytes([payload[off], payload[off + 1]]));
                            }
                            self.last_volume_per_channel = vols;
                            debug!("playback: VOLUME: {:?}", self.last_volume_per_channel);
                        }
                    }
                }
                playback_server::MUTE => {
                    // SPICE_MSG_PLAYBACK_MUTE: u8 mute.
                    if !payload.is_empty() {
                        self.last_mute = Some(payload[0] != 0);
                        debug!("playback: MUTE: {:?}", self.last_mute);
                    }
                }
                playback_server::LATENCY => {
                    // SPICE_MSG_PLAYBACK_LATENCY: u32 latency_ms.
                    if payload.len() >= 4 {
                        let lat =
                            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                        self.last_latency_ms = Some(lat);
                        debug!("playback: LATENCY: {} ms", lat);
                    }
                }
                _ => {
                    logging::log_unknown_once("playback", msg_type, payload);
                    self.opcodes.note_unknown(msg_type);
                }
            }
        }
        Ok(())
    }

    /// Push raw PCM samples into the ring buffer. Bumps the
    /// `ring_overflow_count` atomic on each sample that the
    /// producer fails to enqueue. Always treated as a successful
    /// "decode" (raw mode skips decoding).
    fn push_samples_raw(&mut self, audio_data: &[u8]) {
        if let Some(ref mut producer) = self.audio_producer {
            for chunk in audio_data.as_chunks::<2>().0 {
                let sample = i16::from_le_bytes(*chunk);
                if producer.push(sample).is_err() {
                    self.audio_counters
                        .ring_overflow_count
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Decode Opus audio and push PCM samples into the ring buffer.
    /// Returns `Some(byte_count)` of decoded PCM bytes produced
    /// on success, `None` if the decoder errored or no decoder
    /// is active. Bumps `ring_overflow_count` on each sample the
    /// producer fails to enqueue.
    fn push_samples_opus(&mut self, audio_data: &[u8]) -> Option<u64> {
        let decoder = self.opus_decoder.as_mut()?;
        let ch = self.channels as usize;
        let mut pcm = vec![0i16; opus_decoder::OpusDecoder::MAX_FRAME_SIZE_48K * ch];
        match decoder.decode(audio_data, &mut pcm, false) {
            Ok(samples) => {
                let total = samples * ch;
                if let Some(ref mut producer) = self.audio_producer {
                    for &s in &pcm[..total] {
                        if producer.push(s).is_err() {
                            self.audio_counters
                                .ring_overflow_count
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                // Each i16 sample is 2 bytes of PCM.
                Some((total as u64).saturating_mul(2))
            }
            Err(e) => {
                if self.decode_failure_logged {
                    debug!("playback: Opus decode error: {}", e);
                } else {
                    warn!(
                        "playback: Opus decode error: {} (further failures this \
                         session are counted in data_packets_decode_failed)",
                        e
                    );
                    self.decode_failure_logged = true;
                }
                None
            }
        }
    }

    /// Bookkeeping helper for a successful DATA packet decode:
    /// bumps the success / pcm-byte counters and pushes the
    /// decode duration into the bounded ring (cap
    /// [`MAX_RECENT_PLAYBACK_DECODES`]).
    fn record_decode_success(&mut self, dur_us: u32, pcm_bytes: u64) {
        self.data_packets_decoded = self.data_packets_decoded.saturating_add(1);
        self.pcm_bytes_produced = self.pcm_bytes_produced.saturating_add(pcm_bytes);
        if self.recent_decode_durations_us.len() >= MAX_RECENT_PLAYBACK_DECODES {
            self.recent_decode_durations_us.pop_front();
        }
        self.recent_decode_durations_us.push_back(dur_us);
    }

    /// Create the ring buffer and spawn the audio thread.
    fn start_audio_output(&mut self) {
        // Stop any existing audio thread first.
        self.stop_audio();

        let (producer, consumer) = rtrb::RingBuffer::new(MAX_AUDIO_BUFFER_SAMPLES);
        self.audio_producer = Some(producer);

        match AudioThread::spawn(
            consumer,
            self.volume_control.clone(),
            self.sample_rate,
            self.channels,
            self.audio_counters.clone(),
        ) {
            Some(thread) => {
                self.audio_thread = Some(thread);
            }
            None => {
                warn!("playback: failed to spawn audio thread");
                self.audio_producer = None;
            }
        }
    }

    /// Stop the audio thread and drop the producer.
    fn stop_audio(&mut self) {
        self.audio_producer = None;
        if let Some(thread) = self.audio_thread.take() {
            // Fold in whatever the thread reported since the last
            // drain so the snapshot keeps the final error. The
            // session is over, so nothing is worth notifying.
            for event in thread.stop() {
                let _ = self.output_status.apply(event);
            }
        }
        self.output_status.session_ended();
    }

    /// Fold output-stream events from the audio thread into the
    /// status and raise the notifications they call for.
    async fn drain_output_events(&mut self) {
        let Some(thread) = self.audio_thread.as_ref() else {
            return;
        };
        let events: Vec<AudioOutputEvent> = thread.events.try_iter().collect();
        if events.is_empty() {
            return;
        }
        for event in events {
            let note = self.output_status.apply(event);
            // In web mode the operator hears audio through the
            // browser, not this host's speakers, so local output
            // trouble is diagnostic detail, not news.
            if let (Some(note), None) = (note, self.opus_sink.as_ref()) {
                self.events.emit(ChannelEvent::Notification(note)).await;
            }
        }
        self.update_snapshot();
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let payload_size = data.len().saturating_sub(MessageHeader::SIZE) as u32;
        if self.log_config.verbose {
            logging::log_message(
                "sent",
                "playback",
                msg_type,
                message_names::playback_client(msg_type),
                payload_size,
            );
        }
        // Single send path, so this is the only send-count site.
        self.opcodes.record_send(msg_type);
        match &mut self.stream {
            SpiceStream::Plain(s) => {
                use tokio::io::AsyncWriteExt;
                s.write_all(data).await?;
            }
            SpiceStream::Tls(s) => {
                use tokio::io::AsyncWriteExt;
                s.write_all(data).await?;
            }
            SpiceStream::TlsServer(s) => {
                use tokio::io::AsyncWriteExt;
                s.write_all(data).await?;
            }
        }
        self.bytes_out += data.len() as u64;
        self.last_send_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
        self.update_snapshot();
        Ok(())
    }

    /// Sync local state to the shared snapshot. The atomic
    /// `device_*` / `ring_overflow_count` /
    /// `samples_consumed_total` counters are loaded with
    /// `Ordering::Relaxed` — single non-blocking load per
    /// counter, so the cpal callback is never blocked by a
    /// snapshot read.
    fn update_snapshot(&self) {
        if let Ok(mut snap) = self.snapshot.lock() {
            // Transport common.
            snap.bytes_in = self.bytes_in;
            snap.bytes_out = self.bytes_out;
            snap.last_recv_ts_secs = self.last_recv_ts_secs;
            snap.last_send_ts_secs = self.last_send_ts_secs;
            snap.ping_recv_count = self.ping_recv_count;
            snap.pong_send_count = self.pong_send_count;
            snap.last_ping_recv_ts_secs = self.last_ping_recv_ts_secs;

            // Baseline additions.
            self.opcodes.publish_into(&mut *snap);

            // Per-session audio state.
            snap.current_session = self.current_session.clone();
            snap.start_count = self.start_count;
            snap.stop_count = self.stop_count;

            // Audio-data plumbing counters.
            snap.data_packets_received = self.data_packets_received;
            snap.data_packets_decoded = self.data_packets_decoded;
            snap.data_packets_decode_failed = self.data_packets_decode_failed;
            snap.data_bytes_received = self.data_bytes_received;
            snap.pcm_bytes_produced = self.pcm_bytes_produced;
            snap.recent_decode_durations_us = self.recent_decode_durations_us.clone();
            snap.data_packets_unsupported_codec = self.data_packets_unsupported_codec;

            // Local output stream state.
            snap.output = self.output_status.output.clone();
            snap.output_error = self.output_status.error.clone();
            snap.output_streams_started = self.output_status.streams_started;
            snap.output_failure_count = self.output_status.failure_count;
            snap.output_reroute_count = self.output_status.reroute_count;

            // Device-side counters from the audio thread atomics.
            // `load` and `fetch_add(0, ..)` are equivalent for
            // reading; `load` is the natural choice here.
            snap.device_callbacks_total = self
                .audio_counters
                .device_callbacks_total
                .load(Ordering::Relaxed);
            snap.device_underrun_count = self
                .audio_counters
                .device_underrun_count
                .load(Ordering::Relaxed);
            snap.ring_overflow_count = self
                .audio_counters
                .ring_overflow_count
                .load(Ordering::Relaxed);
            snap.samples_consumed_total = self
                .audio_counters
                .samples_consumed_total
                .load(Ordering::Relaxed);
            snap.device_xrun_count = self
                .audio_counters
                .device_xrun_count
                .load(Ordering::Relaxed);

            // Last server-controlled audio params.
            snap.last_volume_per_channel = self.last_volume_per_channel.clone();
            snap.last_mute = self.last_mute;
            snap.last_latency_ms = self.last_latency_ms;
        }
    }
}

/// Map a SPICE audio-mode value to a [`PlaybackCodec`]. The
/// mode comes from `SPICE_MSG_PLAYBACK_MODE`; values 1 and 3
/// are the standard RAW and OPUS modes, anything else is
/// surfaced via `Other` so a bug report can spot a codec we
/// haven't seen.
fn codec_from_mode(mode: u16) -> crate::snapshots::PlaybackCodec {
    match mode {
        AUDIO_DATA_MODE_RAW => crate::snapshots::PlaybackCodec::Raw,
        AUDIO_DATA_MODE_OPUS => crate::snapshots::PlaybackCodec::Opus,
        other => crate::snapshots::PlaybackCodec::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        opus_packet_samples_48k, pcm_bytes_to_i16, samples_per_frame_48k, AudioCounters,
        AudioOutputEvent, AudioThread, CallbackState, OutputStatus, Resampler,
    };
    use crate::snapshots::PlaybackOutputInfo;
    use shakenfist_spice_protocol::NotifySeverity;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // --- CallbackState and audio-thread helper tests ---

    fn callback_state(queued: &[i16], buffered: &[i16]) -> CallbackState {
        let (mut producer, consumer) = rtrb::RingBuffer::new(64);
        for &sample in queued {
            producer.push(sample).unwrap();
        }
        CallbackState {
            consumer,
            local_buf: buffered.iter().copied().collect(),
        }
    }

    #[test]
    fn callback_state_discard_empties_ring_and_local_buf() {
        let mut st = callback_state(&[1, 2, 3, 4], &[5, 6]);
        st.discard();
        assert_eq!(st.consumer.slots(), 0, "ring must be drained");
        assert!(st.local_buf.is_empty(), "local buffer must be cleared");
    }

    #[test]
    fn callback_state_drain_ring_appends_in_order() {
        let mut st = callback_state(&[3, 4], &[1, 2]);
        st.drain_ring();
        assert_eq!(st.consumer.slots(), 0);
        assert_eq!(st.local_buf, VecDeque::from(vec![1, 2, 3, 4]));
    }

    #[test]
    fn lock_for_callback_recovers_a_poisoned_lock() {
        let state = Arc::new(Mutex::new(callback_state(&[7], &[])));
        let poisoner = state.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the callback lock");
        })
        .join();
        assert!(state.is_poisoned());

        let counters = AudioCounters::new();
        let guard = CallbackState::lock_for_callback(&state, &counters)
            .expect("a poisoned lock must still yield the state");
        assert_eq!(guard.consumer.slots(), 1);
        assert_eq!(counters.device_underrun_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn lock_for_callback_counts_contention_as_underrun() {
        let state = Mutex::new(callback_state(&[], &[]));
        let counters = AudioCounters::new();
        let _held = state.lock().unwrap();
        assert!(CallbackState::lock_for_callback(&state, &counters).is_none());
        assert_eq!(counters.device_underrun_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn sleep_unless_shutdown_returns_promptly_on_shutdown() {
        let shutdown = AtomicBool::new(true);
        let start = Instant::now();
        AudioThread::sleep_unless_shutdown(Duration::from_secs(10), &shutdown);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn sleep_unless_shutdown_sleeps_for_the_delay() {
        let shutdown = AtomicBool::new(false);
        let start = Instant::now();
        AudioThread::sleep_unless_shutdown(Duration::from_millis(50), &shutdown);
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    // --- OutputStatus tests ---

    fn output(device: &str) -> PlaybackOutputInfo {
        PlaybackOutputInfo {
            device: device.to_string(),
            sample_rate_hz: 48000,
            channels: 2,
            sample_format: "f32".to_string(),
        }
    }

    #[test]
    fn output_status_routine_start_is_silent() {
        let mut status = OutputStatus::default();
        let note = status.apply(AudioOutputEvent::Started(output("Speakers")));
        assert!(note.is_none(), "a normal START must not notify");
        assert_eq!(status.output, Some(output("Speakers")));
        assert_eq!(status.streams_started, 1);
        assert!(status.error.is_none());
    }

    #[test]
    fn output_status_failure_streak_notifies_once_then_recovery() {
        let mut status = OutputStatus::default();
        status.apply(AudioOutputEvent::Started(output("Headphones")));

        let first = status.apply(AudioOutputEvent::Failed("device unplugged".to_string()));
        let first = first.expect("first failure must notify");
        assert_eq!(first.severity, NotifySeverity::Warn);
        assert!(first.message.contains("device unplugged"));
        assert!(status.output.is_none());

        // Retries during the same streak are counted, not announced.
        let retry = status.apply(AudioOutputEvent::Failed(
            "no audio output device found".to_string(),
        ));
        assert!(retry.is_none());
        assert_eq!(status.failure_count, 2);
        assert_eq!(
            status.error.as_deref(),
            Some("no audio output device found")
        );

        let restored = status
            .apply(AudioOutputEvent::Started(output("Speakers")))
            .expect("recovery must notify");
        assert_eq!(restored.severity, NotifySeverity::Info);
        assert!(restored.message.contains("Speakers"));
        assert!(status.error.is_none());
        assert_eq!(status.streams_started, 2);
    }

    #[test]
    fn output_status_reroute_updates_device_and_notifies() {
        let mut status = OutputStatus::default();
        status.apply(AudioOutputEvent::Started(output("Speakers")));
        let note = status
            .apply(AudioOutputEvent::Rerouted("AirPods".to_string()))
            .expect("reroute must notify");
        assert!(note.message.contains("AirPods"));
        assert_eq!(
            status.output.as_ref().map(|o| o.device.as_str()),
            Some("AirPods")
        );
        assert_eq!(status.reroute_count, 1);
    }

    #[test]
    fn output_status_session_end_keeps_error_and_rearms_notification() {
        let mut status = OutputStatus::default();
        status.apply(AudioOutputEvent::Failed(
            "no audio output device found".to_string(),
        ));
        status.session_ended();
        assert!(status.output.is_none());
        assert_eq!(
            status.error.as_deref(),
            Some("no audio output device found")
        );
        // A failure in the next session is news again.
        let note = status.apply(AudioOutputEvent::Failed(
            "no audio output device found".to_string(),
        ));
        assert!(note.is_some());
    }

    // --- Resampler tests ---

    /// Helper: fill a VecDeque from a slice.
    fn make_buf(samples: &[i16]) -> VecDeque<i16> {
        samples.iter().copied().collect()
    }

    #[test]
    fn resampler_1to1_mono_produces_correct_samples() {
        let mut r = Resampler::new(48000, 48000, 1);
        // With ratio=1.0 and pos starting at 0.0, idx=0, frac=0.0.
        // We need (0+2)*1 = 2 samples minimum.  Push more to test iteration.
        let samples: &[i16] = &[100, 200, 300, 400];
        let mut buf = make_buf(samples);

        // First call: interpolates between samples[0]=100 and samples[1]=200,
        // frac=0.0 → output should be 100.
        let mut out = [0i16; 1];
        r.next_frame(&mut buf, &mut out);
        assert_eq!(out[0], 100, "first frame should be 100");

        // After first call, pos advances by ratio=1.0 → consume_frames=1,
        // one sample popped.  buf is now [200, 300, 400], pos=0.0.
        // Second call: output should be 200.
        r.next_frame(&mut buf, &mut out);
        assert_eq!(out[0], 200, "second frame should be 200");
    }

    #[test]
    fn resampler_1to1_stereo_separates_channels() {
        let mut r = Resampler::new(48000, 48000, 2);
        // Interleaved L/R: [100, -100, 200, -200, 300, -300, 400, -400]
        // Need (0+2)*2 = 4 samples minimum.
        let samples: &[i16] = &[100, -100, 200, -200, 300, -300, 400, -400];
        let mut buf = make_buf(samples);

        let mut out = [0i16; 2];
        r.next_frame(&mut buf, &mut out);
        // frac=0.0 → output = frame[0] = [100, -100]
        assert_eq!(out[0], 100, "L channel should be 100");
        assert_eq!(out[1], -100, "R channel should be -100");
    }

    #[test]
    fn resampler_underrun_returns_silence_and_leaves_buffer_empty() {
        let mut r = Resampler::new(48000, 48000, 1);
        let mut buf: VecDeque<i16> = VecDeque::new();

        let mut out = [0i16; 1];
        r.next_frame(&mut buf, &mut out);

        assert_eq!(out[0], 0, "underrun should produce silence");
        assert!(buf.is_empty(), "buffer should remain empty after underrun");
    }

    // --- Audio-tap helpers ---

    #[test]
    fn pcm_bytes_to_i16_decodes_little_endian() {
        let bytes = [0x01, 0x00, 0xff, 0xff, 0x00, 0x80];
        let samples = pcm_bytes_to_i16(&bytes);
        assert_eq!(samples, vec![1i16, -1, i16::MIN]);
    }

    #[test]
    fn pcm_bytes_to_i16_drops_trailing_odd_byte() {
        // The chunks_exact(2) loop ignores the trailing single byte.
        let bytes = [0x01, 0x00, 0x42];
        let samples = pcm_bytes_to_i16(&bytes);
        assert_eq!(samples, vec![1i16]);
    }

    #[test]
    fn samples_per_frame_48k_celt_only_20ms_is_960() {
        // CELT-only config 19 (0b10011, top 5 bits) is 20 ms at
        // 48 kHz = 960 samples. TOC layout: config<<3 | s<<2 | code.
        let toc = 19u8 << 3;
        assert_eq!(samples_per_frame_48k(toc), 960);
    }

    #[test]
    fn opus_packet_samples_48k_code0_returns_one_frame_worth() {
        // Code 0 = one frame in the packet. CELT-only 20 ms.
        let toc = 19u8 << 3; // code = 0
        let pkt = [toc, 0xaa, 0xbb];
        assert_eq!(opus_packet_samples_48k(&pkt), 960);
    }

    #[test]
    fn opus_packet_samples_48k_code1_doubles_frame_count() {
        // Code 1 = two frames CBR. 20 ms × 2 = 40 ms = 1920.
        let toc = (19u8 << 3) | 1;
        let pkt = [toc, 0x11, 0x22, 0x33, 0x44];
        assert_eq!(opus_packet_samples_48k(&pkt), 1920);
    }

    #[test]
    fn opus_packet_samples_48k_empty_falls_back_to_960() {
        assert_eq!(opus_packet_samples_48k(&[]), 960);
    }

    #[test]
    fn opus_packet_samples_48k_code3_reads_frame_count_byte() {
        // Code 3 = M frames; the next byte's low 6 bits are M.
        // 20 ms config × 3 frames = 2880 samples.
        let toc = (19u8 << 3) | 3;
        let pkt = [toc, 0x03, 0x00, 0x00];
        assert_eq!(opus_packet_samples_48k(&pkt), 2880);
    }

    #[test]
    fn resampler_2to1_upsampling_interpolates() {
        // source=24000, device=48000 → ratio=0.5
        // Each output frame advances pos by 0.5; every two output frames
        // consume one input frame.
        let mut r = Resampler::new(24000, 48000, 1);
        // Need at least (0+2)*1=2 input samples so lookahead is satisfied.
        let samples: &[i16] = &[0, 1000, 2000];
        let mut buf = make_buf(samples);

        // Output frame 0: pos=0.0, idx=0, frac=0.0 → lerp(0, 1000, 0.0)=0
        let mut out = [0i16; 1];
        r.next_frame(&mut buf, &mut out);
        assert_eq!(out[0], 0, "upsampled frame 0 should be 0");

        // Output frame 1: pos=0.5, idx=0, frac=0.5 → lerp(0, 1000, 0.5)=500
        // After this call pos=1.0, consume_frames=1, pop 1 sample, pos=0.0.
        r.next_frame(&mut buf, &mut out);
        assert_eq!(out[0], 500, "upsampled frame 1 should be ~500");

        // Output frame 2: pos=0.0, buf=[1000,2000], idx=0 → lerp(1000,2000,0.0)=1000
        r.next_frame(&mut buf, &mut out);
        assert_eq!(out[0], 1000, "upsampled frame 2 should be 1000");
    }
}
