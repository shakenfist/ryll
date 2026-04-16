use anyhow::Result;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::ByteCounter;
use crate::bugreport::TrafficBuffers;
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{make_message, MessageHeader, Ping, SetAck};
use shakenfist_spice_protocol::{main_client, playback_server, ChannelType};

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

struct Resampler {
    ratio: f64,
    pos: f64,
    channels: usize,
}

impl Resampler {
    fn new(from_rate: u32, to_rate: u32, channels: u32) -> Self {
        Resampler {
            ratio: from_rate as f64 / to_rate as f64,
            pos: 0.0,
            channels: channels.max(1) as usize,
        }
    }

    /// Produce one output frame (one sample per channel) by
    /// linearly interpolating between adjacent input frames.
    /// Returns silence without modifying the buffer on underrun.
    fn next_frame(&mut self, buffer: &mut VecDeque<i16>, out: &mut [i16]) {
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
    buffer: Vec<u8>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
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
}

impl PlaybackChannel {
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<TrafficBuffers>,
        volume_control: Arc<VolumeControl>,
    ) -> Self {
        PlaybackChannel {
            stream,
            event_tx,
            buffer: Vec::with_capacity(65536),
            byte_counter,
            traffic,
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
                    if crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        info!("playback: shutdown requested");
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

            logging::log_message(
                "received",
                "playback",
                msg_type,
                message_names::playback_server(msg_type),
                header.message_size,
            );

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
                    if payload.len() >= 6 {
                        self.audio_mode = u16::from_le_bytes([payload[4], payload[5]]);
                        info!("playback: MODE: {}", self.audio_mode);
                    }
                }
                playback_server::DATA => {
                    if payload.len() > 4 {
                        let audio_data = &payload[4..];
                        if self.audio_mode == AUDIO_DATA_MODE_RAW {
                            self.push_samples_raw(audio_data);
                        } else if self.audio_mode == AUDIO_DATA_MODE_OPUS {
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
                    logging::log_unknown(
                        "playback",
                        "received",
                        msg_type,
                        header.message_size,
                        &payload,
                    );
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
        logging::log_message(
            "sent",
            "playback",
            msg_type,
            message_names::playback_client(msg_type),
            payload_size,
        );
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
