/// Main application - egui App and headless mode
use anyhow::Result;
use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::bugreport::{
    format_size, AppSnapshot, BugReport, BugReportType, ChannelSnapshots, ReportRegion,
    SurfaceInfo, TrafficBuffers, TrafficDirection, TrafficViewEntry,
};
use crate::capture::CaptureSession;
use crate::channels::inputs::{key_to_scancode, mouse_button_to_spice};
use crate::channels::{
    ChannelEvent, CursorChannel, CursorImage, DisplayChannel, InputEvent, InputsChannel,
    MainChannel, UsbCommand, UsbredirChannel, WebdavChannel, WebdavCommand,
};
use crate::config::{Config, ShareDirConfig, VirtualDiskConfig};
use crate::display::DisplaySurface;
use crate::usb::{self, DeviceSource, UsbDeviceInfo};
use shakenfist_spice_protocol::{ChannelType, ConnectionConfig, SpiceClient, MOUSE_MODE_SERVER};

/// Channel buffer sizes
const EVENT_CHANNEL_SIZE: usize = 1024;
const INPUT_CHANNEL_SIZE: usize = 256;

/// Approximate height of the stats bar at the bottom of the window
const STATS_BAR_HEIGHT: f32 = 20.0;

/// Number of bandwidth samples to keep for the sparkline.
const BANDWIDTH_HISTORY_LEN: usize = 60;

/// Number of recent frame timestamps kept for the FPS sliding window.
const FPS_WINDOW_SIZE: usize = 120;

/// Maximum entries shown in the traffic viewer.
const TRAFFIC_VIEWER_MAX_ENTRIES: usize = 200;

/// How often the traffic viewer refreshes from the ring buffers.
const TRAFFIC_VIEWER_REFRESH_MS: u64 = 250;

/// Statistics tracking
#[derive(Default)]
struct Statistics {
    frames_received: u64,
    bytes_in: u64,
    bytes_out: u64,
    last_latency: Option<f64>,
    /// Timestamps of recent DisplayMark events for sliding-window FPS.
    frame_times: Vec<Instant>,
}

/// Shared byte counter that channels increment from their
/// read loops. The app polls it to compute bandwidth.
pub struct ByteCounter(AtomicU64);

impl ByteCounter {
    pub fn new() -> Self {
        ByteCounter(AtomicU64::new(0))
    }

    /// Add bytes (called from channel read loops).
    pub fn add(&self, bytes: u64) {
        self.0.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Read and reset the counter (called from the app tick).
    fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

/// Rolling bandwidth tracker — samples bytes/sec once per second.
struct BandwidthTracker {
    /// Shared counter incremented by all channels.
    counter: Arc<ByteCounter>,
    /// History of bytes-per-second samples (most recent last).
    history: Vec<f32>,
    /// When the current second started.
    last_tick: Instant,
}

impl BandwidthTracker {
    fn new(counter: Arc<ByteCounter>) -> Self {
        BandwidthTracker {
            counter,
            history: Vec::with_capacity(BANDWIDTH_HISTORY_LEN),
            last_tick: Instant::now(),
        }
    }

    /// Tick the tracker — if a second has elapsed, read the
    /// counter and push a new sample.
    fn tick(&mut self) {
        let elapsed = self.last_tick.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let bytes = self.counter.take();
            let secs = elapsed.as_secs_f64();
            let bps = bytes as f64 / secs;
            self.history.push(bps as f32);
            if self.history.len() > BANDWIDTH_HISTORY_LEN {
                self.history.remove(0);
            }
            self.last_tick = Instant::now();
        }
    }

    /// Format the most recent bandwidth value for display.
    fn label(&self) -> String {
        match self.history.last() {
            Some(&bps) if bps >= 1_000_000.0 => format!("{:.1} MB/s", bps / 1_000_000.0),
            Some(&bps) if bps >= 1_000.0 => format!("{:.0} KB/s", bps / 1_000.0),
            Some(&bps) => format!("{:.0} B/s", bps),
            None => String::from("-- B/s"),
        }
    }
}

/// The egui application
pub struct RyllApp {
    // Communication channels
    event_rx: mpsc::Receiver<ChannelEvent>,
    input_tx: Option<mpsc::Sender<InputEvent>>,
    resize_tx: Option<Arc<mpsc::Sender<(u32, u32)>>>,
    last_sent_resize: Option<(u32, u32)>,
    volume_control: Arc<crate::channels::playback::VolumeControl>,

    // Display state
    surfaces: HashMap<(u8, u32), DisplaySurface>,

    // Cursor state
    cursor_pos: (u16, u16),
    cursor_visible: bool,
    cursor_image: Option<CursorImage>,
    cursor_texture: Option<egui::TextureHandle>,

    // Screen-space rect of the rendered SPICE surface
    surface_rect: egui::Rect,

    // Statistics
    stats: Statistics,

    // Cadence mode
    cadence_enabled: bool,
    last_cadence_key: Instant,

    // Session state
    connected: bool,
    error_message: Option<String>,
    mouse_mode: u32, // 1=server, 2=client
    show_disconnect_dialog: bool,
    disconnect_reason: Option<String>,

    // Last mouse position sent (to avoid flooding with duplicates)
    last_mouse_pos: Option<(u32, u32)>,
    last_modifiers: Option<egui::Modifiers>,

    // Bitmask of mouse buttons we have forwarded as pressed to the
    // inputs channel.  Used to send synthetic releases when input
    // forwarding is suppressed (e.g. bug report dialog opens).
    forwarded_buttons: u32,

    // Pending viewport resize from a new surface
    pending_resize: Option<(f32, f32)>,

    // Bandwidth tracking for the status bar sparkline
    bandwidth: BandwidthTracker,

    // Capture session (None when --capture is not specified)
    capture: Option<Arc<CaptureSession>>,

    // USB command sender and state
    usb_tx: Option<mpsc::Sender<UsbCommand>>,
    usb_channel_ready: bool,
    usb_connecting: bool,
    usb_disconnecting: bool,
    usb_error_message: Option<String>,
    usb_error_time: Option<Instant>,
    usb_device_description: Option<String>,
    usb_connected_at: Option<Instant>,

    // Traffic ring buffers (always active, for bug reports and traffic viewer)
    traffic: Arc<TrafficBuffers>,

    // Channel state snapshots (always active, for bug reports)
    channel_snapshots: ChannelSnapshots,
    app_snapshot: Arc<std::sync::Mutex<AppSnapshot>>,

    // Connection target for bug report metadata
    target_host: String,
    target_port: u16,

    // Bug report dialog state
    show_bug_dialog: bool,
    bug_report_type: BugReportType,
    bug_description: String,
    bug_status_message: Option<(String, Instant)>,

    // Region selection state (Display bug reports)
    region_select_active: bool,
    region_drag_start: Option<(u32, u32)>,
    region_drag_end: Option<(u32, u32)>,

    // USB panel state
    show_usb_panel: bool,
    usb_available_devices: Vec<UsbDeviceInfo>,
    usb_virtual_disks: Vec<(PathBuf, bool)>,
    usb_devices_enumerated: bool,

    // File picker for adding virtual disks
    usb_add_disk_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    usb_add_disk_readonly: bool,
    usb_add_disk_message: Option<String>,

    // WebDAV panel state
    show_webdav_panel: bool,
    webdav_tx: Option<mpsc::Sender<WebdavCommand>>,
    webdav_channel_ready: bool,
    webdav_shared_dir: Option<String>,
    webdav_read_only: bool,
    webdav_sharing_active: bool,
    webdav_connected_at: Option<Instant>,
    webdav_error_message: Option<String>,
    webdav_error_time: Option<Instant>,
    webdav_pick_dir_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    webdav_pick_dir_readonly: bool,

