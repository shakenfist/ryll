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
}

impl Default for ChannelSnapshots {
    fn default() -> Self {
        Self::new()
    }
}
