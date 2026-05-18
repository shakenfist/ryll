//! Per-channel state snapshot types written into bug reports.
//!
//! These types describe channel state, not the bug-report
//! packaging. They live in the renderer so a third-party
//! consumer can inspect channel state (for diagnostics or
//! protocol-level introspection) without taking on ryll's
//! bug-report ZIP machinery, which stays host-side.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Per-stream state for a single SPICE display video stream
/// (e.g. an MJPEG stream the server promoted a region to). One
/// instance per currently-open `STREAM_CREATE` lives in
/// `DisplaySnapshot::streams_active`; entries disappear when the
/// server sends `STREAM_DESTROY` / `STREAM_DESTROY_ALL`.
///
/// Added to answer "is the MJPEG path actually painting?" from
/// bug reports: `frames_received` reflects what arrived on the
/// wire for this stream, `frames_decoded_ok` reflects what was
/// blit to the surface, and the timestamps show staleness. See
/// the diagnostic gap that motivated this in the renderer's
/// stream handling (`channels/display.rs::STREAM_DATA`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StreamSnapshot {
    pub stream_id: u32,
    pub surface_id: u32,
    /// Raw SPICE codec type (1=MJPEG, 2=VP8, 3=H264, 4=VP9, 5=H265).
    pub codec_type: u8,
    /// Native stream dimensions reported by `STREAM_CREATE`.
    pub stream_width: u32,
    pub stream_height: u32,
    /// Destination rect on the surface (where decoded frames blit).
    pub dest_top: u32,
    pub dest_left: u32,
    pub dest_bottom: u32,
    pub dest_right: u32,
    /// Session-relative seconds when `STREAM_CREATE` was processed.
    pub created_at_secs: f64,
    /// `STREAM_DATA` / `STREAM_DATA_SIZED` messages received for
    /// this stream since `STREAM_CREATE`. Includes frames the
    /// codec was unable to decode.
    pub frames_received: u64,
    /// Frames successfully decoded and dispatched as `ImageReady`.
    pub frames_decoded_ok: u64,
    /// Frames where the MJPEG decoder returned `None` (decode
    /// error or unsupported pixel format) or the codec is not
    /// supported by the renderer (anything other than MJPEG today).
    pub frames_decode_failed: u64,
    /// Session-relative seconds of the most recent `STREAM_DATA`
    /// message for this stream. None until the first one arrives.
    pub last_frame_ts_secs: Option<f64>,
    /// Session-relative seconds of the most recent successful
    /// decode. None until the first one succeeds. Compare with
    /// `last_frame_ts_secs` to see "frames arriving but not
    /// decoding".
    pub last_decode_ok_ts_secs: Option<f64>,
    /// Wall-clock microseconds for the most recent successful
    /// decode. Zero until the first one succeeds.
    pub last_decode_duration_us: u32,
    /// Session-relative seconds when the stream was torn down
    /// via `STREAM_DESTROY` / `STREAM_DESTROY_ALL`. `None`
    /// while the stream is still active (i.e. in
    /// `DisplaySnapshot::streams_active`); always `Some` for
    /// entries in `streams_recently_destroyed`.
    pub destroyed_at_secs: Option<f64>,
    /// Whether the server activated client reports for this
    /// stream via STREAM_ACTIVATE_REPORT. False until activation.
    pub report_is_active: bool,
    /// Unique id we must echo back in every STREAM_REPORT for
    /// this stream. The server uses this to correlate reports
    /// to the stream incarnation (a new id is issued each
    /// STREAM_CREATE). Zero before activation.
    pub report_unique_id: u32,
    /// Server-suggested report trigger: send a report once
    /// `report_num_frames` reaches this threshold. Zero before
    /// activation. Server default = 5.
    pub report_max_window_size: u32,
    /// Server-suggested report trigger: send a report once
    /// `now_mm_time - report_start_now_mm_time >= timeout_ms`.
    /// Zero before activation. Server default = 1000.
    pub report_timeout_ms: u32,
    /// Cumulative reports sent since STREAM_CREATE. Zero until
    /// the first send.
    pub report_send_count: u32,
    /// Session-relative seconds of the most recent STREAM_REPORT
    /// send. None until the first send.
    pub last_report_sent_ts_secs: Option<f64>,
    /// Frame count of the most recently sent STREAM_REPORT.
    /// Zero until the first send.
    pub last_report_num_frames: u32,
    /// Drop count of the most recently sent STREAM_REPORT.
    /// Zero until the first send.
    pub last_report_num_drops: u32,
    /// `last_frame_delay` field of the most recently sent
    /// STREAM_REPORT (signed mm-time difference between the
    /// frame's mm_time and "now" at send time). Zero until the
    /// first send.
    pub last_report_last_frame_delay: i32,
}

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
    /// Wall-clock microseconds spent decompressing this image.
    /// Zero for `FromCache` and for failures that short-circuit
    /// before the decoder is invoked.
    pub decode_duration_us: u32,
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
    /// Session-relative seconds when the most recent server
    /// message of any kind was parsed on this channel. Used for
    /// disconnect-cause diagnostics.
    pub last_recv_ts_secs: Option<f64>,
    /// Session-relative seconds when the most recent client
    /// message of any kind was sent on this channel.
    pub last_send_ts_secs: Option<f64>,
    /// Number of server PINGs received since session start.
    pub ping_recv_count: u32,
    /// Number of client PONGs sent since session start.
    pub pong_send_count: u32,
    /// Session-relative seconds of the most recent server PING.
    pub last_ping_recv_ts_secs: Option<f64>,
    /// Total decodes recorded since session start. Counts every
    /// `record_decode` call, including failures and cache hits.
    pub decode_total_count: u64,
    /// Decodes where `success == false`. Subset of the total.
    pub decode_failed_count: u64,
    /// Decodes where `from_cache == true`. Subset of the total.
    pub decode_from_cache_count: u64,
    /// Min / max / mean of `decode_duration_us` over the entries
    /// currently in `recent_decodes` for which the decoder ran
    /// (i.e. excluding cache hits and failures). Zero when the
    /// ring contains no such entries.
    pub decode_recent_min_us: u32,
    pub decode_recent_max_us: u32,
    pub decode_recent_mean_us: u32,
    /// Total reads from the socket since session start. One per
    /// `s.read(&mut chunk).await` that returned `n > 0`.
    pub socket_read_count: u64,
    /// Reads where `n == 262144` (the full chunk size). A high
    /// ratio of these to `socket_read_count` indicates the OS
    /// recv buffer was usually non-empty when we read — i.e.
    /// the read loop is not keeping up with the arrival rate.
    pub socket_reads_at_chunk_cap: u64,
    /// Largest `n` observed over the session. Sanity check for
    /// any future chunk-capacity tuning.
    pub socket_max_chunk_bytes: u32,
    /// Total ACK messages sent to the server since session
    /// start.
    pub ack_send_count: u32,
    /// Session-relative seconds of the most recent ACK send.
    /// None until the first ACK is sent.
    pub last_ack_send_ts_secs: Option<f64>,
    /// Intervals (seconds) between the most recent consecutive
    /// ACK sends, oldest first. Bounded ring; see the
    /// `RECENT_ACK_INTERVALS_CAP` constant in the display
    /// channel for the cap.
    pub recent_ack_intervals_secs: VecDeque<f64>,
    /// Phase-02 "video not keeping up" diagnostic: number of
    /// pcap-capture packets dropped because the writer-task
    /// queue was full. Cumulative since session start; zero
    /// when `--capture` is not in use. A non-zero value
    /// implicates disk speed rather than decode or socket-read
    /// when triaging a "video not keeping up" report. See
    /// PLAN-video-keeping-up-phase-02-pcap-thread.md.
    pub writer_dropped_count: u64,
    /// Currently-open SPICE video streams (one entry per active
    /// `STREAM_CREATE`). Empty when the server has not promoted
    /// any region to a stream. See `StreamSnapshot`.
    pub streams_active: Vec<StreamSnapshot>,
    /// Cumulative `STREAM_CREATE` count since session start.
    pub streams_created_total: u64,
    /// Cumulative count of streams removed via `STREAM_DESTROY`
    /// or `STREAM_DESTROY_ALL` since session start.
    pub streams_destroyed_total: u64,
    /// `STREAM_DATA` / `STREAM_DATA_SIZED` messages whose
    /// `stream_id` did not match any open stream. A non-zero
    /// value points at a `STREAM_CREATE` we missed or processed
    /// in the wrong order — symptom level, not root cause.
    pub stream_data_orphan_count: u64,
    /// Bounded ring of the most recently torn-down streams,
    /// oldest first. Each entry's counters are frozen at
    /// `STREAM_DESTROY` time so a bug report filed after a
    /// teardown can still answer "during stream X's life, did
    /// MJPEG decode?". `destroyed_at_secs` is always `Some` for
    /// entries here. The cap is set by
    /// `MAX_RECENT_DESTROYED_STREAMS` in the display channel.
    pub streams_recently_destroyed: VecDeque<StreamSnapshot>,
    /// Cumulative count of STREAM_REPORT messages sent to the
    /// server since session start. Tracks how often the
    /// client-side adaptive-bitrate feedback channel fires; zero
    /// when no streams have activated reports.
    pub stream_reports_sent_total: u64,
    /// Cumulative count of "unsupported codec" wildcard reports
    /// (num_frames=0, num_drops=UINT32_MAX) sent to the server.
    /// Currently always zero; written by phase 4 when we accept
    /// multi-codec streams and need to tell the server to give
    /// up on one.
    pub stream_reports_unsupported_signals_sent: u64,
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
    /// See `DisplaySnapshot::last_recv_ts_secs`.
    pub last_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::last_send_ts_secs`.
    pub last_send_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::ping_recv_count`.
    pub ping_recv_count: u32,
    /// See `DisplaySnapshot::pong_send_count`.
    pub pong_send_count: u32,
    /// See `DisplaySnapshot::last_ping_recv_ts_secs`.
    pub last_ping_recv_ts_secs: Option<f64>,
    /// Number of unsolicited KEY_MODIFIERS messages we've sent
    /// to the server as a client-driven idle keepalive (Phase
    /// 02 K1 fix). Restating the modifier state with the same
    /// value is a no-op for the guest but keeps the inputs
    /// channel non-idle, which the K1 hypothesis suggests may
    /// also be enough to keep the whole session alive.
    pub client_keepalive_send_count: u32,
    /// Session-relative seconds at the most recent keepalive
    /// send. None until the first one fires.
    pub last_client_keepalive_send_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::writer_dropped_count`.
    pub writer_dropped_count: u64,
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
    /// See `DisplaySnapshot::last_recv_ts_secs`.
    pub last_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::last_send_ts_secs`.
    pub last_send_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::ping_recv_count`.
    pub ping_recv_count: u32,
    /// See `DisplaySnapshot::pong_send_count`.
    pub pong_send_count: u32,
    /// See `DisplaySnapshot::last_ping_recv_ts_secs`.
    pub last_ping_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::writer_dropped_count`.
    pub writer_dropped_count: u64,
}