    // Traffic viewer state
    show_traffic_viewer: bool,
    traffic_viewer_entries: Vec<TrafficViewEntry>,
    traffic_viewer_last_refresh: Instant,
    traffic_viewer_paused: bool,
    traffic_filter_main: bool,
    traffic_filter_display: bool,
    traffic_filter_inputs: bool,
    traffic_filter_cursor: bool,
    traffic_filter_usbredir: bool,
    traffic_filter_webdav: bool,
}

impl RyllApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        cadence: bool,
        virtual_disks: Vec<VirtualDiskConfig>,
        share_dir: Option<ShareDirConfig>,
        capture: Option<Arc<CaptureSession>>,
        monitors: u8,
    ) -> Self {
        // Create event and command channels
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);
        let (usb_tx, usb_rx) = mpsc::channel(16);
        let (webdav_tx, webdav_rx) = mpsc::channel(16);
        let (resize_tx, resize_rx) = mpsc::channel(32);
        let resize_tx = Arc::new(resize_tx);
        let volume_control = crate::channels::playback::VolumeControl::new();

        // Shared byte counter for bandwidth tracking
        let byte_counter = Arc::new(ByteCounter::new());

        // Traffic ring buffers (always active)
        let traffic = Arc::new(TrafficBuffers::new());

        // Channel state snapshots (always active)
        let channel_snapshots = ChannelSnapshots::new();
        let app_snapshot = Arc::new(std::sync::Mutex::new(AppSnapshot::default()));

        // Save connection target for bug report metadata
        let target_host = config.host.clone();
        let target_port = config.port;

        // Retain virtual disk paths for UI re-enumeration
        let usb_virtual_disks: Vec<(PathBuf, bool)> = virtual_disks
            .iter()
            .map(|d| (d.path.clone(), d.read_only))
            .collect();

        // Spawn connection task
        let config_clone = config.clone();
        let event_tx_clone = event_tx.clone();
        let resize_rx_for_conn = resize_rx;
        let ctx = cc.egui_ctx.clone();
        let capture_clone = capture.clone();
        let counter_clone = byte_counter.clone();
        let traffic_clone = traffic.clone();
        let snaps_for_conn = ChannelSnapshots {
            display: channel_snapshots.display.clone(),
            inputs: channel_snapshots.inputs.clone(),
            cursor: channel_snapshots.cursor.clone(),
            main: channel_snapshots.main.clone(),
        };

        let vol_for_conn = volume_control.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                if let Err(e) = run_connection(
                    config_clone,
                    event_tx_clone,
                    input_rx,
                    usb_rx,
                    webdav_rx,
                    virtual_disks,
                    share_dir,
                    capture_clone,
                    counter_clone,
                    traffic_clone,
                    snaps_for_conn,
                    monitors,
                    resize_rx_for_conn,
                    vol_for_conn,
                )
                .await
                {
                    error!("app: connection error: {}", e);
                }
            });
            // Request repaint when connection changes
            ctx.request_repaint();
        });

        RyllApp {
            event_rx,
            input_tx: Some(input_tx),
            resize_tx: Some(resize_tx),
            last_sent_resize: None,
            volume_control,
            surfaces: HashMap::new(),
            cursor_pos: (0, 0),
            cursor_visible: true,
            cursor_image: None,
            cursor_texture: None,
            surface_rect: egui::Rect::NOTHING,
            stats: Statistics::default(),
            cadence_enabled: cadence,
            last_cadence_key: Instant::now(),
            connected: false,
            error_message: None,
            mouse_mode: 0,
            show_disconnect_dialog: false,
            disconnect_reason: None,
            last_mouse_pos: None,
            last_modifiers: None,
            forwarded_buttons: 0,
            pending_resize: None,
            bandwidth: BandwidthTracker::new(byte_counter),
            capture,
            usb_tx: Some(usb_tx),
            webdav_tx: Some(webdav_tx),
            usb_channel_ready: false,
            usb_connecting: false,
            usb_disconnecting: false,
            usb_error_message: None,
            usb_error_time: None,
            usb_device_description: None,
            usb_connected_at: None,
            traffic,
            channel_snapshots,
            app_snapshot,
            target_host,
            target_port,
            show_bug_dialog: false,
            bug_report_type: BugReportType::Display,
            bug_description: String::new(),
            bug_status_message: None,
            region_select_active: false,
            region_drag_start: None,
            region_drag_end: None,
            show_usb_panel: false,
            usb_available_devices: Vec::new(),
            usb_virtual_disks,
            usb_devices_enumerated: false,
            usb_add_disk_rx: None,
            usb_add_disk_readonly: false,
            usb_add_disk_message: None,
            show_webdav_panel: false,
            webdav_channel_ready: false,
            webdav_shared_dir: None,
            webdav_read_only: false,
            webdav_sharing_active: false,
            webdav_connected_at: None,
            webdav_error_message: None,
            webdav_error_time: None,
            webdav_pick_dir_rx: None,
            webdav_pick_dir_readonly: false,
            show_traffic_viewer: false,
            traffic_viewer_entries: Vec::new(),
            traffic_viewer_last_refresh: Instant::now(),
            traffic_viewer_paused: false,
            traffic_filter_main: true,
            traffic_filter_display: true,
            traffic_filter_inputs: true,
            traffic_filter_cursor: true,
            traffic_filter_usbredir: true,
            traffic_filter_webdav: true,
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                ChannelEvent::SessionInitialized(session_id) => {
                    info!("app: session {} initialized", session_id);
                    self.connected = true;
                }

                ChannelEvent::SurfaceCreated {
                    display_channel_id,
                    surface_id,
                    width,
                    height,
                } => {
                    info!(
                        "app: surface {}:{} created: {}x{}",
                        display_channel_id, surface_id, width, height
                    );
                    self.surfaces.insert(
                        (display_channel_id, surface_id),
                        DisplaySurface::new(surface_id, width, height),
                    );
                    self.pending_resize = Some((width as f32, height as f32));
                }

                ChannelEvent::SurfaceDestroyed {
                    display_channel_id,
                    surface_id,
                } => {
                    info!(
                        "app: surface {}:{} destroyed",
                        display_channel_id, surface_id
                    );
                    self.surfaces.remove(&(display_channel_id, surface_id));
                }

                ChannelEvent::ImageReady {
                    display_channel_id,
                    surface_id,
                    left,
                    top,
                    width,
                    height,
                    pixels,
                    ..
                } => {
                    // Auto-create surface if the server draws before sending
                    // SURFACE_CREATE (QEMU does this for the primary surface).
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        self.surfaces.entry((display_channel_id, surface_id))
                    {
                        let surf_w = left + width;
                        let surf_h = top + height;
                        info!(
                            "app: auto-creating surface {} ({}x{}) from draw at ({},{})+{}x{}",
                            surface_id, surf_w, surf_h, left, top, width, height
                        );
                        e.insert(DisplaySurface::new(surface_id, surf_w, surf_h));
                        self.pending_resize = Some((surf_w as f32, surf_h as f32));
                    }

                    let surface = self
                        .surfaces
                        .get_mut(&(display_channel_id, surface_id))
                        .unwrap();
                    surface.blit(left, top, width, height, &pixels);
                    self.stats.frames_received += 1;
                    debug!(
                        "app: blit surface={}, pos=({},{}), size={}x{}",
                        surface_id, left, top, width, height
                    );
                }

                ChannelEvent::DisplayMark => {
                    // Frame boundary — record timestamp for FPS calculation
                    let now = Instant::now();
                    self.stats.frame_times.push(now);
                    if self.stats.frame_times.len() > FPS_WINDOW_SIZE {
                        self.stats.frame_times.remove(0);
                    }

                    // Capture a video frame if enabled
                    if let Some(ref capture) = self.capture {
                        if let Some(surface) = self
                            .surfaces
                            .values()
                            .max_by_key(|s| (s.width as u64) * (s.height as u64))
                        {
                            capture.frame(0, surface.pixels(), surface.width, surface.height);
                        }
                    }
                }

                ChannelEvent::CursorPosition { x, y, visible } => {
                    info!("app: cursor position: ({},{}) visible={}", x, y, visible);
                    self.cursor_pos = (x, y);
                    self.cursor_visible = visible;
                }

                ChannelEvent::CursorShape(img) => {
                    info!(
                        "app: cursor shape: {}x{}, hot=({},{})",
                        img.width, img.height, img.hot_spot_x, img.hot_spot_y
                    );
                    self.cursor_image = Some(img);
                    self.cursor_texture = None; // force recreation
                }

                ChannelEvent::MouseMode(mode) => {
                    info!(
                        "app: mouse mode: {} ({})",
                        mode,
                        if mode == 1 { "server" } else { "client" }
                    );
                    self.mouse_mode = mode;
                }

                ChannelEvent::MonitorsConfig { width, height } => {
                    debug!("app: requested monitors config {}x{}", width, height);
                }

                ChannelEvent::ClipboardReceived { text } => {
                    info!("app: clipboard from guest ({} bytes)", text.len());
                    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)) {
                        Ok(()) => {}
                        Err(e) => debug!("app: failed to set host clipboard: {}", e),
                    }
                }

                ChannelEvent::Statistics {
                    bytes_in,
                    bytes_out,
                    ..
                } => {
                    self.stats.bytes_in += bytes_in;
                    self.stats.bytes_out += bytes_out;
                }

                ChannelEvent::Latency { key_timestamp } => {
                    self.stats.last_latency = Some(key_timestamp);
                }

                ChannelEvent::Error(msg) => {
                    error!("app: channel error: {}", msg);
                    self.show_disconnect_dialog = true;
                    self.disconnect_reason = Some(msg);
                }

                ChannelEvent::UsbChannelReady => {
                    info!("app: USB redirection channel connected");
                    self.usb_channel_ready = true;
                }

                ChannelEvent::UsbDeviceConnected(desc) => {
                    info!("app: USB device connected: {}", desc);
                    self.usb_device_description = Some(desc);
                    self.clear_usb_operation_flags();
                    self.usb_connected_at = Some(Instant::now());
                }

                ChannelEvent::UsbDeviceDisconnected => {
                    info!("app: USB device disconnected");
                    self.usb_device_description = None;
                    self.clear_usb_operation_flags();
                    self.usb_connected_at = None;
                }

                ChannelEvent::UsbConnectFailed(err) => {
                    error!("app: USB connect failed: {}", err);
                    self.clear_usb_operation_flags();
                    self.usb_error_message = Some(err);
                    self.usb_error_time = Some(Instant::now());
                }

                ChannelEvent::WebdavChannelReady => {
                    info!("app: WebDAV channel connected");
                    self.webdav_channel_ready = true;
                }

                ChannelEvent::WebdavSharingStarted { path, read_only } => {
                    info!("app: WebDAV sharing started: {} (ro={})", path, read_only);
                    self.webdav_shared_dir = Some(path);
                    self.webdav_read_only = read_only;
                    self.webdav_sharing_active = true;
                    self.webdav_connected_at = Some(Instant::now());
                }

                ChannelEvent::WebdavSharingStopped => {
                    info!("app: WebDAV sharing stopped");
                    self.webdav_shared_dir = None;
                    self.webdav_sharing_active = false;
                    self.webdav_connected_at = None;
                }

                ChannelEvent::WebdavError(err) => {
                    error!("app: WebDAV error: {}", err);
                    self.webdav_error_message = Some(err);
                    self.webdav_error_time = Some(Instant::now());
                }

                ChannelEvent::Disconnected(channel) => {
                    info!("app: channel {} disconnected", channel.name());

                    // Channel-specific cleanup
                    if channel == ChannelType::Usbredir {
                        self.usb_channel_ready = false;
                        self.usb_device_description = None;
                        self.clear_usb_operation_flags();
                        self.usb_connected_at = None;
                    }
                    if channel == ChannelType::Webdav {
                        self.webdav_channel_ready = false;
                        self.webdav_shared_dir = None;
                        self.webdav_sharing_active = false;
                        self.webdav_connected_at = None;
                    }

                    // Only show disconnect dialog for critical channels.
                    // Non-critical channels (USB, WebDAV, Cursor, Playback)
                    // have independent lifecycles and their disconnect does
                    // not mean the session is over.
                    match channel {
                        ChannelType::Main | ChannelType::Display | ChannelType::Inputs => {
                            self.connected = false;
                            if !self.show_disconnect_dialog {
                                self.show_disconnect_dialog = true;
                                self.disconnect_reason = Some(format!(
                                    "Connection lost ({} channel disconnected)",
                                    channel.name()
                                ));
                            }
                        }
                        _ => {
                            debug!(
                                "app: non-critical channel {} disconnected, session continues",
                                channel.name()
                            );
                        }
                    }
                }

                _ => {}
            }
        }

        self.update_app_snapshot();
    }

    /// Clear USB operation-in-progress flags.
    fn clear_usb_operation_flags(&mut self) {
        self.usb_connecting = false;
        self.usb_disconnecting = false;
    }

    /// Sync app-level state to the shared snapshot.
    fn update_app_snapshot(&self) {
        let mut snap = self.app_snapshot.lock().unwrap();

        // FPS from sliding-window frame_times
        snap.fps = if self.stats.frame_times.len() >= 2 {
            let oldest = self.stats.frame_times.first().unwrap();
            let newest = self.stats.frame_times.last().unwrap();
            let elapsed = newest.duration_since(*oldest).as_secs_f64();
            if elapsed > 0.0 {
                (self.stats.frame_times.len() - 1) as f64 / elapsed
            } else {
                0.0
            }
        } else {
            0.0
        };

        snap.bandwidth_history = self.bandwidth.history.clone();
        snap.bandwidth_current = self.bandwidth.history.last().copied().unwrap_or(0.0);
        snap.last_latency = self.stats.last_latency;
        snap.frames_received = self.stats.frames_received;
        snap.surfaces = self
            .surfaces
            .values()
            .map(|s| SurfaceInfo {
                surface_id: s.id,
                width: s.width,
                height: s.height,
            })
            .collect();
        snap.cursor_pos = self.cursor_pos;
        snap.cursor_visible = self.cursor_visible;
        snap.mouse_mode = self.mouse_mode;
        snap.connected = self.connected;
        snap.uptime_secs = self.traffic.elapsed().as_secs_f64();
    }

    /// Generate a bug report and write it to disk.
    /// Returns the path of the written zip file.
    pub fn generate_bug_report(
        &self,
        report_type: BugReportType,
        description: String,
        region: Option<ReportRegion>,
    ) -> anyhow::Result<std::path::PathBuf> {
        // Get surface pixels for display reports
        let surface_data = if report_type == BugReportType::Display {
            self.surfaces
                .values()
                .max_by_key(|s| (s.width as u64) * (s.height as u64))
                .map(|s| (s.pixels(), s.width, s.height))
        } else {
            None
        };

        // Assemble the report
        let report = BugReport::new(
            report_type,
            description,
            region,
            &self.target_host,
            self.target_port,
            &self.traffic,
            &self.channel_snapshots,
            &self.app_snapshot,
            surface_data,
        )?;

        // Determine output directory
        let output_dir = match &self.capture {
            Some(cap) => cap.dir.join("bug-reports"),
            None => std::env::current_dir().unwrap_or_else(|_| ".".into()),
        };

        report.write_zip(&output_dir)
    }

    /// Run a bug report and set the status bar message from the result.
    fn finish_bug_report(
        &mut self,
        report_type: BugReportType,
        description: String,
        region: Option<ReportRegion>,
    ) {
        match self.generate_bug_report(report_type, description, region) {
            Ok(path) => {
                let msg = format!("Bug report saved to {}", path.display());
                info!("app: {}", msg);
                self.bug_status_message = Some((msg, Instant::now()));
            }
            Err(e) => {
                let msg = format!("Bug report failed: {}", e);
                error!("app: {}", msg);
                self.bug_status_message = Some((msg, Instant::now()));
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Don't forward input to the SPICE server when
        // the bug report dialog or region selection is active.
        if self.show_bug_dialog || self.region_select_active {
            return;
        }

        let input_tx = match &self.input_tx {
            Some(tx) => tx.clone(),
            None => return,
        };

        // Handle keyboard input — read from the global input state so
        // key events are captured regardless of which widget has focus.
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key {
                    key,
                    pressed,
                    repeat: false,
                    ..
                } = event
                {
                    if *key == egui::Key::F11 || *key == egui::Key::F12 {
                        continue;
                    }
                    if let Some((down_code, up_code)) = key_to_scancode(*key) {
                        let ev = if *pressed {
                            InputEvent::KeyDown(down_code)
                        } else {
                            InputEvent::KeyUp(up_code)
                        };
                        let _ = input_tx.try_send(ev);
                    }
                }
            }

            let mods = i.modifiers;
            let prev = self.last_modifiers.unwrap_or_default();

            if mods.ctrl != prev.ctrl {
                let code = 0x1D; // Left Ctrl
                if mods.ctrl {
                    let _ = input_tx.try_send(InputEvent::KeyDown(code));
                } else {
                    let _ = input_tx.try_send(InputEvent::KeyUp(code | 0x80));
                }
            }
            if mods.shift != prev.shift {
                let code = 0x2A; // Left Shift
                if mods.shift {
                    let _ = input_tx.try_send(InputEvent::KeyDown(code));
                } else {
                    let _ = input_tx.try_send(InputEvent::KeyUp(code | 0x80));
                }
            }
            if mods.alt != prev.alt {
                let code = 0x38; // Left Alt
                if mods.alt {
                    let _ = input_tx.try_send(InputEvent::KeyDown(code));
                } else {
                    let _ = input_tx.try_send(InputEvent::KeyUp(code | 0x80));
                }
            }

            self.last_modifiers = Some(mods);
        });
    }

    fn handle_cadence(&mut self) {
        if !self.cadence_enabled {
            return;
        }

        let now = Instant::now();
        if now.duration_since(self.last_cadence_key) >= Duration::from_secs(2) {
            if let Some(tx) = &self.input_tx {
                // Send space key
                let _ = tx.try_send(InputEvent::KeyDown(0x39)); // Space down
                let _ = tx.try_send(InputEvent::KeyUp(0xB9)); // Space up
                self.last_cadence_key = now;
            }
        }
    }

    fn maybe_send_monitors_resize(&mut self, ctx: &egui::Context) {
        let Some(tx) = &self.resize_tx else {
            return;
        };

        let viewport_size = ctx.input(|i| {
            i.viewport()
                .inner_rect
                .map(|rect| rect.size())
                .unwrap_or_else(|| i.screen_rect().size())
        });

        let mut width = viewport_size.x.max(0.0) as u32;
        let mut height = (viewport_size.y - STATS_BAR_HEIGHT).max(0.0) as u32;

        width -= width % 8;
        height -= height % 8;
        width = width.max(8);
        height = height.max(8);

        if self.last_sent_resize.is_none() {
            self.last_sent_resize = Some((width, height));
            return;
        }

        if self.last_sent_resize == Some((width, height)) {
            return;
        }

        if tx.try_send((width, height)).is_ok() {
            self.last_sent_resize = Some((width, height));
        }
    }
}

