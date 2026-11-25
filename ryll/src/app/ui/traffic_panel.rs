use std::time::{Duration, Instant};

use eframe::egui;

use crate::bugreport::{format_size, TrafficBuffers, TrafficDirection, TrafficViewEntry};

/// Maximum entries shown in the traffic viewer.
const TRAFFIC_VIEWER_MAX_ENTRIES: usize = 200;

/// How often the traffic viewer refreshes from the ring buffers.
const TRAFFIC_VIEWER_REFRESH_MS: u64 = 250;

/// State of the traffic viewer side panel.
pub(in crate::app) struct TrafficViewer {
    open: bool,
    entries: Vec<TrafficViewEntry>,
    last_refresh: Instant,
    paused: bool,
    filter_main: bool,
    filter_display: bool,
    filter_inputs: bool,
    filter_cursor: bool,
    filter_usbredir: bool,
    filter_webdav: bool,
    filter_playback: bool,
}

impl TrafficViewer {
    pub(in crate::app) fn new() -> Self {
        Self {
            open: false,
            entries: Vec::new(),
            last_refresh: Instant::now(),
            paused: false,
            filter_main: true,
            filter_display: true,
            filter_inputs: true,
            filter_cursor: true,
            filter_usbredir: true,
            filter_webdav: true,
            filter_playback: true,
        }
    }

    pub(in crate::app) fn is_open(&self) -> bool {
        self.open
    }

    pub(in crate::app) fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    pub(in crate::app) fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub(in crate::app) fn refresh(&mut self, traffic: &TrafficBuffers) {
        // Refresh traffic viewer entries periodically
        if self.open
            && !self.paused
            && self.last_refresh.elapsed() >= Duration::from_millis(TRAFFIC_VIEWER_REFRESH_MS)
        {
            self.entries = traffic.recent_view_entries(TRAFFIC_VIEWER_MAX_ENTRIES);
            self.last_refresh = Instant::now();
        }
    }

    pub(in crate::app) fn show(&mut self, ui: &mut egui::Ui, traffic: &TrafficBuffers) {
        // Traffic viewer side panel (conditional)
        if self.open {
            egui::Panel::right("traffic_viewer")
                .default_size(350.0)
                .show(ui, |ui| {
                    // Header
                    ui.horizontal(|ui| {
                        ui.heading("Traffic");
                        if ui
                            .small_button(if self.paused { "Resume" } else { "Pause" })
                            .clicked()
                        {
                            self.paused = !self.paused;
                        }
                        ui.label(format!("{} msgs", self.entries.len()));
                    });

                    // Channel filters
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.filter_main, "Main");
                        ui.checkbox(&mut self.filter_display, "Display");
                        ui.checkbox(&mut self.filter_inputs, "Inputs");
                        ui.checkbox(&mut self.filter_cursor, "Cursor");
                        ui.checkbox(&mut self.filter_usbredir, "USB");
                        ui.checkbox(&mut self.filter_webdav, "WebDAV");
                        ui.checkbox(&mut self.filter_playback, "Playback");
                    });
                    ui.separator();

                    // Scrollable message list
                    let stick = !self.paused;
                    let now_elapsed = traffic.elapsed();
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(stick)
                        .show(ui, |ui| {
                            for entry in &self.entries {
                                // Apply channel filter
                                let visible = match entry.channel {
                                    "main" => self.filter_main,
                                    "display" => self.filter_display,
                                    "inputs" => self.filter_inputs,
                                    "cursor" => self.filter_cursor,
                                    "usbredir" => self.filter_usbredir,
                                    "webdav" => self.filter_webdav,
                                    "playback" => self.filter_playback,
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
    }
}
