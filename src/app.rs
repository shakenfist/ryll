/// Main application - egui App and headless mode
use anyhow::Result;
use eframe::egui;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::bugreport::{AppSnapshot, ChannelSnapshots, SurfaceInfo, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::channels::inputs::{key_to_scancode, mouse_button_to_spice};
use crate::channels::{
    ChannelEvent, CursorChannel, CursorImage, DisplayChannel, InputEvent, InputsChannel,
    MainChannel, UsbredirChannel,
};
use crate::config::{Config, VirtualDiskConfig};
use crate::display::DisplaySurface;
use crate::protocol::{ChannelType, SpiceClient};

/// Channel buffer sizes
const EVENT_CHANNEL_SIZE: usize = 1024;
const INPUT_CHANNEL_SIZE: usize = 256;

/// Approximate height of the stats bar at the bottom of the window
const STATS_BAR_HEIGHT: f32 = 10.0;

/// Number of bandwidth samples to keep for the sparkline.
const BANDWIDTH_HISTORY_LEN: usize = 60;

/// Number of recent frame timestamps kept for the FPS sliding window.
const FPS_WINDOW_SIZE: usize = 120;

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

    // Display state
    surfaces: HashMap<u32, DisplaySurface>,

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

    // Last mouse position sent (to avoid flooding with duplicates)
    last_mouse_pos: Option<(u32, u32)>,

    // Pending viewport resize from a new surface
    pending_resize: Option<(f32, f32)>,

    // Bandwidth tracking for the status bar sparkline
    bandwidth: BandwidthTracker,

    // Capture session (None when --capture is not specified)
    capture: Option<Arc<CaptureSession>>,

    // USB device status
    usb_device_description: Option<String>,

    // Traffic ring buffers (always active, for bug reports and traffic viewer)
    #[allow(dead_code)]
    traffic: Arc<TrafficBuffers>,

    // Channel state snapshots (always active, for bug reports)
    #[allow(dead_code)]
    channel_snapshots: ChannelSnapshots,
    #[allow(dead_code)]
    app_snapshot: Arc<std::sync::Mutex<AppSnapshot>>,
}

