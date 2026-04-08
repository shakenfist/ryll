/// Bug report infrastructure — per-channel traffic ring buffer
/// and channel state snapshots.
///
/// Always active regardless of `--capture`.  Retains the most
/// recent protocol traffic for bug report export and live
/// traffic viewer display.  Channel snapshots capture mutable
/// state for JSON serialisation in bug reports.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::debug;

#[cfg(feature = "capture")]
use crate::capture;

/// Direction of a protocol message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficDirection {
    Sent,
    Received,
}

/// A single protocol message recorded in the ring buffer.
#[derive(Debug, Clone)]
pub struct TrafficEntry {
    /// Time elapsed since session start.
    pub timestamp: Duration,
    /// Which channel this message belongs to.
    pub channel: &'static str,
    /// Sent by the client or received from the server.
    pub direction: TrafficDirection,
    /// SPICE message type ID (from the 6-byte mini-header).
    pub message_type: u16,
    /// Human-readable message name (e.g. "draw_copy").
    pub message_name: &'static str,
    /// Total wire size including the 6-byte header.
    pub wire_size: u32,
    /// Payload size (wire_size minus 6-byte header).
    pub payload_size: u32,
    /// Full pcap frame bytes (Ethernet + IP + TCP + SPICE
    /// payload).  Used by `drain_to_pcap()` for bug report
    /// export.
    pub pcap_frame: Vec<u8>,
}

/// Lightweight traffic entry for the viewer (no pcap frame).
#[derive(Clone)]
#[allow(dead_code)] // some fields reserved for future use (hex dump, expanded row)
pub struct TrafficViewEntry {
    /// Time elapsed since session start.
    pub timestamp: Duration,
    /// Which channel this message belongs to.
    pub channel: &'static str,
    /// Sent by the client or received from the server.
    pub direction: TrafficDirection,
    /// SPICE message type ID.
    pub message_type: u16,
    /// Human-readable message name.
    pub message_name: &'static str,
    /// Total wire size including the 6-byte header.
    pub wire_size: u32,
    /// Payload size (wire_size minus 6-byte header).
    pub payload_size: u32,
}

/// Per-channel ring buffer of recent protocol traffic.
pub struct TrafficRingBuffer {
    /// Ring of entries, newest at the back.
    entries: VecDeque<TrafficEntry>,
    /// Current total byte count of all pcap_frame data.
    total_bytes: usize,
    /// Maximum byte count before eviction.
    max_bytes: usize,
    /// TCP sequence number for client→server direction.
    client_seq: u32,
    /// TCP sequence number for server→client direction.
    server_seq: u32,
}

impl TrafficRingBuffer {
    /// Create a new ring buffer with the given byte cap.
    pub fn new(max_bytes: usize) -> Self {
        TrafficRingBuffer {
            entries: VecDeque::new(),
            total_bytes: 0,
            max_bytes,
            client_seq: 1000,
            server_seq: 2000,
        }
    }

    /// Push a new entry, evicting oldest entries if the byte
    /// cap would be exceeded.
    pub fn push(&mut self, entry: TrafficEntry) {
        self.total_bytes += entry.pcap_frame.len();
        self.entries.push_back(entry);
        while self.total_bytes > self.max_bytes {
            if let Some(old) = self.entries.pop_front() {
                self.total_bytes -= old.pcap_frame.len();
            } else {
                break;
            }
        }
    }

    /// Return a reference to all buffered entries (oldest first).
    pub fn entries(&self) -> &VecDeque<TrafficEntry> {
        &self.entries
    }

    /// Write all buffered pcap frames to a pcap file at the
    /// given path.  Returns the number of frames written.
    #[cfg(feature = "capture")]
    #[allow(dead_code)]
    pub fn drain_to_pcap(&self, path: &std::path::Path) -> anyhow::Result<usize> {
        use std::fs::File;
        let file = File::create(path)?;
        self.write_pcap_to(file)
    }

    /// Write all buffered pcap frames to a writer.
    #[cfg(feature = "capture")]
    pub fn write_pcap_to<W: std::io::Write>(&self, writer: W) -> anyhow::Result<usize> {
        use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};
        use pcap_file::DataLink;

        let header = PcapHeader {
            datalink: DataLink::ETHERNET,
            ..Default::default()
        };
        let mut pcap = PcapWriter::with_header(writer, header)?;

