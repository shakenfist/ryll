/// Capture session for protocol and display debugging.
///
/// When `--capture <DIR>` is specified, all SPICE protocol
/// traffic and display frames are written to files in the
/// given directory. When not enabled, all methods are no-ops.
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};
use pcap_file::DataLink;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

// ── Pcap capture ────────────────────────────────────────

/// Fake IP addresses for pcap headers.
const CLIENT_IP: [u8; 4] = [10, 0, 0, 1];
const SERVER_IP: [u8; 4] = [10, 0, 0, 2];
const SERVER_PORT: u16 = 5900;

/// Per-channel pcap writer with TCP state tracking.
///
/// Uses unbuffered I/O so that every packet is flushed to disk
/// immediately.  This means the pcap files survive Ctrl+C / SIGINT
/// without needing an explicit flush on shutdown.
struct PcapChannelWriter {
    writer: PcapWriter<File>,
    client_seq: u32,
    server_seq: u32,
    client_port: u16,
}

impl PcapChannelWriter {
    fn new(path: PathBuf, client_port: u16) -> anyhow::Result<Self> {
        let file = File::create(&path)?;
        let header = PcapHeader {
            datalink: DataLink::ETHERNET,
            ..Default::default()
        };
        let writer = PcapWriter::with_header(file, header)?;
        info!("capture: opened {}", path.display());
        Ok(PcapChannelWriter {
            writer,
            client_seq: 1000,
            server_seq: 2000,
            client_port,
        })
    }

    fn write_sent(&mut self, data: &[u8], elapsed: std::time::Duration) {
        self.write_segmented(
            CLIENT_IP,
            self.client_port,
            SERVER_IP,
            SERVER_PORT,
            data,
            elapsed,
        );
        self.client_seq = self.client_seq.wrapping_add(data.len() as u32);
    }

    fn write_received(&mut self, data: &[u8], elapsed: std::time::Duration) {
        self.write_segmented(
            SERVER_IP,
            SERVER_PORT,
            CLIENT_IP,
            self.client_port,
            data,
            elapsed,
        );
        self.server_seq = self.server_seq.wrapping_add(data.len() as u32);
    }

    /// Write data as one or more TCP segments, splitting payloads
    /// that exceed the IPv4 maximum. Delegates segmentation to
    /// the shared `segment_payload` helper; this wrapper handles
    /// the per-direction seq/ack lookup and writes each produced
    /// frame to the pcap.
    fn write_segmented(
        &mut self,
        src_ip: [u8; 4],
        src_port: u16,
        dst_ip: [u8; 4],
        dst_port: u16,
        data: &[u8],
        elapsed: std::time::Duration,
    ) {
        let is_client = src_ip == CLIENT_IP;
        let (seq, ack) = if is_client {
            (self.client_seq, self.server_seq)
        } else {
            (self.server_seq, self.client_seq)
        };
        for frame in segment_payload(src_ip, src_port, dst_ip, dst_port, seq, ack, data) {
            self.write_frame(&frame, elapsed);
        }
    }

    fn write_frame(&mut self, frame: &[u8], elapsed: std::time::Duration) {
        let packet = PcapPacket::new(elapsed, frame.len() as u32, frame);
        self.writer.write_packet(&packet).ok();
    }
}

/// Split `data` into one or more TCP segments and produce the
/// pcap frame bytes for each. Per-segment seq numbers are
/// `seq + offset`; the caller is responsible for advancing
/// its own stream-wide seq tracking by `data.len()` after
/// this call returns.
///
/// `MAX_PAYLOAD = 65495 = 65535 − 20 (IP) − 20 (TCP)` is the
/// IPv4-frame payload ceiling; `build_tcp_frame` itself
/// fails closed above this, which is the K2 (Phase 08) bug
/// in the un-segmented ring path.
///
/// Always returns at least one frame. SPICE messages carry a
/// 6-byte header so `data` is non-empty in practice, but if
/// a caller ever passes an empty slice we fall through to a
/// single empty frame rather than returning an empty `Vec` —
/// keeps the "one push = one or more entries" invariant for
/// both the live and ring callers.
pub(crate) fn segment_payload(
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    seq: u32,
    ack: u32,
    data: &[u8],
) -> Vec<Vec<u8>> {
    const MAX_PAYLOAD: usize = 65495; // 65535 − 20 (IP) − 20 (TCP)
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let end = (offset + MAX_PAYLOAD).min(data.len());
        let chunk = &data[offset..end];
        let segment_seq = seq.wrapping_add(offset as u32);
        frames.push(build_tcp_frame(
            src_ip,
            src_port,
            dst_ip,
            dst_port,
            segment_seq,
            ack,
            chunk,
        ));
        offset = end;
    }
    if frames.is_empty() {
        // Pathological: empty payload. Produce a single empty
        // frame so callers don't have to special-case an empty
        // return vec.
        frames.push(build_tcp_frame(
            src_ip, src_port, dst_ip, dst_port, seq, ack, data,
        ));
    }
    frames
}

