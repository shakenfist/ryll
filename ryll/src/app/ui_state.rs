//! What ryll's own interface is showing, for bug reports (#562).
//!
//! Three reports about the notifications panel opening unasked could
//! not show it: their screenshot was the guest surface, and nothing
//! said which of ryll's panels was open or when it had opened. The
//! tracker here is told each frame which panels, menus and dialogs are
//! open and whether egui is claiming input, and keeps when each opened
//! and a log of the recent openings and closings, on the session clock
//! the key log uses.

use std::collections::VecDeque;
use std::time::Instant;

use eframe::egui;

use crate::bugreport::{OpenUiElement, UiChange, UiSnapshot, UiState};

/// Openings and closings kept for a bug report.
pub(super) const UI_CHANGE_LOG_CAPACITY: usize = 64;

/// Whether each of ryll's panels, menus and dialogs is open in a frame,
/// by name. The names are what bug reports show.
pub(super) type UiElements = [(&'static str, bool)];

/// What egui claims of the input in a frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct EguiInputClaims {
    pub(super) wants_keyboard: bool,
    pub(super) wants_pointer: bool,
    pub(super) focused_widget: Option<String>,
}

impl EguiInputClaims {
    /// Read the claims from egui, at the end of a pass.
    pub(super) fn read(ctx: &egui::Context) -> Self {
        EguiInputClaims {
            wants_keyboard: ctx.egui_wants_keyboard_input(),
            wants_pointer: ctx.egui_wants_pointer_input(),
            focused_widget: ctx.memory(|m| m.focused()).map(|id| format!("{id:?}")),
        }
    }
}

/// Follows ryll's interface frame by frame.
#[derive(Debug)]
pub(super) struct UiStateTracker {
    /// Session start on the traffic buffers' clock.
    start: Instant,
    now: UiState,
    at_report_trigger: Option<UiState>,
    changes: VecDeque<UiChange>,
    updates_seen: u64,
}

impl UiStateTracker {
    /// Track the interface of a session that started at `session_start`,
    /// the `Instant` its `TrafficBuffers` measure time from.
    pub(super) fn new(session_start: Instant) -> Self {
        UiStateTracker {
            start: session_start,
            now: UiState::default(),
            at_report_trigger: None,
            changes: VecDeque::with_capacity(UI_CHANGE_LOG_CAPACITY),
            updates_seen: 0,
        }
    }

    /// Record the frame that has just been drawn.
    pub(super) fn observe(&mut self, elements: &UiElements, claims: EguiInputClaims) {
        let at_secs = self.start.elapsed().as_secs_f64();
        self.observe_at(at_secs, elements, claims);
    }

    fn observe_at(&mut self, at_secs: f64, elements: &UiElements, claims: EguiInputClaims) {
        let mut changed = false;
        let mut open = Vec::with_capacity(elements.len());
        for &(element, is_open) in elements {
            let was = self.now.open.iter().find(|e| e.element == element);
            match (was, is_open) {
                (Some(previous), true) => open.push(*previous),
                (None, true) => {
                    open.push(OpenUiElement {
                        element,
                        opened_at_secs: at_secs,
                    });
                    self.log_change(at_secs, element, true);
                    changed = true;
                }
                (Some(_), false) => {
                    self.log_change(at_secs, element, false);
                    changed = true;
                }
                (None, false) => {}
            }
        }
        changed |= self.now.egui_wants_keyboard != claims.wants_keyboard
            || self.now.egui_wants_pointer != claims.wants_pointer
            || self.now.focused_widget != claims.focused_widget;
        if changed {
            self.now = UiState {
                at_secs,
                open,
                egui_wants_keyboard: claims.wants_keyboard,
                egui_wants_pointer: claims.wants_pointer,
                focused_widget: claims.focused_widget,
            };
            self.updates_seen += 1;
        }
    }

    /// Keep the interface as of the last frame drawn as the one a bug
    /// report was started from. Call it before the report's dialog is
    /// drawn, so it describes what the user was reporting.
    pub(super) fn mark_report_trigger(&mut self) {
        let at_secs = self.start.elapsed().as_secs_f64();
        self.mark_report_trigger_at(at_secs);
    }

    fn mark_report_trigger_at(&mut self, at_secs: f64) {
        self.at_report_trigger = Some(UiState {
            at_secs,
            ..self.now.clone()
        });
        self.updates_seen += 1;
    }

    /// Changes so far. It changes whenever the snapshot would.
    pub(super) fn updates_seen(&self) -> u64 {
        self.updates_seen
    }

    pub(super) fn snapshot(&self) -> UiSnapshot {
        UiSnapshot {
            updates_seen: self.updates_seen,
            now: self.now.clone(),
            at_report_trigger: self.at_report_trigger.clone(),
            recent_changes: self.changes.iter().copied().collect(),
        }
    }

    fn log_change(&mut self, at_secs: f64, element: &'static str, open: bool) {
        if self.changes.len() == UI_CHANGE_LOG_CAPACITY {
            self.changes.pop_front();
        }
        self.changes.push_back(UiChange {
            at_secs,
            element,
            open,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> UiStateTracker {
        UiStateTracker::new(Instant::now())
    }

    fn names(state: &UiState) -> Vec<(&'static str, f64)> {
        state
            .open
            .iter()
            .map(|e| (e.element, e.opened_at_secs))
            .collect()
    }

    #[test]
    fn records_when_each_element_opened_and_closed() {
        let mut t = tracker();
        let none = EguiInputClaims::default();
        t.observe_at(
            1.0,
            &[("notifications_panel", false), ("menu", false)],
            none.clone(),
        );
        t.observe_at(
            2.0,
            &[("notifications_panel", true), ("menu", false)],
            none.clone(),
        );
        t.observe_at(
            3.0,
            &[("notifications_panel", true), ("menu", true)],
            none.clone(),
        );
        t.observe_at(4.0, &[("notifications_panel", true), ("menu", false)], none);

        let snap = t.snapshot();
        // Still open, and since when it first opened, not since the
        // latest frame.
        assert_eq!(names(&snap.now), vec![("notifications_panel", 2.0)]);
        assert_eq!(snap.now.at_secs, 4.0);
        let changes: Vec<_> = snap
            .recent_changes
            .iter()
            .map(|c| (c.at_secs, c.element, c.open))
            .collect();
        assert_eq!(
            changes,
            vec![
                (2.0, "notifications_panel", true),
                (3.0, "menu", true),
                (4.0, "menu", false),
            ]
        );
        assert_eq!(snap.updates_seen, 3);
    }

    #[test]
    fn unchanged_frames_do_not_count_as_updates() {
        let mut t = tracker();
        let claims = EguiInputClaims {
            wants_pointer: true,
            ..Default::default()
        };
        t.observe_at(1.0, &[("menu", true)], claims.clone());
        let seen = t.updates_seen();
        t.observe_at(2.0, &[("menu", true)], claims.clone());
        t.observe_at(3.0, &[("menu", true)], claims);
        assert_eq!(t.updates_seen(), seen);
        // `now` says when the interface last changed.
        assert_eq!(t.snapshot().now.at_secs, 1.0);
    }

    #[test]
    fn egui_claims_are_recorded_and_count_as_updates() {
        let mut t = tracker();
        t.observe_at(1.0, &[], EguiInputClaims::default());
        let seen = t.updates_seen();
        t.observe_at(
            2.0,
            &[],
            EguiInputClaims {
                wants_keyboard: true,
                wants_pointer: false,
                focused_widget: Some("Id(0x1)".to_string()),
            },
        );
        assert_eq!(t.updates_seen(), seen + 1);
        let now = t.snapshot().now;
        assert!(now.egui_wants_keyboard);
        assert_eq!(now.focused_widget.as_deref(), Some("Id(0x1)"));
    }

    #[test]
    fn report_trigger_keeps_the_interface_before_the_dialog() {
        let mut t = tracker();
        let none = EguiInputClaims::default();
        t.observe_at(
            5.0,
            &[("notifications_panel", true), ("bug_report_dialog", false)],
            none.clone(),
        );
        t.mark_report_trigger_at(5.5);
        t.observe_at(
            6.0,
            &[("notifications_panel", true), ("bug_report_dialog", true)],
            none,
        );
        let snap = t.snapshot();
        let trigger = snap.at_report_trigger.expect("trigger state kept");
        assert_eq!(names(&trigger), vec![("notifications_panel", 5.0)]);
        assert_eq!(trigger.at_secs, 5.5);
        assert_eq!(
            names(&snap.now),
            vec![("notifications_panel", 5.0), ("bug_report_dialog", 6.0)]
        );
    }

    #[test]
    fn change_log_keeps_only_the_most_recent() {
        let mut t = tracker();
        let none = EguiInputClaims::default();
        for i in 0..(UI_CHANGE_LOG_CAPACITY + 10) {
            t.observe_at(i as f64, &[("menu", i % 2 == 0)], none.clone());
        }
        let snap = t.snapshot();
        assert_eq!(snap.recent_changes.len(), UI_CHANGE_LOG_CAPACITY);
        let last = snap.recent_changes.last().unwrap();
        assert_eq!(last.at_secs, (UI_CHANGE_LOG_CAPACITY + 9) as f64);
        assert!(snap.updates_seen > UI_CHANGE_LOG_CAPACITY as u64);
    }
}
