use anyhow::Result;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

use crate::{
    ByteCounter, LogConfig, NotificationEntry, NotificationSource, OpusPacketSink, TrafficSink,
};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, MessageHeader, Notify as NotifyMessage, Ping, SetAck,
};
use shakenfist_spice_protocol::{main_client, playback_server, ChannelType, NotifySeverity};

use super::ChannelEvent;

const AUDIO_DATA_MODE_RAW: u16 = 1;
const AUDIO_DATA_MODE_OPUS: u16 = 3;

/// Maximum ring buffer capacity in samples. At 48kHz stereo this is
/// ~2 seconds. Prevents unbounded memory growth if audio data
/// arrives faster than it is consumed.
const MAX_AUDIO_BUFFER_SAMPLES: usize = 48000 * 2 * 2;

pub struct VolumeControl {
    volume: AtomicU8,
    muted: AtomicBool,
}

impl VolumeControl {
    pub fn new() -> Arc<Self> {
        Arc::new(VolumeControl {
            volume: AtomicU8::new(80),
            muted: AtomicBool::new(false),
        })
    }

    pub fn volume(&self) -> u8 {
        self.volume.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, v: u8) {
        self.volume.store(v.min(100), Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    pub fn effective_volume(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            self.volume() as f32 / 100.0
        }
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
    for chunk in bytes.chunks_exact(2) {
        out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
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

/// State for the dedicated audio output thread.
struct AudioThread {
    handle: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
}

impl AudioThread {
    /// Spawn a dedicated OS thread that owns the cpal stream.
    /// Samples are read from `consumer` via a lock-free ring buffer.
    fn spawn(
        consumer: rtrb::Consumer<i16>,
        vol: Arc<VolumeControl>,
        source_rate: u32,
        source_channels: u32,
    ) -> Option<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown.clone();

        let handle = std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                Self::run_audio(consumer, vol, source_rate, source_channels, shutdown_flag);
            })
            .ok()?;

        Some(AudioThread { handle, shutdown })
    }

    fn run_audio(
        mut consumer: rtrb::Consumer<i16>,
        vol: Arc<VolumeControl>,
        source_rate: u32,
        source_channels: u32,
        shutdown: Arc<AtomicBool>,
    ) {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                warn!("playback: no audio output device found");
                return;
            }
        };
        let default_config = match device.default_output_config() {
            Ok(c) => c,
            Err(e) => {
                warn!("playback: failed to get default output config: {}", e);
                return;
            }
        };
        info!(
            "playback: device config: {}Hz, {} ch, {:?}",
            default_config.sample_rate().0,
            default_config.channels(),
            default_config.sample_format()
        );
        let config = cpal::StreamConfig {
            channels: source_channels as u16,
            sample_rate: default_config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let device_rate = config.sample_rate.0;

        // Build the callback state. The resampler and local buffer
        // live in the callback closure -- no mutex needed since the
        // cpal callback is the sole consumer.
        let mut resampler = Resampler::new(source_rate, device_rate, source_channels);
        let mut local_buf: VecDeque<i16> = VecDeque::with_capacity(8192);

        // Drain available samples from the ring buffer into the
        // local VecDeque so the resampler can use random access.
        let drain_ring = move |consumer: &mut rtrb::Consumer<i16>, local: &mut VecDeque<i16>| {
            let available = consumer.slots();
            if available > 0 {
                let chunk = consumer.read_chunk(available).unwrap();
                let (first, second) = chunk.as_slices();
                local.extend(first.iter().copied());
                local.extend(second.iter().copied());
                chunk.commit_all();
            }
        };

        let stream = match default_config.sample_format() {
            cpal::SampleFormat::I16 => {
                let vol = vol.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        drain_ring(&mut consumer, &mut local_buf);
                        write_samples_i16(data, &mut local_buf, &vol, &mut resampler);
                    },
                    |err| warn!("playback: audio stream error: {}", err),
                    None,
                )
            }
            cpal::SampleFormat::F32 => {
                let vol = vol.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        drain_ring(&mut consumer, &mut local_buf);
                        write_samples_f32(data, &mut local_buf, &vol, &mut resampler);
                    },
                    |err| warn!("playback: audio stream error: {}", err),
                    None,
                )
            }
            fmt => {
                warn!("playback: unsupported sample format: {:?}", fmt);
                return;
            }
        };
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!("playback: failed to create audio stream: {}", e);
                return;
            }
        };
        if let Err(e) = stream.play() {
            warn!("playback: failed to start audio stream: {}", e);
            return;
        }
        info!(
            "playback: audio output started ({}Hz {} ch)",
            device_rate, source_channels
        );

        // Keep the stream alive until shutdown is requested.
        while !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }

        // stream is dropped here, releasing the audio device.
        info!("playback: audio thread shutting down");
    }

    /// Signal the audio thread to stop and wait for it to finish.
    fn stop(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

pub struct PlaybackChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    repaint_notify: Arc<Notify>,
    buffer: Vec<u8>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    log_config: LogConfig,
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,
    bytes_in: u64,
    bytes_out: u64,
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
}