/// Build a fake Ethernet + IPv4 + TCP frame wrapping `payload`.
///
/// All current callers should route through `segment_payload`,
/// which chunks at `MAX_PAYLOAD = 65495` so the IPv4 ceiling is
/// never exceeded. The `> 65515` defensive check below is
/// therefore expected to be unreachable; if it fires, an
/// unsegmented caller has snuck in. Phase 15B instruments the
/// first hit per process with a `Backtrace::force_capture()` so
/// the offending call site can be identified — subsequent hits
/// log the bare warn without a backtrace to keep a busy session
/// from spamming thousands of stacks. Grep `payload too large
/// (FIRST HIT, backtrace follows)` to find the diagnostic line.
pub(crate) fn build_tcp_frame(
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    seq: u32,
    ack: u32,
    payload: &[u8],
) -> Vec<u8> {
    use etherparse::{Ethernet2Header, IpNumber, Ipv4Header, TcpHeader};

    let tcp_payload_len = payload.len();

    let mut tcp = TcpHeader::new(src_port, dst_port, seq, 65535);
    tcp.acknowledgment_number = ack;
    tcp.ack = true;

    let ip_payload_len = tcp.header_len() + tcp_payload_len;
    if ip_payload_len > 65515 {
        // One-shot backtrace: AtomicBool guard so the first
        // firing per process captures a stack (via
        // force_capture, which ignores RUST_BACKTRACE — we want
        // the trace regardless of how the binary was launched),
        // and every subsequent firing emits only the bare warn.
        // See PLAN-stream-caps-and-flap-phase-15 step 15B.
        static BACKTRACE_CAPTURED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !BACKTRACE_CAPTURED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let bt = std::backtrace::Backtrace::force_capture();
            warn!(
                "build_tcp_frame: payload too large for IPv4 ({} bytes), \
                 dropping (FIRST HIT, backtrace follows)\n{:?}",
                ip_payload_len, bt
            );
        } else {
            warn!(
                "build_tcp_frame: payload too large for IPv4 ({} bytes), dropping",
                ip_payload_len
            );
        }
        return Vec::new();
    }
    let mut ipv4 =
        Ipv4Header::new(ip_payload_len as u16, 64, IpNumber::TCP, src_ip, dst_ip).unwrap();
    ipv4.dont_fragment = true;

    tcp.checksum = tcp.calc_checksum_ipv4(&ipv4, payload).unwrap_or(0);

    let eth = Ethernet2Header {
        source: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        destination: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        ether_type: etherparse::ether_type::IPV4,
    };

    let mut frame = Vec::with_capacity(14 + 20 + tcp.header_len() + tcp_payload_len);
    eth.write(&mut frame).ok();
    ipv4.write(&mut frame).ok();
    tcp.write(&mut frame).ok();
    frame.extend_from_slice(payload);
    frame
}

/// Map channel name to a unique client port number.
pub(crate) fn channel_port(channel: &str) -> u16 {
    match channel {
        "main" => 10001,
        "display" => 10002,
        "inputs" => 10003,
        "cursor" => 10004,
        "usbredir" => 10009,
        _ => 10099,
    }
}

/// Known channel names that get pcap writers.
const CHANNELS: &[&str] = &["main", "display", "cursor", "inputs", "usbredir"];

/// Map an incoming channel name to its `&'static str` entry in
/// `CHANNELS`. Returning `&'static str` lets `PcapQueueItem`
/// carry the name without an allocation per enqueue. Names not
/// in `CHANNELS` (e.g. `"webdav"`, `"playback"`) return `None`
/// and are dropped at the call-site before any queue work.
fn channel_static(name: &str) -> Option<&'static str> {
    CHANNELS.iter().copied().find(|&c| c == name)
}

// ── Pcap writer task ────────────────────────────────────

/// Bound on the queue feeding the dedicated pcap writer task.
/// In steady state with a keeping-up writer the queue sits near
/// zero; the cap exists so a slow disk burst is dropped rather
/// than allowed to back-pressure the SPICE socket. See
/// PLAN-video-keeping-up-phase-02-pcap-thread.md.
const PCAP_QUEUE_CAPACITY: usize = 1024;

/// Direction of a queued packet.
#[derive(Debug, Clone, Copy)]
enum PcapDirection {
    Sent,
    Received,
}

/// One queued pcap write. `payload` is `Arc<[u8]>` so the
/// hot-path enqueue copies into a single allocation that the
/// writer task consumes by reference. `elapsed` is captured at
/// enqueue time so pcap timestamps reflect wire arrival, not
/// the writer task's later dequeue.
#[derive(Debug)]
struct PcapQueueItem {
    channel: &'static str,
    direction: PcapDirection,
    payload: Arc<[u8]>,
    elapsed: Duration,
}

/// Long-lived task that owns every `PcapChannelWriter` and
/// drains the shared mpsc queue. Exits when the sender is
/// dropped (signalled by `CaptureSession::close`).
async fn pcap_writer_task(
    mut rx: mpsc::Receiver<PcapQueueItem>,
    mut writers: HashMap<&'static str, PcapChannelWriter>,
) {
    while let Some(item) = rx.recv().await {
        let Some(writer) = writers.get_mut(item.channel) else {
            // Channel name not in CHANNELS. channel_static would
            // have rejected this at enqueue time, but treat
            // defensively so a future channel-name typo doesn't
            // panic the writer task.
            continue;
        };
        match item.direction {
            PcapDirection::Sent => writer.write_sent(&item.payload, item.elapsed),
            PcapDirection::Received => writer.write_received(&item.payload, item.elapsed),
        }
    }
    debug!("capture: pcap writer task drained and exiting");
}

// ── Video capture ───────────────────────────────────────

/// H.264 video writer with lazy initialisation.
///
/// Created on the first frame() call once we know the
/// surface dimensions. Encodes RGBA → YUV420 → H.264
/// and muxes into an MP4 container.
struct VideoWriter {
    encoder: shakenfist_spice_renderer::H264Encoder,
    mp4_writer: mp4::Mp4Writer<File>,
    track_id: u32,
    width: u32,
    height: u32,
    frame_count: u64,
    last_timestamp_ms: u64,
}