impl eframe::App for RyllApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Graceful shutdown on Ctrl+C: close capture session (flushes
        // the MP4 moov atom) then ask eframe to exit.
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            info!("app: shutdown requested (SIGINT)");
            if let Some(ref capture) = self.capture {
                capture.close();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Process incoming events
        self.process_events();

        // Resize viewport to match the remote surface (plus stats bar)
        if let Some((_w, _h)) = self.pending_resize.take() {
            debug!("app: surface resize {}x{} (not resizing window)", _w, _h);
        }

        self.maybe_send_monitors_resize(ctx);

        // Tick the bandwidth tracker
        self.bandwidth.tick();

        // Expire old status messages
        if let Some((_, created)) = &self.bug_status_message {
            if created.elapsed() >= Duration::from_secs(5) {
                self.bug_status_message = None;
            }
        }

        // Escape during region selection: skip and generate without region
        if self.region_select_active {
            let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc {
                let report_type = self.bug_report_type;
                let description = self.bug_description.clone();
                self.finish_bug_report(report_type, description, None);
                self.region_select_active = false;
                self.region_drag_start = None;
                self.region_drag_end = None;
            }
        }

        // F12 toggles bug report dialog (not during region selection)
        if !self.region_select_active {
            let f12_pressed = ctx.input(|i| i.key_pressed(egui::Key::F12));
            if f12_pressed {
                self.show_bug_dialog = !self.show_bug_dialog;
                if self.show_bug_dialog {
                    self.bug_report_type = BugReportType::Display;
                    self.bug_description.clear();
                }
            }
        }

        // F11 toggles traffic viewer (not during region selection)
        if !self.region_select_active {
            let f11_pressed = ctx.input(|i| i.key_pressed(egui::Key::F11));
            if f11_pressed {
                self.show_traffic_viewer = !self.show_traffic_viewer;
            }
        }

        // Escape closes the dialog
        if self.show_bug_dialog {
            let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc {
                self.show_bug_dialog = false;
            }
        }

        // Handle input
        self.handle_input(ctx);

        // Handle cadence mode
        self.handle_cadence();

        // Refresh traffic viewer entries periodically
        if self.show_traffic_viewer
            && !self.traffic_viewer_paused
            && self.traffic_viewer_last_refresh.elapsed()
                >= Duration::from_millis(TRAFFIC_VIEWER_REFRESH_MS)
        {
            self.traffic_viewer_entries =
                self.traffic.recent_view_entries(TRAFFIC_VIEWER_MAX_ENTRIES);
            self.traffic_viewer_last_refresh = Instant::now();
        }

        // Statistics panel (bottom) — rendered before CentralPanel
        // so egui reserves its space correctly.
        let stats_frame = egui::Frame::none()
            .inner_margin(egui::Margin::symmetric(4.0, 2.0))
            .fill(ctx.style().visuals.panel_fill);
        egui::TopBottomPanel::bottom("stats")
            .frame(stats_frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(latency) = self.stats.last_latency {
                        ui.label(format!("Latency: {:.1}ms", latency * 1000.0));
                        ui.separator();
                    }

                    // Sliding-window FPS from DisplayMark timestamps
                    if self.stats.frame_times.len() >= 2 {
                        let oldest = self.stats.frame_times.first().unwrap();
                        let newest = self.stats.frame_times.last().unwrap();
                        let elapsed = newest.duration_since(*oldest).as_secs_f64();
                        if elapsed > 0.0 {
                            let fps = (self.stats.frame_times.len() - 1) as f64 / elapsed;
                            ui.label(format!("FPS: {:.1}", fps));
                        }
                    }

                    ui.separator();
                    if self.mouse_mode == 1 {
                        ui.label("Cursor: server mode");
                    } else {
                        ui.label(format!(
                            "Cursor: ({}, {}) {}",
                            self.cursor_pos.0,
                            self.cursor_pos.1,
                            if self.cursor_visible {
                                "visible"
                            } else {
                                "hidden"
                            }
                        ));
                    }

                    if self.cadence_enabled {
                        ui.separator();
                        ui.label("Cadence: ON");
                    }

                    if let Some(ref desc) = self.usb_device_description {
                        ui.separator();
                        ui.label(format!("USB: {}", desc));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let vol = &self.volume_control;
                        let mut muted = vol.muted();
                        if ui.small_button(if muted { "🔇" } else { "🔊" }).clicked() {
                            muted = !muted;
                            vol.set_muted(muted);
                        }
                        let mut v = vol.volume() as f32;
                        let slider = egui::Slider::new(&mut v, 0.0..=100.0).show_value(false);
                        if ui
                            .add_sized([80.0, ui.available_height()], slider)
                            .changed()
                        {
                            vol.set_volume(v as u8);
                        }
                        ui.separator();

                        ui.allocate_ui_with_layout(
                            egui::vec2(75.0, ui.available_height()),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(self.bandwidth.label());
                            },
                        );
                        if self.bandwidth.history.len() >= 2 {
                            let max_val = self
                                .bandwidth
                                .history
                                .iter()
                                .cloned()
                                .fold(1.0f32, f32::max);
                            let sparkline_w = 80.0;
                            let sparkline_h = 12.0;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(sparkline_w, sparkline_h),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter_at(rect);
                            let n = self.bandwidth.history.len();
                            let bar_w = sparkline_w / n as f32;
                            for (i, &val) in self.bandwidth.history.iter().enumerate() {
                                let h = (val / max_val) * sparkline_h;
                                let x = rect.min.x + i as f32 * bar_w;
                                let bar = egui::Rect::from_min_max(
                                    egui::pos2(x, rect.max.y - h),
                                    egui::pos2(x + bar_w - 0.5, rect.max.y),
                                );
                                painter.rect_filled(bar, 0.0, egui::Color32::from_rgb(80, 180, 80));
                            }
                        }

                        ui.separator();
                        if ui.small_button("Traffic").clicked() {
                            self.show_traffic_viewer = !self.show_traffic_viewer;
                        }
                        if ui.small_button("USB").clicked() {
                            self.show_usb_panel = !self.show_usb_panel;
                        }
                        if ui.small_button("Folders").clicked() {
                            self.show_webdav_panel = !self.show_webdav_panel;
                        }
                        if ui.small_button("Report").clicked() {
                            self.show_bug_dialog = true;
                            self.bug_report_type = BugReportType::Display;
                            self.bug_description.clear();
                        }

                        // Transient status message from bug report
                        if let Some((ref msg, created)) = self.bug_status_message {
                            if created.elapsed() < Duration::from_secs(5) {
                                ui.separator();
                                ui.label(msg);
                            }
                        }
                    });
                });
            });

        // Traffic viewer side panel (conditional)
        if self.show_traffic_viewer {
            egui::SidePanel::right("traffic_viewer")
                .default_width(350.0)
                .show(ctx, |ui| {
                    // Header
                    ui.horizontal(|ui| {
                        ui.heading("Traffic");
                        if ui
                            .small_button(if self.traffic_viewer_paused {
                                "Resume"
                            } else {
                                "Pause"
                            })
                            .clicked()
                        {
                            self.traffic_viewer_paused = !self.traffic_viewer_paused;
                        }
                        ui.label(format!("{} msgs", self.traffic_viewer_entries.len()));
                    });

                    // Channel filters
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.traffic_filter_main, "Main");
                        ui.checkbox(&mut self.traffic_filter_display, "Display");
                        ui.checkbox(&mut self.traffic_filter_inputs, "Inputs");
                        ui.checkbox(&mut self.traffic_filter_cursor, "Cursor");
                        ui.checkbox(&mut self.traffic_filter_usbredir, "USB");
                        ui.checkbox(&mut self.traffic_filter_webdav, "WebDAV");
                    });
                    ui.separator();

                    // Scrollable message list
                    let stick = !self.traffic_viewer_paused;
                    let now_elapsed = self.traffic.elapsed();
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(stick)
                        .show(ui, |ui| {
                            for entry in &self.traffic_viewer_entries {
                                // Apply channel filter
                                let visible = match entry.channel {
                                    "main" => self.traffic_filter_main,
                                    "display" => self.traffic_filter_display,
                                    "inputs" => self.traffic_filter_inputs,
                                    "cursor" => self.traffic_filter_cursor,
                                    "usbredir" => self.traffic_filter_usbredir,
                                    "webdav" => self.traffic_filter_webdav,
                                    _ => true,
                                };
                                if !visible {
                                    continue;
                                }

                                let relative =
                                    entry.timestamp.as_secs_f64() - now_elapsed.as_secs_f64();
                                let dir = match entry.direction {
                                    TrafficDirection::Sent => "\u{2192}",
                                    TrafficDirection::Received => "\u{2190}",
                                };
                                let channel_color = match entry.channel {
                                    "main" => egui::Color32::from_rgb(120, 160, 255),
                                    "display" => egui::Color32::from_rgb(100, 200, 100),
                                    "inputs" => egui::Color32::from_rgb(255, 180, 80),
                                    "cursor" => egui::Color32::from_rgb(200, 130, 255),
                                    "usbredir" => egui::Color32::from_rgb(255, 100, 100),
                                    "webdav" => egui::Color32::from_rgb(100, 200, 200),
                                    _ => egui::Color32::GRAY,
                                };
                                let size_str = format_size(entry.wire_size);

                                ui.horizontal(|ui| {
                                    ui.monospace(format!("{:>7.1}s", relative));
                                    ui.colored_label(
                                        channel_color,
                                        format!("{:<8}", entry.channel),
                                    );
                                    ui.monospace(dir);
                                    ui.label(entry.message_name);
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.monospace(size_str);
                                        },
                                    );
                                });
                            }
                        });
                });
        }

        // USB device management panel (conditional)
        if self.show_usb_panel {
            // Auto-enumerate on first open
            if !self.usb_devices_enumerated {
                self.usb_available_devices = usb::enumerate_devices(&self.usb_virtual_disks);
                self.usb_devices_enumerated = true;
            }

            // Poll for file picker result
            let mut picked_path = None;
            if let Some(ref rx) = self.usb_add_disk_rx {
                if let Ok(result) = rx.try_recv() {
                    picked_path = Some(result);
                }
            }
            if picked_path.is_some() {
                self.usb_add_disk_rx = None;
            }
            if let Some(Some(path)) = picked_path {
                self.usb_add_disk_message = None;
                match std::fs::metadata(&path) {
                    Ok(meta) => {
                        if !meta.is_file() {
                            self.usb_add_disk_message =
                                Some("Selected path is not a regular file.".to_string());
                        } else if meta.len() < 512 {
                            self.usb_add_disk_message =
                                Some("File is too small (< 512 bytes).".to_string());
                        } else {
                            let read_only = self.usb_add_disk_readonly;
                            self.usb_virtual_disks.push((path.clone(), read_only));
                            self.usb_available_devices =
                                usb::enumerate_devices(&self.usb_virtual_disks);
                            let warn = if meta.len() % 512 != 0 {
                                " (warning: size not a multiple of 512)"
                            } else {
                                ""
                            };
                            let ro = if read_only { " [RO]" } else { "" };
                            self.usb_add_disk_message =
                                Some(format!("Added: {}{}{}", path.display(), ro, warn));
                        }
                    }
                    Err(e) => {
                        self.usb_add_disk_message = Some(format!("Cannot read file: {}", e));
                    }
                }
            }

            // Auto-clear USB errors after 10 seconds
            if let Some(error_time) = self.usb_error_time {
                if error_time.elapsed() > Duration::from_secs(10) {
                    self.usb_error_message = None;
                    self.usb_error_time = None;
                }
            }

            // Request repaint for elapsed timer and error auto-clear
            if self.usb_connected_at.is_some() || self.usb_error_time.is_some() {
                ctx.request_repaint_after(Duration::from_secs(1));
            }

            let mut usb_action = None;
            let mut open_usb_bug_report = false;

            egui::SidePanel::right("usb_panel")
                .default_width(300.0)
                .show(ctx, |ui| {
                    // Header with refresh button
                    ui.horizontal(|ui| {
                        ui.heading("USB Devices");
                        if ui.small_button("Refresh").clicked() {
                            self.usb_available_devices =
                                usb::enumerate_devices(&self.usb_virtual_disks);
                        }
                    });
                    ui.separator();

                    // Channel status
                    if self.usb_channel_ready {
                        ui.label("Channel: Ready");
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "Channel: Not available");
                    }

                    // Connected device with elapsed time
                    if let Some(ref desc) = self.usb_device_description {
                        ui.separator();
                        let elapsed = self
                            .usb_connected_at
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let mins = elapsed / 60;
                        let secs = elapsed % 60;
                        ui.label(format!("Connected: {} ({}m {}s)", desc, mins, secs));
                    }

                    // Error message with dismiss and bug report buttons
                    if self.usb_error_message.is_some() {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::RED,
                            self.usb_error_message.as_ref().unwrap(),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.usb_error_message = None;
                                self.usb_error_time = None;
                            }
                            if ui.small_button("Report this as a bug").clicked() {
                                open_usb_bug_report = true;
                            }
                        });
                    }

                    // Operation in progress indicator
                    if self.usb_connecting {
                        ui.separator();
                        ui.label("Connecting...");
                    } else if self.usb_disconnecting {
                        ui.separator();
                        ui.label("Disconnecting...");
                    }

                    ui.separator();

                    // Device list with connect/disconnect buttons
                    let buttons_disabled =
                        !self.usb_channel_ready || self.usb_connecting || self.usb_disconnecting;
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if self.usb_available_devices.is_empty() {
                            ui.colored_label(egui::Color32::GRAY, "No USB devices found.");
                        } else {
                            for device in &self.usb_available_devices {
                                let label = device.label();
                                let is_connected = self
                                    .usb_device_description
                                    .as_ref()
                                    .is_some_and(|d| *d == label);

                                ui.horizontal(|ui| {
                                    if is_connected {
                                        ui.colored_label(egui::Color32::GREEN, "\u{25CF}");
                                        ui.label(&label);
                                        if ui
                                            .add_enabled(
                                                !buttons_disabled,
                                                egui::Button::new("Disconnect"),
                                            )
                                            .clicked()
                                        {
                                            usb_action = Some(UsbCommand::DisconnectDevice);
                                        }
                                    } else {
                                        ui.label(&label);
                                        let connect_enabled = !buttons_disabled
                                            && self.usb_device_description.is_none();
                                        if ui
                                            .add_enabled(
                                                connect_enabled,
                                                egui::Button::new("Connect"),
                                            )
                                            .clicked()
                                        {
                                            usb_action = Some(match &device.source {
                                                #[cfg(target_os = "linux")]
                                                DeviceSource::Physical { bus, address } => {
                                                    UsbCommand::ConnectPhysical {
                                                        bus: *bus,
                                                        address: *address,
                                                    }
                                                }
                                                DeviceSource::VirtualDisk { path, read_only } => {
                                                    UsbCommand::ConnectVirtualDisk {
                                                        path: path.clone(),
                                                        read_only: *read_only,
                                                    }
                                                }
                                            });
                                        }
                                    }
                                });
                            }
                        }
                    });

                    // Add virtual disk section
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.usb_add_disk_readonly, "Read-only");
                        let picker_active = self.usb_add_disk_rx.is_some();
                        if ui
                            .add_enabled(!picker_active, egui::Button::new("Add Disk..."))
                            .clicked()
                        {
                            let (tx, rx) = std::sync::mpsc::channel();
                            std::thread::spawn(move || {
                                let result = rfd::FileDialog::new()
                                    .set_title("Select RAW disk image")
                                    .add_filter("Disk images", &["raw", "img"])
                                    .add_filter("All files", &["*"])
                                    .pick_file();
                                let _ = tx.send(result);
                            });
                            self.usb_add_disk_rx = Some(rx);
                        }
                    });

                    // Add-disk message
                    if let Some(ref msg) = self.usb_add_disk_message {
                        if msg.starts_with("Added:") {
                            ui.label(msg);
                        } else {
                            ui.colored_label(egui::Color32::RED, msg);
                        }
                    }
                });

            // Execute USB action outside the closure
            if let Some(cmd) = usb_action {
                self.usb_error_message = None;
                self.usb_error_time = None;
                let is_disconnect = matches!(cmd, UsbCommand::DisconnectDevice);
                if is_disconnect {
                    self.usb_disconnecting = true;
                } else {
                    self.usb_connecting = true;
                }
                if let Some(ref tx) = self.usb_tx {
                    if let Err(e) = tx.try_send(cmd) {
                        self.usb_connecting = false;
                        self.usb_disconnecting = false;
                        self.usb_error_message = Some(format!("Failed to send command: {}", e));
                        self.usb_error_time = Some(Instant::now());
                    }
                }
            }

            // Open bug report dialog for USB error (two-pass)
            if open_usb_bug_report {
                self.show_bug_dialog = true;
                self.bug_report_type = BugReportType::Usb;
                self.bug_description = self.usb_error_message.clone().unwrap_or_default();
            }
        }

        // ── WebDAV Folders panel ─────────────────────────
        if self.show_webdav_panel {
            // Poll directory picker result
            let mut picked_dir = None;
            if let Some(ref rx) = self.webdav_pick_dir_rx {
                if let Ok(result) = rx.try_recv() {
                    picked_dir = Some(result);
                }
            }
            if picked_dir.is_some() {
                self.webdav_pick_dir_rx = None;
            }

            let mut webdav_action = None;

            if let Some(Some(path)) = picked_dir {
                if path.is_dir() {
                    webdav_action = Some(WebdavCommand::ShareDirectory {
                        path,
                        read_only: self.webdav_pick_dir_readonly,
                    });
                }
            }

            // Auto-clear WebDAV errors after 10 seconds
            if let Some(error_time) = self.webdav_error_time {
                if error_time.elapsed() > Duration::from_secs(10) {
                    self.webdav_error_message = None;
                    self.webdav_error_time = None;
                }
            }

            // Request repaint for elapsed timer and error auto-clear
            if self.webdav_connected_at.is_some() || self.webdav_error_time.is_some() {
                ctx.request_repaint_after(Duration::from_secs(1));
            }

            egui::SidePanel::right("webdav_panel")
                .default_width(300.0)
                .show(ctx, |ui| {
                    ui.heading("Shared Folders");
                    ui.separator();

                    // Channel status
                    if self.webdav_channel_ready {
                        ui.label("Channel: Ready");
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "Channel: Not available");
                    }

                    // Active share with elapsed timer
                    if self.webdav_sharing_active {
                        if let Some(ref dir) = self.webdav_shared_dir {
                            ui.separator();
                            let elapsed = self
                                .webdav_connected_at
                                .map(|t| t.elapsed().as_secs())
                                .unwrap_or(0);
                            let mins = elapsed / 60;
                            let secs = elapsed % 60;
                            let ro = if self.webdav_read_only { " [RO]" } else { "" };
                            ui.label(format!("Sharing: {}{} ({}m {}s)", dir, ro, mins, secs));
                            if ui.button("Stop Sharing").clicked() {
                                webdav_action = Some(WebdavCommand::StopSharing);
                            }
                        }
                    }

                    // Error display
                    if self.webdav_error_message.is_some() {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::RED,
                            self.webdav_error_message.as_ref().unwrap(),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.webdav_error_message = None;
                                self.webdav_error_time = None;
                            }
                        });
                    }

                    // Share controls (when not sharing)
                    if !self.webdav_sharing_active {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.webdav_pick_dir_readonly, "Read-only");
                            let picker_active = self.webdav_pick_dir_rx.is_some();
                            let enabled = self.webdav_channel_ready && !picker_active;
                            if ui
                                .add_enabled(enabled, egui::Button::new("Share Directory..."))
                                .clicked()
                            {
                                let (tx, rx) = std::sync::mpsc::channel();
                                std::thread::spawn(move || {
                                    let result = rfd::FileDialog::new()
                                        .set_title("Select directory to share")
                                        .pick_folder();
                                    let _ = tx.send(result);
                                });
                                self.webdav_pick_dir_rx = Some(rx);
                            }
                        });
                    }
                });

            // Execute WebDAV action outside the closure
            if let Some(cmd) = webdav_action {
                self.webdav_error_message = None;
                self.webdav_error_time = None;
                if let Some(ref tx) = self.webdav_tx {
                    if let Err(e) = tx.try_send(cmd) {
                        self.webdav_error_message = Some(format!("Failed to send command: {}", e));
                        self.webdav_error_time = Some(Instant::now());
                    }
                }
            }
        }

        // Main display area (no margin so the surface fills edge-to-edge)
        let mut open_channel_bug_report = false;
        let panel_frame = egui::Frame::none().inner_margin(0.0);
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                if self.error_message.is_some() {
                    ui.colored_label(
                        egui::Color32::RED,
                        format!("Error: {}", self.error_message.as_ref().unwrap()),
                    );
                    ui.horizontal(|ui| {
                        if ui.small_button("Dismiss").clicked() {
                            self.error_message = None;
                        }
                        if ui.small_button("Report this as a bug").clicked() {
                            open_channel_bug_report = true;
                        }
                    });
                    ui.separator();
                }

                if !self.connected {
                    ui.centered_and_justified(|ui| {
                        ui.label("Connecting...");
                    });
                    return;
                }

                let mut keys: Vec<(u8, u32)> = self.surfaces.keys().copied().collect();
                keys.sort_unstable();
                let primary_key = keys
                    .iter()
                    .copied()
                    .find(|(_, sid)| *sid == 0)
                    .or_else(|| keys.first().copied());

                if let Some(primary_key) = primary_key {
                    if let Some(surface) = self.surfaces.get_mut(&primary_key) {
                        let width = surface.width;
                        let height = surface.height;
                        let texture = surface.texture(ctx);
                        let size = egui::vec2(width as f32, height as f32);

                        let response = ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .sense(egui::Sense::click_and_drag()),
                        );

                        self.surface_rect = response.rect;

                        if response.hovered()
                            && !self.region_select_active
                            && self.cursor_image.is_some()
                        {
                            ui.ctx()
                                .output_mut(|o| o.cursor_icon = egui::CursorIcon::None);
                        }

                        let input_suppressed = self.show_bug_dialog || self.region_select_active;
                        if !input_suppressed {
                            if let Some(tx) = &self.input_tx {
                                if let Some(pos) = response.hover_pos() {
                                    let x = (pos.x - response.rect.min.x).max(0.0) as u32;
                                    let y = (pos.y - response.rect.min.y).max(0.0) as u32;
                                    if self.last_mouse_pos != Some((x, y)) {
                                        if self.mouse_mode == MOUSE_MODE_SERVER {
                                            // Server mode: send relative deltas.
                                            let (prev_x, prev_y) =
                                                self.last_mouse_pos.unwrap_or((x, y));
                                            let dx = x as i32 - prev_x as i32;
                                            let dy = y as i32 - prev_y as i32;
                                            let _ = tx.try_send(InputEvent::MouseMotion { dx, dy });
                                        } else {
                                            // Client mode: send absolute position.
                                            let _ = tx.try_send(InputEvent::MouseMove { x, y });
                                        }
                                        self.last_mouse_pos = Some((x, y));
                                    }
                                }

                                ctx.input(|i| {
                                    let pos = self.last_mouse_pos.unwrap_or((0, 0));
                                    for button in [
                                        egui::PointerButton::Primary,
                                        egui::PointerButton::Secondary,
                                        egui::PointerButton::Middle,
                                    ] {
                                        if i.pointer.button_pressed(button) {
                                            let spice_btn = mouse_button_to_spice(button);
                                            self.forwarded_buttons |= spice_btn;
                                            let _ = tx.try_send(InputEvent::MouseDown {
                                                button: spice_btn,
                                                x: pos.0,
                                                y: pos.1,
                                            });
                                        }
                                        if i.pointer.button_released(button) {
                                            let spice_btn = mouse_button_to_spice(button);
                                            self.forwarded_buttons &= !spice_btn;
                                            let _ = tx.try_send(InputEvent::MouseUp {
                                                button: spice_btn,
                                                x: pos.0,
                                                y: pos.1,
                                            });
                                        }
                                    }

                                    let scroll_y = i.smooth_scroll_delta.y;
                                    if scroll_y.abs() > 0.5 {
                                        let btn = if scroll_y > 0.0 { 0x08 } else { 0x10 };
                                        let _ = tx.try_send(InputEvent::MouseDown {
                                            button: btn,
                                            x: pos.0,
                                            y: pos.1,
                                        });
                                        let _ = tx.try_send(InputEvent::MouseUp {
                                            button: btn,
                                            x: pos.0,
                                            y: pos.1,
                                        });
                                    }
                                });
                            }
                        } else if self.forwarded_buttons != 0 {
                            if let Some(tx) = &self.input_tx {
                                let pos = self.last_mouse_pos.unwrap_or((0, 0));
                                for bit in 0..5u32 {
                                    let mask = 1 << bit;
                                    if self.forwarded_buttons & mask != 0 {
                                        let _ = tx.try_send(InputEvent::MouseUp {
                                            button: mask,
                                            x: pos.0,
                                            y: pos.1,
                                        });
                                    }
                                }
                            }
                            self.forwarded_buttons = 0;
                        }
                    }
                }

                if self.surfaces.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("Waiting for display...");
                    });
                }
            });

        // Open bug report dialog for channel error (two-pass)
        if open_channel_bug_report {
            self.show_bug_dialog = true;
            self.bug_report_type = BugReportType::Connection;
            self.bug_description = self.error_message.clone().unwrap_or_default();
        }

        // Bug report dialog (two-pass: render then act)
        let mut dialog_action = None;
        if self.show_bug_dialog {
            egui::Window::new("Bug Report")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(350.0);

                    ui.label(
                        "Bug reports may contain sensitive data including \
                         screen contents, typed keystrokes, and protocol \
                         traffic. Review the report before sharing and \
                         ensure no confidential information is visible on \
                         screen or was recently typed.",
                    );
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label("Report type:");
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Display,
                        "Display (screenshot + image state)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Input,
                        "Input (keyboard + mouse state)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Cursor,
                        "Cursor (cursor cache + position)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Connection,
                        "Connection (session + main channel)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Usb,
                        "USB (usbredir channel + device state)",
                    );

                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label("Description (optional):");
                    ui.text_edit_singleline(&mut self.bug_description);

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Capture").clicked() {
                            dialog_action = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            dialog_action = Some(false);
                        }
                    });
                });
        }

        // Execute dialog action outside the closure
        match dialog_action {
            Some(true) => {
                if self.bug_report_type == BugReportType::Display {
                    // Enter region selection mode for display reports
                    self.region_select_active = true;
                    self.region_drag_start = None;
                    self.region_drag_end = None;
                } else {
                    // Non-display: generate immediately
                    let report_type = self.bug_report_type;
                    let description = self.bug_description.clone();
                    self.finish_bug_report(report_type, description, None);
                }
                self.show_bug_dialog = false;
            }
            Some(false) => {
                self.show_bug_dialog = false;
            }
            None => {}
        }

        // Region selection mode: crosshair, drag tracking, overlays
        if self.region_select_active && self.surface_rect != egui::Rect::NOTHING {
            // Show crosshair cursor over the surface
            ctx.output_mut(|o| o.cursor_icon = egui::CursorIcon::Crosshair);

            // Get surface dimensions for clamping
            let surf_w = self.surface_rect.width() as u32;
            let surf_h = self.surface_rect.height() as u32;

            // Track drag (two-pass: collect action, execute outside)
            let mut region_completed = false;
            ctx.input(|i| {
                if i.pointer.primary_pressed() {
                    if let Some(pos) = i.pointer.interact_pos() {
                        let x = ((pos.x - self.surface_rect.min.x).max(0.0) as u32).min(surf_w);
                        let y = ((pos.y - self.surface_rect.min.y).max(0.0) as u32).min(surf_h);
                        self.region_drag_start = Some((x, y));
                        self.region_drag_end = Some((x, y));
                    }
                }
                if i.pointer.primary_down() {
                    if let Some(pos) = i.pointer.interact_pos() {
                        let x = ((pos.x - self.surface_rect.min.x).max(0.0) as u32).min(surf_w);
                        let y = ((pos.y - self.surface_rect.min.y).max(0.0) as u32).min(surf_h);
                        self.region_drag_end = Some((x, y));
                    }
                }
                if i.pointer.primary_released() && self.region_drag_start.is_some() {
                    region_completed = true;
                }
            });

            // Draw instruction banner
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("region_select_banner"),
            ));
            let banner_rect = egui::Rect::from_min_size(
                self.surface_rect.min,
                egui::vec2(self.surface_rect.width(), 28.0),
            );
            painter.rect_filled(
                banner_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180),
            );
            painter.text(
                banner_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Click and drag to select the affected region. Press Escape to skip.",
                egui::FontId::proportional(13.0),
                egui::Color32::WHITE,
            );

            // Draw selection rectangle while dragging
            if let (Some((sx, sy)), Some((ex, ey))) = (self.region_drag_start, self.region_drag_end)
            {
                let left = sx.min(ex) as f32 + self.surface_rect.min.x;
                let top = sy.min(ey) as f32 + self.surface_rect.min.y;
                let right = sx.max(ex) as f32 + self.surface_rect.min.x;
                let bottom = sy.max(ey) as f32 + self.surface_rect.min.y;
                let sel_rect =
                    egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(right, bottom));
                let sel_painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("region_select_rect"),
                ));
                sel_painter.rect_filled(
                    sel_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(255, 0, 0, 60),
                );
                sel_painter.rect_stroke(
                    sel_rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 0, 0)),
                );
            }

            // Generate report on drag release
            if region_completed {
                let (sx, sy) = self.region_drag_start.unwrap();
                let (ex, ey) = self.region_drag_end.unwrap();
                let region = ReportRegion {
                    left: sx.min(ex),
                    top: sy.min(ey),
                    right: sx.max(ex),
                    bottom: sy.max(ey),
                };
                let report_type = self.bug_report_type;
                let description = self.bug_description.clone();
                self.finish_bug_report(report_type, description, Some(region));
                self.region_select_active = false;
                self.region_drag_start = None;
                self.region_drag_end = None;
            }
        }

        // Create a default cursor if the server hasn't sent one yet
        if self.cursor_image.is_none() && self.connected {
            self.cursor_image = Some(CursorImage {
                width: 12,
                height: 19,
                hot_spot_x: 0,
                hot_spot_y: 0,
                pixels: default_arrow_cursor(),
            });
            self.cursor_texture = None;
        }

        // Create cursor texture if we have a new shape
        if self.cursor_image.is_some() && self.cursor_texture.is_none() {
            if let Some(ref img) = self.cursor_image {
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [img.width as usize, img.height as usize],
                    &img.pixels,
                );
                let options = egui::TextureOptions {
                    magnification: egui::TextureFilter::Nearest,
                    minification: egui::TextureFilter::Nearest,
                    ..Default::default()
                };
                self.cursor_texture = Some(ctx.load_texture("spice_cursor", color_image, options));
            }
        }

        // Draw cursor overlay using the painter so it doesn't
        // interfere with mouse input on the surface below.
        // (hidden during region selection — crosshair cursor is shown instead)
        if self.cursor_visible
            && !self.region_select_active
            && self.surface_rect != egui::Rect::NOTHING
        {
            if let (Some(ref tex), Some(ref img)) = (&self.cursor_texture, &self.cursor_image) {
                let (cx, cy) = self
                    .last_mouse_pos
                    .map(|(x, y)| (x as f32, y as f32))
                    .unwrap_or((self.cursor_pos.0 as f32, self.cursor_pos.1 as f32));

                let x = self.surface_rect.min.x + cx - img.hot_spot_x as f32;
                let y = self.surface_rect.min.y + cy - img.hot_spot_y as f32;
                let size = egui::vec2(img.width as f32, img.height as f32);
                let rect = egui::Rect::from_min_size(egui::pos2(x, y), size);

                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("spice_cursor"),
                ));
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                painter.image(tex.id(), rect, uv, egui::Color32::WHITE);
            }
        }

        if self.show_disconnect_dialog {
            let reason = self
                .disconnect_reason
                .as_deref()
                .unwrap_or("Unknown reason");
            egui::Window::new("Disconnected")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(reason);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            if let Some(ref capture) = self.capture {
                                capture.close();
                            }
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

/// Generate a simple 12x19 white arrow cursor with a black outline (RGBA).
fn default_arrow_cursor() -> Vec<u8> {
    #[rustfmt::skip]
    let shape: &[&[u8]] = &[
        &[1,0,0,0,0,0,0,0,0,0,0,0],
        &[1,1,0,0,0,0,0,0,0,0,0,0],
        &[1,2,1,0,0,0,0,0,0,0,0,0],
        &[1,2,2,1,0,0,0,0,0,0,0,0],
        &[1,2,2,2,1,0,0,0,0,0,0,0],
        &[1,2,2,2,2,1,0,0,0,0,0,0],
        &[1,2,2,2,2,2,1,0,0,0,0,0],
        &[1,2,2,2,2,2,2,1,0,0,0,0],
        &[1,2,2,2,2,2,2,2,1,0,0,0],
        &[1,2,2,2,2,2,2,2,2,1,0,0],
        &[1,2,2,2,2,2,2,2,2,2,1,0],
        &[1,2,2,2,2,2,2,2,2,2,2,1],
        &[1,2,2,2,2,2,2,1,1,1,1,1],
        &[1,2,2,2,1,2,2,1,0,0,0,0],
        &[1,2,2,1,0,1,2,2,1,0,0,0],
        &[1,2,1,0,0,1,2,2,1,0,0,0],
        &[1,1,0,0,0,0,1,2,2,1,0,0],
        &[1,0,0,0,0,0,1,2,2,1,0,0],
        &[0,0,0,0,0,0,0,1,1,0,0,0],
    ];

    let mut pixels = vec![0u8; 12 * 19 * 4];
    for (y, row) in shape.iter().enumerate() {
        for (x, &val) in row.iter().enumerate() {
            let idx = (y * 12 + x) * 4;
            match val {
                1 => {
                    // Black outline
                    pixels[idx] = 0;
                    pixels[idx + 1] = 0;
                    pixels[idx + 2] = 0;
                    pixels[idx + 3] = 255;
                }
                2 => {
                    // White fill
                    pixels[idx] = 255;
                    pixels[idx + 1] = 255;
                    pixels[idx + 2] = 255;
                    pixels[idx + 3] = 255;
                }
                _ => {} // transparent (already 0)
            }
        }
    }
    pixels
}

/// Run the SPICE connection in async context
#[allow(clippy::too_many_arguments)]
async fn run_connection(
    config: Config,
    event_tx: mpsc::Sender<ChannelEvent>,
    input_rx: mpsc::Receiver<InputEvent>,
    usb_rx: mpsc::Receiver<UsbCommand>,
    webdav_rx: mpsc::Receiver<WebdavCommand>,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    snapshots: ChannelSnapshots,
    monitors: u8,
    resize_rx: mpsc::Receiver<(u32, u32)>,
    volume_control: Arc<crate::channels::playback::VolumeControl>,
) -> Result<()> {
    let client = SpiceClient::new(ConnectionConfig::from(&config))?;

    // Wait for session initialization

    // Connect main channel and run until we get session ID and channel list
    let (event_tx_clone, mut temp_rx) = mpsc::channel(64);

    let main_stream = client.connect_channel(0, ChannelType::Main, 0).await?;

    let mut main_channel = MainChannel::new(
        main_stream,
        event_tx_clone,
        capture.clone(),
        byte_counter.clone(),
        traffic.clone(),
        snapshots.main,
        resize_rx,
        monitors,
    );

    // Spawn main channel task
    let main_handle = tokio::spawn(async move { main_channel.run().await });

    // Wait for session init and channel list
    let mut got_session = false;
    let mut got_channels = false;
    let mut temp_session_id = 0u32;
    let mut temp_channels = Vec::new();

    loop {
        match temp_rx.recv().await {
            Some(ChannelEvent::SessionInitialized(id)) => {
                temp_session_id = id;
                got_session = true;
                event_tx
                    .send(ChannelEvent::SessionInitialized(id))
                    .await
                    .ok();
            }
            Some(ChannelEvent::ChannelsAvailable(chs)) => {
                temp_channels = chs;
                got_channels = true;
                event_tx
                    .send(ChannelEvent::ChannelsAvailable(temp_channels.clone()))
                    .await
                    .ok();
            }
            Some(other) => {
                event_tx.send(other).await.ok();
            }
            None => break,
        }

        if got_session && got_channels {
            break;
        }
    }

    let session_id = temp_session_id;
    let channels = temp_channels;

    info!(
        "Session {} ready with {} channels",
        session_id,
        channels.len()
    );

    // Connect other channels
    let mut handles = vec![main_handle];
    let mut usb_rx = Some(usb_rx);
    let mut webdav_rx = Some(webdav_rx);
    let shared_glz_dictionary = DisplayChannel::new_shared_glz_dictionary();

    for (channel_type, channel_id) in channels {
        match channel_type {
            ChannelType::Display => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = DisplayChannel::new(
                    channel_id,
                    stream,
                    event_tx.clone(),
                    capture.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.display.clone(),
                    shared_glz_dictionary.clone(),
                );
                handles.push(tokio::spawn(async move { channel.run().await }));
            }

            ChannelType::Cursor => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = CursorChannel::new(
                    stream,
                    event_tx.clone(),
                    capture.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.cursor.clone(),
                );
                handles.push(tokio::spawn(async move { channel.run().await }));
            }

            ChannelType::Inputs => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = InputsChannel::new(
                    stream,
                    event_tx.clone(),
                    input_rx,
                    capture.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.inputs.clone(),
                );
                handles.push(tokio::spawn(async move { channel.run().await }));
                // input_rx is moved, can't connect more inputs channels
                break;
            }

            ChannelType::Usbredir => {
                if let Some(usb_rx) = usb_rx.take() {
                    let stream = client
                        .connect_channel(session_id, channel_type, channel_id)
                        .await?;
                    let mut channel = UsbredirChannel::new(
                        stream,
                        event_tx.clone(),
                        usb_rx,
                        virtual_disks.clone(),
                        capture.clone(),
                        byte_counter.clone(),
                    );
                    handles.push(tokio::spawn(async move { channel.run().await }));
                } else {
                    info!(
                        "Skipping additional usbredir channel (id={}): only one supported",
                        channel_id
                    );
                }
            }

            ChannelType::Webdav => {
                if let Some(webdav_rx) = webdav_rx.take() {
                    let stream = client
                        .connect_channel(session_id, channel_type, channel_id)
                        .await?;
                    let mut channel = WebdavChannel::new(
                        stream,
                        event_tx.clone(),
                        webdav_rx,
                        share_dir.clone(),
                        capture.clone(),
                        byte_counter.clone(),
                    );
                    handles.push(tokio::spawn(async move { channel.run().await }));
                } else {
                    info!(
                        "Skipping additional webdav channel (id={}): only one supported",
                        channel_id
                    );
                }
            }

            ChannelType::Playback => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = crate::channels::playback::PlaybackChannel::new(
                    stream,
                    event_tx.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    volume_control.clone(),
                );
                handles.push(tokio::spawn(async move { channel.run().await }));
            }

            _ => {
                info!(
                    "Skipping channel: {} (id={})",
                    channel_type.name(),
                    channel_id
                );
            }
        }
    }

    // Wait for all channel tasks
    for handle in handles {
        match handle.await {
            Err(e) => {
                error!("Channel task panic: {}", e);
            }
            Ok(Err(e)) => {
                let msg = format!("channel error: {}", e);
                error!("app: {}", msg);
                event_tx.send(ChannelEvent::Error(msg)).await.ok();
            }
            Ok(Ok(())) => {}
        }
    }

    Ok(())
}

