/// Bug report infrastructure — per-channel traffic ring buffer
/// and bug-report ZIP assembly.
///
/// Always active regardless of `--capture`. Retains the most
/// recent protocol traffic for bug-report export and the live
/// traffic viewer. Channel-state snapshot types live in
/// `shakenfist_spice_renderer::snapshots`; this module re-exports
/// them so callers in `ryll/` can keep importing them from
/// `crate::bugreport::*`.
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::debug;

#[cfg(feature = "capture")]
use crate::capture;
use crate::notifications::{NotificationStore, SharedNotifications};
use shakenfist_spice_renderer::metrics::RuntimeMetrics;
use shakenfist_spice_renderer::traffic::TrafficSink;

// Re-export channel-state snapshot types for ryll-side callers
// (e.g. tests in this file and consumers under `app.rs`).
#[allow(unused_imports)]
pub use shakenfist_spice_renderer::snapshots::{
    ChannelSnapshots, CursorCacheEntry, CursorSnapshot, DecodeResult, DisplaySnapshot,
    InputEventRecord, InputsSnapshot, MainSnapshot,
};

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
const PER_CHANNEL_BYTES: usize = 50 * 1024 * 1024 / 6;

/// Known channel names.
const CHANNELS: [&str; 6] = [
    "main", "display", "inputs", "cursor", "usbredir", "playback",
];

/// Holds all four per-channel ring buffers plus a shared session
/// start timestamp.
pub struct TrafficBuffers {
    main: Mutex<TrafficRingBuffer>,
    display: Mutex<TrafficRingBuffer>,
    inputs: Mutex<TrafficRingBuffer>,
    cursor: Mutex<TrafficRingBuffer>,
    usbredir: Mutex<TrafficRingBuffer>,
    playback: Mutex<TrafficRingBuffer>,
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
            playback: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
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
            "playback" => Some(&self.playback),
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

        let mut guard = match buf.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
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

/// Bridge `TrafficBuffers` into the renderer's `TrafficSink`
/// trait so that channel handlers can receive it as
/// `Arc<dyn TrafficSink>` without taking a concrete dependency
/// on this module.
impl TrafficSink for TrafficBuffers {
    fn record_sent(
        &self,
        channel: &'static str,
        msg_type: u16,
        msg_name: &'static str,
        raw: &[u8],
    ) {
        TrafficBuffers::record_sent(self, channel, msg_type, msg_name, raw);
    }

    fn record_received(
        &self,
        channel: &'static str,
        msg_type: u16,
        msg_name: &'static str,
        raw: &[u8],
    ) {
        TrafficBuffers::record_received(self, channel, msg_type, msg_name, raw);
    }

