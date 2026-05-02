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

/// Holds all four per-channel snapshot `Arc<Mutex<T>>`s.
#[derive(Clone)]
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

impl Default for ChannelSnapshots {
    fn default() -> Self {
        Self::new()
    }
}