impl VideoWriter {
    /// Create a new video writer, encoding the first frame.
    fn new(
        dir: &std::path::Path,
        pixels: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> Option<Self> {
        use mp4::{AvcConfig, MediaConfig, Mp4Config, TrackConfig, TrackType};
        use shakenfist_spice_renderer::H264Encoder;

        // openh264 requires even dimensions; H264Encoder enforces this.
        let w = width & !1;
        let h = height & !1;

        let mut encoder = match H264Encoder::new(w, h) {
            Ok(e) => e,
            Err(e) => {
                warn!("capture: failed to create H.264 encoder: {}", e);
                return None;
            }
        };

        let pixel_count = (w * h) as usize;
        if pixels.len() < pixel_count * 4 {
            warn!(
                "capture: first frame too short: {} bytes, need {}",
                pixels.len(),
                pixel_count * 4
            );
            return None;
        }
        // Pass the first w*h*4 bytes. When source dims are odd this
        // reads slightly into the next row, which matches the
        // pre-existing behaviour. TODO: repack when source dims are odd.
        let rgba = &pixels[..pixel_count * 4];

        // First frame is implicitly an IDR (openh264 default); no need
        // to force_keyframe.
        let frame = match encoder.encode(rgba, false) {
            Ok(f) => f,
            Err(e) => {
                warn!("capture: failed to encode first frame: {}", e);
                return None;
            }
        };

        // Strip Annex-B start codes and partition NALs.
        let mut sps: Vec<u8> = Vec::new();
        let mut pps: Vec<u8> = Vec::new();
        let mut avcc_frame_data: Vec<u8> = Vec::new();
        let mut is_sync = false;

        for annex_b_nal in &frame.nal_units {
            if annex_b_nal.len() < 5 {
                continue;
            }
            debug_assert_eq!(
                &annex_b_nal[0..4],
                &[0x00, 0x00, 0x00, 0x01],
                "H264Encoder must emit Annex-B start codes"
            );
            let raw_nal = &annex_b_nal[4..];
            let nal_type = raw_nal[0] & 0x1F;
            match nal_type {
                7 => sps = raw_nal.to_vec(),
                8 => pps = raw_nal.to_vec(),
                5 => {
                    is_sync = true;
                    // SPS/PPS go in the AvcConfig, not the sample.
                    // Slices go in the sample, AVCC-framed (each with
                    // its own length prefix).
                    append_avcc_nal(&mut avcc_frame_data, raw_nal);
                }
                _ => {
                    append_avcc_nal(&mut avcc_frame_data, raw_nal);
                }
            }
        }

        if sps.is_empty() || pps.is_empty() || avcc_frame_data.is_empty() {
            warn!(
                "capture: encoder did not produce SPS/PPS/IDR \
                 (sps={} pps={} frame={})",
                sps.len(),
                pps.len(),
                avcc_frame_data.len()
            );
            return None;
        }

        // Create MP4 writer
        let path = dir.join("display.mp4");
        let file = match File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                warn!("capture: failed to create {}: {}", path.display(), e);
                return None;
            }
        };

        let mp4_config = Mp4Config {
            major_brand: str::parse("isom").unwrap(),
            minor_version: 512,
            compatible_brands: vec![
                str::parse("isom").unwrap(),
                str::parse("iso2").unwrap(),
                str::parse("avc1").unwrap(),
                str::parse("mp41").unwrap(),
            ],
            timescale: 1000,
        };

        let mut mp4_writer = match mp4::Mp4Writer::write_start(file, &mp4_config) {
            Ok(w) => w,
            Err(e) => {
                warn!("capture: failed to create MP4 writer: {}", e);
                return None;
            }
        };

        let avc_config = AvcConfig {
            width: w as u16,
            height: h as u16,
            seq_param_set: sps,
            pic_param_set: pps,
        };

        let track_config = TrackConfig {
            track_type: TrackType::Video,
            timescale: 1000,
            language: String::from("und"),
            media_conf: MediaConfig::AvcConfig(avc_config),
        };

        if let Err(e) = mp4_writer.add_track(&track_config) {
            warn!("capture: failed to add video track: {}", e);
            return None;
        }

        let track_id = 1;

        // avcc_frame_data is already properly AVCC-framed (each NAL
        // has its own 4-byte length prefix).
        let sample = mp4::Mp4Sample {
            start_time: timestamp_ms,
            duration: 33, // placeholder until next frame
            rendering_offset: 0,
            is_sync,
            bytes: bytes::Bytes::from(avcc_frame_data),
        };

        if let Err(e) = mp4_writer.write_sample(track_id, &sample) {
            warn!("capture: failed to write first video sample: {}", e);
            return None;
        }

        info!("capture: opened {} ({}x{} H.264)", path.display(), w, h);

        Some(VideoWriter {
            encoder,
            mp4_writer,
            track_id,
            width: w,
            height: h,
            frame_count: 1,
            last_timestamp_ms: timestamp_ms,
        })
    }

    /// Encode and write a subsequent frame.
    fn write_frame(&mut self, pixels: &[u8], width: u32, height: u32, timestamp_ms: u64) {
        if width != self.width || height != self.height {
            warn!(
                "capture: surface dimensions changed {}x{} -> {}x{}, stopping video",
                self.width, self.height, width, height
            );
            return;
        }

        let pixel_count = (self.width * self.height) as usize;
        if pixels.len() < pixel_count * 4 {
            return;
        }
        let rgba = &pixels[..pixel_count * 4];

        let frame = match self.encoder.encode(rgba, false) {
            Ok(f) => f,
            Err(e) => {
                warn!("capture: H.264 encode failed: {}", e);
                return;
            }
        };

        let mut avcc_frame_data: Vec<u8> = Vec::new();
        let mut is_sync = false;

        for annex_b_nal in &frame.nal_units {
            if annex_b_nal.len() < 5 {
                continue;
            }
            let raw_nal = &annex_b_nal[4..];
            let nal_type = raw_nal[0] & 0x1F;
            if nal_type == 5 {
                is_sync = true;
            }
            // Skip in-band SPS/PPS: they're already in the AvcConfig.
            // The encoder may re-emit them after a forced keyframe;
            // capture mode never forces, so this is mostly defensive.
            if nal_type == 7 || nal_type == 8 {
                continue;
            }
            append_avcc_nal(&mut avcc_frame_data, raw_nal);
        }

        if avcc_frame_data.is_empty() {
            return;
        }

        let duration_ms = timestamp_ms.saturating_sub(self.last_timestamp_ms).max(1) as u32;

        let sample = mp4::Mp4Sample {
            start_time: timestamp_ms,
            duration: duration_ms,
            rendering_offset: 0,
            is_sync,
            bytes: bytes::Bytes::from(avcc_frame_data),
        };

        if let Err(e) = self.mp4_writer.write_sample(self.track_id, &sample) {
            warn!("capture: failed to write video sample: {}", e);
            return;
        }

        self.last_timestamp_ms = timestamp_ms;
        self.frame_count += 1;
    }

    /// Finalise the MP4 container.
    fn close(&mut self) {
        if let Err(e) = self.mp4_writer.write_end() {
            warn!("capture: failed to finalise MP4: {}", e);
        } else {
            info!("capture: video closed ({} frames)", self.frame_count);
        }
    }
}