/// Snapshot of the main channel's mutable state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MainSnapshot {
    pub session_id: Option<u32>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// See `DisplaySnapshot::last_recv_ts_secs`.
    pub last_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::last_send_ts_secs`.
    pub last_send_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::ping_recv_count`.
    pub ping_recv_count: u32,
    /// See `DisplaySnapshot::pong_send_count`.
    pub pong_send_count: u32,
    /// See `DisplaySnapshot::last_ping_recv_ts_secs`.
    pub last_ping_recv_ts_secs: Option<f64>,
    /// Current SPICE `mm_time` (server-side millisecond
    /// counter) computed from the most recent
    /// `MAIN_INIT::multi_media_time` or `MULTI_MEDIA_TIME`
    /// update plus elapsed wall time. Informational; recomputed
    /// at snapshot time. Wraps at 2^32 ms (~49.7 days).
    pub mm_time_now: u32,
    /// Number of `MmClock::set` calls since session start
    /// (one per `MAIN_INIT` plus one per `MULTI_MEDIA_TIME`).
    /// A frozen `mm_time_now` with a non-advancing
    /// `mm_time_set_count` points at the server having stopped
    /// sending `MULTI_MEDIA_TIME` ticks.
    pub mm_time_set_count: u64,
    /// Session-relative seconds at the most recent `MmClock`
    /// set. `None` until the first `MAIN_INIT` lands.
    pub last_mm_time_set_ts_secs: Option<f64>,
    /// Set to true by the main channel's read loop when its
    /// 30 s client-side keepalive timeout fires (i.e. ryll
    /// considered itself disconnected because no main-channel
    /// message arrived for 30 s). Distinguishes that path from
    /// a real EOF / RST when the disconnect-cause record is
    /// captured. Reset on reconnect by the app layer.
    pub keepalive_timeout_fired: bool,
    /// Number of unsolicited PONG messages we've sent on main
    /// as a client-driven idle keepalive (Phase 02 K1 fix).
    /// SPICE has no client→server PING opcode, so we use a
    /// PONG with synthesised id/timestamp; the server's PONG
    /// handler reads any inbound bytes as "client is alive"
    /// and resets its per-channel rcc connectivity timer.
    pub client_keepalive_send_count: u32,
    /// Session-relative seconds at the most recent keepalive
    /// send. None until the first one fires.
    pub last_client_keepalive_send_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::writer_dropped_count`.
    pub writer_dropped_count: u64,
}

/// Generic snapshot for non-critical channels (playback,
/// usbredir, webdav). These don't carry channel-specific
/// state worth surfacing in bug reports today, but we want
/// disconnect-cause diagnostics to include them so a dropped
/// non-critical channel produces actionable data.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PlaybackSnapshot {
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// See `DisplaySnapshot::last_recv_ts_secs`.
    pub last_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::last_send_ts_secs`.
    pub last_send_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::ping_recv_count`.
    pub ping_recv_count: u32,
    /// See `DisplaySnapshot::pong_send_count`.
    pub pong_send_count: u32,
    /// See `DisplaySnapshot::last_ping_recv_ts_secs`.
    pub last_ping_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::writer_dropped_count`.
    pub writer_dropped_count: u64,
}

