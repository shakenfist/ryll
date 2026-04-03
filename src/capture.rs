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
    /// that exceed the IPv4 maximum (65535 - headers ≈ 65495).
    fn write_segmented(
        &mut self,
        src_ip: [u8; 4],
        src_port: u16,
        dst_ip: [u8; 4],
        dst_port: u16,
        data: &[u8],
        elapsed: std::time::Duration,
    ) {
        const MAX_PAYLOAD: usize = 65495; // 65535 - 20 (IP) - 20 (TCP)
        let is_client = src_ip == CLIENT_IP;
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + MAX_PAYLOAD).min(data.len());
            let chunk = &data[offset..end];
            let (seq, ack) = if is_client {
                (self.client_seq.wrapping_add(offset as u32), self.server_seq)
            } else {
                (self.server_seq.wrapping_add(offset as u32), self.client_seq)
            };
            let frame = build_tcp_frame(src_ip, src_port, dst_ip, dst_port, seq, ack, chunk);
            self.write_frame(&frame, elapsed);
            offset = end;
        }
    }

    fn write_frame(&mut self, frame: &[u8], elapsed: std::time::Duration) {
        let packet = PcapPacket::new(elapsed, frame.len() as u32, frame);
        self.writer.write_packet(&packet).ok();
    }
}