        let mut count = 0;
        for entry in &self.entries {
            let packet = PcapPacket::new(
                entry.timestamp,
                entry.pcap_frame.len() as u32,
                &entry.pcap_frame,
            );
            pcap.write_packet(&packet).ok();
            count += 1;
        }

        Ok(count)
    }

    /// Current number of entries.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Current byte usage.
    #[allow(dead_code)]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// Default per-channel ring buffer cap: 10 MB (50 MB / 5 channels).
const PER_CHANNEL_BYTES: usize = 50 * 1024 * 1024 / 5;

/// Known channel names.
const CHANNELS: [&str; 5] = ["main", "display", "inputs", "cursor", "usbredir"];

/// Holds all four per-channel ring buffers plus a shared session
/// start timestamp.
pub struct TrafficBuffers {
    main: Mutex<TrafficRingBuffer>,
    display: Mutex<TrafficRingBuffer>,
    inputs: Mutex<TrafficRingBuffer>,
    cursor: Mutex<TrafficRingBuffer>,
    usbredir: Mutex<TrafficRingBuffer>,
    /// Session start time for relative timestamps.
    start: Instant,
}

impl TrafficBuffers {
    /// Create a new set of traffic buffers.
    pub fn new() -> Self {
        TrafficBuffers {
            main: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            display: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            inputs: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            cursor: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            usbredir: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            start: Instant::now(),
        }
    }

    /// Get the duration since session start.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Get the ring buffer for a channel by name.
    fn buffer_for(&self, channel: &str) -> Option<&Mutex<TrafficRingBuffer>> {
        match channel {
            "main" => Some(&self.main),
            "display" => Some(&self.display),
            "inputs" => Some(&self.inputs),
            "cursor" => Some(&self.cursor),
            "usbredir" => Some(&self.usbredir),
            _ => None,
        }
    }

    /// Record a message received from the server.
    pub fn record_received(
        &self,
        channel: &'static str,
        msg_type: u16,
        msg_name: &'static str,
        raw_message: &[u8],
    ) {
        let buf = match self.buffer_for(channel) {
            Some(b) => b,
            None => return,
        };

        let elapsed = self.elapsed();
        let wire_size = raw_message.len() as u32;
        let payload_size = wire_size.saturating_sub(6);

        let mut guard = buf.lock().unwrap();
        let pcap_frame = self.build_frame(channel, false, raw_message, &mut guard);
        let entry = TrafficEntry {
            timestamp: elapsed,
            channel,
            direction: TrafficDirection::Received,
            message_type: msg_type,
            message_name: msg_name,
            wire_size,
            payload_size,
            pcap_frame,
        };
        guard.push(entry);
    }

    /// Record a message sent by the client.
    pub fn record_sent(
        &self,
        channel: &'static str,
        msg_type: u16,
        msg_name: &'static str,
        raw_message: &[u8],
    ) {
        let buf = match self.buffer_for(channel) {
            Some(b) => b,
            None => return,
        };

        let elapsed = self.elapsed();
        let wire_size = raw_message.len() as u32;
        let payload_size = wire_size.saturating_sub(6);

        let mut guard = buf.lock().unwrap();
        let pcap_frame = self.build_frame(channel, true, raw_message, &mut guard);
        let entry = TrafficEntry {
            timestamp: elapsed,
            channel,
            direction: TrafficDirection::Sent,
            message_type: msg_type,
            message_name: msg_name,
            wire_size,
            payload_size,
            pcap_frame,
        };
        guard.push(entry);
    }

    /// Build a pcap frame for the given message, updating TCP
    /// sequence numbers on the ring buffer.
    #[cfg(feature = "capture")]
    fn build_frame(
        &self,
        channel: &str,
        is_sent: bool,
        data: &[u8],
        ring: &mut TrafficRingBuffer,
    ) -> Vec<u8> {
        let port = capture::channel_port(channel);
        let (src_ip, src_port, dst_ip, dst_port, seq, ack) = if is_sent {
            let s = ring.client_seq;
            let a = ring.server_seq;
            ring.client_seq = ring.client_seq.wrapping_add(data.len() as u32);
            ([10, 0, 0, 1], port, [10, 0, 0, 2], 5900u16, s, a)
        } else {
            let s = ring.server_seq;
            let a = ring.client_seq;
            ring.server_seq = ring.server_seq.wrapping_add(data.len() as u32);
            ([10, 0, 0, 2], 5900u16, [10, 0, 0, 1], port, s, a)
        };
        capture::build_tcp_frame(src_ip, src_port, dst_ip, dst_port, seq, ack, data)
    }