    fn elapsed(&self) -> Duration {
        TrafficBuffers::elapsed(self)
    }
}

// ── Channel state snapshots ─────────────────────────────────
//
// `*Snapshot`, `CursorCacheEntry`, `InputEventRecord`, and
// `DecodeResult` moved to `shakenfist_spice_renderer::snapshots`
// because they describe channel state rather than bug-report
// packaging. They are re-exported above as a transitional
// convenience.

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
    /// Most recent inter-PING interval observed on the main
    /// channel, in milliseconds.  `None` until the second PING
    /// arrives.
    pub last_latency_ms: Option<f64>,
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
            last_latency_ms: None,
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

// `ChannelSnapshots` moved to `shakenfist_spice_renderer::snapshots`.

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

/// Extract the RGBA sub-rectangle `region` from a
/// `width × height` RGBA buffer and PNG-encode it.
///
/// `region` is clamped to the surface bounds. Returns
/// `Ok(None)` when the clamped rectangle is empty (e.g. a
/// zero-width click, or a region entirely outside the
/// surface after clamping). Returns `Err` only on PNG
/// encoder failure.
pub(crate) fn encode_region_png(
    pixels: &[u8],
    width: u32,
    height: u32,
    region: &ReportRegion,
) -> anyhow::Result<Option<Vec<u8>>> {
    // Reject obviously-unsafe dimensions before any arithmetic.
    // `DisplaySurface` already clamps at construction, so every
    // live caller passes values within bounds — this guard is
    // insurance against a future caller that skips the surface
    // layer and defends `(width as usize) * 4` from overflow on
    // 32-bit targets.
    if width > shakenfist_spice_renderer::display::MAX_SURFACE_DIMENSION
        || height > shakenfist_spice_renderer::display::MAX_SURFACE_DIMENSION
    {
        return Ok(None);
    }
    let left = region.left.min(width);
    let top = region.top.min(height);
    let right = region.right.min(width);
    let bottom = region.bottom.min(height);
    if right <= left || bottom <= top {
        return Ok(None);
    }
    let crop_w = (right - left) as usize;
    let crop_h = (bottom - top) as usize;

    let src_stride = (width as usize) * 4;
    let dst_stride = crop_w * 4;
    // Defensive: caller should always pass a correctly-sized
    // buffer, but skip silently rather than indexing out of
    // range if they don't.
    if pixels.len() < src_stride * (height as usize) {
        return Ok(None);
    }

    let mut out = vec![0u8; dst_stride * crop_h];
    for row in 0..crop_h {
        let src_y = (top as usize) + row;
        let src_start = src_y * src_stride + (left as usize) * 4;
        let dst_start = row * dst_stride;
        out[dst_start..dst_start + dst_stride]
            .copy_from_slice(&pixels[src_start..src_start + dst_stride]);
    }

    Ok(Some(encode_png(&out, crop_w as u32, crop_h as u32)?))
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

/// Configuration bundle passed from main.rs into the
/// app constructors when --pedantic is enabled.
#[derive(Debug, Clone)]
pub(crate) struct PedanticConfig {
    pub dir: std::path::PathBuf,
}

/// Cap on the number of pedantic reports per session.
/// Prevents disk-fill if the dedupe key set explodes for
/// some reason.
pub(crate) const PEDANTIC_REPORT_CAP: usize = 50;

/// Which channel the bug report is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum BugReportType {
    Display,
    Input,
    Cursor,
    Connection,
    Usb,
    /// Auto-generated by --pedantic mode on first-seen
    /// gap.  `gap_key` is the warn_once key string from
    /// the registry (e.g. "display:unimpl:draw_rop3").
    Pedantic {
        gap_key: String,
    },
    /// Auto-generated when ryll observes a channel disconnect
    /// (transport error, EOF, or its own keepalive timeout).
    /// `channel` is the channel name that fired the disconnect
    /// signal, or "error" for `ChannelEvent::Error` paths
    /// without a specific channel attribution.
    Disconnect {
        channel: String,
    },
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
            BugReportType::Pedantic { gap_key } => match gap_key.split(':').next() {
                Some("display") => "display",
                Some("cursor") => "cursor",
                Some("inputs") => "inputs",
                Some("main") => "main",
                Some("usbredir") => "usbredir",
                _ => "display",
            },
            BugReportType::Disconnect { channel } => match channel.as_str() {
                "main" => "main",
                "display" => "display",
                "inputs" => "inputs",
                "cursor" => "cursor",
                "playback" => "playback",
                "usbredir" => "usbredir",
                "webdav" => "webdav",
                _ => "main",
            },
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

/// Per-channel diagnostic snapshot embedded in
/// `disconnect-cause.json`. Lets a maintainer reading the zip
/// compare the channel that fired the disconnect against the
/// other channels' last-known state — were they all silent at
/// the same time, or just the one that dropped?
#[derive(Debug, Clone, Default, Serialize)]
pub struct PerChannelDiagnostics {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub last_recv_ts_secs: Option<f64>,
    pub last_send_ts_secs: Option<f64>,
    pub ping_recv_count: u32,
    pub pong_send_count: u32,
    pub last_ping_recv_ts_secs: Option<f64>,
    /// Idle keepalives this client sent on this channel
    /// (Phase 02 K1 fix). Today only the inputs channel sends
    /// these; the field is present on every entry for
    /// uniform JSON shape, with 0 / None where unimplemented.
    pub client_keepalive_send_count: u32,
    pub last_client_keepalive_send_ts_secs: Option<f64>,
}

/// Structured cause record for an auto-disconnect bug report.
/// Goal: a maintainer reading the resulting zip should be able
/// to tell which side dropped the connection and what each
/// channel was doing at the moment of failure, without having
/// to re-run the session.
#[derive(Debug, Clone, Serialize)]
pub struct DisconnectCause {
    /// Channel name that fired the disconnect signal, or
    /// "error" for `ChannelEvent::Error` without a specific
    /// channel.
    pub channel: String,
    /// Free-form cause / reason captured at the disconnect
    /// site (the `info!`/`error!` log line text).
    pub error_message: String,
    /// `std::io::ErrorKind` debug string when known. None for
    /// EOF / clean close / our own keepalive timeout.
    pub error_kind: Option<String>,
    /// True if the main-channel client-side keepalive timeout
    /// (`main_channel.rs`, 30 s) fired. Distinguishes "we
    /// timed ourselves out" from a real EOF/RST.
    pub keepalive_timeout_fired: bool,
    /// Session uptime at the moment of failure.
    pub session_uptime_secs: f64,
    /// Per-channel last-known state. Keys are channel names
    /// ("main", "display", "inputs", "cursor", "playback",
    /// "usbredir", "webdav").
    pub per_channel: BTreeMap<String, PerChannelDiagnostics>,
}

impl DisconnectCause {
    /// Snapshot every channel's last-known diagnostic state into
    /// a `BTreeMap` keyed by channel name. Used by the
    /// disconnect-snapshot hook to populate
    /// `DisconnectCause::per_channel`.
    pub fn collect_per_channel(
        snapshots: &ChannelSnapshots,
    ) -> BTreeMap<String, PerChannelDiagnostics> {
        let mut out = BTreeMap::new();
        if let Ok(s) = snapshots.main.lock() {
            out.insert(
                "main".to_string(),
                PerChannelDiagnostics {
                    bytes_in: s.bytes_in,
                    bytes_out: s.bytes_out,
                    last_recv_ts_secs: s.last_recv_ts_secs,
                    last_send_ts_secs: s.last_send_ts_secs,
                    ping_recv_count: s.ping_recv_count,
                    pong_send_count: s.pong_send_count,
                    last_ping_recv_ts_secs: s.last_ping_recv_ts_secs,
                    client_keepalive_send_count: 0,
                    last_client_keepalive_send_ts_secs: None,
                },
            );
        }
        if let Ok(s) = snapshots.display.lock() {
            out.insert(
                "display".to_string(),
                PerChannelDiagnostics {
                    bytes_in: s.bytes_in,
                    bytes_out: s.bytes_out,
                    last_recv_ts_secs: s.last_recv_ts_secs,
                    last_send_ts_secs: s.last_send_ts_secs,
                    ping_recv_count: s.ping_recv_count,
                    pong_send_count: s.pong_send_count,
                    last_ping_recv_ts_secs: s.last_ping_recv_ts_secs,
                    client_keepalive_send_count: 0,
                    last_client_keepalive_send_ts_secs: None,
                },
            );
        }
        if let Ok(s) = snapshots.inputs.lock() {
            out.insert(
                "inputs".to_string(),
                PerChannelDiagnostics {
                    bytes_in: s.bytes_in,
                    bytes_out: s.bytes_out,
                    last_recv_ts_secs: s.last_recv_ts_secs,
                    last_send_ts_secs: s.last_send_ts_secs,
                    ping_recv_count: s.ping_recv_count,
                    pong_send_count: s.pong_send_count,
                    last_ping_recv_ts_secs: s.last_ping_recv_ts_secs,
                    client_keepalive_send_count: s.client_keepalive_send_count,
                    last_client_keepalive_send_ts_secs: s.last_client_keepalive_send_ts_secs,
                },
            );
        }
        if let Ok(s) = snapshots.cursor.lock() {
            out.insert(
                "cursor".to_string(),
                PerChannelDiagnostics {
                    bytes_in: s.bytes_in,
                    bytes_out: s.bytes_out,
                    last_recv_ts_secs: s.last_recv_ts_secs,
                    last_send_ts_secs: s.last_send_ts_secs,
                    ping_recv_count: s.ping_recv_count,
                    pong_send_count: s.pong_send_count,
                    last_ping_recv_ts_secs: s.last_ping_recv_ts_secs,
                    client_keepalive_send_count: 0,
                    last_client_keepalive_send_ts_secs: None,
                },
            );
        }
        if let Ok(s) = snapshots.playback.lock() {
            out.insert(
                "playback".to_string(),
                PerChannelDiagnostics {
                    bytes_in: s.bytes_in,
                    bytes_out: s.bytes_out,
                    last_recv_ts_secs: s.last_recv_ts_secs,
                    last_send_ts_secs: s.last_send_ts_secs,
                    ping_recv_count: s.ping_recv_count,
                    pong_send_count: s.pong_send_count,
                    last_ping_recv_ts_secs: s.last_ping_recv_ts_secs,
                    client_keepalive_send_count: 0,
                    last_client_keepalive_send_ts_secs: None,
                },
            );
        }
        if let Ok(s) = snapshots.usbredir.lock() {
            out.insert(
                "usbredir".to_string(),
                PerChannelDiagnostics {
                    bytes_in: s.bytes_in,
                    bytes_out: s.bytes_out,
                    last_recv_ts_secs: s.last_recv_ts_secs,
                    last_send_ts_secs: s.last_send_ts_secs,
                    ping_recv_count: s.ping_recv_count,
                    pong_send_count: s.pong_send_count,
                    last_ping_recv_ts_secs: s.last_ping_recv_ts_secs,
                    client_keepalive_send_count: 0,
                    last_client_keepalive_send_ts_secs: None,
                },
            );
        }
        if let Ok(s) = snapshots.webdav.lock() {
            out.insert(
                "webdav".to_string(),
                PerChannelDiagnostics {
                    bytes_in: s.bytes_in,
                    bytes_out: s.bytes_out,
                    last_recv_ts_secs: s.last_recv_ts_secs,
                    last_send_ts_secs: s.last_send_ts_secs,
                    ping_recv_count: s.ping_recv_count,
                    pong_send_count: s.pong_send_count,
                    last_ping_recv_ts_secs: s.last_ping_recv_ts_secs,
                    client_keepalive_send_count: 0,
                    last_client_keepalive_send_ts_secs: None,
                },
            );
        }
        out
    }
}

/// Captured at the moment a bug-report dialog opened, or at
/// the moment a --pedantic observer fired. Threaded through
/// `BugReport::new` / `BugReport::write_pedantic` so the
/// submitted zip can record when the user *saw* the bug in
/// addition to when they *submitted* the report.
///
/// Phase 1 of the trigger-snapshot work plumbs this through
/// with every caller passing `None` (which falls back to
/// submit time); phase 2 wires up real trigger-time capture
/// from the app.
#[derive(Debug, Clone)]
pub struct TriggerTimestamps {
    /// ISO 8601 UTC timestamp (same format as
    /// `ReportMetadata::timestamp`).
    pub triggered_at: String,
    /// Session uptime in seconds at the moment of trigger.
    pub triggered_uptime_secs: f64,
}

/// Top-level metadata written to metadata.json.
#[derive(Debug, Clone, Serialize)]
pub struct ReportMetadata {
    pub ryll_version: String,
    /// Short git SHA of the build (with `-dirty` suffix when the
    /// working tree had uncommitted changes at build time).
    /// Populated from `env!("RYLL_GIT_SHA")` at compile time;
    /// falls back to "unknown" when the build environment can't
    /// reach git. Lets a maintainer reading a bug-report zip
    /// confirm exactly which commit produced the binary.
    pub ryll_git_sha: String,
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
    /// ISO 8601 UTC timestamp of when the user opened the
    /// bug-report dialog (or when the --pedantic observer
    /// fired). Equal to `timestamp` when the caller did not
    /// supply an explicit trigger.
    pub triggered_at: String,
    /// Session uptime in seconds at the moment of trigger.
    /// Equal to `session_uptime_secs` when no explicit
    /// trigger was supplied.
    pub triggered_uptime_secs: f64,
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
    /// PNG bytes for `screenshot-region.png`: a crop of the
    /// submit-time surface at `ReportMetadata::region`. `None`
    /// when the report isn't Display, no region was selected,
    /// or the clamped region is degenerate. Deliberately
    /// cropped from the submit-time surface rather than the
    /// trigger-time PNG — the two images represent different
    /// moments and the trigger surface may have been a
    /// different size.
    screenshot_region_png: Option<Vec<u8>>,
    /// Runtime process and per-thread CPU metrics.
    runtime_metrics: RuntimeMetrics,
    /// Pretty-printed notifications.json content (Vec<NotificationEntry>).
    notifications_json: String,
}

impl BugReport {
    /// Assemble a bug report from the available data.
    ///
    /// Note: this function samples runtime metrics over a 2-second
    /// window before assembling the rest of the report.  The sample
    /// blocks the calling thread, which is acceptable because bug
    /// report saving is already a deliberate, non-interactive
    /// operation gated on a file dialog.
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
        notifications: &Mutex<NotificationStore>,
        surface_pixels: Option<(&[u8], u32, u32)>,
        trigger: Option<TriggerTimestamps>,
        precomputed_screenshot_png: Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        // 0. Sample runtime metrics first (blocks for 2 seconds).
        //    This runs before the rest of the report is assembled so
        //    the CPU% numbers reflect the system state at the moment
        //    the user triggered the report, not after all the JSON
        //    serialisation work has run.
        let runtime_metrics = shakenfist_spice_renderer::metrics::sample(Duration::from_secs(2));
        Self::assemble(
            report_type,
            description,
            region,
            target_host,
            target_port,
            traffic,
            channel_snapshots,
            app_snapshot,
            notifications,
            surface_pixels,
            runtime_metrics,
            trigger,
            precomputed_screenshot_png,
        )
    }

    /// Assemble a bug report with a caller-supplied `RuntimeMetrics`.
    ///
    /// This is the inner implementation used by both `new()` (which
    /// samples real metrics) and by tests (which inject a stub to
    /// avoid a 2-second sleep).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn assemble(
        report_type: BugReportType,
        description: String,
        region: Option<ReportRegion>,
        target_host: &str,
        target_port: u16,
        traffic: &TrafficBuffers,
        channel_snapshots: &ChannelSnapshots,
        app_snapshot: &Mutex<AppSnapshot>,
        notifications: &Mutex<NotificationStore>,
        surface_pixels: Option<(&[u8], u32, u32)>,
        runtime_metrics: RuntimeMetrics,
        trigger: Option<TriggerTimestamps>,
        precomputed_screenshot_png: Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        // 1. Session snapshot (AppSnapshot)
        let mut session = app_snapshot.lock().unwrap().clone();
        session.uptime_secs = traffic.elapsed().as_secs_f64();
        let session_json = serde_json::to_string_pretty(&session)?;

        // 2. Channel state snapshot
        let channel_state_json = match &report_type {
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
            BugReportType::Pedantic { .. } | BugReportType::Disconnect { .. } => {
                // Pick the snapshot based on the channel the gap / disconnect
                // came from. For unknown channels we picked "main" or
                // "display" as the fallback in channel_name(), so this match
                // mirrors that.
                match report_type.channel_name() {
                    "cursor" => {
                        let snap = channel_snapshots.cursor.lock().unwrap().clone();
                        serde_json::to_string_pretty(&snap)?
                    }
                    "inputs" => {
                        let snap = channel_snapshots.inputs.lock().unwrap().clone();
                        serde_json::to_string_pretty(&snap)?
                    }
                    "main" => {
                        let snap = channel_snapshots.main.lock().unwrap().clone();
                        serde_json::to_string_pretty(&snap)?
                    }
                    "playback" => {
                        let snap = channel_snapshots.playback.lock().unwrap().clone();
                        serde_json::to_string_pretty(&snap)?
                    }
                    "usbredir" => {
                        let snap = channel_snapshots.usbredir.lock().unwrap().clone();
                        serde_json::to_string_pretty(&snap)?
                    }
                    "webdav" => {
                        let snap = channel_snapshots.webdav.lock().unwrap().clone();
                        serde_json::to_string_pretty(&snap)?
                    }
                    _ => {
                        let snap = channel_snapshots.display.lock().unwrap().clone();
                        serde_json::to_string_pretty(&snap)?
                    }
                }
            }
        };

        // 3. Notifications snapshot
        let notifications_snapshot = notifications
            .lock()
            .map(|s| s.snapshot())
            .unwrap_or_default();
        let notifications_json = serde_json::to_string_pretty(&notifications_snapshot)?;

        // 4. Pcap traffic for the affected channel
        let channel_name = report_type.channel_name();
        let pcap_bytes = traffic.drain_channel_pcap_bytes(channel_name);

        // 5. PNG screenshot (display reports only). Prefer a
        //    precomputed PNG from the trigger-time encoder thread
        //    over re-encoding the live surface at submit time; fall
        //    back to a live encode when the background thread was
        //    never spawned (e.g. pedantic observer path) or hasn't
        //    produced bytes yet.
        //
        //    Non-Display submissions drop the precomputed PNG on
        //    the floor. The policy is to always *capture* on dialog
        //    open and only *include* the PNG when the user actually
        //    submits a Display report.
        let screenshot_png = if report_type == BugReportType::Display {
            if let Some(bytes) = precomputed_screenshot_png {
                Some(bytes)
            } else if let Some((pixels, w, h)) = surface_pixels {
                Some(encode_png(pixels, w, h)?)
            } else {
                None
            }
        } else {
            None
        };

        // 5b. Region crop PNG (Display reports with a non-empty
        //     region only). Cropped from the submit-time surface
        //     pixels, deliberately *not* from the precomputed
        //     trigger PNG — the trigger surface may have been a
        //     different size, and the region coordinates are in
        //     submit-time surface space.
        let screenshot_region_png = if report_type == BugReportType::Display {
            match (&region, surface_pixels) {
                (Some(r), Some((pixels, w, h))) => encode_region_png(pixels, w, h, r)?,
                _ => None,
            }
        } else {
            None
        };

        // 6. Report metadata. When the caller did not supply explicit
        //    trigger timestamps, substitute the submit-time values so
        //    downstream tooling can treat the fields as always present.
        //    Single `chrono_now()` call means the fallback string is
        //    byte-identical to `timestamp`.
        let submit_iso = chrono_now();
        let submit_uptime = session.uptime_secs;
        let (triggered_at, triggered_uptime_secs) = match trigger {
            Some(t) => (t.triggered_at, t.triggered_uptime_secs),
            None => (submit_iso.clone(), submit_uptime),
        };
        let metadata = ReportMetadata {
            ryll_version: env!("CARGO_PKG_VERSION").to_string(),
            ryll_git_sha: env!("RYLL_GIT_SHA").to_string(),
            platform_os: std::env::consts::OS.to_string(),
            platform_arch: std::env::consts::ARCH.to_string(),
            channel: channel_name.to_string(),
            report_type,
            description,
            region,
            timestamp: submit_iso,
            target_host: target_host.to_string(),
            target_port,
            session_uptime_secs: submit_uptime,
            triggered_at,
            triggered_uptime_secs,
        };
        let metadata_json = serde_json::to_string_pretty(&metadata)?;

        Ok(BugReport {
            metadata_json,
            session_json,
            channel_state_json,
            pcap_bytes,
            screenshot_png,
            screenshot_region_png,
            runtime_metrics,
            notifications_json,
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

        if let Some(ref png) = self.screenshot_region_png {
            zip.start_file("screenshot-region.png", opts)?;
            zip.write_all(png)?;
        }

        zip.start_file("notifications.json", opts)?;
        zip.write_all(self.notifications_json.as_bytes())?;

        let metrics_json = serde_json::to_string_pretty(&self.runtime_metrics)?;
        zip.start_file("runtime-metrics.json", opts)?;
        zip.write_all(metrics_json.as_bytes())?;

        zip.finish()?;
        Ok(path)
    }

    /// Auto-generate a bug report for a just-seen gap, write it to
    /// `dir`, and return the path.  Used by --pedantic mode; shares
    /// all the assemble-and-write plumbing with the F8 flow.
    ///
    /// The output filename encodes the gap_key (colons replaced with
    /// hyphens) so users can identify which gap produced which zip
    /// without opening it:
    /// `ryll-pedantic-display-unimpl-draw_rop3-2026-04-22T...zip`
    #[allow(clippy::too_many_arguments)]
    pub fn write_pedantic(
        dir: &std::path::Path,
        gap_key: &str,
        target_host: &str,
        target_port: u16,
        traffic: &TrafficBuffers,
        channel_snapshots: &ChannelSnapshots,
        app_snapshot: &Mutex<AppSnapshot>,
        notifications: &Mutex<NotificationStore>,
        runtime_metrics: RuntimeMetrics,
        trigger: Option<TriggerTimestamps>,
    ) -> anyhow::Result<std::path::PathBuf> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        // Pedantic reports never carry a precomputed PNG — the
        // observer fires inside a protocol handler, not the GUI,
        // and pedantic reports use the non-Display branch of
        // `BugReportType` anyway.
        let report = Self::assemble(
            BugReportType::Pedantic {
                gap_key: gap_key.to_string(),
            },
            format!("pedantic: {}", gap_key),
            None,
            target_host,
            target_port,
            traffic,
            channel_snapshots,
            app_snapshot,
            notifications,
            None,
            runtime_metrics,
            trigger,
            None,
        )?;

        std::fs::create_dir_all(dir)?;

        // Encode the gap_key into the filename so it's human-readable
        // without opening the zip.  Colons become hyphens; slashes and
        // other characters that are unsafe in filenames become underscores.
        let safe_key = gap_key
            .chars()
            .map(|c| match c {
                ':' | '/' | '\\' | ' ' => '-',
                c if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' => c,
                _ => '_',
            })
            .collect::<String>();
        let filename = format!("ryll-pedantic-{}-{}.zip", safe_key, filename_timestamp());
        let path = dir.join(&filename);
        let file = std::fs::File::create(&path)?;
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

        zip.start_file("metadata.json", opts)?;
        zip.write_all(report.metadata_json.as_bytes())?;

        zip.start_file("session.json", opts)?;
        zip.write_all(report.session_json.as_bytes())?;

        zip.start_file("channel-state.json", opts)?;
        zip.write_all(report.channel_state_json.as_bytes())?;

        if let Some(ref pcap) = report.pcap_bytes {
            zip.start_file("traffic.pcap", opts)?;
            zip.write_all(pcap)?;
        }

        zip.start_file("notifications.json", opts)?;
        zip.write_all(report.notifications_json.as_bytes())?;

        let metrics_json = serde_json::to_string_pretty(&report.runtime_metrics)?;
        zip.start_file("runtime-metrics.json", opts)?;
        zip.write_all(metrics_json.as_bytes())?;

        zip.finish()?;
        Ok(path)
    }

    /// Auto-generate a bug report for a channel disconnect,
    /// write it to `dir`, and return the path. Used by the
    /// app's disconnect-snapshot hook so the next disconnect
    /// captures the run-up to the failure rather than
    /// post-reconnect noise.
    ///
    /// Mirrors `write_pedantic` for the assemble-and-write
    /// plumbing, plus a new `disconnect-cause.json` carrying the
    /// structured `DisconnectCause` record. The pcap section is
    /// the channel that fired the disconnect (per
    /// `BugReportType::Disconnect::channel_name`); if the channel
    /// is "error" or otherwise unknown, the main-channel pcap
    /// is included as a fallback.
    #[allow(clippy::too_many_arguments)]
    pub fn write_disconnect(
        dir: &std::path::Path,
        cause: DisconnectCause,
        target_host: &str,
        target_port: u16,
        traffic: &TrafficBuffers,
        channel_snapshots: &ChannelSnapshots,
        app_snapshot: &Mutex<AppSnapshot>,
        notifications: &Mutex<NotificationStore>,
        runtime_metrics: RuntimeMetrics,
    ) -> anyhow::Result<std::path::PathBuf> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let description = format!(
            "auto: connection lost on {} ({})",
            cause.channel, cause.error_message
        );
        let report = Self::assemble(
            BugReportType::Disconnect {
                channel: cause.channel.clone(),
            },
            description,
            None,
            target_host,
            target_port,
            traffic,
            channel_snapshots,
            app_snapshot,
            notifications,
            None,
            runtime_metrics,
            None,
            None,
        )?;

        std::fs::create_dir_all(dir)?;

        let safe_channel = cause
            .channel
            .chars()
            .map(|c| match c {
                ':' | '/' | '\\' | ' ' => '-',
                c if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' => c,
                _ => '_',
            })
            .collect::<String>();
        let filename = format!(
            "ryll-disconnect-{}-{}.zip",
            safe_channel,
            filename_timestamp()
        );
        let path = dir.join(&filename);
        let file = std::fs::File::create(&path)?;
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

        zip.start_file("metadata.json", opts)?;
        zip.write_all(report.metadata_json.as_bytes())?;

        zip.start_file("session.json", opts)?;
        zip.write_all(report.session_json.as_bytes())?;

        zip.start_file("channel-state.json", opts)?;
        zip.write_all(report.channel_state_json.as_bytes())?;

        if let Some(ref pcap) = report.pcap_bytes {
            zip.start_file("traffic.pcap", opts)?;
            zip.write_all(pcap)?;
        }

        zip.start_file("notifications.json", opts)?;
        zip.write_all(report.notifications_json.as_bytes())?;

        let metrics_json = serde_json::to_string_pretty(&report.runtime_metrics)?;
        zip.start_file("runtime-metrics.json", opts)?;
        zip.write_all(metrics_json.as_bytes())?;

        let cause_json = serde_json::to_string_pretty(&cause)?;
        zip.start_file("disconnect-cause.json", opts)?;
        zip.write_all(cause_json.as_bytes())?;

        zip.finish()?;
        Ok(path)
    }