/// Append a NAL body to an AVCC-framed buffer. AVCC requires each
/// NAL to be prefixed with its own 4-byte big-endian length.
fn append_avcc_nal(out: &mut Vec<u8>, raw_nal: &[u8]) {
    let len = raw_nal.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(raw_nal);
}

/// Re-export from bugreport module to avoid duplication.
fn chrono_now() -> String {
    crate::bugreport::chrono_now()
}

// ── Video writer task ───────────────────────────────────

/// Bound on the queue feeding the dedicated video encoder
/// task. Smaller than `PCAP_QUEUE_CAPACITY` because per-item
/// payload is dominated by full RGBA surface bytes (~8 MB at
/// 1080p, ~33 MB at 4K). Eight slots absorb ~100-250 ms of
/// encoder backlog at typical SPICE presentation rates before
/// drops begin. See PLAN-video-keeping-up-phase-03.
const VIDEO_QUEUE_CAPACITY: usize = 8;

/// One queued frame for the encoder task. `pixels` is
/// `Arc<[u8]>` so the egui hot-path enqueue copies the
/// surface once and the encoder task consumes it by
/// reference. `timestamp_ms` is captured at enqueue time so
/// MP4 presentation timestamps reflect when the frame was
/// produced, not when the encoder caught up.
#[derive(Debug)]
struct VideoQueueItem {
    surface_id: u32,
    pixels: Arc<[u8]>,
    width: u32,
    height: u32,
    timestamp_ms: u64,
}

/// Long-lived task that owns the `VideoWriter`. Lazily
/// initialises it from the first received frame's dimensions
/// (matching pre-phase-3 behaviour where the writer was
/// created inside `CaptureSession::frame` on first call).
/// When the sender drops, drains any in-flight items, then
/// finalises the MP4 by calling `VideoWriter::close()` —
/// which writes the moov atom and makes the file playable.
async fn video_writer_task(mut rx: mpsc::Receiver<VideoQueueItem>, dir: PathBuf) {
    let mut writer: Option<VideoWriter> = None;
    let mut init_attempted = false;

    while let Some(item) = rx.recv().await {
        // Only surface 0 is recorded today; non-primary
        // surfaces are dropped silently. The filter lives in
        // the task so the hot-path enqueue stays uniformly
        // cheap.
        if item.surface_id != 0 {
            debug!("capture: skipping non-primary surface {}", item.surface_id);
            continue;
        }
        if writer.is_none() && !init_attempted {
            init_attempted = true;
            writer = VideoWriter::new(
                &dir,
                &item.pixels,
                item.width,
                item.height,
                item.timestamp_ms,
            );
            // VideoWriter::new() writes the first frame as
            // part of init, so no separate write_frame call.
            continue;
        }
        if let Some(vw) = writer.as_mut() {
            vw.write_frame(&item.pixels, item.width, item.height, item.timestamp_ms);
        }
    }
    // Sender dropped → write the MP4 moov atom and exit.
    if let Some(mut vw) = writer.take() {
        vw.close();
    }
    debug!("capture: video writer task drained and exiting");
}

// ── Capture session ─────────────────────────────────────

/// Holds state for an active capture session.
pub struct CaptureSession {
    /// Output directory for capture files.
    pub dir: PathBuf,
    /// Timestamp of session start, for relative timing.
    pub start: Instant,
    /// Sender side of the queue feeding the dedicated pcap
    /// writer task. Held inside `Option<Mutex<>>` so `close()`
    /// can `take()` it and drop it, signalling the writer task
    /// to drain and exit.
    queue_tx: Mutex<Option<mpsc::Sender<PcapQueueItem>>>,
    /// Join handle for the writer task. Awaited by `close()`
    /// after the sender is dropped to guarantee the queue has
    /// drained before this `CaptureSession` is destroyed.
    writer_handle: Mutex<Option<JoinHandle<()>>>,
    /// Phase-03: sender side of the queue feeding the
    /// dedicated H.264/MP4 encoder task. Held inside
    /// `Option<Mutex<>>` so `close()` can `take()` it and
    /// drop it, signalling the encoder task to drain,
    /// finalise the MP4 (moov atom), and exit.
    video_tx: Mutex<Option<mpsc::Sender<VideoQueueItem>>>,
    /// Phase-03: join handle for the encoder task. Detached
    /// at `close()` time; the task continues on the runtime
    /// until it has drained the queue and finalised the MP4.
    video_handle: Mutex<Option<JoinHandle<()>>>,
    /// Guard against duplicate close() calls (explicit + Drop).
    closed: std::sync::atomic::AtomicBool,
}

