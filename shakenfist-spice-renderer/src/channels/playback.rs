use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(feature = "audio")]
use std::sync::{mpsc, MutexGuard, TryLockError};
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(feature = "audio")]
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use super::volume::VolumeControl;
use crate::opcode_counters::OpcodeCounters;
use crate::playback_sink::{PlaybackFormat, PlaybackOutput, PlaybackSink};
use crate::{ByteCounter, LogConfig, NotificationEntry, NotificationSource, TrafficSink};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, MessageHeader, Notify as NotifyMessage, Ping, PlaybackLatency, PlaybackMode,
    PlaybackMute, PlaybackPacketRef, PlaybackStart, PlaybackVolume, ReceiveBuffer, SetAck,
    WireType,
};
use shakenfist_spice_protocol::{
    audio_data_mode, main_client, playback_server, warn_once, ChannelType, NotifySeverity,
};

use super::{ChannelEvent, DisconnectReason, EventSink, MAX_MESSAGE_BODY};

/// Maximum ring buffer capacity in samples. At 48kHz stereo this is
/// ~2 seconds. Prevents unbounded memory growth if audio data
/// arrives faster than it is consumed.
#[cfg(feature = "audio")]
const MAX_AUDIO_BUFFER_SAMPLES: usize = 48000 * 2 * 2;

/// Cap for the recent-decode-duration ring published into the
/// playback snapshot. Mirrors display.rs's `recent_decodes`
/// pattern: a bounded window of recent measurements so a bug
/// report has fresh latency data without growing unbounded.
const MAX_RECENT_PLAYBACK_DECODES: usize = 64;

/// The rate Opus decodes at. The decoder is created at 48 kHz, so
/// every decoded sample handed on is at this rate.
const OPUS_DECODE_RATE_HZ: u32 = 48_000;

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
#[cfg(feature = "audio")]
const OUTPUT_RETRY_INITIAL: Duration = Duration::from_secs(1);

/// Cap on the doubling retry delay. Long enough that a machine
/// with no output device at all is not busy-looping, short
/// enough that plugging headphones back in is noticed promptly.
#[cfg(feature = "audio")]
const OUTPUT_RETRY_MAX: Duration = Duration::from_secs(30);

/// How long a stream must keep playing before it counts as
/// recovered. Until then a failure keeps doubling the backoff
/// and continues the failure streak, so a device that dies
/// straight after opening is retried quietly, at most every
/// `OUTPUT_RETRY_MAX`, rather than rebuilt every second with a
/// "restored" and an "unavailable" notification each time.
#[cfg(feature = "audio")]
const OUTPUT_STABLE_AFTER: Duration = Duration::from_secs(10);

/// How often the audio thread wakes to check for shutdown while
/// it is watching a stream or waiting to retry.
#[cfg(feature = "audio")]
const AUDIO_THREAD_POLL: Duration = Duration::from_millis(50);

/// What the audio thread reports about the output stream. It
/// has no async context to emit channel events from, so it
/// sends these to the playback channel, which folds them into
/// the snapshot and raises notifications (see [`OutputStatus`]).
///
/// Only the audio thread sends these, so a build without `audio`
/// never constructs one; the type and [`OutputStatus::apply`] stay
/// compiled so the state machine's tests run in every build.
#[derive(Debug)]
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
enum AudioOutputEvent {
    /// An output stream is playing.
    Started(crate::snapshots::PlaybackOutputInfo),
    /// The stream from the last `Started` has played for
    /// `OUTPUT_STABLE_AFTER`, which ends a failure streak.
    Stable,
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
    /// True from the first failure until a stream has played for
    /// `OUTPUT_STABLE_AFTER`; a failure streak raises one
    /// notification, not one per retry, however often a flapping
    /// device opens and dies within it.
    failing: bool,
    streams_started: u64,
    failure_count: u64,
    reroute_count: u64,
}