    /// Stub when capture feature is disabled — produce an empty
    /// frame since pcap construction is unavailable.
    #[cfg(not(feature = "capture"))]
    fn build_frame(
        &self,
        _channel: &str,
        _is_sent: bool,
        _data: &[u8],
        _ring: &mut TrafficRingBuffer,
    ) -> Vec<u8> {
        Vec::new()
    }

    /// Log a summary of ring buffer state (for verbose mode).
    #[allow(dead_code)]
    pub fn log_summary(&self) {
        for name in &CHANNELS {
            if let Some(buf) = self.buffer_for(name) {
                let guard = buf.lock().unwrap();
                debug!(
                    "bugreport: {} ring buffer: {} entries, {} bytes",
                    name,
                    guard.len(),
                    guard.total_bytes()
                );
            }
        }
    }

    /// Drain a channel's ring buffer to pcap bytes in memory.
    /// Returns `None` if the capture feature is disabled or the
    /// channel name is unknown.
    pub fn drain_channel_pcap_bytes(&self, channel: &str) -> Option<Vec<u8>> {
        #[cfg(feature = "capture")]
        {
            let buf = self.buffer_for(channel)?;
            let guard = buf.lock().unwrap();
            let mut output = Vec::new();
            guard.write_pcap_to(&mut output).ok()?;
            Some(output)
        }
        #[cfg(not(feature = "capture"))]
        {
            let _ = channel;
            None
        }
    }

    /// Collect recent entries from all channels for the traffic
    /// viewer.  Returns at most `max` entries sorted by timestamp
    /// (oldest first).  Does not copy pcap frame data.
    pub fn recent_view_entries(&self, max: usize) -> Vec<TrafficViewEntry> {
        let mut all = Vec::new();
        for name in &CHANNELS {
            if let Some(buf) = self.buffer_for(name) {
                let guard = buf.lock().unwrap();
                for entry in guard.entries().iter().rev().take(max) {
                    all.push(TrafficViewEntry {
                        timestamp: entry.timestamp,
                        channel: entry.channel,
                        direction: entry.direction,
                        message_type: entry.message_type,
                        message_name: entry.message_name,
                        wire_size: entry.wire_size,
                        payload_size: entry.payload_size,
                    });
                }
            }
        }
        all.sort_by_key(|e| e.timestamp);
        if all.len() > max {
            all.drain(..all.len() - max);
        }
        all
    }
}

// ── Channel state snapshots ─────────────────────────────────

/// Result of a single image decode in the display channel.
#[derive(Debug, Clone, Serialize)]
pub struct DecodeResult {
    /// SPICE image type (e.g. "GlzRgb", "Lz4", "Pixmap").
    pub image_type: String,
    /// Image ID from the ImageDescriptor.
    pub image_id: u64,
    /// Decoded width in pixels.
    pub width: u32,
    /// Decoded height in pixels.
    pub height: u32,
    /// Whether this was a cache hit (FromCache type).
    pub from_cache: bool,
    /// Whether decompression succeeded.
    pub success: bool,
    /// Seconds since session start when this decode occurred.
    pub timestamp_secs: f64,
}

/// Snapshot of the display channel's mutable state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DisplaySnapshot {
    pub image_cache_entries: usize,
    pub image_cache_ids: Vec<u64>,
    pub image_cache_bytes: usize,
    pub recent_decodes: VecDeque<DecodeResult>,
    pub ack_generation: u32,
    pub ack_window: u32,
    pub message_count: u32,
    pub last_ack: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// A recorded input event for the inputs channel snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct InputEventRecord {
    /// "KeyDown", "KeyUp", "MouseDown", "MouseUp", "MouseMove".
    pub event_type: String,
    /// Scancode for key events, 0 for mouse events.
    pub scancode: u32,
    /// Mouse position (0,0 for key events).
    pub x: u32,
    pub y: u32,
    /// Button bitmask for mouse press/release events.
    pub button_mask: u32,
    /// Seconds since session start.
    pub timestamp_secs: f64,
}

