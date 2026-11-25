use std::time::Instant;

use eframe::egui;
use shakenfist_spice_protocol::NotifySeverity;

use crate::app::reconnect::ReconnectState;
use crate::app::trigger_snapshot::REGION_SELECT_PENDING;
use crate::app::RyllApp;
use crate::bugreport::BugReportType;
use crate::input_egui::HostShortcut;
use crate::notifications::{self as notifications, NotificationSource};
use crate::reconnect_policy::MAX_RECONNECT_ATTEMPTS;
use crate::streaming_state::{self, StreamingState};

impl RyllApp {
    pub(in crate::app) fn draw_stats_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let streaming_state = self.classify_streaming_state();

        // Statistics panel (bottom) — rendered before CentralPanel
        // so egui reserves its space correctly.
        let is_maximized = ctx.input(|i| {
            i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)
        });

        let stats_frame = egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(4, 2))
            .fill(ctx.style_of(ctx.theme()).visuals.panel_fill);
        // egui 0.35: `TopBottomPanel`/`SidePanel` were unified into
        // `Panel`, and `show_animated(ctx, bool, …)` became
        // `show_collapsible(ui, &mut bool, …)`. The bool is taken by
        // `&mut` so the panel can flip it; we recompute it from
        // `is_maximized` every frame, so a flip is harmless.
        let mut show_stats = !is_maximized;
        egui::Panel::bottom("stats")
            .frame(stats_frame)
            .show_collapsible(ui, &mut show_stats, |ui| {
                ui.horizontal(|ui| {
                    self.draw_stats_metrics(ui);

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.draw_volume_controls(ui);
                        self.draw_streaming_indicator(ui, &streaming_state);
                        self.draw_agent_indicator(ui);
                        self.draw_bandwidth_sparkline(ui);
                        self.draw_notification_bell(ui);
                        self.draw_panel_menu(ui);
                    });
                });
            });
    }

    // Compute the live streaming-state classification once per frame.
    // The result drives the status-bar indicator below; a fired
    // notification (Flapping + cool-down elapsed) is pushed before we
    // render so the bell can pick it up on the same frame.
    fn classify_streaming_state(&mut self) -> StreamingState {
        let snap = self
            .channel_snapshots
            .display
            .lock()
            .expect("lock poisoned");
        // `classify` borrows the snapshot; nothing is copied out of
        // it here. The per-stream detail the tooltip wants is read
        // back under the lock inside `on_hover_ui` below, on the
        // frames the pointer is actually over the indicator —
        // cloning `streams_active` (two heap Strings per stream)
        // every frame to serve a tooltip almost nobody is looking
        // at was pure waste on the UI thread.
        let (state, notif) = streaming_state::classify(
            &snap,
            Instant::now(),
            self.traffic.session_start(),
            self.last_flap_notification_ts,
        );
        drop(snap);
        if let Some(notification) = notif {
            self.last_flap_notification_ts = Some(Instant::now());
            self.push_notification(
                NotifySeverity::Warn,
                NotificationSource::Internal,
                notification.message,
            );
        }
        state
    }

    fn draw_stats_metrics(&mut self, ui: &mut egui::Ui) {
        if !self.input_rtt.history.is_empty() {
            ui.label(format!("RTT: {}", self.input_rtt.label()))
                .on_hover_text(
                    "Round trip on the inputs channel, from a pointer move \
                     to the server's acknowledgement. Measured only while \
                     the pointer moves.",
                );
            ui.separator();
        }
        if !self.ping_interval.history.is_empty() {
            ui.label(format!("PING interval: {}", self.ping_interval.label()));
        }
        if self.ping_interval.history.len() >= 2 {
            let max_val = self
                .ping_interval
                .history
                .iter()
                .cloned()
                .fold(1.0f32, f32::max);
            let sparkline_w = 80.0;
            let sparkline_h = 12.0;
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(sparkline_w, sparkline_h), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            let n = self.ping_interval.history.len();
            let bar_w = sparkline_w / n as f32;
            for (i, &val) in self.ping_interval.history.iter().enumerate() {
                let h = (val / max_val) * sparkline_h;
                let x = rect.min.x + i as f32 * bar_w;
                let bar = egui::Rect::from_min_max(
                    egui::pos2(x, rect.max.y - h),
                    egui::pos2(x + bar_w - 0.5, rect.max.y),
                );
                painter.rect_filled(bar, 0.0, egui::Color32::from_rgb(180, 140, 80));
            }
        }
        if !self.ping_interval.history.is_empty() {
            ui.separator();
        }

        // Sliding-window FPS from DisplayMark timestamps. A
        // single-entry window yields elapsed == 0.0 and shows
        // no label, like the empty window.
        if let (Some(oldest), Some(newest)) = (
            self.stats.frame_times.first(),
            self.stats.frame_times.last(),
        ) {
            let elapsed = newest.duration_since(*oldest).as_secs_f64();
            if elapsed > 0.0 {
                let fps = (self.stats.frame_times.len() - 1) as f64 / elapsed;
                ui.label(format!("FPS: {:.1}", fps));
            }
        }

        if self.cadence_enabled {
            ui.separator();
            ui.label("Cadence: ON");
        }

        // Show the auto-snapshot counter only when the mode is
        // active (hiding the line avoids visual noise in normal
        // sessions).
        if let Some(_interval_secs) = self.auto_snapshot_interval {
            let (saved, _pruned) = self
                .app_snapshot
                .lock()
                .map(|s| (s.auto_snapshots_saved, s.auto_snapshots_pruned))
                .unwrap_or((0, 0));
            ui.separator();
            ui.label(format!(
                "Auto-snapshot: {}/{}",
                saved, self.auto_snapshot_cap
            ));
        }

        if let ReconnectState::Pending { attempt, .. } = &self.reconnect_state {
            ui.separator();
            ui.colored_label(
                egui::Color32::from_rgb(220, 160, 60),
                format!("Reconnecting… ({}/{})", attempt, MAX_RECONNECT_ATTEMPTS),
            );
        }

        if let Some(desc) = self.usb.device_description() {
            ui.separator();
            ui.label(format!("USB: {}", desc));
        }
    }

    fn draw_volume_controls(&mut self, ui: &mut egui::Ui) {
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
    }

    fn draw_streaming_indicator(&mut self, ui: &mut egui::Ui, streaming_state: &StreamingState) {
        // Live streaming indicator. Sits to the left of the
        // volume controls (which stay rightmost as the
        // operator-action zone) and to the right of the
        // bandwidth sparkline. Glyph: ▶ (U+25B6) — a Unicode
        // triangle that egui's default font renders cleanly on
        // every platform we ship to. 📹 is a colour emoji and
        // renders as a tofu box in egui's monochrome font; ▶
        // keeps the visual cue (something "playing") without
        // the font problem.
        let icon_colour = match &streaming_state {
            StreamingState::Off => egui::Color32::from_rgb(120, 120, 120),
            StreamingState::Active => egui::Color32::from_rgb(60, 180, 60),
            StreamingState::RecentlyDestroyed { .. } => egui::Color32::from_rgb(220, 160, 60),
            StreamingState::Flapping { .. } => egui::Color32::from_rgb(220, 60, 60),
        };
        let icon_text = egui::RichText::new("\u{25B6}").color(icon_colour);
        let icon_resp = ui.label(icon_text);
        // Every string below is built inside the hover
        // closure, which egui only runs on the frames the
        // tooltip is actually shown.
        icon_resp.on_hover_ui(|ui| match &streaming_state {
            StreamingState::Off => {
                ui.label("No streams active");
            }
            StreamingState::Active => {
                let snap = self
                    .channel_snapshots
                    .display
                    .lock()
                    .expect("lock poisoned");
                let active = &snap.streams_active;
                ui.label(format!(
                    "{} active stream{}",
                    active.len(),
                    if active.len() == 1 { "" } else { "s" },
                ));
                let now_secs = self.traffic.elapsed().as_secs_f64();
                for s in active {
                    let codec = match s.codec_type {
                        1 => "MJPEG".to_string(),
                        2 => "VP8".to_string(),
                        3 => "H264".to_string(),
                        4 => "VP9".to_string(),
                        5 => "H265".to_string(),
                        other => format!("codec{}", other),
                    };
                    let lifetime = (now_secs - s.created_at_secs).max(0.0);
                    // `frames` counts painted frames, so a
                    // stream whose dest rect encloses no
                    // pixels reads as zero while frames
                    // arrive. Say where they went.
                    let dropped = if s.frames_dropped_empty_dest > 0 {
                        format!(" dropped={}", s.frames_dropped_empty_dest)
                    } else {
                        String::new()
                    };
                    ui.label(format!(
                        "codec={} {}x{} frames={}{} lifetime={:.0}s",
                        codec,
                        s.stream_width,
                        s.stream_height,
                        s.frames_decoded_ok,
                        dropped,
                        lifetime,
                    ));
                }
            }
            StreamingState::RecentlyDestroyed { secs_since } => {
                ui.label(format!("Last stream destroyed {:.1}s ago", secs_since));
            }
            StreamingState::Flapping {
                destroys_in_window,
                window_secs,
                mean_lifetime_secs,
            } => {
                ui.label(format!(
                    "Streams flapping: {} destroys in {:.0} s, \
                     mean lifetime {:.1} s",
                    destroys_in_window, window_secs, mean_lifetime_secs,
                ));
            }
        });
    }

    fn draw_agent_indicator(&mut self, ui: &mut egui::Ui) {
        // Guest agent indicator: what clipboard sharing and
        // guest resizing depend on (#537).
        ui.separator();
        let agent = self.agent_state;
        ui.label(egui::RichText::new("Agent").color(agent.indicator_colour()))
            .on_hover_text(agent.tooltip());

        ui.separator();
    }

    fn draw_bandwidth_sparkline(&mut self, ui: &mut egui::Ui) {
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
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(sparkline_w, sparkline_h), egui::Sense::hover());
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
    }

    fn draw_notification_bell(&mut self, ui: &mut egui::Ui) {
        // Bell notification button
        let (unread_count, bell_severity) = self
            .notifications
            .lock()
            .map(|s| (s.unread_count(), s.highest_bell_severity()))
            .unwrap_or((0, None));

        let mut bell_text = egui::RichText::new("\u{1F514}");
        if let Some(sev) = bell_severity {
            let (_, colour) = notifications::severity_visuals(sev);
            if let Some(c) = colour {
                bell_text = bell_text.color(c);
            }
        }
        let bell_button = ui.add(egui::Button::new(bell_text));
        let bell_button = if unread_count > 0 {
            bell_button.on_hover_text(format!(
                "{} unread notification{}",
                unread_count,
                if unread_count == 1 { "" } else { "s" },
            ))
        } else {
            bell_button
        };
        if bell_button.clicked() {
            self.show_notifications_panel = !self.show_notifications_panel;
        }
    }

    fn draw_panel_menu(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.menu_button("☰", |ui| {
            ui.checkbox(&mut self.obey_guest_size, "Obey guest size hints");
            ui.separator();
            let mut traffic_open = self.traffic_viewer.is_open();
            if ui.checkbox(&mut traffic_open, "Traffic").changed() {
                self.traffic_viewer.set_open(traffic_open);
            }
            let mut usb_open = self.usb.is_open();
            if ui.checkbox(&mut usb_open, "USB").changed() {
                self.usb.set_open(usb_open);
            }
            let mut webdav_open = self.webdav.is_open();
            if ui.checkbox(&mut webdav_open, "Folders").changed() {
                self.webdav.set_open(webdav_open);
            }
            // Disabled during region selection, as
            // their F8 and F12 shortcuts are.
            let menu_enabled = !self.region_select_active;
            if ui
                .add_enabled(
                    menu_enabled,
                    egui::Button::new("Screenshot")
                        .shortcut_text(HostShortcut::Screenshot.key().name()),
                )
                .on_disabled_hover_text(REGION_SELECT_PENDING)
                .clicked()
            {
                // Opened next frame, after that frame's
                // input is forwarded.
                self.screenshot_requested = true;
                ui.ctx().request_repaint();
                ui.close();
            }
            if ui
                .add_enabled(
                    menu_enabled,
                    egui::Button::new("Report").shortcut_text(HostShortcut::BugReport.key().name()),
                )
                .on_disabled_hover_text(REGION_SELECT_PENDING)
                .clicked()
            {
                self.open_bug_dialog(BugReportType::Display, String::new());
                ui.close();
            }
            if self.enable_paste {
                ui.separator();
                let label = egui::Button::new("Paste").shortcut_text("Ctrl+Alt+V");
                let enabled = !self.agent_state.connected();
                let response = ui.add_enabled(enabled, label);
                if response.clicked() {
                    self.trigger_paste();
                    ui.close();
                }
                if !enabled {
                    response.on_disabled_hover_text("vdagent is connected — use Ctrl+V instead");
                }
            }
        });
    }
}