impl OutputStatus {
    /// Fold one event into the status. Returns the notification
    /// the operator should see, if any: the first failure of a
    /// streak, the stable stream that ends it, and reroutes. A
    /// routine start on PLAYBACK_START is silent.
    #[cfg_attr(not(feature = "audio"), allow(dead_code))]
    fn apply(&mut self, event: AudioOutputEvent) -> Option<NotificationEntry> {
        match event {
            AudioOutputEvent::Started(output) => {
                self.streams_started = self.streams_started.saturating_add(1);
                self.error = None;
                self.output = Some(output);
                None
            }
            AudioOutputEvent::Stable => {
                let note = self.failing.then(|| {
                    let device = self
                        .output
                        .as_ref()
                        .map_or("unknown device", |o| o.device.as_str());
                    NotificationEntry::new(
                        NotifySeverity::Info,
                        NotificationSource::Internal,
                        format!("Audio output restored on {}", device),
                    )
                });
                self.failing = false;
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

/// Linear-interpolation resampler from the stream's rate to the
/// output device's. Only the local output thread uses it, but it
/// holds no device state, so it stays compiled (and tested) in a
/// build without `audio`.
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
pub(crate) struct Resampler {
    ratio: f64,
    pos: f64,
    channels: usize,
}

#[cfg_attr(not(feature = "audio"), allow(dead_code))]
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
#[cfg(feature = "audio")]
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
#[cfg(feature = "audio")]
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

/// Convert a little-endian PCM byte stream into `out`, replacing
/// what it held, for raw-mode DATA. A trailing odd byte is
/// dropped. The caller reuses `out` across packets, so the hot
/// path does not allocate once it has grown.
fn pcm_bytes_to_i16(bytes: &[u8], out: &mut Vec<i16>) {
    out.clear();
    out.reserve(bytes.len() / 2);
    for chunk in bytes.as_chunks::<2>().0 {
        out.push(i16::from_le_bytes(*chunk));
    }
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
#[cfg(feature = "audio")]
struct CallbackState {
    consumer: rtrb::Consumer<i16>,
    local_buf: VecDeque<i16>,
}

#[cfg(feature = "audio")]
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

/// The retry delay to carry on with after a stream that started
/// and then ended. Only a stream that reached `Stable` resets the
/// backoff; one that died sooner keeps doubling it, so a device
/// that dies straight after opening is not rebuilt every second.
#[cfg(feature = "audio")]
fn backoff_after_stream(backoff: Duration, stable: bool) -> Duration {
    if stable {
        OUTPUT_RETRY_INITIAL
    } else {
        backoff
    }
}

/// Human-readable name for an output device, for logs,
/// notifications and the snapshot.
#[cfg(feature = "audio")]
fn device_name(device: &cpal::Device) -> String {
    use cpal::traits::DeviceTrait;
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown device".to_string())
}

/// State for the dedicated audio output thread.
#[cfg(feature = "audio")]
struct AudioThread {
    handle: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
    events: mpsc::Receiver<AudioOutputEvent>,
}

#[cfg(feature = "audio")]
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
        // Set by the first failure. Every later pass of the loop
        // is a retry: there is stale audio to discard, and the
        // failure being retried has already been logged loudly.
        let mut retrying = false;

        while !shutdown.load(Ordering::Relaxed) {
            if retrying {
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

                    let (died, stable) = Self::watch_stream(&err_rx, &shutdown, &events);
                    // Dropping the stream releases the device.
                    drop(stream);
                    backoff = backoff_after_stream(backoff, stable);
                    match died {
                        Some(reason) => {
                            warn!("playback: audio output stream failed: {}", reason);
                            let _ = events.send(AudioOutputEvent::Failed(reason));
                            retrying = true;
                        }
                        None => break,
                    }
                }
                Err(reason) => {
                    // Retries repeat the same failure; log the
                    // first loudly and the rest quietly.
                    if retrying {
                        debug!("playback: audio output still unavailable: {}", reason);
                    } else {
                        warn!("playback: audio output unavailable: {}", reason);
                    }
                    let _ = events.send(AudioOutputEvent::Failed(reason));
                    retrying = true;
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

    /// Block until the stream dies or shutdown is requested,
    /// sending `Stable` once it has played for
    /// `OUTPUT_STABLE_AFTER`. Returns why the stream died (`None`
    /// on shutdown), and whether it got as far as `Stable`.
    fn watch_stream(
        err_rx: &mpsc::Receiver<cpal::Error>,
        shutdown: &AtomicBool,
        events: &mpsc::Sender<AudioOutputEvent>,
    ) -> (Option<String>, bool) {
        use cpal::traits::HostTrait;

        let started = Instant::now();
        let mut stable = false;
        let mut realtime_denied_logged = false;
        while !shutdown.load(Ordering::Relaxed) {
            if !stable && started.elapsed() >= OUTPUT_STABLE_AFTER {
                stable = true;
                let _ = events.send(AudioOutputEvent::Stable);
            }
            let err = match err_rx.recv_timeout(AUDIO_THREAD_POLL) {
                Ok(err) => err,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return (Some("audio error channel closed".to_string()), stable);
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
                _ => return (Some(err.to_string()), stable),
            }
        }
        (None, stable)
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
    buffer: ReceiveBuffer,
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
    /// Producer end of the ring feeding the local output thread.
    /// Some only while a `Local` session's thread is running.
    #[cfg(feature = "audio")]
    audio_producer: Option<rtrb::Producer<i16>>,
    #[cfg(feature = "audio")]
    audio_thread: Option<AudioThread>,
    /// How many times this channel has tried to start the local
    /// output thread, so tests can check a `Sink` session never does.
    #[cfg(test)]
    audio_thread_starts: u32,
    opus_decoder: Option<opus_decoder::OpusDecoder>,
    #[cfg(feature = "audio")]
    volume_control: Arc<VolumeControl>,
    /// Where the audio goes. `Local` plays it on the cpal thread
    /// (only in a build with `audio`); `Sink` hands it to the sink
    /// and never starts that thread. Both decode Opus, so the
    /// decode counters mean the same either way.
    output: PlaybackOutput,
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
    /// Raw-mode DATA converted to samples, kept between packets so
    /// the conversion does not allocate per packet.
    raw_samples: Vec<i16>,
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
    /// Whether an unsupported mode has been warned about, so the
    /// per-packet path only reaches `warn_once!` once.
    unsupported_mode_warned: bool,
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
        output: PlaybackOutput,
    ) -> Self {
        // Only the local output thread applies the volume.
        #[cfg(not(feature = "audio"))]
        let _ = volume_control;
        PlaybackChannel {
            stream,
            events,
            buffer: ReceiveBuffer::with_capacity(65536),
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
            #[cfg(feature = "audio")]
            audio_producer: None,
            #[cfg(feature = "audio")]
            audio_thread: None,
            #[cfg(test)]
            audio_thread_starts: 0,
            opus_decoder: None,
            #[cfg(feature = "audio")]
            volume_control,
            output,
            cancel,
            audio_counters: AudioCounters::new(),
            opcodes: OpcodeCounters::new(
                message_names::playback_server,
                message_names::playback_client,
            ),
            current_session: None,
            raw_samples: Vec::new(),
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
            unsupported_mode_warned: false,
            decode_failure_logged: false,
            output_status: OutputStatus::default(),
        }
    }

    /// Public entry point. Wraps `run_loop` so errors
    /// propagating out of the inner select! arms are logged
    /// before the task ends — see `MainChannel::run` for the
    /// rationale (including the `Box::pin` reason).
    pub async fn run(&mut self) -> Result<()> {
        // Held across the loop, so the snapshot reads as unlinked again
        // however the channel ends, cancellation included.
        let _unlink = UnlinkOnDrop(Arc::clone(&self.snapshot));
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
                        .emit(ChannelEvent::Disconnected(
                            ChannelType::Playback,
                            DisconnectReason::ServerClosed,
                        ))
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
        while let Some(message) = self.buffer.take_message(MAX_MESSAGE_BODY)? {
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
                    let set_ack = SetAck::decode(payload).context("malformed SET_ACK")?;
                    self.ack_generation = set_ack.generation;
                    self.ack_window = set_ack.window;
                    self.message_count = 0;
                    self.last_ack = 0;
                    let mut ack_payload = Vec::new();
                    set_ack.ack_sync().write(&mut ack_payload);
                    let response = make_message(main_client::ACK_SYNC, &ack_payload);
                    self.send_with_log(main_client::ACK_SYNC, &response).await?;
                }
                playback_server::PING => {
                    self.ping_recv_count = self.ping_recv_count.saturating_add(1);
                    self.last_ping_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

                    let ping = Ping::decode(payload).context("malformed PING")?;
                    let mut pong_payload = Vec::new();
                    ping.pong().write(&mut pong_payload);
                    let response = make_message(main_client::PONG, &pong_payload);
                    self.send_with_log(main_client::PONG, &response).await?;
                    self.pong_send_count = self.pong_send_count.saturating_add(1);
                }
                playback_server::NOTIFY => {
                    let notify = NotifyMessage::decode(payload).context("malformed NOTIFY")?;
                    let severity = notify.severity_kind();
                    let message = notify.message_text().into_owned();
                    if self.log_config.verbose {
                        logging::log_detail(&format!(
                            "severity={:?}, visibility={:?}, what={}, message=\"{}\"",
                            severity,
                            notify.visibility_kind(),
                            notify.what,
                            message,
                        ));
                    }
                    match severity {
                        NotifySeverity::Error => {
                            warn!("playback: server notify (error): {}", message)
                        }
                        NotifySeverity::Warn => {
                            warn!("playback: server notify (warn): {}", message)
                        }
                        NotifySeverity::Info => {
                            info!("playback: server notify: {}", message)
                        }
                    }
                    let mut entry = NotificationEntry::new(
                        severity,
                        NotificationSource::Spice {
                            channel: ChannelType::Playback,
                            what: notify.what,
                        },
                        message,
                    );
                    if let Some(v) = notify.visibility_kind() {
                        entry = entry.with_visibility(v);
                    }
                    self.events.emit(ChannelEvent::Notification(entry)).await;
                }
                playback_server::START => {
                    // A malformed body is ignored, as it always has been.
                    if let Ok(start) = PlaybackStart::decode(payload) {
                        self.channels = start.channels;
                        let format = start.format;
                        self.sample_rate = start.frequency;
                        let time = start.time;
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
                            channels: self.channels_u8(),
                            codec: codec_from_mode(self.audio_mode),
                        });
                        self.start_audio_output();
                        // Opus always operates at 48kHz internally; the
                        // channel count comes from the SPICE START message.
                        self.opus_decoder = match opus_decoder::OpusDecoder::new(
                            OPUS_DECODE_RATE_HZ,
                            self.channels as usize,
                        ) {
                            Ok(d) => {
                                info!("playback: Opus decoder initialized");
                                Some(d)
                            }
                            Err(e) => {
                                warn!("playback: failed to create Opus decoder: {}", e);
                                None
                            }
                        };
                        if let Some(sink) = self.sink() {
                            sink.on_start(self.format());
                        }
                    }
                }
                playback_server::MODE => {
                    // The multimedia time is not used.
                    if let Ok(mode) = PlaybackMode::decode(payload) {
                        self.audio_mode = mode.mode;
                        info!("playback: MODE: {}", self.audio_mode);
                        // If a session is active, refresh its codec
                        // to match the new MODE — the server can
                        // change codec mid-session in principle.
                        if let Some(ref mut session) = self.current_session {
                            session.codec = codec_from_mode(self.audio_mode);
                            // A sink hears of the change as a fresh
                            // start, before any data in the new mode.
                            if let Some(sink) = self.sink() {
                                sink.on_start(self.format());
                            }
                        }
                    }
                }
                playback_server::DATA => {
                    // The multimedia time is not used: samples play in
                    // arrival order. A packet with no data is skipped.
                    self.data_packets_received = self.data_packets_received.saturating_add(1);
                    let packet = PlaybackPacketRef::decode(payload).ok();
                    if let Some(audio_data) = packet.map(|p| p.data).filter(|d| !d.is_empty()) {
                        self.data_bytes_received = self
                            .data_bytes_received
                            .saturating_add(audio_data.len() as u64);
                        // The sink is called outside the timed spans, so
                        // a slow sink does not read as slow decode.
                        if self.audio_mode == audio_data_mode::RAW {
                            let start = Instant::now();
                            let mut samples = std::mem::take(&mut self.raw_samples);
                            pcm_bytes_to_i16(audio_data, &mut samples);
                            self.push_local_output(&samples);
                            let dur_us = start.elapsed().as_micros().min(u32::MAX as u128) as u32;
                            self.record_decode_success(dur_us, audio_data.len() as u64);
                            if let Some(sink) = self.sink() {
                                sink.on_pcm_samples(&samples, self.sample_rate, self.channels_u8());
                            }
                            self.raw_samples = samples;
                        } else if self.audio_mode == audio_data_mode::OPUS {
                            // Pre-decode tap: the sink gets the packet
                            // as it arrived, then what it decodes to.
                            if let Some(sink) = self.sink() {
                                let samples = opus_packet_samples_48k(audio_data);
                                sink.on_opus_packet(audio_data, samples);
                            }
                            let start = Instant::now();
                            let decoded = self.decode_opus(audio_data);
                            if let Some(ref pcm) = decoded {
                                self.push_local_output(pcm);
                            }
                            let dur_us = start.elapsed().as_micros().min(u32::MAX as u128) as u32;
                            match decoded {
                                Some(pcm) => {
                                    // Each i16 sample is 2 bytes of PCM.
                                    let bytes = (pcm.len() as u64).saturating_mul(2);
                                    self.record_decode_success(dur_us, bytes);
                                    if let Some(sink) = self.sink() {
                                        sink.on_decoded_pcm(
                                            &pcm,
                                            OPUS_DECODE_RATE_HZ,
                                            self.channels_u8(),
                                        );
                                    }
                                }
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
                            // The key is fixed: the mode is a server-
                            // chosen u16, so a key per mode would let a
                            // server leak an interned key, and write a
                            // --pedantic report, for each value it sends.
                            if !self.unsupported_mode_warned {
                                self.unsupported_mode_warned = true;
                                warn_once!(
                                    "playback:unsupported_mode",
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
                    let was_running = self.current_session.take().is_some();
                    self.stop_audio();
                    self.opus_decoder = None;
                    // A STOP with no stream running (a repeat, or one
                    // before any START) is not passed on, so a sink
                    // always sees on_start before on_stop.
                    if was_running {
                        if let Some(sink) = self.sink() {
                            sink.on_stop();
                        }
                    }
                }
                playback_server::VOLUME => {
                    // Recorded for bug reports; ryll does not apply the
                    // server's volume. A malformed body is ignored.
                    if let Ok(volume) = PlaybackVolume::decode(payload) {
                        self.last_volume_per_channel = volume.volumes;
                        debug!("playback: VOLUME: {:?}", self.last_volume_per_channel);
                    }
                }
                playback_server::MUTE => {
                    if let Ok(mute) = PlaybackMute::decode(payload) {
                        self.last_mute = Some(mute.mute);
                        debug!("playback: MUTE: {:?}", self.last_mute);
                    }
                }
                playback_server::LATENCY => {
                    if let Ok(latency) = PlaybackLatency::decode(payload) {
                        let lat = latency.latency_ms;
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

    /// Push samples into the local output thread's ring, if one is
    /// running. Bumps the `ring_overflow_count` atomic on each
    /// sample that the producer fails to enqueue. Without the
    /// `audio` feature there is no local output and this does
    /// nothing.
    fn push_local_output(&mut self, samples: &[i16]) {
        #[cfg(feature = "audio")]
        if let Some(ref mut producer) = self.audio_producer {
            for &s in samples {
                if producer.push(s).is_err() {
                    self.audio_counters
                        .ring_overflow_count
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        #[cfg(not(feature = "audio"))]
        let _ = samples;
    }

    /// Decode one Opus packet. Returns the interleaved PCM, at
    /// [`OPUS_DECODE_RATE_HZ`], or `None` if the decoder errored or
    /// no decoder is active.
    fn decode_opus(&mut self, audio_data: &[u8]) -> Option<Vec<i16>> {
        let decoder = self.opus_decoder.as_mut()?;
        let ch = self.channels as usize;
        let mut pcm = vec![0i16; opus_decoder::OpusDecoder::MAX_FRAME_SIZE_48K * ch];
        match decoder.decode(audio_data, &mut pcm, false) {
            Ok(samples) => {
                pcm.truncate(samples * ch);
                Some(pcm)
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

    /// The sink, in a `Sink` session.
    fn sink(&self) -> Option<&Arc<dyn PlaybackSink>> {
        match &self.output {
            PlaybackOutput::Local => None,
            PlaybackOutput::Sink(sink) => Some(sink),
        }
    }

    /// The channel count from START, saturated rather than
    /// truncated into a `u8`.
    fn channels_u8(&self) -> u8 {
        u8::try_from(self.channels).unwrap_or(u8::MAX)
    }

    /// The current stream's shape, for [`PlaybackSink::on_start`].
    fn format(&self) -> PlaybackFormat {
        PlaybackFormat {
            mode: self.audio_mode,
            sample_rate_hz: self.sample_rate,
            channels: self.channels_u8(),
        }
    }

    /// Start local output for a new audio session, stopping any
    /// previous one first. A `Sink` session plays nothing on this
    /// host, and a build without `audio` has no local output, so
    /// neither starts the thread.
    fn start_audio_output(&mut self) {
        self.stop_audio();
        if let PlaybackOutput::Local = self.output {
            #[cfg(feature = "audio")]
            self.spawn_audio_thread();
        }
    }

    /// Create the ring buffer and spawn the audio thread.
    #[cfg(feature = "audio")]
    fn spawn_audio_thread(&mut self) {
        #[cfg(test)]
        {
            self.audio_thread_starts += 1;
        }

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
        #[cfg(feature = "audio")]
        {
            self.audio_producer = None;
            if let Some(thread) = self.audio_thread.take() {
                // Fold in whatever the thread reported since the last
                // drain so the snapshot keeps the final error. The
                // session is over, so nothing is worth notifying.
                for event in thread.stop() {
                    let _ = self.output_status.apply(event);
                }
            }
        }
        self.output_status.session_ended();
    }

    /// Fold output-stream events from the audio thread into the
    /// status and raise the notifications they call for. Without
    /// the `audio` feature there is no thread and nothing to fold.
    async fn drain_output_events(&mut self) {
        #[cfg(feature = "audio")]
        {
            let Some(thread) = self.audio_thread.as_ref() else {
                return;
            };
            let events: Vec<AudioOutputEvent> = thread.events.try_iter().collect();
            if events.is_empty() {
                return;
            }
            for event in events {
                let note = self.output_status.apply(event);
                // A sink session's audio goes elsewhere (the browser,
                // in web mode), not to this host's speakers, so local
                // output trouble would be diagnostic detail, not news.
                // Such a session never starts the thread, so this is
                // only a guard.
                if let (Some(note), None) = (note, self.sink()) {
                    self.events.emit(ChannelEvent::Notification(note)).await;
                }
            }
            self.update_snapshot();
        }
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
            snap.channel_linked = true;
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
/// Clears the snapshot's `channel_linked` when dropped, so `stats`
/// leaves out `playback` once the channel has closed, rather than
/// reporting a gone channel's last counters as if it were still open.
struct UnlinkOnDrop(Arc<Mutex<crate::snapshots::PlaybackSnapshot>>);

impl Drop for UnlinkOnDrop {
    fn drop(&mut self) {
        let mut snap = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        snap.channel_linked = false;
    }
}

/// Map a SPICE audio-mode value to a [`PlaybackCodec`]. The
/// mode comes from `SPICE_MSG_PLAYBACK_MODE`; values 1 and 3
/// are the standard RAW and OPUS modes, anything else is
/// surfaced via `Other` so a bug report can spot a codec we
/// haven't seen.
fn codec_from_mode(mode: u16) -> crate::snapshots::PlaybackCodec {
    match mode {
        audio_data_mode::RAW => crate::snapshots::PlaybackCodec::Raw,
        audio_data_mode::OPUS => crate::snapshots::PlaybackCodec::Opus,
        other => crate::snapshots::PlaybackCodec::Other(other),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "audio")]
    use super::{
        backoff_after_stream, AudioCounters, AudioThread, CallbackState, OUTPUT_RETRY_INITIAL,
    };
    use super::{
        opus_packet_samples_48k, pcm_bytes_to_i16, samples_per_frame_48k, AudioOutputEvent,
        OutputStatus, PlaybackChannel, Resampler, UnlinkOnDrop,
    };
    use crate::channels::test_support::{loopback, NullTraffic, TestChannelPeers};
    use crate::channels::VolumeControl;
    use crate::playback_sink::{PlaybackFormat, PlaybackOutput, PlaybackSink};
    use crate::snapshots::{PlaybackOutputInfo, PlaybackSnapshot};
    use crate::{ByteCounter, LogConfig};
    use shakenfist_spice_protocol::messages::{
        make_message, PlaybackMode, PlaybackPacket, PlaybackStart, WireType,
    };
    use shakenfist_spice_protocol::{audio_data_mode, audio_fmt, playback_server, NotifySeverity};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    #[cfg(feature = "audio")]
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    #[cfg(feature = "audio")]
    use std::time::{Duration, Instant};

    // --- Channel dispatch to a sink ---

    /// One call a [`RecordingSink`] saw. Decoded PCM is recorded by
    /// length only: the samples are whatever the decoder makes of
    /// the packet.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum SinkCall {
        Start(PlaybackFormat),
        Opus {
            packet: Vec<u8>,
            samples_in_packet: u32,
        },
        Pcm {
            samples: Vec<i16>,
            sample_rate_hz: u32,
            channels: u8,
        },
        Decoded {
            len: usize,
            sample_rate_hz: u32,
            channels: u8,
        },
        Stop,
    }

    /// A sink that records every call, in order.
    #[derive(Default)]
    struct RecordingSink {
        calls: Mutex<Vec<SinkCall>>,
    }

    impl RecordingSink {
        fn record(&self, call: SinkCall) {
            self.calls.lock().expect("sink lock").push(call);
        }

        fn calls(&self) -> Vec<SinkCall> {
            self.calls.lock().expect("sink lock").clone()
        }
    }

    impl PlaybackSink for RecordingSink {
        fn on_start(&self, format: PlaybackFormat) {
            self.record(SinkCall::Start(format));
        }

        fn on_opus_packet(&self, packet: &[u8], samples_in_packet: u32) {
            self.record(SinkCall::Opus {
                packet: packet.to_vec(),
                samples_in_packet,
            });
        }

        fn on_pcm_samples(&self, samples: &[i16], sample_rate_hz: u32, channels: u8) {
            self.record(SinkCall::Pcm {
                samples: samples.to_vec(),
                sample_rate_hz,
                channels,
            });
        }

        fn on_decoded_pcm(&self, samples: &[i16], sample_rate_hz: u32, channels: u8) {
            self.record(SinkCall::Decoded {
                len: samples.len(),
                sample_rate_hz,
                channels,
            });
        }

        fn on_stop(&self) {
            self.record(SinkCall::Stop);
        }
    }

    /// A real Opus packet, made without an encoder. TOC byte 0xFC is
    /// configuration 31 (CELT-only, fullband, 20 ms), stereo,
    /// frame-count code 0, so the rest of the packet is its one
    /// frame, here zero bytes long. RFC 6716 allows a zero-length
    /// frame (it is how DTX sends nothing), and a decoder still
    /// outputs a whole frame for it: opus-decoder takes the CELT
    /// silence flag as set when no bits remain, and libopus conceals
    /// it as a lost frame. Either way that is 960 samples per channel.
    const SILENT_STEREO_OPUS: [u8; 1] = [0xFC];

    async fn channel_with(output: PlaybackOutput) -> (PlaybackChannel, TestChannelPeers) {
        let (stream, events, peers) = loopback().await;
        let channel = PlaybackChannel::new(
            stream,
            events,
            Arc::new(ByteCounter::new()),
            Arc::new(NullTraffic::new()),
            Arc::new(Mutex::new(PlaybackSnapshot::default())),
            VolumeControl::new(),
            LogConfig::default(),
            Arc::new(AtomicBool::new(false)),
            output,
        );
        (channel, peers)
    }

    async fn sink_channel() -> (PlaybackChannel, Arc<RecordingSink>, TestChannelPeers) {
        let sink = Arc::new(RecordingSink::default());
        let (channel, peers) = channel_with(PlaybackOutput::Sink(sink.clone())).await;
        (channel, sink, peers)
    }

    fn message<T: WireType>(msg_type: u16, body: &T) -> Vec<u8> {
        let mut payload = Vec::new();
        body.write(&mut payload);
        make_message(msg_type, &payload)
    }

    fn start(channels: u32, frequency: u32) -> Vec<u8> {
        let body = PlaybackStart {
            channels,
            format: audio_fmt::S16,
            frequency,
            time: 0,
        };
        message(playback_server::START, &body)
    }

    fn mode(mode: u16) -> Vec<u8> {
        message(playback_server::MODE, &PlaybackMode { time: 0, mode })
    }

    fn data(bytes: &[u8]) -> Vec<u8> {
        let body = PlaybackPacket {
            time: 0,
            data: bytes.to_vec(),
        };
        message(playback_server::DATA, &body)
    }

    fn stop() -> Vec<u8> {
        make_message(playback_server::STOP, &[])
    }

    async fn feed(channel: &mut PlaybackChannel, messages: &[Vec<u8>]) {
        for m in messages {
            channel.buffer.extend_from_slice(m);
        }
        channel
            .process_messages()
            .await
            .expect("the channel accepts the messages");
    }

    /// The channel never started the local output thread.
    fn assert_no_audio_thread(channel: &PlaybackChannel) {
        assert_eq!(
            channel.audio_thread_starts, 0,
            "a sink session started the audio thread"
        );
        #[cfg(feature = "audio")]
        {
            assert!(channel.audio_thread.is_none());
            assert!(channel.audio_producer.is_none());
        }
    }

    #[tokio::test]
    async fn a_sink_session_sees_start_packets_decoded_pcm_and_stop() {
        let (mut channel, sink, _peers) = sink_channel().await;
        feed(
            &mut channel,
            &[
                start(2, 48_000),
                mode(audio_data_mode::OPUS),
                data(&SILENT_STEREO_OPUS),
            ],
        )
        .await;
        assert_no_audio_thread(&channel);
        feed(&mut channel, &[stop()]).await;

        let format = |mode| PlaybackFormat {
            mode,
            sample_rate_hz: 48_000,
            channels: 2,
        };
        assert_eq!(
            sink.calls(),
            vec![
                // START, before any MODE: the mode is not known yet.
                SinkCall::Start(format(audio_data_mode::INVALID)),
                // MODE during the stream restarts it in the new mode,
                // before any data.
                SinkCall::Start(format(audio_data_mode::OPUS)),
                SinkCall::Opus {
                    packet: SILENT_STEREO_OPUS.to_vec(),
                    samples_in_packet: 960,
                },
                SinkCall::Decoded {
                    len: 960 * 2,
                    sample_rate_hz: 48_000,
                    channels: 2,
                },
                SinkCall::Stop,
            ]
        );
        // Decode ran with no device behind it, so the counters the
        // control socket reports still move.
        assert_eq!(channel.data_packets_received, 1);
        assert_eq!(channel.data_packets_decoded, 1);
        assert_eq!(channel.pcm_bytes_produced, 960 * 2 * 2);
        assert_no_audio_thread(&channel);
    }

    #[tokio::test]
    async fn a_sink_session_in_raw_mode_sees_pcm_samples() {
        let (mut channel, sink, _peers) = sink_channel().await;
        // spice-server's own order: MODE, then START.
        let pcm = [0x01, 0x00, 0xff, 0xff, 0x00, 0x80, 0x10, 0x00];
        feed(
            &mut channel,
            &[
                mode(audio_data_mode::RAW),
                start(2, 44_100),
                data(&pcm),
                stop(),
            ],
        )
        .await;

        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Start(PlaybackFormat {
                    mode: audio_data_mode::RAW,
                    sample_rate_hz: 44_100,
                    channels: 2,
                }),
                SinkCall::Pcm {
                    samples: vec![1, -1, i16::MIN, 16],
                    sample_rate_hz: 44_100,
                    channels: 2,
                },
                SinkCall::Stop,
            ]
        );
        assert_eq!(channel.data_packets_decoded, 1);
        assert_no_audio_thread(&channel);
    }

    #[cfg(feature = "audio")]
    #[tokio::test]
    async fn a_local_session_starts_and_stops_the_audio_thread() {
        // The thread retries a missing output device rather than
        // failing, so this holds on a host with no sound card.
        let (mut channel, _peers) = channel_with(PlaybackOutput::Local).await;
        feed(
            &mut channel,
            &[mode(audio_data_mode::RAW), start(2, 48_000)],
        )
        .await;
        assert_eq!(channel.audio_thread_starts, 1);
        assert!(channel.audio_thread.is_some());
        assert!(channel.audio_producer.is_some());
        feed(&mut channel, &[stop()]).await;
        assert!(channel.audio_thread.is_none());
        assert!(channel.audio_producer.is_none());
    }

    #[tokio::test]
    async fn a_stop_with_no_stream_running_is_not_passed_on() {
        let (mut channel, sink, _peers) = sink_channel().await;
        // A STOP before any START, then a stream, then a repeated STOP.
        feed(
            &mut channel,
            &[
                stop(),
                mode(audio_data_mode::RAW),
                start(2, 48_000),
                stop(),
                stop(),
            ],
        )
        .await;
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Start(PlaybackFormat {
                    mode: audio_data_mode::RAW,
                    sample_rate_hz: 48_000,
                    channels: 2,
                }),
                SinkCall::Stop,
            ]
        );
        // Every STOP still counts, for the control socket.
        assert_eq!(channel.stop_count, 3);
    }

    #[tokio::test]
    async fn the_channel_count_saturates_rather_than_truncates() {
        let (mut channel, sink, _peers) = sink_channel().await;
        // 300 truncated to a u8 would be 44.
        feed(
            &mut channel,
            &[mode(audio_data_mode::RAW), start(300, 48_000)],
        )
        .await;
        assert_eq!(
            sink.calls(),
            vec![SinkCall::Start(PlaybackFormat {
                mode: audio_data_mode::RAW,
                sample_rate_hz: 48_000,
                channels: u8::MAX,
            })]
        );
    }

    // --- CallbackState and audio-thread helper tests ---

    #[cfg(feature = "audio")]
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

    #[cfg(feature = "audio")]
    #[test]
    fn callback_state_discard_empties_ring_and_local_buf() {
        let mut st = callback_state(&[1, 2, 3, 4], &[5, 6]);
        st.discard();
        assert_eq!(st.consumer.slots(), 0, "ring must be drained");
        assert!(st.local_buf.is_empty(), "local buffer must be cleared");
    }

    #[cfg(feature = "audio")]
    #[test]
    fn callback_state_drain_ring_appends_in_order() {
        let mut st = callback_state(&[3, 4], &[1, 2]);
        st.drain_ring();
        assert_eq!(st.consumer.slots(), 0);
        assert_eq!(st.local_buf, VecDeque::from(vec![1, 2, 3, 4]));
    }

    #[cfg(feature = "audio")]
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

    #[cfg(feature = "audio")]
    #[test]
    fn lock_for_callback_counts_contention_as_underrun() {
        let state = Mutex::new(callback_state(&[], &[]));
        let counters = AudioCounters::new();
        let _held = state.lock().unwrap();
        assert!(CallbackState::lock_for_callback(&state, &counters).is_none());
        assert_eq!(counters.device_underrun_count.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn unlink_on_drop_clears_channel_linked() {
        let snapshot = Arc::new(Mutex::new(crate::snapshots::PlaybackSnapshot {
            channel_linked: true,
            start_count: 3,
            ..Default::default()
        }));
        drop(UnlinkOnDrop(Arc::clone(&snapshot)));
        let snap = snapshot.lock().unwrap();
        assert!(!snap.channel_linked);
        // Only the flag changes; the counters stay for bug reports.
        assert_eq!(snap.start_count, 3);
    }

    #[cfg(feature = "audio")]
    #[test]
    fn sleep_unless_shutdown_returns_promptly_on_shutdown() {
        let shutdown = AtomicBool::new(true);
        let start = Instant::now();
        AudioThread::sleep_unless_shutdown(Duration::from_secs(10), &shutdown);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(feature = "audio")]
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
        let note = status.apply(AudioOutputEvent::Stable);
        assert!(note.is_none(), "a normal stream settling must not notify");
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

        // A stream starting is not yet a recovery...
        let started = status.apply(AudioOutputEvent::Started(output("Speakers")));
        assert!(started.is_none());
        assert!(status.error.is_none());
        assert_eq!(status.streams_started, 2);

        // ...one that stays up is.
        let restored = status
            .apply(AudioOutputEvent::Stable)
            .expect("recovery must notify");
        assert_eq!(restored.severity, NotifySeverity::Info);
        assert!(restored.message.contains("Speakers"));

        // The streak is over, so the next failure is news.
        assert!(status
            .apply(AudioOutputEvent::Failed("device unplugged".to_string()))
            .is_some());
    }

    #[test]
    fn output_status_flapping_device_stays_in_one_streak() {
        let mut status = OutputStatus::default();
        assert!(status
            .apply(AudioOutputEvent::Failed("device unplugged".to_string()))
            .is_some());
        // A device that opens and dies before `Stable` must not
        // raise a restored/unavailable pair per cycle.
        for _ in 0..5 {
            assert!(status
                .apply(AudioOutputEvent::Started(output("Broken")))
                .is_none());
            assert!(status
                .apply(AudioOutputEvent::Failed("stream died".to_string()))
                .is_none());
        }
        assert_eq!(status.streams_started, 5);
        assert_eq!(status.failure_count, 6);
        assert!(status.output.is_none());
    }

    #[cfg(feature = "audio")]
    #[test]
    fn backoff_resets_only_after_a_stable_stream() {
        let backoff = Duration::from_secs(8);
        assert_eq!(backoff_after_stream(backoff, true), OUTPUT_RETRY_INITIAL);
        assert_eq!(backoff_after_stream(backoff, false), backoff);
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
        // Whatever the buffer held before is replaced.
        let mut samples = vec![7i16; 5];
        pcm_bytes_to_i16(&bytes, &mut samples);
        assert_eq!(samples, vec![1i16, -1, i16::MIN]);
    }

    #[test]
    fn pcm_bytes_to_i16_drops_trailing_odd_byte() {
        // The chunks_exact(2) loop ignores the trailing single byte.
        let bytes = [0x01, 0x00, 0x42];
        let mut samples = Vec::new();
        pcm_bytes_to_i16(&bytes, &mut samples);
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