/// Snapshot of the inputs channel's mutable state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputsSnapshot {
    pub button_state: u32,
    pub motion_count: u32,
    pub secs_since_last_key: Option<f64>,
    pub recent_events: VecDeque<InputEventRecord>,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Summary of a cached cursor shape.
#[derive(Debug, Clone, Serialize)]
pub struct CursorCacheEntry {
    pub cursor_id: u64,
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
}

/// Snapshot of the cursor channel's mutable state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CursorSnapshot {
    pub cache_entries: usize,
    pub cache_contents: Vec<CursorCacheEntry>,
    pub ack_generation: u32,
    pub ack_window: u32,
    pub message_count: u32,
    pub last_ack: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Snapshot of the main channel's mutable state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MainSnapshot {
    pub session_id: Option<u32>,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Summary of an active display surface.
#[derive(Debug, Clone, Serialize)]
pub struct SurfaceInfo {
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
}

/// Snapshot of application-level state.
#[derive(Debug, Clone, Serialize)]
pub struct AppSnapshot {
    pub fps: f64,
    pub bandwidth_history: Vec<f32>,
    pub bandwidth_current: f32,
    pub last_latency: Option<f64>,
    pub frames_received: u64,
    pub surfaces: Vec<SurfaceInfo>,
    pub cursor_pos: (u16, u16),
    pub cursor_visible: bool,
    pub mouse_mode: u32,
    pub connected: bool,
    pub uptime_secs: f64,
}

impl Default for AppSnapshot {
    fn default() -> Self {
        AppSnapshot {
            fps: 0.0,
            bandwidth_history: Vec::new(),
            bandwidth_current: 0.0,
            last_latency: None,
            frames_received: 0,
            surfaces: Vec::new(),
            cursor_pos: (0, 0),
            cursor_visible: true,
            mouse_mode: 0,
            connected: false,
            uptime_secs: 0.0,
        }
    }
}

/// Holds all four per-channel snapshot `Arc<Mutex<T>>`s.
pub struct ChannelSnapshots {
    pub display: Arc<Mutex<DisplaySnapshot>>,
    pub inputs: Arc<Mutex<InputsSnapshot>>,
    pub cursor: Arc<Mutex<CursorSnapshot>>,
    pub main: Arc<Mutex<MainSnapshot>>,
}

impl ChannelSnapshots {
    pub fn new() -> Self {
        ChannelSnapshots {
            display: Arc::new(Mutex::new(DisplaySnapshot::default())),
            inputs: Arc::new(Mutex::new(InputsSnapshot::default())),
            cursor: Arc::new(Mutex::new(CursorSnapshot::default())),
            main: Arc::new(Mutex::new(MainSnapshot::default())),
        }
    }
}

// ── Timestamp utilities ────────────────────────────────────

/// Return the current UTC time as an ISO-8601 string without
/// pulling in a datetime crate.  Uses UNIX_EPOCH + SystemTime.
pub(crate) fn chrono_now() -> String {
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

/// Filename-safe timestamp: colons replaced with hyphens.
pub(crate) fn filename_timestamp() -> String {
    chrono_now().replace(':', "-")
}

// ── PNG encoding ───────────────────────────────────────────

/// Encode RGBA pixels to PNG bytes in memory.
pub(crate) fn encode_png(pixels: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut encoder = png::Encoder::new(cursor, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
    }
    Ok(buf)
}

/// Format a byte size for human-readable display.
pub(crate) fn format_size(bytes: u32) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1}M", bytes as f64 / 1_000_000.0)
    } else if bytes >= 10_000 {
        format!("{:.1}K", bytes as f64 / 1_000.0)
    } else {
        format!("{}", bytes)
    }
}

// ── Bug report assembly ────────────────────────────────────

/// Which channel the bug report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BugReportType {
    Display,
    Input,
    Cursor,
    Connection,
    Usb,
}

impl BugReportType {
    /// SPICE channel name used for ring buffer drain and snapshot
    /// selection.
    pub fn channel_name(&self) -> &'static str {
        match self {
            BugReportType::Display => "display",
            BugReportType::Input => "inputs",
            BugReportType::Cursor => "cursor",
            BugReportType::Connection => "main",
            BugReportType::Usb => "usbredir",
        }
    }
}