/// Run in headless mode (no GUI)
pub async fn run_headless(
    config: Config,
    cadence: bool,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
    monitors: u8,
) -> Result<()> {
    info!("Running in headless mode");

    // Keep a reference for clean shutdown
    let capture_for_shutdown = capture.clone();

    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);
    let (_usb_tx, usb_rx) = mpsc::channel(16);
    let (_webdav_tx, webdav_rx) = mpsc::channel(16);
    let (_resize_tx, resize_rx) = mpsc::channel(32);

    // Headless mode doesn't display bandwidth, but channels still need the counter
    let byte_counter = Arc::new(ByteCounter::new());

    // Traffic ring buffers (always active)
    let traffic = Arc::new(TrafficBuffers::new());

    // Channel state snapshots (always active)
    let snapshots = ChannelSnapshots::new();

    // Spawn connection task
    let connection_handle = tokio::spawn(async move {
        run_connection(
            config,
            event_tx,
            input_rx,
            usb_rx,
            webdav_rx,
            virtual_disks,
            share_dir,
            capture,
            byte_counter,
            traffic,
            snapshots,
            monitors,
            resize_rx,
            crate::channels::playback::VolumeControl::new(),
        )
        .await
    });
    // Pin the handle so it can be polled multiple times in the select loop
    tokio::pin!(connection_handle);

    // Cadence task if enabled
    let cadence_handle = if cadence {
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = input_tx.try_send(InputEvent::KeyDown(0x39));
                let _ = input_tx.try_send(InputEvent::KeyUp(0xB9));
            }
        }))
    } else {
        None
    };

    // Process events
    let mut stats = Statistics::default();
    let mut last_stats_print = Instant::now();

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                match event {
                    ChannelEvent::SessionInitialized(id) => {
                        info!("Session {} initialized", id);
                    }
                    ChannelEvent::SurfaceCreated { .. } => {
                    }
                    ChannelEvent::ImageReady { .. } => {
                        stats.frames_received += 1;
                    }
                    ChannelEvent::Statistics { bytes_in, bytes_out, .. } => {
                        stats.bytes_in += bytes_in;
                        stats.bytes_out += bytes_out;
                    }
                    ChannelEvent::Disconnected(ChannelType::Main) => {
                        info!("Main channel disconnected, exiting");
                        break;
                    }
                    ChannelEvent::UsbDeviceConnected(desc) => {
                        info!("headless: USB device connected: {}", desc);
                    }
                    ChannelEvent::UsbDeviceDisconnected => {
                        info!("headless: USB device disconnected");
                    }
                    ChannelEvent::UsbConnectFailed(err) => {
                        error!("headless: USB connect failed: {}", err);
                    }
                    ChannelEvent::WebdavChannelReady => {
                        info!("headless: WebDAV channel connected");
                    }
                    ChannelEvent::WebdavSharingStarted { path, read_only } => {
                        info!("headless: WebDAV sharing: {} (ro={})", path, read_only);
                    }
                    ChannelEvent::WebdavSharingStopped => {
                        info!("headless: WebDAV sharing stopped");
                    }
                    ChannelEvent::WebdavError(err) => {
                        error!("headless: WebDAV error: {}", err);
                    }
                    ChannelEvent::Error(msg) => {
                        error!("Error: {}", msg);
                    }
                    _ => {}
                }

                // Print stats periodically
                if last_stats_print.elapsed() >= Duration::from_secs(10) {
                    info!(
                        "Stats: frames={}, bytes_in={}, bytes_out={}",
                        stats.frames_received, stats.bytes_in, stats.bytes_out
                    );
                    last_stats_print = Instant::now();
                }
            }
            _ = &mut connection_handle => {
                info!("Connection task completed");
                break;
            }
            // Poll for Ctrl+C (SIGINT) at a reasonable interval
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
                    info!("app: shutdown requested (SIGINT)");
                    break;
                }
            }
        }
    }

    if let Some(handle) = cadence_handle {
        handle.abort();
    }

    // Close capture session (flushes MP4 moov atom)
    if let Some(ref capture) = capture_for_shutdown {
        capture.close();
    }

    info!("Headless mode finished");
    Ok(())
}