impl CaptureSession {
    /// Create a new capture session writing to `dir`.
    ///
    /// Writes a `metadata.json` file with session context (platform,
    /// version, connection target) so that capture directories are
    /// self-describing when shared for bug reports.
    pub fn new(dir: PathBuf, host: &str, port: u16, tls_port: Option<u16>) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir)?;
        info!("capture: writing to {}", dir.display());

        // Write session metadata
        Self::write_metadata(&dir, host, port, tls_port)?;

        let mut writers: HashMap<&'static str, PcapChannelWriter> = HashMap::new();
        for &channel in CHANNELS {
            let path = dir.join(format!("{}.pcap", channel));
            let port = channel_port(channel);
            let writer = PcapChannelWriter::new(path, port)?;
            writers.insert(channel, writer);
        }

        let (queue_tx, queue_rx) = mpsc::channel(PCAP_QUEUE_CAPACITY);
        let writer_handle = tokio::spawn(pcap_writer_task(queue_rx, writers));

        let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE_CAPACITY);
        let video_handle = tokio::spawn(video_writer_task(video_rx, dir.clone()));

        Ok(CaptureSession {
            dir,
            start: Instant::now(),
            queue_tx: Mutex::new(Some(queue_tx)),
            writer_handle: Mutex::new(Some(writer_handle)),
            video_tx: Mutex::new(Some(video_tx)),
            video_handle: Mutex::new(Some(video_handle)),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Write a metadata.json file describing this capture session.
    fn write_metadata(
        dir: &std::path::Path,
        host: &str,
        port: u16,
        tls_port: Option<u16>,
    ) -> anyhow::Result<()> {
        use std::io::Write;

        let path = dir.join("metadata.json");
        let mut f = File::create(&path)?;

        let version = env!("CARGO_PKG_VERSION");
        let git_sha = env!("RYLL_GIT_SHA");
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let tls_str = match tls_port {
            Some(p) => format!("{}", p),
            None => String::from("null"),
        };

        // Simple hand-written JSON to avoid a serde dependency
        write!(
            f,
            "{{\n\
             \x20 \"ryll_version\": \"{}\",\n\
             \x20 \"ryll_git_sha\": \"{}\",\n\
             \x20 \"platform_os\": \"{}\",\n\
             \x20 \"platform_arch\": \"{}\",\n\
             \x20 \"target_host\": \"{}\",\n\
             \x20 \"target_port\": {},\n\
             \x20 \"target_tls_port\": {},\n\
             \x20 \"capture_started\": \"{}\"\n\
             }}\n",
            version,
            git_sha,
            os,
            arch,
            host.replace('\\', "\\\\").replace('"', "\\\""),
            port,
            tls_str,
            chrono_now(),
        )?;

        info!("capture: wrote {}", path.display());
        Ok(())
    }

    /// Record a packet sent by the client on the given channel.
    /// Returns `true` if the packet was queued to the writer
    /// task, `false` if the queue was full (the packet was
    /// dropped) or the channel name is not in `CHANNELS` (no
    /// writer exists).
    pub fn packet_sent(&self, channel: &str, data: &[u8]) -> bool {
        self.enqueue(channel, PcapDirection::Sent, data)
    }

    /// Record a packet received from the server on the given
    /// channel. Same `bool` semantics as `packet_sent`.
    pub fn packet_received(&self, channel: &str, data: &[u8]) -> bool {
        self.enqueue(channel, PcapDirection::Received, data)
    }

    fn enqueue(&self, channel: &str, direction: PcapDirection, data: &[u8]) -> bool {
        let Some(channel) = channel_static(channel) else {
            // Channels not in CHANNELS (e.g. webdav, playback) have
            // no pcap writer. Matches today's silent-drop behaviour
            // at the writer-task dispatch level, but avoids enqueue
            // and Arc allocation overhead for these channels.
            return true;
        };
        let tx_guard = self.queue_tx.lock().unwrap();
        let Some(tx) = tx_guard.as_ref() else {
            // close() has run; treat as drop.
            return false;
        };
        let item = PcapQueueItem {
            channel,
            direction,
            payload: Arc::from(data),
            elapsed: self.start.elapsed(),
        };
        tx.try_send(item).is_ok()
    }

    /// Record a display frame after a MARK boundary. Returns
    /// `true` if the frame was enqueued to the encoder task,
    /// `false` if the encoder's queue was full and the frame
    /// was dropped, or if the session has been closed. The
    /// surface-0 filter and lazy `VideoWriter::new()` both
    /// run on the encoder task; this method only allocates
    /// the `Arc<[u8]>` for the pixel buffer and `try_send`s.
    pub fn frame(&self, surface_id: u32, pixels: &[u8], width: u32, height: u32) -> bool {
        let tx_guard = self.video_tx.lock().unwrap();
        let Some(tx) = tx_guard.as_ref() else {
            return false; // close() has run
        };
        let item = VideoQueueItem {
            surface_id,
            pixels: Arc::from(pixels),
            width,
            height,
            timestamp_ms: self.start.elapsed().as_millis() as u64,
        };
        tx.try_send(item).is_ok()
    }

    /// Finalise and close the capture session.
    ///
    /// Takes `&self` so it can be called through an `Arc`
    /// (including from the sync egui frame-update path).
    /// Drops both queue senders; the dedicated writer tasks
    /// observe the sender drop, drain any in-flight items,
    /// and exit on their own. The encoder task additionally
    /// runs `VideoWriter::close()` (writes the MP4 moov atom)
    /// after its loop exits.
    ///
    /// We do *not* await the writer tasks' join handles here:
    /// two of the four close call sites are inside the sync
    /// egui `App::update` method, where awaiting is not
    /// feasible. Dropping a `JoinHandle` does not abort the
    /// task, so both writers keep running on the tokio
    /// runtime until they drain naturally; in practice they
    /// finish well before the runtime shuts down at process
    /// exit.
    ///
    /// **Phase-3 regression**: MP4 finalisation is no longer
    /// synchronous with `close()`. A bug report assembled
    /// within milliseconds of `close()` may see a not-yet-
    /// finalised (unplayable) MP4. At process exit the tokio
    /// runtime may also shut down before the encoder task
    /// drains, in which case the in-progress MP4 will be
    /// missing its moov atom and unplayable regardless of
    /// `close()` timing. See PLAN-video-keeping-up-phase-03
    /// for the trade-off and mitigation options.
    pub fn close(&self) {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return; // already closed
        }
        // Drop the pcap sender so its writer task drains and exits.
        drop(self.queue_tx.lock().unwrap().take());
        let _ = self.writer_handle.lock().unwrap().take();
        // Drop the video sender so its encoder task drains,
        // finalises the MP4 (writes the moov atom), and exits.
        drop(self.video_tx.lock().unwrap().take());
        let _ = self.video_handle.lock().unwrap().take();
        info!("capture: session closed ({})", self.dir.display());
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // close() is idempotent (guarded by self.closed); Drop just
        // delegates so implicit shutdown gets the same best-effort
        // sender-drop + video-finalize as an explicit close.
        self.close();
    }
}