/// Highlighted region for display bug reports.
#[derive(Debug, Clone, Serialize)]
pub struct ReportRegion {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

/// Top-level metadata written to metadata.json.
#[derive(Debug, Clone, Serialize)]
pub struct ReportMetadata {
    pub ryll_version: String,
    pub platform_os: String,
    pub platform_arch: String,
    pub report_type: BugReportType,
    pub channel: String,
    pub description: String,
    pub region: Option<ReportRegion>,
    pub timestamp: String,
    pub target_host: String,
    pub target_port: u16,
    pub session_uptime_secs: f64,
}

/// A fully assembled bug report ready to write to disk.
pub struct BugReport {
    /// Serialised metadata.json content.
    metadata_json: String,
    /// Serialised session.json (AppSnapshot).
    session_json: String,
    /// Serialised channel-state.json.
    channel_state_json: String,
    /// Pcap bytes (None when capture feature disabled).
    pcap_bytes: Option<Vec<u8>>,
    /// PNG screenshot bytes (display reports only).
    screenshot_png: Option<Vec<u8>>,
}

impl BugReport {
    /// Assemble a bug report from the available data.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        report_type: BugReportType,
        description: String,
        region: Option<ReportRegion>,
        target_host: &str,
        target_port: u16,
        traffic: &TrafficBuffers,
        channel_snapshots: &ChannelSnapshots,
        app_snapshot: &Mutex<AppSnapshot>,
        surface_pixels: Option<(&[u8], u32, u32)>,
    ) -> anyhow::Result<Self> {
        // 1. Session snapshot (AppSnapshot)
        let mut session = app_snapshot.lock().unwrap().clone();
        session.uptime_secs = traffic.elapsed().as_secs_f64();
        let session_json = serde_json::to_string_pretty(&session)?;

        // 2. Channel state snapshot
        let channel_state_json = match report_type {
            BugReportType::Display => {
                let snap = channel_snapshots.display.lock().unwrap().clone();
                serde_json::to_string_pretty(&snap)?
            }
            BugReportType::Input => {
                let snap = channel_snapshots.inputs.lock().unwrap().clone();
                serde_json::to_string_pretty(&snap)?
            }
            BugReportType::Cursor => {
                let snap = channel_snapshots.cursor.lock().unwrap().clone();
                serde_json::to_string_pretty(&snap)?
            }
            BugReportType::Connection => {
                let snap = channel_snapshots.main.lock().unwrap().clone();
                serde_json::to_string_pretty(&snap)?
            }
            BugReportType::Usb => {
                // No dedicated usbredir snapshot yet; pcap traffic is captured via channel_name()
                "{}".to_string()
            }
        };

        // 3. Pcap traffic for the affected channel
        let pcap_bytes = traffic.drain_channel_pcap_bytes(report_type.channel_name());

        // 4. PNG screenshot (display reports only)
        let screenshot_png = if report_type == BugReportType::Display {
            if let Some((pixels, w, h)) = surface_pixels {
                Some(encode_png(pixels, w, h)?)
            } else {
                None
            }
        } else {
            None
        };

        // 5. Report metadata
        let metadata = ReportMetadata {
            ryll_version: env!("CARGO_PKG_VERSION").to_string(),
            platform_os: std::env::consts::OS.to_string(),
            platform_arch: std::env::consts::ARCH.to_string(),
            report_type,
            channel: report_type.channel_name().to_string(),
            description,
            region,
            timestamp: chrono_now(),
            target_host: target_host.to_string(),
            target_port,
            session_uptime_secs: session.uptime_secs,
        };
        let metadata_json = serde_json::to_string_pretty(&metadata)?;

        Ok(BugReport {
            metadata_json,
            session_json,
            channel_state_json,
            pcap_bytes,
            screenshot_png,
        })
    }

