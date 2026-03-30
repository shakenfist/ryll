/// Main application - egui App and headless mode
use anyhow::Result;
use eframe::egui;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, info};

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
                    error!("Connection error: {}", e);
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
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                ChannelEvent::SessionInitialized(session_id) => {
                    info!("Session {} initialized", session_id);
                    self.connected = true;
                }

                ChannelEvent::SurfaceCreated {
                    surface_id,
                    width,
                    height,
                } => {
                    info!("Surface {} created: {}x{}", surface_id, width, height);
                    self.surfaces
                        .insert(surface_id, DisplaySurface::new(surface_id, width, height));
                }

                ChannelEvent::SurfaceDestroyed { surface_id } => {
                    info!("Surface {} destroyed", surface_id);
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
                    if let Some(surface) = self.surfaces.get_mut(&surface_id) {
                        surface.blit(left, top, width, height, &pixels);
                        self.stats.frames_received += 1;
                    }
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
                    error!("Channel error: {}", msg);
                    self.error_message = Some(msg);
                }

                ChannelEvent::Disconnected(channel) => {
                    info!("Channel {} disconnected", channel.name());
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

        // Handle keyboard input
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed, .. } = event {
                    if let Some((down_code, up_code)) = key_to_scancode(*key) {
                        let event = if *pressed {
                            InputEvent::KeyDown(down_code)
                        } else {
                            InputEvent::KeyUp(up_code)
                        };
                        let _ = input_tx.try_send(event);
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

        // Handle input
        self.handle_input(ctx);

        // Handle cadence mode
        self.handle_cadence();

        // Main display area
        egui::CentralPanel::default().show(ctx, |ui| {
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

                // Create a frame for the surface
                egui::Frame::none()
                    .fill(egui::Color32::BLACK)
                    .show(ui, |ui| {
                        let response = ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .sense(egui::Sense::click_and_drag()),
                        );

                        // Handle mouse input on the surface
                        if let Some(tx) = &self.input_tx {
                            if let Some(pos) = response.interact_pointer_pos() {
                                let x = (pos.x - response.rect.min.x) as u32;
                                let y = (pos.y - response.rect.min.y) as u32;

                                // Mouse movement
                                let _ = tx.try_send(InputEvent::MouseMove { x, y });

                                // Mouse buttons
                                if response.clicked_by(egui::PointerButton::Primary) {
                                    let button =
                                        mouse_button_to_spice(egui::PointerButton::Primary);
                                    let _ = tx.try_send(InputEvent::MouseDown { button, x, y });
                                    let _ = tx.try_send(InputEvent::MouseUp { button, x, y });
                                }

                                if response.clicked_by(egui::PointerButton::Secondary) {
                                    let button =
                                        mouse_button_to_spice(egui::PointerButton::Secondary);
                                    let _ = tx.try_send(InputEvent::MouseDown { button, x, y });
                                    let _ = tx.try_send(InputEvent::MouseUp { button, x, y });
                                }
                            }
                        }
                    });
            }

            if self.surfaces.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("Waiting for display...");
                });
            }
        });

        // Statistics panel (bottom)
        egui::TopBottomPanel::bottom("stats").show(ctx, |ui| {
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

        // Request continuous repainting
        ctx.request_repaint();
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