impl RyllApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        cadence: bool,
        virtual_disks: Vec<VirtualDiskConfig>,
        capture: Option<Arc<CaptureSession>>,
    ) -> Self {
        // Create event channel
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);

        // Shared byte counter for bandwidth tracking
        let byte_counter = Arc::new(ByteCounter::new());

        // Traffic ring buffers (always active)
        let traffic = Arc::new(TrafficBuffers::new());

        // Channel state snapshots (always active)
        let channel_snapshots = ChannelSnapshots::new();
        let app_snapshot = Arc::new(std::sync::Mutex::new(AppSnapshot::default()));

        // Spawn connection task
        let config_clone = config.clone();
        let event_tx_clone = event_tx.clone();
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

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                if let Err(e) = run_connection(
                    config_clone,
                    event_tx_clone,
                    input_rx,
                    virtual_disks,
                    capture_clone,
                    counter_clone,
                    traffic_clone,
                    snaps_for_conn,
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
            last_mouse_pos: None,
            pending_resize: None,
            bandwidth: BandwidthTracker::new(byte_counter),
            capture,
            usb_device_description: None,
            traffic,
            channel_snapshots,
            app_snapshot,
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
                    surface_id,
                    width,
                    height,
                } => {
                    info!("app: surface {} created: {}x{}", surface_id, width, height);
                    self.surfaces
                        .insert(surface_id, DisplaySurface::new(surface_id, width, height));
                    self.pending_resize = Some((width as f32, height as f32));
                }

                ChannelEvent::SurfaceDestroyed { surface_id } => {
                    info!("app: surface {} destroyed", surface_id);
                    self.surfaces.remove(&surface_id);
                }

                ChannelEvent::ImageReady {
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
                        self.surfaces.entry(surface_id)
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

                    let surface = self.surfaces.get_mut(&surface_id).unwrap();
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
                        if let Some(surface) = self.surfaces.get(&0) {
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
                    self.error_message = Some(msg);
                }

                ChannelEvent::UsbChannelReady => {
                    info!("app: USB redirection channel connected");
                }

                ChannelEvent::UsbDeviceConnected(desc) => {
                    info!("app: USB device connected: {}", desc);
                    self.usb_device_description = Some(desc);
                }

                ChannelEvent::Disconnected(channel) => {
                    info!("app: channel {} disconnected", channel.name());
                    if channel == ChannelType::Main {
                        self.connected = false;
                    }
                }

                _ => {}
            }
        }

        self.update_app_snapshot();
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

    fn handle_input(&mut self, ctx: &egui::Context) {
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
                    if let Some((down_code, up_code)) = key_to_scancode(*key) {
                        let ev = if *pressed {
                            InputEvent::KeyDown(down_code)
                        } else {
                            InputEvent::KeyUp(up_code)
                        };
                        debug!(
                            "app: key {:?} pressed={} scancode={:#x}",
                            key, pressed, down_code
                        );
                        let _ = input_tx.try_send(ev);
                    }
                }
            }
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
        if let Some((w, h)) = self.pending_resize.take() {
            let total_h = h + STATS_BAR_HEIGHT;
            info!(
                "app: resizing viewport to {}x{} (surface {}x{})",
                w, total_h, w, h
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, total_h)));
        }

        // Tick the bandwidth tracker
        self.bandwidth.tick();

        // Handle input
        self.handle_input(ctx);

        // Handle cadence mode
        self.handle_cadence();

        // Main display area (no margin so the surface fills edge-to-edge)
        let panel_frame = egui::Frame::none().inner_margin(0.0);
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                if let Some(error) = &self.error_message {
                    ui.colored_label(egui::Color32::RED, format!("Error: {}", error));
                    ui.separator();
                }

                if !self.connected {
                    ui.centered_and_justified(|ui| {
                        ui.label("Connecting...");
                    });
                    return;
                }

                // Display surfaces
                for surface in self.surfaces.values_mut() {
                    let width = surface.width;
                    let height = surface.height;
                    let texture = surface.texture(ctx);
                    let size = egui::vec2(width as f32, height as f32);

                    let response = ui.add(
                        egui::Image::new(texture)
                            .fit_to_exact_size(size)
                            .sense(egui::Sense::click_and_drag()),
                    );

                    // Track the surface rect for cursor overlay positioning
                    self.surface_rect = response.rect;

                    // Hide the OS cursor when hovering over the surface
                    if response.hovered() && self.cursor_image.is_some() {
                        ui.ctx()
                            .output_mut(|o| o.cursor_icon = egui::CursorIcon::None);
                    }

                    // Handle mouse input on the surface
                    if let Some(tx) = &self.input_tx {
                        // Send mouse position only when it changes
                        if let Some(pos) = response.hover_pos() {
                            let x = (pos.x - response.rect.min.x).max(0.0) as u32;
                            let y = (pos.y - response.rect.min.y).max(0.0) as u32;
                            if self.last_mouse_pos != Some((x, y)) {
                                self.last_mouse_pos = Some((x, y));
                                let _ = tx.try_send(InputEvent::MouseMove { x, y });
                            }
                        }

                        // Mouse buttons — use the raw pointer state from egui
                        // so press and release are sent at the correct times,
                        // not batched together on release like clicked_by().
                        ctx.input(|i| {
                            let pos = self.last_mouse_pos.unwrap_or((0, 0));
                            for button in [
                                egui::PointerButton::Primary,
                                egui::PointerButton::Secondary,
                                egui::PointerButton::Middle,
                            ] {
                                if i.pointer.button_pressed(button) {
                                    let spice_btn = mouse_button_to_spice(button);
                                    let _ = tx.try_send(InputEvent::MouseDown {
                                        button: spice_btn,
                                        x: pos.0,
                                        y: pos.1,
                                    });
                                    debug!("app: mouse down {:?} at ({},{})", button, pos.0, pos.1);
                                }
                                if i.pointer.button_released(button) {
                                    let spice_btn = mouse_button_to_spice(button);
                                    let _ = tx.try_send(InputEvent::MouseUp {
                                        button: spice_btn,
                                        x: pos.0,
                                        y: pos.1,
                                    });
                                    debug!("app: mouse up {:?} at ({},{})", button, pos.0, pos.1);
                                }
                            }
                        });
                    }
                }

                if self.surfaces.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("Waiting for display...");
                    });
                }
            });

        // Statistics panel (bottom)
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

                    // Bandwidth sparkline (right-aligned)
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(self.bandwidth.label());
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
                    });
                });
            });

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
        if self.cursor_visible && self.surface_rect != egui::Rect::NOTHING {
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

        // Repaint at ~60 fps to keep input responsive (clicks, key
        // presses).  Incoming events also trigger repaints via
        // request_repaint() from the connection thread.
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
async fn run_connection(
    config: Config,
    event_tx: mpsc::Sender<ChannelEvent>,
    input_rx: mpsc::Receiver<InputEvent>,
    virtual_disks: Vec<VirtualDiskConfig>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    snapshots: ChannelSnapshots,
) -> Result<()> {
    let client = SpiceClient::new(config)?;

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

    for (channel_type, channel_id) in channels {
        match channel_type {
            ChannelType::Display => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = DisplayChannel::new(
                    stream,
                    event_tx.clone(),
                    capture.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.display.clone(),
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
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let (_usb_tx, usb_rx) = mpsc::channel(16);
                let mut channel = UsbredirChannel::new(
                    stream,
                    event_tx.clone(),
                    usb_rx,
                    virtual_disks.clone(),
                    capture.clone(),
                    byte_counter.clone(),
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
        if let Err(e) = handle.await {
            error!("Channel task error: {}", e);
        }
    }

    Ok(())
}

/// Run in headless mode (no GUI)
pub async fn run_headless(
    config: Config,
    cadence: bool,
    virtual_disks: Vec<VirtualDiskConfig>,
    capture: Option<Arc<CaptureSession>>,
) -> Result<()> {
    info!("Running in headless mode");

    // Keep a reference for clean shutdown
    let capture_for_shutdown = capture.clone();

    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);

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
            virtual_disks,
            capture,
            byte_counter,
            traffic,
            snapshots,
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
                let _ = input_tx.try_send(InputEvent::KeyDown(0x39)); // Space
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