    /// Write the bug report as a zip file to `dir`.
    /// Creates `dir` if it does not exist.
    /// Returns the path of the written file.
    pub fn write_zip(&self, dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        std::fs::create_dir_all(dir)?;

        let filename = format!("ryll-bugreport-{}.zip", filename_timestamp());
        let path = dir.join(&filename);
        let file = std::fs::File::create(&path)?;
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

        zip.start_file("metadata.json", opts)?;
        zip.write_all(self.metadata_json.as_bytes())?;

        zip.start_file("session.json", opts)?;
        zip.write_all(self.session_json.as_bytes())?;

        zip.start_file("channel-state.json", opts)?;
        zip.write_all(self.channel_state_json.as_bytes())?;

        if let Some(ref pcap) = self.pcap_bytes {
            zip.start_file("traffic.pcap", opts)?;
            zip.write_all(pcap)?;
        }

        if let Some(ref png) = self.screenshot_png {
            zip.start_file("screenshot.png", opts)?;
            zip.write_all(png)?;
        }

        zip.finish()?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_push_and_evict() {
        let mut rb = TrafficRingBuffer::new(100);

        // Push entries totalling 60 bytes of pcap frames
        for i in 0..3 {
            rb.push(TrafficEntry {
                timestamp: Duration::from_millis(i * 100),
                channel: "test",
                direction: TrafficDirection::Received,
                message_type: i as u16,
                message_name: "test",
                wire_size: 20,
                payload_size: 14,
                pcap_frame: vec![0u8; 20],
            });
        }
        assert_eq!(rb.len(), 3);
        assert_eq!(rb.total_bytes(), 60);

        // Push more to exceed the 100-byte cap
        for i in 3..8 {
            rb.push(TrafficEntry {
                timestamp: Duration::from_millis(i * 100),
                channel: "test",
                direction: TrafficDirection::Sent,
                message_type: i as u16,
                message_name: "test",
                wire_size: 20,
                payload_size: 14,
                pcap_frame: vec![0u8; 20],
            });
        }

        // Should have evicted oldest entries to stay under 100 bytes
        assert!(rb.total_bytes() <= 100);
        assert!(rb.len() <= 5);

        // Oldest remaining entry should have a later timestamp
        let first = rb.entries().front().unwrap();
        assert!(first.timestamp >= Duration::from_millis(300));
    }

    #[test]
    fn test_traffic_buffers_record() {
        let buffers = TrafficBuffers::new();

        // Record some messages
        buffers.record_sent("main", 101, "key_down", &[0u8; 10]);
        buffers.record_received("display", 302, "draw_copy", &[0u8; 100]);
        buffers.record_sent("inputs", 101, "key_down", &[0u8; 10]);

        // Check entries are in the right buffers
        let main = buffers.main.lock().unwrap();
        assert_eq!(main.len(), 1);
        assert_eq!(main.entries().front().unwrap().message_name, "key_down");

        let display = buffers.display.lock().unwrap();
        assert_eq!(display.len(), 1);

        let inputs = buffers.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 1);

        let cursor = buffers.cursor.lock().unwrap();
        assert_eq!(cursor.len(), 0);
    }

    #[test]
    fn test_traffic_buffers_unknown_channel_ignored() {
        let buffers = TrafficBuffers::new();
        buffers.record_sent("nonexistent", 1, "test", &[0u8; 10]);
        // Should not panic
    }

    #[test]
    fn test_display_snapshot_serialises() {
        let mut snap = DisplaySnapshot {
            image_cache_entries: 3,
            image_cache_ids: vec![1, 2, 3],
            image_cache_bytes: 12345,
            bytes_in: 100_000,
            ..Default::default()
        };
        snap.recent_decodes.push_back(DecodeResult {
            image_type: "GlzRgb".to_string(),
            image_id: 42,
            width: 800,
            height: 600,
            from_cache: false,
            success: true,
            timestamp_secs: 1.5,
        });
        let json = serde_json::to_string_pretty(&snap).unwrap();
        assert!(json.contains("\"image_cache_entries\": 3"));
        assert!(json.contains("\"image_type\": \"GlzRgb\""));
        assert!(json.contains("\"bytes_in\": 100000"));
    }

    #[test]
    fn test_inputs_snapshot_serialises() {
        let mut snap = InputsSnapshot {
            button_state: 1,
            ..Default::default()
        };
        snap.recent_events.push_back(InputEventRecord {
            event_type: "KeyDown".to_string(),
            scancode: 0x1E,
            x: 0,
            y: 0,
            button_mask: 0,
            timestamp_secs: 2.0,
        });
        let json = serde_json::to_string_pretty(&snap).unwrap();
        assert!(json.contains("\"button_state\": 1"));
        assert!(json.contains("\"event_type\": \"KeyDown\""));
    }

