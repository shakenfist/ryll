/// Main application - egui App and headless mode
use anyhow::Result;
use eframe::egui;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::channels::inputs::{key_to_scancode, mouse_button_to_spice};
use crate::channels::{
    ChannelEvent, CursorChannel, DisplayChannel, InputEvent, InputsChannel, MainChannel,
};
use crate::config::Config;
use crate::display::DisplaySurface;
use crate::protocol::{ChannelType, SpiceClient};

/// Channel buffer sizes
const EVENT_CHANNEL_SIZE: usize = 1024;
const INPUT_CHANNEL_SIZE: usize = 256;

/// Approximate height of the stats bar at the bottom of the window
const STATS_BAR_HEIGHT: f32 = 10.0;

/// Statistics tracking
#[derive(Default)]
struct Statistics {
    frames_received: u64,
    bytes_in: u64,
    bytes_out: u64,
    last_latency: Option<f64>,
    start_time: Option<Instant>,
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

    // Statistics
    stats: Statistics,

    // Cadence mode
    cadence_enabled: bool,
    last_cadence_key: Instant,

    // Session state
    connected: bool,
    error_message: Option<String>,

    // Last mouse position sent (to avoid flooding with duplicates)
    last_mouse_pos: Option<(u32, u32)>,

    // Pending viewport resize from a new surface
    pending_resize: Option<(f32, f32)>,
}

impl RyllApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config, cadence: bool) -> Self {
        // Create event channel
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);

        // Spawn connection task
        let config_clone = config.clone();
        let event_tx_clone = event_tx.clone();
        let ctx = cc.egui_ctx.clone();

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                if let Err(e) = run_connection(config_clone, event_tx_clone, input_rx).await {
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
            stats: Statistics {
                start_time: Some(Instant::now()),
                ..Default::default()
            },
            cadence_enabled: cadence,
            last_cadence_key: Instant::now(),
            connected: false,
            error_message: None,
            last_mouse_pos: None,
            pending_resize: None,
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
                        "app: blit surface={}, pos=({},{}), size={}x{}, frame={}",
                        surface_id, left, top, width, height, self.stats.frames_received
                    );
                }

                ChannelEvent::DisplayMark => {
                    // Frame boundary - could trigger repaint
                }

                ChannelEvent::CursorPosition { x, y, visible } => {
                    self.cursor_pos = (x, y);
                    self.cursor_visible = visible;
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

                ChannelEvent::Disconnected(channel) => {
                    info!("app: channel {} disconnected", channel.name());
                    if channel == ChannelType::Main {
                        self.connected = false;
                    }
                }

                _ => {}
            }
        }
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

                        // Mouse buttons
                        if response.clicked_by(egui::PointerButton::Primary) {
                            let pos = response.interact_pointer_pos().unwrap_or(response.rect.min);
                            let x = (pos.x - response.rect.min.x).max(0.0) as u32;
                            let y = (pos.y - response.rect.min.y).max(0.0) as u32;
                            let button = mouse_button_to_spice(egui::PointerButton::Primary);
                            let _ = tx.try_send(InputEvent::MouseDown { button, x, y });
                            let _ = tx.try_send(InputEvent::MouseUp { button, x, y });
                            debug!("app: mouse click at ({},{})", x, y);
                        }

                        if response.clicked_by(egui::PointerButton::Secondary) {
                            let pos = response.interact_pointer_pos().unwrap_or(response.rect.min);
                            let x = (pos.x - response.rect.min.x).max(0.0) as u32;
                            let y = (pos.y - response.rect.min.y).max(0.0) as u32;
                            let button = mouse_button_to_spice(egui::PointerButton::Secondary);
                            let _ = tx.try_send(InputEvent::MouseDown { button, x, y });
                            let _ = tx.try_send(InputEvent::MouseUp { button, x, y });
                        }
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
                    ui.label(format!("Frames: {}", self.stats.frames_received));
                    ui.separator();

                    if let Some(latency) = self.stats.last_latency {
                        ui.label(format!("Latency: {:.1}ms", latency * 1000.0));
                        ui.separator();
                    }

                    if let Some(start) = self.stats.start_time {
                        let elapsed = start.elapsed().as_secs_f64();
                        if elapsed > 0.0 {
                            let fps = self.stats.frames_received as f64 / elapsed;
                            ui.label(format!("FPS: {:.1}", fps));
                        }
                    }

                    ui.separator();
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

                    if self.cadence_enabled {
                        ui.separator();
                        ui.label("Cadence: ON");
                    }
                });
            });

        // Repaint at a modest rate to pick up new frames without
        // spinning the CPU.  Incoming events will also trigger a
        // repaint via request_repaint() from the connection thread.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
}

/// Run the SPICE connection in async context
async fn run_connection(
    config: Config,
    event_tx: mpsc::Sender<ChannelEvent>,
    input_rx: mpsc::Receiver<InputEvent>,
) -> Result<()> {
    let client = SpiceClient::new(config)?;

    // Wait for session initialization

    // Connect main channel and run until we get session ID and channel list
    let (event_tx_clone, mut temp_rx) = mpsc::channel(64);

    let main_stream = client.connect_channel(0, ChannelType::Main, 0).await?;

    let mut main_channel = MainChannel::new(main_stream, event_tx_clone);

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
                let mut channel = DisplayChannel::new(stream, event_tx.clone());
                handles.push(tokio::spawn(async move { channel.run().await }));
            }

            ChannelType::Cursor => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = CursorChannel::new(stream, event_tx.clone());
                handles.push(tokio::spawn(async move { channel.run().await }));
            }

            ChannelType::Inputs => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = InputsChannel::new(stream, event_tx.clone(), input_rx);
                handles.push(tokio::spawn(async move { channel.run().await }));
                // input_rx is moved, can't connect more inputs channels
                break;
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
pub async fn run_headless(config: Config, cadence: bool) -> Result<()> {
    info!("Running in headless mode");

    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);

    // Spawn connection task
    let connection_handle =
        tokio::spawn(async move { run_connection(config, event_tx, input_rx).await });
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
        }
    }

    if let Some(handle) = cadence_handle {
        handle.abort();
    }

    info!("Headless mode finished");
    Ok(())
}
