/// Capture session for protocol and display debugging.
///
/// When `--capture <DIR>` is specified, all SPICE protocol
/// traffic and display frames are written to files in the
/// given directory. When not enabled, all methods are no-ops.
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};
use pcap_file::DataLink;
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
        warn!(
            "build_tcp_frame: payload too large for IPv4 ({} bytes), dropping",
            ip_payload_len
        );
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

// ── Capture session ─────────────────────────────────────

/// Holds state for an active capture session.
pub struct CaptureSession {
    /// Output directory for capture files.
    pub dir: PathBuf,
    /// Timestamp of session start, for relative timing.
    pub start: Instant,
    /// Per-channel pcap writers.
    pcap_writers: HashMap<String, Mutex<PcapChannelWriter>>,
    /// Video writer (lazily initialised on first frame).
    video_writer: Mutex<Option<VideoWriter>>,
    /// Set to true after video init has been attempted (even if it failed).
    video_init_attempted: Mutex<bool>,
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

        let mut pcap_writers = HashMap::new();
        for &channel in CHANNELS {
            let path = dir.join(format!("{}.pcap", channel));
            let port = channel_port(channel);
            let writer = PcapChannelWriter::new(path, port)?;
            pcap_writers.insert(channel.to_string(), Mutex::new(writer));
        }

        Ok(CaptureSession {
            dir,
            start: Instant::now(),
            pcap_writers,
            video_writer: Mutex::new(None),
            video_init_attempted: Mutex::new(false),
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
    pub fn packet_sent(&self, channel: &str, data: &[u8]) {
        if let Some(writer) = self.pcap_writers.get(channel) {
            let elapsed = self.start.elapsed();
            let mut w = writer.lock().unwrap();
            w.write_sent(data, elapsed);
        } else {
            debug!("capture: no pcap writer for channel '{}'", channel);
        }
    }

    /// Record a packet received from the server on the given channel.
    pub fn packet_received(&self, channel: &str, data: &[u8]) {
        if let Some(writer) = self.pcap_writers.get(channel) {
            let elapsed = self.start.elapsed();
            let mut w = writer.lock().unwrap();
            w.write_received(data, elapsed);
        } else {
            debug!("capture: no pcap writer for channel '{}'", channel);
        }
    }

    /// Record a display frame after a MARK boundary.
    pub fn frame(&self, surface_id: u32, pixels: &[u8], width: u32, height: u32) {
        if surface_id != 0 {
            debug!("capture: skipping non-primary surface {}", surface_id);
            return;
        }

        let timestamp_ms = self.start.elapsed().as_millis() as u64;

        let mut writer = self.video_writer.lock().unwrap();
        let mut attempted = self.video_init_attempted.lock().unwrap();

        if writer.is_none() && !*attempted {
            *attempted = true;
            *writer = VideoWriter::new(&self.dir, pixels, width, height, timestamp_ms);
            return; // first frame already written by VideoWriter::new()
        }

        if let Some(ref mut vw) = *writer {
            vw.write_frame(pixels, width, height, timestamp_ms);
        }
    }

    /// Finalise and close the capture session.
    ///
    /// Takes `&self` so it can be called through an `Arc` (e.g. from
    /// the Ctrl+C handler).  Pcap writers use unbuffered I/O and need
    /// no explicit flush; only the MP4 video writer requires
    /// finalisation to write the moov atom.
    pub fn close(&self) {
        if self.closed.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return; // already closed
        }
        if let Ok(mut writer) = self.video_writer.lock() {
            if let Some(ref mut vw) = *writer {
                vw.close();
            }
            *writer = None;
        }
        info!("capture: session closed ({})", self.dir.display());
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// Bridge `CaptureSession` into the renderer's `CaptureSink`
/// trait so that channel handlers can receive it as
/// `Arc<dyn CaptureSink>` without taking a concrete dependency
/// on this module.
impl shakenfist_spice_renderer::CaptureSink for CaptureSession {
    fn packet_sent(&self, channel: &str, data: &[u8]) {
        CaptureSession::packet_sent(self, channel, data);
    }

    fn packet_received(&self, channel: &str, data: &[u8]) {
        CaptureSession::packet_received(self, channel, data);
    }

    fn frame(&self, surface_id: u32, pixels: &[u8], width: u32, height: u32) {
        CaptureSession::frame(self, surface_id, pixels, width, height);
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
}