    #[test]
    fn test_cursor_snapshot_serialises() {
        let mut snap = CursorSnapshot {
            cache_entries: 1,
            ..Default::default()
        };
        snap.cache_contents.push(CursorCacheEntry {
            cursor_id: 99,
            width: 24,
            height: 24,
            hot_spot_x: 0,
            hot_spot_y: 0,
        });
        let json = serde_json::to_string_pretty(&snap).unwrap();
        assert!(json.contains("\"cursor_id\": 99"));
    }

    #[test]
    fn test_main_snapshot_serialises() {
        let snap = MainSnapshot {
            session_id: Some(42),
            bytes_in: 500,
            bytes_out: 100,
        };
        let json = serde_json::to_string_pretty(&snap).unwrap();
        assert!(json.contains("\"session_id\": 42"));
    }

    #[test]
    fn test_app_snapshot_serialises() {
        let mut snap = AppSnapshot {
            fps: 59.9,
            connected: true,
            ..Default::default()
        };
        snap.surfaces.push(SurfaceInfo {
            surface_id: 0,
            width: 1920,
            height: 1080,
        });
        let json = serde_json::to_string_pretty(&snap).unwrap();
        assert!(json.contains("\"fps\": 59.9"));
        assert!(json.contains("\"connected\": true"));
        assert!(json.contains("\"surface_id\": 0"));
    }

    #[test]
    fn test_channel_snapshots_new() {
        let snapshots = ChannelSnapshots::new();
        let display = snapshots.display.lock().unwrap();
        assert_eq!(display.bytes_in, 0);
        assert_eq!(display.recent_decodes.len(), 0);
    }

    #[test]
    fn test_chrono_now_format() {
        let ts = chrono_now();
        // Should match YYYY-MM-DDTHH:MM:SSZ pattern
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    #[test]
    fn test_filename_timestamp() {
        let ts = filename_timestamp();
        assert!(!ts.contains(':'));
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn test_encode_png() {
        // 2x2 red RGBA image
        let pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let png_bytes = encode_png(&pixels, 2, 2).unwrap();
        // PNG magic bytes
        assert_eq!(&png_bytes[..4], b"\x89PNG");
        assert!(png_bytes.len() > 20);
    }

    #[test]
    fn test_report_metadata_serialises() {
        let meta = ReportMetadata {
            ryll_version: "0.1.0".to_string(),
            platform_os: "linux".to_string(),
            platform_arch: "x86_64".to_string(),
            report_type: BugReportType::Display,
            channel: "display".to_string(),
            description: "test bug".to_string(),
            region: Some(ReportRegion {
                left: 10,
                top: 20,
                right: 200,
                bottom: 150,
            }),
            timestamp: "2026-04-03T12:34:56Z".to_string(),
            target_host: "192.168.1.100".to_string(),
            target_port: 5900,
            session_uptime_secs: 45.3,
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        assert!(json.contains("\"report_type\": \"Display\""));
        assert!(json.contains("\"channel\": \"display\""));
        assert!(json.contains("\"description\": \"test bug\""));
        assert!(json.contains("\"left\": 10"));
    }

    #[test]
    fn test_bug_report_assemble_display() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());

        // 2x2 red RGBA pixels
        let pixels = vec![
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];

        let report = BugReport::new(
            BugReportType::Display,
            "corruption test".to_string(),
            Some(ReportRegion {
                left: 0,
                top: 0,
                right: 2,
                bottom: 2,
            }),
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            Some((&pixels, 2, 2)),
        )
        .unwrap();

        // Write zip to a temp directory
        let tmp = std::env::temp_dir().join("ryll-test-bugreport-display");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = report.write_zip(&tmp).unwrap();
        assert!(path.exists());

        // Open and verify contents
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"metadata.json".to_string()));
        assert!(names.contains(&"session.json".to_string()));
        assert!(names.contains(&"channel-state.json".to_string()));
        assert!(names.contains(&"screenshot.png".to_string()));