/// Bridge `CaptureSession` into the renderer's `CaptureSink`
/// trait so that channel handlers can receive it as
/// `Arc<dyn CaptureSink>` without taking a concrete dependency
/// on this module.
impl shakenfist_spice_renderer::CaptureSink for CaptureSession {
    fn packet_sent(&self, channel: &str, data: &[u8]) -> bool {
        CaptureSession::packet_sent(self, channel, data)
    }

    fn packet_received(&self, channel: &str, data: &[u8]) -> bool {
        CaptureSession::packet_received(self, channel, data)
    }

    fn frame(&self, surface_id: u32, pixels: &[u8], width: u32, height: u32) -> bool {
        CaptureSession::frame(self, surface_id, pixels, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_payload_single_segment_under_max() {
        // A 1000-byte payload fits in one IPv4 frame; the
        // helper must return exactly one frame with the full
        // payload appended after the headers.
        let frames = segment_payload(
            CLIENT_IP,
            10001,
            SERVER_IP,
            SERVER_PORT,
            1234,
            5678,
            &[0u8; 1000],
        );
        assert_eq!(frames.len(), 1);
        // Frame size = ethernet(14) + ipv4(20) + tcp(20) + payload(1000).
        assert_eq!(frames[0].len(), 14 + 20 + 20 + 1000);
    }

    #[test]
    fn segment_payload_split_at_max() {
        // A 130 000-byte payload must produce 2 frames: the
        // first with 65 495 bytes of payload, the second with
        // the 64 505-byte tail. Phase 08 / K2 fix.
        let payload = vec![0u8; 130_000];
        let frames = segment_payload(CLIENT_IP, 10002, SERVER_IP, SERVER_PORT, 0, 0, &payload);
        assert_eq!(frames.len(), 2, "130KB payload should split into 2 frames");
        const HEADERS: usize = 14 + 20 + 20;
        assert_eq!(frames[0].len(), HEADERS + 65_495);
        assert_eq!(frames[1].len(), HEADERS + (130_000 - 65_495));
    }

    #[test]
    fn segment_payload_seqs_chain_correctly() {
        // Per-segment seq must be base + offset. Parse the
        // 4-byte big-endian seq field out of each frame's TCP
        // header (offset 14 + 20 + 4 = 38 bytes into the frame).
        let payload = vec![0u8; 200_000];
        let base_seq: u32 = 0x1000_0000;
        let frames = segment_payload(
            CLIENT_IP,
            10003,
            SERVER_IP,
            SERVER_PORT,
            base_seq,
            0,
            &payload,
        );
        assert_eq!(frames.len(), 4, "200KB payload should split into 4 frames");
        let extract_seq = |frame: &[u8]| -> u32 {
            // Ethernet 14, IPv4 20, then TCP header. Seq is at
            // bytes 4..8 of the TCP header, big-endian.
            let off = 14 + 20 + 4;
            u32::from_be_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]])
        };
        assert_eq!(extract_seq(&frames[0]), base_seq);
        assert_eq!(extract_seq(&frames[1]), base_seq.wrapping_add(65_495));
        assert_eq!(extract_seq(&frames[2]), base_seq.wrapping_add(130_990));
        assert_eq!(extract_seq(&frames[3]), base_seq.wrapping_add(196_485));
    }

    // ── Boundary + edge cases (wave 2b audit follow-up) ─────

    #[test]
    fn segment_payload_empty_data_returns_one_empty_frame() {
        // Pathological / hardening case from the function's
        // doc comment: empty payload must still produce
        // exactly one frame (the empty-frame fallback) so the
        // caller's "one push = one or more entries" invariant
        // holds. The frame contains only headers.
        let frames = segment_payload(CLIENT_IP, 10001, SERVER_IP, SERVER_PORT, 0, 0, &[]);
        assert_eq!(
            frames.len(),
            1,
            "empty payload must still produce one frame"
        );
        assert_eq!(frames[0].len(), 14 + 20 + 20, "empty frame is headers only");
    }

    #[test]
    fn segment_payload_exactly_at_max_payload_one_frame() {
        // MAX_PAYLOAD = 65 495 — the largest payload that fits
        // in a single IPv4 frame. Must produce exactly one
        // frame; off-by-one in the split condition would
        // silently produce two.
        let payload = vec![0u8; 65_495];
        let frames = segment_payload(CLIENT_IP, 10001, SERVER_IP, SERVER_PORT, 0, 0, &payload);
        assert_eq!(
            frames.len(),
            1,
            "exactly MAX_PAYLOAD bytes must fit in one frame"
        );
        assert_eq!(frames[0].len(), 14 + 20 + 20 + 65_495);
    }

    #[test]
    fn segment_payload_one_byte_over_max_payload_splits() {
        // The next byte past MAX_PAYLOAD must produce two
        // frames: the first with MAX_PAYLOAD bytes, the
        // second with 1 byte. Off-by-one in the other
        // direction would keep this in one frame.
        let payload = vec![0u8; 65_496];
        let frames = segment_payload(CLIENT_IP, 10001, SERVER_IP, SERVER_PORT, 0, 0, &payload);
        assert_eq!(frames.len(), 2, "one byte past MAX_PAYLOAD must split");
        assert_eq!(frames[0].len(), 14 + 20 + 20 + 65_495);
        assert_eq!(frames[1].len(), 14 + 20 + 20 + 1);
    }

    #[test]
    fn build_tcp_frame_oversized_payload_returns_empty_vec() {
        // Regression guard for the defensive branch in
        // build_tcp_frame: an over-65515-byte tcp_payload_len
        // must return Vec::new() (and warn), not panic. The
        // warn itself is instrumented with a one-shot backtrace
        // (Phase 15B); this test just pins the return contract
        // so a future refactor doesn't silently turn the warn
        // into a panic or a partial frame. We use 100 000 bytes
        // — comfortably past the 65515 ceiling and matching the
        // live observation range that motivated Phase 15.
        let payload = vec![0u8; 100_000];
        let frame = build_tcp_frame(CLIENT_IP, 10001, SERVER_IP, SERVER_PORT, 0, 0, &payload);
        assert!(
            frame.is_empty(),
            "oversized payload must return empty Vec, got {} bytes",
            frame.len()
        );
    }

    // ── Phase-02 pcap writer-task tests ──────────────────

    #[test]
    fn channel_static_resolves_known_channel_names() {
        // Every name in CHANNELS must resolve back to itself
        // as a &'static str so PcapQueueItem can carry it
        // without per-enqueue allocation.
        for &c in CHANNELS {
            let s = channel_static(c).expect("CHANNELS entry must resolve");
            assert_eq!(s, c);
        }
    }

    #[test]
    fn channel_static_rejects_unknown_channels() {
        // webdav and playback have packet_* call sites but no
        // pcap writer; channel_static filters them out before
        // we pay the Arc-allocation cost.
        assert!(channel_static("webdav").is_none());
        assert!(channel_static("playback").is_none());
        assert!(channel_static("nonexistent").is_none());
        assert!(channel_static("").is_none());
    }

    /// Open a pcap file and count its packets. Used by the
    /// writer-task tests to assert end-to-end delivery without
    /// taking on a parser dependency.
    fn count_pcap_packets(path: &std::path::Path) -> usize {
        use pcap_file::pcap::PcapReader;
        let f = File::open(path).expect("pcap open");
        let mut rdr = PcapReader::new(f).expect("pcap header");
        let mut count = 0;
        while let Some(pkt) = rdr.next_packet() {
            pkt.expect("pcap next");
            count += 1;
        }
        count
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pcap_writer_task_writes_all_enqueued_frames() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Build one writer per channel like CaptureSession::new.
        let mut writers: HashMap<&'static str, PcapChannelWriter> = HashMap::new();
        for &c in CHANNELS {
            let path = dir.path().join(format!("{}.pcap", c));
            let w = PcapChannelWriter::new(path, channel_port(c)).expect("writer new");
            writers.insert(c, w);
        }

        let (tx, rx) = mpsc::channel::<PcapQueueItem>(16);
        let handle = tokio::spawn(pcap_writer_task(rx, writers));

        // Enqueue 3 received + 2 sent on display, 1 received on main.
        for i in 0..3 {
            tx.send(PcapQueueItem {
                channel: "display",
                direction: PcapDirection::Received,
                payload: Arc::from(vec![i as u8; 100].as_slice()),
                elapsed: Duration::from_millis(i * 10),
            })
            .await
            .unwrap();
        }
        for i in 0..2 {
            tx.send(PcapQueueItem {
                channel: "display",
                direction: PcapDirection::Sent,
                payload: Arc::from(vec![i as u8; 50].as_slice()),
                elapsed: Duration::from_millis(40 + i * 10),
            })
            .await
            .unwrap();
        }
        tx.send(PcapQueueItem {
            channel: "main",
            direction: PcapDirection::Received,
            payload: Arc::from(vec![0u8; 30].as_slice()),
            elapsed: Duration::from_millis(60),
        })
        .await
        .unwrap();

        // Drop sender; writer task should drain and exit.
        drop(tx);
        handle.await.expect("writer task join");

        // Every enqueued item produces at least one pcap frame
        // (large payloads segment, small payloads stay as one).
        assert_eq!(count_pcap_packets(&dir.path().join("display.pcap")), 5);
        assert_eq!(count_pcap_packets(&dir.path().join("main.pcap")), 1);
        // Channels we didn't write to still have valid empty pcaps.
        assert_eq!(count_pcap_packets(&dir.path().join("cursor.pcap")), 0);
        assert_eq!(count_pcap_packets(&dir.path().join("inputs.pcap")), 0);
        assert_eq!(count_pcap_packets(&dir.path().join("usbredir.pcap")), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pcap_writer_task_drops_unknown_channel_in_dispatch() {
        // If a PcapQueueItem reaches the task with a channel
        // name that has no writer (shouldn't happen at the API
        // layer because channel_static filters), the task must
        // skip it and keep processing the next item rather than
        // panic.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writers: HashMap<&'static str, PcapChannelWriter> = HashMap::new();
        let path = dir.path().join("display.pcap");
        writers.insert(
            "display",
            PcapChannelWriter::new(path, channel_port("display")).expect("writer"),
        );

        let (tx, rx) = mpsc::channel::<PcapQueueItem>(8);
        let handle = tokio::spawn(pcap_writer_task(rx, writers));

        // Mix one valid and one unknown-channel item.
        tx.send(PcapQueueItem {
            channel: "ghost",
            direction: PcapDirection::Sent,
            payload: Arc::from(vec![0u8; 10].as_slice()),
            elapsed: Duration::from_millis(0),
        })
        .await
        .unwrap();
        tx.send(PcapQueueItem {
            channel: "display",
            direction: PcapDirection::Received,
            payload: Arc::from(vec![0u8; 10].as_slice()),
            elapsed: Duration::from_millis(1),
        })
        .await
        .unwrap();

        drop(tx);
        handle.await.expect("writer task join");

        // Only the valid item produced a frame.
        assert_eq!(count_pcap_packets(&dir.path().join("display.pcap")), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_session_enqueue_returns_false_when_queue_full() {
        // Saturate the queue by holding the runtime in this
        // task: the spawned writer task gets no chance to run
        // until we await, so try_send fills the queue and then
        // begins returning false. Validates the bool contract
        // CaptureSink callers rely on.
        let dir = tempfile::tempdir().expect("tempdir");
        let session =
            CaptureSession::new(dir.path().to_path_buf(), "test", 5900, None).expect("session new");

        let mut accepted = 0u64;
        let mut dropped = 0u64;
        // Send more than PCAP_QUEUE_CAPACITY items without
        // yielding so the writer task cannot drain. The exact
        // accepted/dropped split depends on whether the writer
        // task gets any cycles, but a multi-thousand burst on a
        // current_thread runtime should produce both.
        for i in 0..(PCAP_QUEUE_CAPACITY as u64 * 4) {
            let payload = vec![i as u8; 64];
            if session.packet_received("display", &payload) {
                accepted += 1;
            } else {
                dropped += 1;
            }
        }
        assert!(
            accepted > 0,
            "at least some packets should be accepted (got {})",
            accepted
        );
        assert!(
            dropped > 0,
            "queue should overflow with the writer task starved (got {})",
            dropped
        );

        // Close cleanly so the test doesn't leak the task; on
        // current_thread the task drains after this point as
        // the runtime keeps running until the test returns.
        session.close();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_session_close_is_idempotent_and_stops_accepting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session =
            CaptureSession::new(dir.path().to_path_buf(), "test", 5900, None).expect("session new");

        assert!(session.packet_received("display", &[0u8; 10]));
        session.close();
        // Second close() is a no-op (idempotent via self.closed).
        session.close();
        // After close() the queue sender is gone, so further
        // enqueues return false.
        assert!(!session.packet_received("display", &[0u8; 10]));
    }

    // ── Phase-03 video writer-task tests ─────────────────

    /// Build an RGBA pixel buffer of the requested size with
    /// a simple gradient. Content doesn't matter for H.264
    /// (the encoder will compress whatever bytes it gets);
    /// we just need at least `w*h*4` bytes.
    fn rgba_test_frame(w: u32, h: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                pixels[i] = (x % 256) as u8;
                pixels[i + 1] = (y % 256) as u8;
                pixels[i + 2] = ((x + y) % 256) as u8;
                pixels[i + 3] = 255;
            }
        }
        pixels
    }

    /// Open an MP4 and return (track_count, sample_count_in_track_1).
    /// Used to verify the encoder task finalised the moov atom and
    /// wrote samples. If the file was never finalised (no moov),
    /// `Mp4Reader::read_header` returns an error.
    fn read_mp4_track1(path: &std::path::Path) -> anyhow::Result<(usize, u32)> {
        let f = File::open(path)?;
        let size = f.metadata()?.len();
        let reader = std::io::BufReader::new(f);
        let mp4 = mp4::Mp4Reader::read_header(reader, size)?;
        let tracks = mp4.tracks().len();
        let samples = mp4.tracks().get(&1).map(|t| t.sample_count()).unwrap_or(0);
        Ok((tracks, samples))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn video_writer_task_encodes_and_finalises_mp4() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = mpsc::channel::<VideoQueueItem>(8);
        let handle = tokio::spawn(video_writer_task(rx, dir.path().to_path_buf()));

        let w: u32 = 64;
        let h: u32 = 64;
        let pixels = Arc::from(rgba_test_frame(w, h).as_slice());
        // Send 3 frames at 33 ms spacing.
        for i in 0..3u64 {
            tx.send(VideoQueueItem {
                surface_id: 0,
                pixels: Arc::clone(&pixels),
                width: w,
                height: h,
                timestamp_ms: i * 33,
            })
            .await
            .unwrap();
        }
        drop(tx);
        handle.await.expect("video task join");

        // The encoder task should have finalised the MP4. Read it
        // back and assert the moov atom is present (Mp4Reader
        // would otherwise fail) and that exactly one video track
        // exists with 3 samples.
        let mp4_path = dir.path().join("display.mp4");
        let (tracks, samples) = read_mp4_track1(&mp4_path).expect("read mp4 header");
        assert_eq!(tracks, 1, "expected one video track");
        assert_eq!(samples, 3, "expected three samples (frames)");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn video_writer_task_skips_non_primary_surfaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = mpsc::channel::<VideoQueueItem>(8);
        let handle = tokio::spawn(video_writer_task(rx, dir.path().to_path_buf()));

        let w: u32 = 64;
        let h: u32 = 64;
        let pixels = Arc::from(rgba_test_frame(w, h).as_slice());
        // First item: non-primary surface — task should skip it
        // without consuming the lazy-init slot.
        tx.send(VideoQueueItem {
            surface_id: 7,
            pixels: Arc::clone(&pixels),
            width: w,
            height: h,
            timestamp_ms: 0,
        })
        .await
        .unwrap();
        // Then a primary-surface frame that should init the writer.
        tx.send(VideoQueueItem {
            surface_id: 0,
            pixels: Arc::clone(&pixels),
            width: w,
            height: h,
            timestamp_ms: 33,
        })
        .await
        .unwrap();
        // Then another non-primary that should be dropped post-init.
        tx.send(VideoQueueItem {
            surface_id: 2,
            pixels: Arc::clone(&pixels),
            width: w,
            height: h,
            timestamp_ms: 66,
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.expect("video task join");

        // Only one surface-0 frame; expect exactly one sample.
        let mp4_path = dir.path().join("display.mp4");
        let (_, samples) = read_mp4_track1(&mp4_path).expect("read mp4 header");
        assert_eq!(samples, 1, "expected one sample from surface 0 only");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_session_frame_returns_false_when_video_queue_full() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session =
            CaptureSession::new(dir.path().to_path_buf(), "test", 5900, None).expect("session new");

        let w: u32 = 64;
        let h: u32 = 64;
        let pixels = rgba_test_frame(w, h);

        let mut accepted = 0u64;
        let mut dropped = 0u64;
        // Saturate the video queue without yielding. With a
        // current_thread runtime the encoder task gets no cycles
        // until we await, so try_send eventually returns
        // Err(Full). Send more than VIDEO_QUEUE_CAPACITY items.
        for _ in 0..(VIDEO_QUEUE_CAPACITY as u64 * 4) {
            if session.frame(0, &pixels, w, h) {
                accepted += 1;
            } else {
                dropped += 1;
            }
        }
        assert!(
            accepted > 0,
            "at least some frames should be accepted (got {})",
            accepted
        );
        assert!(
            dropped > 0,
            "queue should overflow with the encoder task starved (got {})",
            dropped
        );
        session.close();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_session_frame_returns_false_after_close() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session =
            CaptureSession::new(dir.path().to_path_buf(), "test", 5900, None).expect("session new");
        let w: u32 = 64;
        let h: u32 = 64;
        let pixels = rgba_test_frame(w, h);
        assert!(session.frame(0, &pixels, w, h));
        session.close();
        // Second close() is a no-op.
        session.close();
        assert!(!session.frame(0, &pixels, w, h));
    }
}