impl PlaybackChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<Notify>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<dyn TrafficSink>,
        volume_control: Arc<VolumeControl>,
        log_config: LogConfig,
        cancel: Arc<AtomicBool>,
        opus_sink: Option<Arc<dyn OpusPacketSink>>,
    ) -> Self {
        PlaybackChannel {
            stream,
            event_tx,
            repaint_notify,
            buffer: Vec::with_capacity(65536),
            byte_counter,
            traffic,
            log_config,
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
            audio_mode: 0,
            sample_rate: 0,
            channels: 0,
            audio_producer: None,
            audio_thread: None,
            opus_decoder: None,
            volume_control,
            opus_sink,
            cancel,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("playback: channel started");
        loop {
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
                    self.event_tx
                        .send(ChannelEvent::Disconnected(ChannelType::Playback))
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                    break;
                }
                Some(Ok(n)) => n,
                Some(Err(e)) => return Err(e.into()),
                None => continue, // timeout, no data yet
            };

            self.byte_counter.add(n as u64);
            self.buffer.extend_from_slice(&chunk[..n]);
            self.bytes_in += n as u64;
            self.process_messages().await?;
        }

        // Clean shutdown: stop the audio thread.
        self.stop_audio();
        Ok(())
    }

    async fn process_messages(&mut self) -> Result<()> {
        loop {
            if self.buffer.len() < MessageHeader::SIZE {
                break;
            }
            let header = MessageHeader::read(&self.buffer)?;
            let total = MessageHeader::SIZE + header.message_size as usize;
            if self.buffer.len() < total {
                break;
            }
            let payload = self.buffer[MessageHeader::SIZE..total].to_vec();
            self.buffer.drain(..total);
            let msg_type = header.message_type;

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
                &payload,
            );

            match msg_type {
                playback_server::SET_ACK => {
                    let set_ack = SetAck::read(&payload)?;
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
                    let ping = Ping::read(&payload)?;
                    let mut pong_payload = Vec::new();
                    ping.write_pong(&mut pong_payload)?;
                    let response = make_message(main_client::PONG, &pong_payload);
                    self.send_with_log(main_client::PONG, &response).await?;
                }
                playback_server::NOTIFY => {
                    let notify = NotifyMessage::read(&payload)?;
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
                    self.event_tx
                        .send(ChannelEvent::Notification(entry))
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
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
                    }
                }
                playback_server::DATA => {
                    // SpiceMsgPlaybackPacket: time(u32) + data.
                    // Skip the 4-byte multimedia timestamp.
                    if payload.len() > 4 {
                        let audio_data = &payload[4..];
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
                            self.push_samples_raw(audio_data);
                        } else if self.audio_mode == AUDIO_DATA_MODE_OPUS {
                            // Pre-decode tap: forward the raw Opus
                            // packet to the optional sink before the
                            // libopus decode + cpal path runs.
                            if let Some(ref sink) = self.opus_sink {
                                let samples = opus_packet_samples_48k(audio_data);
                                sink.on_opus_packet(audio_data, samples);
                            }
                            self.push_samples_opus(audio_data);
                        }
                    }
                }
                playback_server::STOP => {
                    info!("playback: STOP");
                    self.stop_audio();
                    self.opus_decoder = None;
                }
                playback_server::VOLUME | playback_server::MUTE | playback_server::LATENCY => {
                    debug!("playback: received opcode {} (ignored)", msg_type);
                }
                _ => {
                    logging::log_unknown_once("playback", msg_type, &payload);
                }
            }
        }
        Ok(())
    }

    /// Push raw PCM samples into the ring buffer.
    fn push_samples_raw(&mut self, audio_data: &[u8]) {
        if let Some(ref mut producer) = self.audio_producer {
            for chunk in audio_data.chunks_exact(2) {
                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                // Drop samples if the ring buffer is full (back-pressure).
                let _ = producer.push(sample);
            }
        }
    }

    /// Decode Opus audio and push PCM samples into the ring buffer.
    fn push_samples_opus(&mut self, audio_data: &[u8]) {
        if let Some(ref mut decoder) = self.opus_decoder {
            let ch = self.channels as usize;
            let mut pcm = vec![0i16; opus_decoder::OpusDecoder::MAX_FRAME_SIZE_48K * ch];
            match decoder.decode(audio_data, &mut pcm, false) {
                Ok(samples) => {
                    if let Some(ref mut producer) = self.audio_producer {
                        for &s in &pcm[..samples * ch] {
                            let _ = producer.push(s);
                        }
                    }
                }
                Err(e) => {
                    debug!("playback: Opus decode error: {}", e);
                }
            }
        }
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
            thread.stop();
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
        match &mut self.stream {
            SpiceStream::Plain(s) => {
                use tokio::io::AsyncWriteExt;
                s.write_all(data).await?;
            }
            SpiceStream::Tls(s) => {
                use tokio::io::AsyncWriteExt;
                s.write_all(data).await?;
            }
        }
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        opus_packet_samples_48k, pcm_bytes_to_i16, samples_per_frame_48k, Resampler, VolumeControl,
    };
    use std::collections::VecDeque;

    // --- VolumeControl tests ---

    #[test]
    fn volume_control_new_defaults() {
        let vc = VolumeControl::new();
        assert_eq!(vc.volume(), 80);
        assert!(!vc.muted());
    }

    #[test]
    fn volume_control_set_volume_clamps_to_100() {
        let vc = VolumeControl::new();
        vc.set_volume(150);
        assert_eq!(vc.volume(), 100);
    }

    #[test]
    fn volume_control_effective_volume_when_muted_is_zero() {
        let vc = VolumeControl::new();
        vc.set_muted(true);
        assert_eq!(vc.effective_volume(), 0.0);
    }

    #[test]
    fn volume_control_effective_volume_default() {
        let vc = VolumeControl::new();
        let ev = vc.effective_volume();
        assert!((ev - 0.8).abs() < 1e-6, "expected 0.8, got {}", ev);
    }

    #[test]
    fn volume_control_mute_unmute_preserves_volume() {
        let vc = VolumeControl::new();
        vc.set_volume(65);
        vc.set_muted(true);
        assert_eq!(vc.effective_volume(), 0.0);
        vc.set_muted(false);
        assert_eq!(vc.volume(), 65);
        let ev = vc.effective_volume();
        assert!((ev - 0.65).abs() < 1e-6, "expected 0.65, got {}", ev);
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
