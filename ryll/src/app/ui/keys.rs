use eframe::egui;
use shakenfist_spice_protocol::NotifySeverity;

use crate::app::agent_state::{mac_paste_chord, paste_hint};
use crate::app::trigger_snapshot::RegionSelectEnd;
use crate::app::RyllApp;
use crate::bugreport::BugReportType;
use crate::input_egui::{
    modifier_events, paste_shortcut_pressed, translate_key_events, HostShortcut,
};
use crate::notifications::NotificationSource;

impl RyllApp {
    pub(in crate::app) fn gate_keyboard_focus(&mut self, ctx: &egui::Context) {
        // The bug report dialog's description box is the one widget that
        // takes typing; input forwarding is off while it is open.
        if !self.show_bug_dialog {
            keep_keyboard_for_guest(ctx);
        }
    }

    pub(in crate::app) fn handle_hotkeys(&mut self, ctx: &egui::Context) {
        // Escape during region selection: skip and generate without region
        if self.region_select_active {
            let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc {
                self.end_region_select(RegionSelectEnd::Skipped);
            }
        }

        // F12 toggles bug report dialog (not during region selection)
        if !self.region_select_active {
            let f12_pressed = ctx.input(|i| i.key_pressed(HostShortcut::BugReport.key()));
            if f12_pressed {
                if self.show_bug_dialog {
                    self.show_bug_dialog = false;
                    self.discard_trigger_snapshot();
                } else {
                    self.open_bug_dialog(BugReportType::Display, String::new());
                }
            }
        }

        // F11 toggles traffic viewer (not during region selection)
        if !self.region_select_active {
            let f11_pressed = ctx.input(|i| i.key_pressed(HostShortcut::TrafficViewer.key()));
            if f11_pressed {
                self.traffic_viewer.toggle();
            }
        }

        // F8 opens screenshot save dialog (not during region selection)
        if !self.region_select_active {
            let f8_pressed = ctx.input(|i| i.key_pressed(HostShortcut::Screenshot.key()));
            if f8_pressed {
                self.screenshot_requested = true;
            }
        }

        // Escape closes the dialog
        if self.show_bug_dialog {
            let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc {
                self.show_bug_dialog = false;
                self.discard_trigger_snapshot();
            }
        }

        // Ctrl+Alt+V triggers paste-as-keystrokes (not during
        // region selection or dialogs)
        let mut paste_triggered = false;
        if !self.region_select_active && !self.show_bug_dialog && self.paste_error_message.is_none()
        {
            let ctrl_alt_v = ctx.input(|i| paste_shortcut_pressed(i.modifiers, &i.events));
            if ctrl_alt_v {
                paste_triggered = self.trigger_paste();
                if paste_triggered {
                    self.held_keys.withhold_paste_release();
                }
            }

            // Cmd+V with no agent does nothing in the guest; say why, once
            // a session (#537).
            if self.connected
                && !self.paste_hint_shown
                && !self.agent_state.connected()
                && ctx.input(|i| mac_paste_chord(i.modifiers, &i.events))
            {
                self.paste_hint_shown = true;
                self.push_notification(
                    NotifySeverity::Warn,
                    NotificationSource::Internal,
                    paste_hint(self.enable_paste),
                );
            }
        }

        // Handle input (skip if paste was triggered this frame)
        if !paste_triggered {
            self.handle_input(ctx);
        }
        // Whether or not this frame's keys were translated, the clipboard
        // chord tracking must see the modifiers it ended with.
        let modifiers = ctx.input(|i| i.modifiers);
        self.held_keys.end_frame(modifiers);

        // Only now, with this frame's keys forwarded, is it safe to block
        // in the screenshot save dialog (see `open_screenshot_dialog`).
        if std::mem::take(&mut self.screenshot_requested) {
            self.open_screenshot_dialog();
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Don't forward input to the SPICE server when
        // the bug report dialog or region selection is active.
        if self.show_bug_dialog || self.region_select_active {
            self.release_guest_keys();
            return;
        }

        let input_tx = match &self.input_tx {
            Some(tx) => tx.clone(),
            None => return,
        };

        // Handle keyboard input — read from the global input state so
        // key events are captured regardless of which widget has focus.
        ctx.input(|i| {
            let mods = i.modifiers;
            let prev = self.last_modifiers.unwrap_or_default();
            for ev in modifier_events(prev, mods) {
                let _ = input_tx.try_send(ev);
            }
            self.last_modifiers = Some(mods);

            for ev in translate_key_events(&i.events, &mut self.held_keys) {
                let _ = input_tx.try_send(ev);
            }
        });
    }

    /// Release every key the guest has been told is held: the keys in
    /// `held_keys` and the Ctrl / Shift / Alt modifiers forwarded by
    /// `handle_input`. Used whenever ryll stops forwarding input or may
    /// have missed the releases, so the guest is not left auto-repeating
    /// a key nobody is holding. A modifier still held afterwards is
    /// pressed again by the next `handle_input`.
    pub(in crate::app) fn release_guest_keys(&mut self) {
        let mut events = self.held_keys.release_all();
        if let Some(prev) = self.last_modifiers.take() {
            events.extend(modifier_events(prev, egui::Modifiers::default()));
        }
        if let Some(tx) = &self.input_tx {
            for ev in events {
                let _ = tx.try_send(ev);
            }
        }
    }
}

/// Keep egui's keyboard navigation away from keys meant for the guest.
///
/// `handle_input` forwards every key to the guest, but egui reads the same
/// keys: Tab and the arrows move focus between widgets, and Space or Enter
/// clicks the focused one. So typing in the guest could focus the status
/// bar's bell or ☰ button, and every later space or newline toggled the
/// notifications panel or the menu; the arrows could move the volume
/// slider (#535). Called at the start of each pass, after egui has read this
/// pass's Tab and arrow presses, it cancels the move they asked for and
/// drops any focus already held.
fn keep_keyboard_for_guest(ctx: &egui::Context) {
    ctx.memory_mut(|m| {
        m.move_focus(egui::FocusDirection::None);
        if let Some(id) = m.focused() {
            m.surrender_focus(id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Keys typed for the guest must not drive egui's widgets
    //
    // Tab then Space, typed into the guest, opened the notifications panel
    // or the ☰ menu: egui moved focus to a status-bar button and clicked it.
    // -------------------------------------------------------------------------

    fn key_press(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Run one pass per batch of events over a status bar with one button.
    /// Returns how many times the button was clicked.
    fn status_button_clicks(passes: &[Vec<egui::Event>], guard: bool) -> usize {
        let ctx = egui::Context::default();
        let mut clicks = 0;
        for events in passes {
            let input = egui::RawInput {
                events: events.clone(),
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                if guard {
                    keep_keyboard_for_guest(ui.ctx());
                }
                egui::Panel::bottom("stats").show(ui, |ui| {
                    if ui.button("bell").clicked() {
                        clicks += 1;
                    }
                });
            })
            .drop_without_applying_deltas();
        }
        clicks
    }

    #[test]
    fn tab_then_space_clicks_a_status_button_unguarded() {
        // Pins the egui behaviour behind the bug, so the guarded test below
        // keeps proving something.
        let passes = vec![
            vec![],
            vec![key_press(egui::Key::Tab)],
            vec![],
            vec![key_press(egui::Key::Space)],
        ];
        assert_eq!(status_button_clicks(&passes, false), 1);
    }

    #[test]
    fn guest_keys_do_not_click_status_buttons() {
        let separate = vec![
            vec![],
            vec![key_press(egui::Key::Tab)],
            vec![],
            vec![key_press(egui::Key::Space)],
            vec![key_press(egui::Key::Enter)],
        ];
        assert_eq!(status_button_clicks(&separate, true), 0);
        let same_pass = vec![
            vec![],
            vec![key_press(egui::Key::Tab), key_press(egui::Key::Space)],
            vec![key_press(egui::Key::Space)],
        ];
        assert_eq!(status_button_clicks(&same_pass, true), 0);
        let arrows = vec![
            vec![],
            vec![key_press(egui::Key::ArrowDown)],
            vec![key_press(egui::Key::Enter)],
        ];
        assert_eq!(status_button_clicks(&arrows, true), 0);
    }
}