/// See `PlaybackSnapshot`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsbredirSnapshot {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub last_recv_ts_secs: Option<f64>,
    pub last_send_ts_secs: Option<f64>,
    pub ping_recv_count: u32,
    pub pong_send_count: u32,
    pub last_ping_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::writer_dropped_count`.
    pub writer_dropped_count: u64,
}

/// See `PlaybackSnapshot`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WebdavSnapshot {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub last_recv_ts_secs: Option<f64>,
    pub last_send_ts_secs: Option<f64>,
    pub ping_recv_count: u32,
    pub pong_send_count: u32,
    pub last_ping_recv_ts_secs: Option<f64>,
    /// See `DisplaySnapshot::writer_dropped_count`.
    pub writer_dropped_count: u64,
}

/// Holds every per-channel snapshot `Arc<Mutex<T>>`. Includes
/// non-critical channels so disconnect-cause records can
/// describe a dropped audio / USB / file-share channel.
#[derive(Clone)]
pub struct ChannelSnapshots {
    pub display: Arc<Mutex<DisplaySnapshot>>,
    pub inputs: Arc<Mutex<InputsSnapshot>>,
    pub cursor: Arc<Mutex<CursorSnapshot>>,
    pub main: Arc<Mutex<MainSnapshot>>,
    pub playback: Arc<Mutex<PlaybackSnapshot>>,
    pub usbredir: Arc<Mutex<UsbredirSnapshot>>,
    pub webdav: Arc<Mutex<WebdavSnapshot>>,
}

impl ChannelSnapshots {
    pub fn new() -> Self {
        ChannelSnapshots {
            display: Arc::new(Mutex::new(DisplaySnapshot::default())),
            inputs: Arc::new(Mutex::new(InputsSnapshot::default())),
            cursor: Arc::new(Mutex::new(CursorSnapshot::default())),
            main: Arc::new(Mutex::new(MainSnapshot::default())),
            playback: Arc::new(Mutex::new(PlaybackSnapshot::default())),
            usbredir: Arc::new(Mutex::new(UsbredirSnapshot::default())),
            webdav: Arc::new(Mutex::new(WebdavSnapshot::default())),
        }
    }

    /// Serialise a single channel's snapshot to a pretty JSON
    /// string, dispatching on channel name. Returns `None` for
    /// channel names that don't have a dedicated snapshot
    /// (currently: anything outside the set
    /// {`"display"`, `"inputs"`, `"cursor"`, `"main"`,
    /// `"playback"`, `"usbredir"`, `"webdav"`}).
    ///
    /// Used by the bug-report writer's per-report-type
    /// channel-state.json dispatch — extracted as a helper
    /// so the dispatch site doesn't duplicate the
    /// channel-name match and the lock/clone/serialise
    /// boilerplate per channel.
    pub fn snapshot_json_for(&self, channel: &str) -> Option<serde_json::Result<String>> {
        match channel {
            "display" => {
                let snap = self.display.lock().unwrap().clone();
                Some(serde_json::to_string_pretty(&snap))
            }
            "inputs" => {
                let snap = self.inputs.lock().unwrap().clone();
                Some(serde_json::to_string_pretty(&snap))
            }
            "cursor" => {
                let snap = self.cursor.lock().unwrap().clone();
                Some(serde_json::to_string_pretty(&snap))
            }
            "main" => {
                let snap = self.main.lock().unwrap().clone();
                Some(serde_json::to_string_pretty(&snap))
            }
            "playback" => {
                let snap = self.playback.lock().unwrap().clone();
                Some(serde_json::to_string_pretty(&snap))
            }
            "usbredir" => {
                let snap = self.usbredir.lock().unwrap().clone();
                Some(serde_json::to_string_pretty(&snap))
            }
            "webdav" => {
                let snap = self.webdav.lock().unwrap().clone();
                Some(serde_json::to_string_pretty(&snap))
            }
            _ => None,
        }
    }
}

impl Default for ChannelSnapshots {
    fn default() -> Self {
        Self::new()
    }
}