        // Verify metadata contains expected fields
        {
            let mut meta_file = archive.by_name("metadata.json").unwrap();
            let mut meta_str = String::new();
            std::io::Read::read_to_string(&mut meta_file, &mut meta_str).unwrap();
            assert!(meta_str.contains("\"report_type\": \"Display\""));
            assert!(meta_str.contains("\"description\": \"corruption test\""));
        }

        // Verify screenshot starts with PNG magic
        {
            let mut png_file = archive.by_name("screenshot.png").unwrap();
            let mut png_bytes = Vec::new();
            std::io::Read::read_to_end(&mut png_file, &mut png_bytes).unwrap();
            assert_eq!(&png_bytes[..4], b"\x89PNG");
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_bug_report_assemble_input() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());

        let report = BugReport::new(
            BugReportType::Input,
            "keyboard not working".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            None,
        )
        .unwrap();

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-input");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = report.write_zip(&tmp).unwrap();
        assert!(path.exists());

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"metadata.json".to_string()));
        assert!(names.contains(&"session.json".to_string()));
        assert!(names.contains(&"channel-state.json".to_string()));
        // No screenshot for input reports
        assert!(!names.contains(&"screenshot.png".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_bug_report_type_channel_name() {
        assert_eq!(BugReportType::Display.channel_name(), "display");
        assert_eq!(BugReportType::Input.channel_name(), "inputs");
        assert_eq!(BugReportType::Cursor.channel_name(), "cursor");
        assert_eq!(BugReportType::Connection.channel_name(), "main");
    }

    #[test]
    fn test_recent_view_entries() {
        let buffers = TrafficBuffers::new();

        // Push entries to multiple channels
        buffers.record_sent("main", 101, "attach_channels", &[0u8; 10]);
        buffers.record_received("display", 302, "draw_copy", &[0u8; 100]);
        buffers.record_sent("inputs", 101, "key_down", &[0u8; 10]);
        buffers.record_received("cursor", 401, "cursor_set", &[0u8; 50]);

        let entries = buffers.recent_view_entries(100);
        assert_eq!(entries.len(), 4);
        // Entries should be sorted by timestamp
        for w in entries.windows(2) {
            assert!(w[0].timestamp <= w[1].timestamp);
        }

        // Verify max limit
        let limited = buffers.recent_view_entries(2);
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn test_bug_report_assemble_cursor() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());

        let report = BugReport::new(
            BugReportType::Cursor,
            "cursor disappeared".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            None,
        )
        .unwrap();

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-cursor");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = report.write_zip(&tmp).unwrap();
        assert!(path.exists());

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"metadata.json".to_string()));
        assert!(names.contains(&"channel-state.json".to_string()));
        assert!(!names.contains(&"screenshot.png".to_string()));

        {
            let mut meta_file = archive.by_name("metadata.json").unwrap();
            let mut meta_str = String::new();
            std::io::Read::read_to_string(&mut meta_file, &mut meta_str).unwrap();
            assert!(meta_str.contains("\"report_type\": \"Cursor\""));
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_bug_report_assemble_connection() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());

        let report = BugReport::new(
            BugReportType::Connection,
            "session dropped".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            None,
        )
        .unwrap();

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-connection");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = report.write_zip(&tmp).unwrap();
        assert!(path.exists());

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"metadata.json".to_string()));
        assert!(names.contains(&"channel-state.json".to_string()));
        assert!(!names.contains(&"screenshot.png".to_string()));

        {
            let mut meta_file = archive.by_name("metadata.json").unwrap();
            let mut meta_str = String::new();
            std::io::Read::read_to_string(&mut meta_file, &mut meta_str).unwrap();
            assert!(meta_str.contains("\"report_type\": \"Connection\""));
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_drain_unknown_channel() {
        let buffers = TrafficBuffers::new();
        assert!(buffers.drain_channel_pcap_bytes("nonexistent").is_none());
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0");
        assert_eq!(format_size(500), "500");
        assert_eq!(format_size(9999), "9999");
        assert_eq!(format_size(15000), "15.0K");
        assert_eq!(format_size(2_500_000), "2.5M");
    }

    #[test]
    fn test_encode_png_1x1() {
        let pixels = vec![128u8, 64, 32, 255];
        let png_bytes = encode_png(&pixels, 1, 1).unwrap();
        assert_eq!(&png_bytes[..4], b"\x89PNG");
    }
}