    /// Build the --pedantic observer closure and register it
    /// with the warn_once registry. Called from
    /// `app::RyllApp::new` (GUI) and `app::run_headless`
    /// after the live handles are built; rely on
    /// `register_gap_observer`'s replay semantics to cover
    /// the window between session start and observer
    /// registration (empirically empty on the current code
    /// paths; replay catches anything that slips in).
    ///
    /// Spawns a tokio task per new gap so the firing thread
    /// (usually a channel task) never blocks on disk I/O or
    /// metrics sampling.
    pub(crate) fn register_pedantic_observer(
        config: PedanticConfig,
        target_host: String,
        target_port: u16,
        traffic: Arc<TrafficBuffers>,
        channel_snapshots: ChannelSnapshots, // cheap Clone (4 Arcs)
        app_snapshot: Arc<Mutex<AppSnapshot>>,
        notifications: SharedNotifications,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = Arc::new(config.dir);
        let host = Arc::new(target_host);
        let counter = Arc::new(AtomicUsize::new(0));

        let dir_for_log = dir.clone();

        shakenfist_spice_protocol::logging::register_gap_observer(Arc::new(
            move |key: &'static str| {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n >= PEDANTIC_REPORT_CAP {
                    return;
                }
                let dir = dir.clone();
                let host = host.clone();
                let traffic = traffic.clone();
                let snaps = channel_snapshots.clone();
                let app_snap = app_snapshot.clone();
                let notifs = notifications.clone();
                let key_str = key.to_string();
                tokio::spawn(async move {
                    // metrics::sample blocks for its sample window; run it on a
                    // dedicated thread so the tokio executor is not stalled.
                    let metrics = tokio::task::spawn_blocking(|| {
                        shakenfist_spice_renderer::metrics::sample(std::time::Duration::from_secs(
                            1,
                        ))
                    })
                    .await
                    .unwrap_or_else(|_| {
                        shakenfist_spice_renderer::metrics::RuntimeMetrics::unavailable(
                            "spawn_blocking panicked during metrics sample",
                        )
                    });
                    // Pedantic observers fire synchronously on the
                    // gap event, so trigger-time and submit-time are
                    // the same moment; None lets `assemble()` default
                    // the triggered_* metadata fields to submit time.
                    match BugReport::write_pedantic(
                        &dir,
                        &key_str,
                        &host,
                        target_port,
                        &traffic,
                        &snaps,
                        &app_snap,
                        &notifs,
                        metrics,
                        None,
                    ) {
                        Ok(path) => tracing::info!("pedantic: wrote {}", path.display()),
                        Err(e) => {
                            tracing::warn!("pedantic: write failed for {}: {}", key_str, e)
                        }
                    }
                });
            },
        ));

        tracing::info!(
            "pedantic mode enabled; reports will land in {}",
            dir_for_log.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::NotificationStore;

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
            ..Default::default()
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
            ryll_git_sha: "deadbeef".to_string(),
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
            triggered_at: "2026-04-03T12:34:50Z".to_string(),
            triggered_uptime_secs: 39.1,
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        assert!(json.contains("\"report_type\": \"Display\""));
        assert!(json.contains("\"channel\": \"display\""));
        assert!(json.contains("\"description\": \"test bug\""));
        assert!(json.contains("\"left\": 10"));
        assert!(json.contains("\"triggered_at\": \"2026-04-03T12:34:50Z\""));
        assert!(json.contains("\"triggered_uptime_secs\": 39.1"));
    }

    /// Stub metrics used by tests to avoid a 2-second sleep.
    fn stub_metrics() -> RuntimeMetrics {
        RuntimeMetrics::unavailable("stub metrics for testing")
    }

    #[test]
    fn test_bug_report_assemble_display() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        // 2x2 red RGBA pixels
        let pixels = vec![
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];

        let report = BugReport::assemble(
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
            &notifications,
            Some((&pixels, 2, 2)),
            stub_metrics(),
            None,
            None,
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

    // When no explicit TriggerTimestamps are supplied, assemble()
    // must default the new fields to the submit-time values so
    // downstream tooling can treat them as always present.
    #[test]
    fn test_bug_report_assemble_defaults_trigger_to_submit() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let report = BugReport::assemble(
            BugReportType::Input,
            "no trigger supplied".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            None,
            None,
        )
        .unwrap();

        let meta: serde_json::Value = serde_json::from_str(&report.metadata_json).unwrap();
        let submit_iso = meta["timestamp"].as_str().unwrap();
        let submit_uptime = meta["session_uptime_secs"].as_f64().unwrap();
        assert_eq!(meta["triggered_at"].as_str().unwrap(), submit_iso);
        assert_eq!(
            meta["triggered_uptime_secs"].as_f64().unwrap(),
            submit_uptime
        );
    }

    // An explicit TriggerTimestamps must surface verbatim in the
    // metadata without being overwritten by the submit-time values.
    #[test]
    fn test_bug_report_assemble_propagates_explicit_trigger() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let trigger = TriggerTimestamps {
            triggered_at: "2020-01-01T00:00:00Z".to_string(),
            triggered_uptime_secs: 1.5,
        };

        let report = BugReport::assemble(
            BugReportType::Input,
            "explicit trigger".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            Some(trigger),
            None,
        )
        .unwrap();

        let meta: serde_json::Value = serde_json::from_str(&report.metadata_json).unwrap();
        assert_eq!(
            meta["triggered_at"].as_str().unwrap(),
            "2020-01-01T00:00:00Z"
        );
        assert_eq!(meta["triggered_uptime_secs"].as_f64().unwrap(), 1.5);
        // The submit-time fields must not have been clobbered by the
        // explicit trigger values.
        assert_ne!(meta["timestamp"].as_str().unwrap(), "2020-01-01T00:00:00Z");
    }

    // Round-trip sanity: the metadata.json inside the zip still
    // contains both new fields once it survives ZipWriter + serde.
    #[test]
    fn test_bug_report_zip_metadata_has_trigger_fields() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let report = BugReport::assemble(
            BugReportType::Input,
            "zip round-trip".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            Some(TriggerTimestamps {
                triggered_at: "2021-06-06T06:06:06Z".to_string(),
                triggered_uptime_secs: 12.25,
            }),
            None,
        )
        .unwrap();

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-trigger-zip");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = report.write_zip(&tmp).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut meta_file = archive.by_name("metadata.json").unwrap();
        let mut meta_str = String::new();
        std::io::Read::read_to_string(&mut meta_file, &mut meta_str).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
        assert_eq!(
            meta["triggered_at"].as_str().unwrap(),
            "2021-06-06T06:06:06Z"
        );
        assert_eq!(meta["triggered_uptime_secs"].as_f64().unwrap(), 12.25);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // When the caller supplies a precomputed PNG, assemble() must
    // use those bytes verbatim rather than re-encoding the live
    // surface pixels. This is the trigger-snapshot hot path: the
    // background thread produces the PNG and the submit path uses
    // it as-is instead of encoding the potentially-changed surface.
    #[test]
    fn test_bug_report_uses_precomputed_png_when_provided() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        // Raw 2x2 RGBA that would produce a very different PNG if
        // re-encoded vs. the sentinel bytes we hand in.
        let raw = vec![
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        // Sentinel bytes: not a valid PNG, but we're proving they
        // reach the zip untouched.
        let sentinel = b"sentinel-precomputed-png-bytes".to_vec();

        let report = BugReport::assemble(
            BugReportType::Display,
            "precomputed png path".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            Some((&raw, 2, 2)),
            stub_metrics(),
            None,
            Some(sentinel.clone()),
        )
        .unwrap();

        assert_eq!(report.screenshot_png.as_deref(), Some(sentinel.as_slice()));
    }

    // Regression: when no precomputed PNG is supplied, assemble()
    // still encodes the live surface pixels. This is the fallback
    // path used when the background encoder never ran (pedantic
    // observers, tests) or hasn't finished by submit time.
    #[test]
    fn test_bug_report_falls_back_to_live_encoding_when_no_precomputed_png() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let raw = vec![
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];

        let report = BugReport::assemble(
            BugReportType::Display,
            "live encode fallback".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            Some((&raw, 2, 2)),
            stub_metrics(),
            None,
            None,
        )
        .unwrap();

        let png = report
            .screenshot_png
            .expect("screenshot.png was not encoded");
        assert_eq!(&png[..4], b"\x89PNG");
    }

    // Non-Display submissions must drop any precomputed PNG on the
    // floor. Policy is "always capture on dialog open, only include
    // the PNG when the submitted type is Display".
    #[test]
    fn test_bug_report_drops_precomputed_png_for_non_display() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let sentinel = b"sentinel-precomputed-png-bytes".to_vec();

        let report = BugReport::assemble(
            BugReportType::Input,
            "non-display drops png".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            None,
            Some(sentinel),
        )
        .unwrap();

        assert!(report.screenshot_png.is_none());
    }

    // The background encoder thread's contract: given a
    // shared Arc<Mutex<Option<Result<Vec<u8>>>>> slot, a clone of
    // the surface pixels, and width/height, it eventually writes
    // a valid PNG into the slot. No egui or RyllApp involved.
    #[test]
    fn test_trigger_snapshot_worker_encodes_png() {
        use std::sync::Mutex as StdMutex;

        let raw = vec![
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let slot: std::sync::Arc<StdMutex<Option<anyhow::Result<Vec<u8>>>>> =
            std::sync::Arc::new(StdMutex::new(None));
        let slot_for_thread = std::sync::Arc::clone(&slot);

        let handle = std::thread::Builder::new()
            .name("ryll-bugreport-png-test".to_string())
            .spawn(move || {
                let result = encode_png(&raw, 2, 2);
                if let Ok(mut guard) = slot_for_thread.lock() {
                    *guard = Some(result);
                }
            })
            .expect("failed to spawn encoder thread");
        handle.join().expect("encoder thread panicked");

        let guard = slot.lock().expect("slot lock poisoned");
        let bytes = guard
            .as_ref()
            .expect("encoder did not write into slot")
            .as_ref()
            .expect("encoder returned Err");
        assert_eq!(&bytes[..4], b"\x89PNG");
    }

    // When a Display report carries a region, the zip gets a
    // second PNG cropped from the submit-time surface.
    #[test]
    fn test_bug_report_writes_region_crop_when_region_present() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        // 2x2 RGBA — enough to produce a non-empty crop.
        let pixels = vec![
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];

        let report = BugReport::assemble(
            BugReportType::Display,
            "region crop test".to_string(),
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
            &notifications,
            Some((&pixels, 2, 2)),
            stub_metrics(),
            None,
            None,
        )
        .unwrap();

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-region");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = report.write_zip(&tmp).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"screenshot.png".to_string()));
        assert!(names.contains(&"screenshot-region.png".to_string()));

        let mut region_file = archive.by_name("screenshot-region.png").unwrap();
        let mut region_bytes = Vec::new();
        std::io::Read::read_to_end(&mut region_file, &mut region_bytes).unwrap();
        assert_eq!(&region_bytes[..4], b"\x89PNG");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Display report without a region: only the full screenshot.
    #[test]
    fn test_bug_report_no_region_crop_without_region() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let pixels = vec![
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];

        let report = BugReport::assemble(
            BugReportType::Display,
            "no region".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            Some((&pixels, 2, 2)),
            stub_metrics(),
            None,
            None,
        )
        .unwrap();

        assert!(report.screenshot_png.is_some());
        assert!(report.screenshot_region_png.is_none());
    }

    // Non-Display reports never carry a region crop, even if a
    // caller wrongly passes a region. Belt-and-braces for the
    // report-type guard in `assemble`.
    #[test]
    fn test_bug_report_no_region_crop_for_non_display() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let report = BugReport::assemble(
            BugReportType::Input,
            "buggy caller set a region on an Input report".to_string(),
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
            &notifications,
            None,
            stub_metrics(),
            None,
            None,
        )
        .unwrap();

        assert!(report.screenshot_png.is_none());
        assert!(report.screenshot_region_png.is_none());
    }

    // `encode_region_png` clamps out-of-bounds rectangles to the
    // surface, and returns `Ok(None)` for degenerate or
    // fully-outside rectangles.
    #[test]
    fn test_encode_region_png_clamps_and_skips_degenerate() {
        // 2x2 RGBA.
        let pixels = vec![
            10u8, 0, 0, 255, 0, 20, 0, 255, 0, 0, 30, 255, 40, 40, 40, 255,
        ];

        // Region extends past the surface → clamps to 2x2 and
        // produces a valid PNG.
        let oversize = ReportRegion {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let png = encode_region_png(&pixels, 2, 2, &oversize)
            .unwrap()
            .expect("expected Some(PNG) for a clamped region");
        assert_eq!(&png[..4], b"\x89PNG");

        // Zero-width region → None.
        let zero_width = ReportRegion {
            left: 1,
            top: 0,
            right: 1,
            bottom: 2,
        };
        assert!(encode_region_png(&pixels, 2, 2, &zero_width)
            .unwrap()
            .is_none());

        // Zero-height region → None.
        let zero_height = ReportRegion {
            left: 0,
            top: 1,
            right: 2,
            bottom: 1,
        };
        assert!(encode_region_png(&pixels, 2, 2, &zero_height)
            .unwrap()
            .is_none());

        // Entirely outside → clamps to 0×0 → None.
        let outside = ReportRegion {
            left: 10,
            top: 10,
            right: 20,
            bottom: 20,
        };
        assert!(encode_region_png(&pixels, 2, 2, &outside)
            .unwrap()
            .is_none());
    }

    // Stride math: the cropped PNG must contain exactly the
    // source pixels at the cropped sub-rect, round-tripped
    // through the PNG encoder/decoder.
    #[test]
    fn test_encode_region_png_pixels_match_source() {
        // 4x4 RGBA laid out so every pixel is distinct.
        let mut src = Vec::with_capacity(4 * 4 * 4);
        for y in 0..4u8 {
            for x in 0..4u8 {
                src.extend_from_slice(&[x * 16, y * 16, (x + y) * 8, 255]);
            }
        }

        // Crop to the 2x2 sub-rect (1,1)-(3,3).
        let region = ReportRegion {
            left: 1,
            top: 1,
            right: 3,
            bottom: 3,
        };
        let png_bytes = encode_region_png(&src, 4, 4, &region)
            .unwrap()
            .expect("expected Some(PNG)");

        let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut decoded = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);

        // Expected 2x2 = pixels at (x=1,y=1), (2,1), (1,2), (2,2).
        let mut expected = Vec::with_capacity(2 * 2 * 4);
        for y in 1..3u8 {
            for x in 1..3u8 {
                expected.extend_from_slice(&[x * 16, y * 16, (x + y) * 8, 255]);
            }
        }
        assert_eq!(&decoded[..info.buffer_size()], expected.as_slice());
    }

    #[test]
    fn test_bug_report_assemble_input() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let report = BugReport::assemble(
            BugReportType::Input,
            "keyboard not working".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            None,
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
        let notifications = Mutex::new(NotificationStore::new());

        let report = BugReport::assemble(
            BugReportType::Cursor,
            "cursor disappeared".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            None,
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
        let notifications = Mutex::new(NotificationStore::new());

        let report = BugReport::assemble(
            BugReportType::Connection,
            "session dropped".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            None,
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
    fn test_bug_report_runtime_metrics_in_zip() {
        // Verify that runtime-metrics.json is present in the ZIP and
        // contains the expected JSON shape when a stub metrics value
        // is injected (no actual 2-second sleep).
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let metrics = RuntimeMetrics::unavailable("stub metrics for testing");
        let report = BugReport::assemble(
            BugReportType::Connection,
            "runtime metrics test".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            metrics,
            None,
            None,
        )
        .unwrap();

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-metrics");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = report.write_zip(&tmp).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        // runtime-metrics.json must be present.
        assert!(
            names.contains(&"runtime-metrics.json".to_string()),
            "runtime-metrics.json missing from ZIP; found: {:?}",
            names
        );

        // Verify the expected JSON shape.
        {
            let mut metrics_file = archive.by_name("runtime-metrics.json").unwrap();
            let mut metrics_str = String::new();
            std::io::Read::read_to_string(&mut metrics_file, &mut metrics_str).unwrap();
            assert!(
                metrics_str.contains("\"available\": false"),
                "expected 'available: false' in metrics JSON"
            );
            assert!(
                metrics_str.contains("\"reason\""),
                "expected 'reason' field in metrics JSON"
            );
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

    #[test]
    fn test_bug_report_type_pedantic_channel_name() {
        // Known channel prefixes route to the right channel name.
        assert_eq!(
            BugReportType::Pedantic {
                gap_key: "display:unimpl:draw_rop3".to_string()
            }
            .channel_name(),
            "display"
        );
        assert_eq!(
            BugReportType::Pedantic {
                gap_key: "cursor:unknown:42".to_string()
            }
            .channel_name(),
            "cursor"
        );
        assert_eq!(
            BugReportType::Pedantic {
                gap_key: "inputs:unimpl:foo".to_string()
            }
            .channel_name(),
            "inputs"
        );
        assert_eq!(
            BugReportType::Pedantic {
                gap_key: "main:decode_failure:bar".to_string()
            }
            .channel_name(),
            "main"
        );
        assert_eq!(
            BugReportType::Pedantic {
                gap_key: "usbredir:unknown:99".to_string()
            }
            .channel_name(),
            "usbredir"
        );
        // Unknown prefix falls back to "display".
        assert_eq!(
            BugReportType::Pedantic {
                gap_key: "unknown:key".to_string()
            }
            .channel_name(),
            "display"
        );
        // Empty key also falls back to "display".
        assert_eq!(
            BugReportType::Pedantic {
                gap_key: String::new()
            }
            .channel_name(),
            "display"
        );
    }

    #[test]
    fn test_write_pedantic_produces_zip() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());
        let metrics = stub_metrics();

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-pedantic");
        let _ = std::fs::remove_dir_all(&tmp);

        let path = BugReport::write_pedantic(
            &tmp,
            "display:unimpl:draw_rop3",
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            metrics,
            None,
        )
        .unwrap();

        // The returned path must exist and have a .zip extension.
        assert!(path.exists(), "pedantic zip does not exist: {:?}", path);
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some("zip"),
            "expected .zip extension, got {:?}",
            path
        );

        // The filename should encode the gap_key.
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(
            filename.starts_with("ryll-pedantic-display-unimpl-draw_rop3-"),
            "filename does not encode gap_key: {}",
            filename
        );

        // Verify the zip contains expected files.
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"metadata.json".to_string()));
        assert!(names.contains(&"session.json".to_string()));
        assert!(names.contains(&"channel-state.json".to_string()));
        assert!(names.contains(&"runtime-metrics.json".to_string()));
        // Pedantic reports do not include a screenshot.
        assert!(!names.contains(&"screenshot.png".to_string()));

        // Verify metadata identifies this as a Pedantic report.
        {
            let mut meta_file = archive.by_name("metadata.json").unwrap();
            let mut meta_str = String::new();
            std::io::Read::read_to_string(&mut meta_file, &mut meta_str).unwrap();
            assert!(
                meta_str.contains("\"description\": \"pedantic: display:unimpl:draw_rop3\""),
                "metadata missing expected description: {}",
                meta_str
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bug_report_zip_includes_notifications_json() {
        use crate::notifications::{NotificationEntry, NotificationSource};
        use shakenfist_spice_protocol::{ChannelType, NotifySeverity};

        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());

        // Push two distinct entries into the store.
        let notifications = Mutex::new(NotificationStore::new());
        {
            let mut s = notifications.lock().unwrap();
            s.push(NotificationEntry::new(
                NotifySeverity::Warn,
                NotificationSource::Gap,
                "first-gap-key",
            ));
            s.push(NotificationEntry::new(
                NotifySeverity::Info,
                NotificationSource::Spice {
                    channel: ChannelType::Main,
                    what: 0,
                },
                "second-spice-message",
            ));
        }

        let report = BugReport::assemble(
            BugReportType::Connection,
            "notifications zip test".to_string(),
            None,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            None,
            stub_metrics(),
            None,
            None,
        )
        .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let path = report.write_zip(tmp.path()).unwrap();

        // Read the zip back, find notifications.json, deserialise.
        let f = std::fs::File::open(&path).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut nf = zip
            .by_name("notifications.json")
            .expect("notifications.json missing from zip");
        let mut json = String::new();
        use std::io::Read;
        nf.read_to_string(&mut json).unwrap();
        let entries: Vec<NotificationEntry> = serde_json::from_str(&json).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "first-gap-key");
        assert_eq!(entries[1].message, "second-spice-message");
    }

    #[test]
    fn test_disconnect_channel_name_routing() {
        for ch in [
            "main", "display", "inputs", "cursor", "playback", "usbredir", "webdav",
        ] {
            assert_eq!(
                BugReportType::Disconnect {
                    channel: ch.to_string()
                }
                .channel_name(),
                ch,
                "channel {} should round-trip",
                ch
            );
        }
        // Unknown / "error" channels fall back to main, since the
        // main pcap is the most useful default for an auto-disconnect
        // bug report when the originating channel is not known.
        assert_eq!(
            BugReportType::Disconnect {
                channel: "error".to_string()
            }
            .channel_name(),
            "main"
        );
        assert_eq!(
            BugReportType::Disconnect {
                channel: "bogus".to_string()
            }
            .channel_name(),
            "main"
        );
    }

    #[test]
    fn test_collect_per_channel_round_trips_keepalive_and_traffic() {
        let snapshots = ChannelSnapshots::new();
        {
            let mut s = snapshots.main.lock().unwrap();
            s.bytes_in = 1234;
            s.bytes_out = 5678;
            s.last_recv_ts_secs = Some(12.5);
            s.last_send_ts_secs = Some(11.5);
            s.ping_recv_count = 3;
            s.pong_send_count = 3;
            s.last_ping_recv_ts_secs = Some(12.5);
            s.keepalive_timeout_fired = true;
        }
        {
            let mut s = snapshots.display.lock().unwrap();
            s.bytes_in = 99;
        }

        let per_channel = DisconnectCause::collect_per_channel(&snapshots);
        let main = per_channel.get("main").expect("main entry missing");
        assert_eq!(main.bytes_in, 1234);
        assert_eq!(main.bytes_out, 5678);
        assert_eq!(main.last_recv_ts_secs, Some(12.5));
        assert_eq!(main.ping_recv_count, 3);
        assert_eq!(main.pong_send_count, 3);

        // keepalive_timeout_fired isn't in PerChannelDiagnostics; it
        // is read separately at the disconnect site, but we verify
        // here that the snapshot retains the flag the caller set.
        assert!(snapshots.main.lock().unwrap().keepalive_timeout_fired);

        // Every channel name should appear, even ones we didn't touch.
        for ch in [
            "main", "display", "inputs", "cursor", "playback", "usbredir", "webdav",
        ] {
            assert!(per_channel.contains_key(ch), "per_channel missing {}", ch);
        }
    }

    #[test]
    fn test_collect_per_channel_surfaces_inputs_keepalive_fields() {
        let snapshots = ChannelSnapshots::new();
        {
            let mut s = snapshots.inputs.lock().unwrap();
            s.client_keepalive_send_count = 7;
            s.last_client_keepalive_send_ts_secs = Some(305.5);
        }

        let per_channel = DisconnectCause::collect_per_channel(&snapshots);
        let inputs = per_channel.get("inputs").expect("inputs entry missing");
        assert_eq!(inputs.client_keepalive_send_count, 7);
        assert_eq!(inputs.last_client_keepalive_send_ts_secs, Some(305.5));

        // Other channels do not yet implement keepalive, so the
        // fields default to 0 / None on every other entry.
        for ch in [
            "main", "display", "cursor", "playback", "usbredir", "webdav",
        ] {
            let entry = per_channel
                .get(ch)
                .unwrap_or_else(|| panic!("missing {}", ch));
            assert_eq!(
                entry.client_keepalive_send_count, 0,
                "{} should report zero keepalives sent",
                ch
            );
            assert_eq!(
                entry.last_client_keepalive_send_ts_secs, None,
                "{} should report no last-keepalive-send timestamp",
                ch
            );
        }
    }

    #[test]
    fn test_write_disconnect_produces_zip_with_cause_json() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        {
            let mut s = snapshots.main.lock().unwrap();
            s.bytes_in = 4096;
        }
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());
        let metrics = stub_metrics();

        let cause = DisconnectCause {
            channel: "main".to_string(),
            error_message: "test disconnect".to_string(),
            error_kind: None,
            keepalive_timeout_fired: true,
            session_uptime_secs: 10.0,
            per_channel: DisconnectCause::collect_per_channel(&snapshots),
        };

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-disconnect");
        let _ = std::fs::remove_dir_all(&tmp);

        let path = BugReport::write_disconnect(
            &tmp,
            cause,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            metrics,
        )
        .unwrap();

        assert!(path.exists(), "disconnect zip does not exist: {:?}", path);
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(
            filename.starts_with("ryll-disconnect-main-"),
            "filename does not encode channel: {}",
            filename
        );

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"metadata.json".to_string()));
        assert!(names.contains(&"session.json".to_string()));
        assert!(names.contains(&"channel-state.json".to_string()));
        assert!(names.contains(&"runtime-metrics.json".to_string()));
        assert!(names.contains(&"disconnect-cause.json".to_string()));

        // disconnect-cause.json should round-trip the fields we set,
        // including keepalive_timeout_fired and the per-channel map.
        let mut cf = archive.by_name("disconnect-cause.json").unwrap();
        let mut json = String::new();
        std::io::Read::read_to_string(&mut cf, &mut json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["channel"], "main");
        assert_eq!(parsed["error_message"], "test disconnect");
        assert_eq!(parsed["keepalive_timeout_fired"], true);
        assert_eq!(parsed["session_uptime_secs"], 10.0);
        assert_eq!(parsed["per_channel"]["main"]["bytes_in"], 4096);
        assert!(parsed["per_channel"]
            .as_object()
            .unwrap()
            .contains_key("playback"));
        assert!(parsed["per_channel"]
            .as_object()
            .unwrap()
            .contains_key("webdav"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_disconnect_sanitises_channel_in_filename() {
        let traffic = TrafficBuffers::new();
        let snapshots = ChannelSnapshots::new();
        let app_snap = Mutex::new(AppSnapshot::default());
        let notifications = Mutex::new(NotificationStore::new());

        let cause = DisconnectCause {
            channel: "weird/name with:colons".to_string(),
            error_message: "x".to_string(),
            error_kind: Some("ConnectionReset".to_string()),
            keepalive_timeout_fired: false,
            session_uptime_secs: 0.0,
            per_channel: DisconnectCause::collect_per_channel(&snapshots),
        };

        let tmp = std::env::temp_dir().join("ryll-test-bugreport-disconnect-sanitise");
        let _ = std::fs::remove_dir_all(&tmp);

        let path = BugReport::write_disconnect(
            &tmp,
            cause,
            "10.0.0.1",
            5900,
            &traffic,
            &snapshots,
            &app_snap,
            &notifications,
            stub_metrics(),
        )
        .unwrap();

        let filename = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !filename.contains('/') && !filename.contains(':') && !filename.contains(' '),
            "filename was not sanitised: {}",
            filename
        );
        assert!(filename.starts_with("ryll-disconnect-weird-name-with-colons-"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