/// Build a fake Ethernet + IPv4 + TCP frame wrapping `payload`.
fn build_tcp_frame(
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
fn channel_port(channel: &str) -> u16 {
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
    encoder: openh264::encoder::Encoder,
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
        use openh264::encoder::Encoder;
        use openh264::formats::{RgbaSliceU8, YUVBuffer};

        // openh264 requires even dimensions
        let w = width & !1;
        let h = height & !1;
        if w == 0 || h == 0 {
            warn!("capture: video dimensions too small: {}x{}", width, height);
            return None;
        }

        // Create encoder (dimensions auto-detected from first frame)
        let mut encoder = match Encoder::new() {
            Ok(e) => e,
            Err(e) => {
                warn!("capture: failed to create H.264 encoder: {}", e);
                return None;
            }
        };

        // Convert RGBA to YUV via openh264's built-in conversion
        let rgba = RgbaSliceU8::new(pixels, (w as usize, h as usize));
        let yuv = YUVBuffer::from_rgb_source(rgba);

        // Force first frame to be an IDR keyframe (produces SPS/PPS)
        encoder.force_intra_frame();

        // Encode first frame
        let bitstream = match encoder.encode(&yuv) {
            Ok(bs) => bs,
            Err(e) => {
                warn!("capture: failed to encode first frame: {}", e);
                return None;
            }
        };

        // Collect NAL units from the bitstream
        let mut sps: Vec<u8> = Vec::new();
        let mut pps: Vec<u8> = Vec::new();
        let mut frame_data: Vec<u8> = Vec::new();
        let mut is_sync = false;

        for layer_idx in 0..bitstream.num_layers() {
            if let Some(layer) = bitstream.layer(layer_idx) {
                for nal_idx in 0..layer.nal_count() {
                    if let Some(nal) = layer.nal_unit(nal_idx) {
                        if nal.is_empty() {
                            continue;
                        }
                        let nal_type = nal[0] & 0x1F;
                        match nal_type {
                            7 => sps = nal.to_vec(),
                            8 => pps = nal.to_vec(),
                            5 => {
                                is_sync = true;
                                frame_data.extend_from_slice(nal);
                            }
                            _ => {
                                frame_data.extend_from_slice(nal);
                            }
                        }
                    }
                }
            }
        }

        if sps.is_empty() || pps.is_empty() {
            // Try to get SPS/PPS by writing the full bitstream and
            // scanning for start codes (0x00 0x00 0x00 0x01)
            let full = bitstream.to_vec();
            debug!(
                "capture: bitstream {} bytes, {} layers, scanning for NAL start codes",
                full.len(),
                bitstream.num_layers()
            );

            let mut pos = 0;
            while pos + 4 < full.len() {
                if full[pos] == 0 && full[pos + 1] == 0 && full[pos + 2] == 0 && full[pos + 3] == 1
                {
                    let nal_start = pos + 4;
                    // Find next start code or end
                    let mut end = full.len();
                    for j in nal_start..full.len().saturating_sub(3) {
                        if full[j] == 0 && full[j + 1] == 0 && full[j + 2] == 0 && full[j + 3] == 1
                        {
                            end = j;
                            break;
                        }
                    }
                    let nal_type = full[nal_start] & 0x1F;
                    debug!(
                        "capture: found NAL type={} at offset {} len={}",
                        nal_type,
                        pos,
                        end - nal_start
                    );
                    match nal_type {
                        7 if sps.is_empty() => sps = full[nal_start..end].to_vec(),
                        8 if pps.is_empty() => pps = full[nal_start..end].to_vec(),
                        5 => {
                            is_sync = true;
                            if frame_data.is_empty() {
                                frame_data = full[nal_start..end].to_vec();
                            }
                        }
                        1 => {
                            if frame_data.is_empty() {
                                frame_data = full[nal_start..end].to_vec();
                            }
                        }
                        _ => {}
                    }
                    pos = end;
                } else {
                    pos += 1;
                }
            }
        }

        if sps.is_empty() || pps.is_empty() {
            warn!(
                "capture: encoder did not produce SPS/PPS (sps={} pps={} frame={})",
                sps.len(),
                pps.len(),
                frame_data.len()
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

        // Write first frame as length-prefixed NAL (AVCC format)
        let nal_with_length = length_prefix_nal(&frame_data);
        let sample = mp4::Mp4Sample {
            start_time: timestamp_ms,
            duration: 33, // placeholder until next frame
            rendering_offset: 0,
            is_sync,
            bytes: bytes::Bytes::from(nal_with_length),
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
        use openh264::formats::{RgbaSliceU8, YUVBuffer};

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

        let rgba = RgbaSliceU8::new(pixels, (self.width as usize, self.height as usize));
        let yuv = YUVBuffer::from_rgb_source(rgba);

        let bitstream = match self.encoder.encode(&yuv) {
            Ok(bs) => bs,
            Err(e) => {
                warn!("capture: H.264 encode failed: {}", e);
                return;
            }
        };

        // Collect NAL units (skip SPS/PPS for subsequent frames)
        let mut frame_data: Vec<u8> = Vec::new();
        let mut is_sync = false;

        for layer_idx in 0..bitstream.num_layers() {
            if let Some(layer) = bitstream.layer(layer_idx) {
                for nal_idx in 0..layer.nal_count() {
                    if let Some(nal) = layer.nal_unit(nal_idx) {
                        if nal.is_empty() {
                            continue;
                        }
                        let nal_type = nal[0] & 0x1F;
                        if nal_type == 5 {
                            is_sync = true;
                        }
                        if nal_type != 7 && nal_type != 8 {
                            frame_data.extend_from_slice(nal);
                        }
                    }
                }
            }
        }

        if frame_data.is_empty() {
            return;
        }

        let duration_ms = timestamp_ms.saturating_sub(self.last_timestamp_ms).max(1) as u32;

        let nal_with_length = length_prefix_nal(&frame_data);
        let sample = mp4::Mp4Sample {
            start_time: timestamp_ms,
            duration: duration_ms,
            rendering_offset: 0,
            is_sync,
            bytes: bytes::Bytes::from(nal_with_length),
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

/// Wrap raw NAL unit data with a 4-byte big-endian length prefix
/// (Annex B to AVCC format, required by MP4).
fn length_prefix_nal(nal: &[u8]) -> Vec<u8> {
    let len = nal.len() as u32;
    let mut out = Vec::with_capacity(4 + nal.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(nal);
    out
}

/// Return the current UTC time as an ISO-8601 string without
/// pulling in a datetime crate.  Uses UNIX_EPOCH + SystemTime.
fn chrono_now() -> String {
    use std::time::SystemTime;
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            // Rough UTC decomposition (no leap-second handling)
            let days = secs / 86400;
            let time_secs = secs % 86400;
            let h = time_secs / 3600;
            let m = (time_secs % 3600) / 60;
            let s = time_secs % 60;

            // Days since 1970-01-01 → (year, month, day)
            // Algorithm from Howard Hinnant's date library (public domain)
            let z = days as i64 + 719468;
            let era = if z >= 0 { z } else { z - 146096 } / 146097;
            let doe = (z - era * 146097) as u64;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y = yoe as i64 + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let mon = if mp < 10 { mp + 3 } else { mp - 9 };
            let year = if mon <= 2 { y + 1 } else { y };
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                year, mon, d, h, m, s
            )
        }
        Err(_) => String::from("unknown"),
    }
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
             \x20 \"platform_os\": \"{}\",\n\
             \x20 \"platform_arch\": \"{}\",\n\
             \x20 \"target_host\": \"{}\",\n\
             \x20 \"target_port\": {},\n\
             \x20 \"target_tls_port\": {},\n\
             \x20 \"capture_started\": \"{}\"\n\
             }}\n",
            version,
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
