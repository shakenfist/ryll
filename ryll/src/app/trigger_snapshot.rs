use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::bugreport::{chrono_now, encode_png, ReportRegion, TriggerTimestamps};
use crate::notifications::NotificationSource;
use shakenfist_spice_protocol::NotifySeverity;

use super::reconnect::ReconnectState;
use super::ui_state::EguiInputClaims;
use super::RyllApp;

/// How long the bug report dialog waits for the window screenshot
/// asked for when the report started, before it is drawn anyway. The
/// dialog is not drawn until the screenshot arrives, so it is not in
/// it; eframe normally delivers it the next frame.
const WINDOW_SHOT_WAIT: Duration = Duration::from_millis(500);

/// A screenshot of ryll's whole window, panels and menus included, asked
/// of eframe in the frame a bug report starts (#562). The renderer hands
/// the image to a callback after a later frame, which sets `arrived` and
/// PNG-encodes it on a worker thread into `png_slot`, as
/// `TriggerSnapshot` encodes the guest surface. Each request owns its
/// flag and slot, so a screenshot from a discarded request lands nowhere.
pub(super) struct WindowShot {
    requested_at: Instant,
    arrived: Arc<AtomicBool>,
    png_slot: Arc<std::sync::Mutex<Option<anyhow::Result<Vec<u8>>>>>,
}

/// PNG-encode a window screenshot on a worker thread into `slot`, so
/// the renderer that delivered it is not held up.
fn encode_window_shot(
    image: Arc<egui::ColorImage>,
    slot: Arc<std::sync::Mutex<Option<anyhow::Result<Vec<u8>>>>>,
) {
    let spawned = std::thread::Builder::new()
        .name("ryll-bugreport-window-png".to_string())
        .spawn(move || {
            let [width, height] = image.size;
            let result = encode_png(image.as_raw(), width as u32, height as u32);
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(result);
            }
        });
    if let Err(e) = spawned {
        debug!("app: failed to spawn window PNG encoder thread: {}", e);
    }
}

/// Shown when a Display report enters region selection, so the
/// mode is announced somewhere that outlives the banner.
const REGION_SELECT_PROMPT: &str = "Drag a rectangle over the problem to finish the bug report, \
     or press Escape to save it without a region";

/// Shown when something tries to open the bug report dialog while a
/// Display report is still waiting for its region.
pub(super) const REGION_SELECT_PENDING: &str =
    "A Display bug report is waiting for its region: drag a \
     rectangle over the problem, or press Escape to save it without one";

/// Captured on the UI thread when a bug-report dialog opens.
///
/// The raw RGBA is cloned and handed to a background
/// `std::thread` that PNG-encodes into `png_slot`; the UI
/// thread holds another clone of the `Arc` and consumes the
/// bytes at submit time (or drops the `Arc` on cancel, letting
/// the worker write into what becomes garbage).
///
/// Timestamps are recorded synchronously on the UI thread at
/// dialog-open — they never depend on the encoder thread
/// finishing — so they always make it into metadata.json even
/// when the surface is missing or encoding fails.
pub(super) struct TriggerSnapshot {
    /// ISO 8601 UTC timestamp of when the dialog opened. Same
    /// format as `ReportMetadata::timestamp`.
    triggered_at: String,
    /// Session uptime in seconds at the moment of dialog open.
    /// Same units as `AppSnapshot::uptime_secs`.
    triggered_uptime_secs: f64,
    /// Slot the encoder thread fills with either the PNG bytes
    /// or an `Err` on encode failure. `None` while the worker is
    /// still running.
    png_slot: Arc<std::sync::Mutex<Option<anyhow::Result<Vec<u8>>>>>,
}

/// How a Display report's region selection ended. Every ending
/// writes the report; only a completed drag gives it a region. A
/// selection the user never finishes -- the window closes, or a
/// reconnect clears the surface out from under it -- still has a
/// report the user asked for behind it, so it is written without a
/// region rather than dropped, and the log says which ending it was.
#[derive(Debug, Clone)]
pub(super) enum RegionSelectEnd {
    /// A drag was released over a rectangle with non-zero area.
    Selected(ReportRegion),
    /// Escape: the user chose to save without a region.
    Skipped,
    /// A reconnect is about to clear the surface being selected
    /// from.
    Reconnecting,
    /// The connection was lost, and its surfaces are about to be
    /// cleared.
    Disconnected,
    /// The window is closing.
    WindowClosed,
}

impl RegionSelectEnd {
    /// The region the report is written with.
    fn region(&self) -> Option<ReportRegion> {
        match self {
            RegionSelectEnd::Selected(region) => Some(region.clone()),
            _ => None,
        }
    }

    /// How the selection ended, for the log line.
    fn describe(&self) -> &'static str {
        match self {
            RegionSelectEnd::Selected(_) => "completed",
            RegionSelectEnd::Skipped => "skipped with Escape; saving without a region",
            RegionSelectEnd::Reconnecting => "abandoned by a reconnect; saving without a region",
            RegionSelectEnd::Disconnected => {
                "abandoned by a lost connection; saving without a region"
            }
            RegionSelectEnd::WindowClosed => {
                "abandoned by the window closing; saving without a region"
            }
        }
    }
}

impl RyllApp {
    /// Clone the largest surface's RGBA pixels, capture trigger
    /// timestamps, and spawn a background thread to PNG-encode.
    ///
    /// Called from every dialog-open path. Idempotent — a no-op
    /// when a snapshot is already pending, so call sites can
    /// fire it unconditionally without worrying about a second
    /// open stomping the first.
    ///
    /// The operator's rule is to always capture here, regardless
    /// of which report type the dialog was opened with, so the
    /// artefact survives the form-filling delay. The decision to
    /// *include* the resulting PNG in the zip happens at submit
    /// time in `BugReport::assemble`, which drops it for
    /// non-Display submissions.
    pub(super) fn begin_trigger_snapshot(&mut self) {
        if self.pending_trigger.is_some() {
            return;
        }
        // The interface as the last frame drew it, before the dialog.
        self.ui_state.mark_report_trigger();
        self.request_window_shot();
        let triggered_at = chrono_now();
        let triggered_uptime_secs = self.traffic.elapsed().as_secs_f64();

        // Pre-first-SURFACE_CREATE: no surface to snap. Record
        // timestamps anyway and seed an Err into the slot so the
        // submit path falls back cleanly to live encoding (which
        // will also produce None when there's no surface).
        let Some(surface) = self
            .surfaces
            .surfaces
            .values()
            .max_by_key(|s| (s.width as u64) * (s.height as u64))
        else {
            let slot = Arc::new(std::sync::Mutex::new(Some(Err(anyhow::anyhow!(
                "no surface available at trigger time"
            )))));
            self.pending_trigger = Some(TriggerSnapshot {
                triggered_at,
                triggered_uptime_secs,
                png_slot: slot,
            });
            return;
        };

        let pixels: Vec<u8> = surface.pixels().to_vec();
        let width = surface.width;
        let height = surface.height;
        let slot: Arc<std::sync::Mutex<Option<anyhow::Result<Vec<u8>>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_for_thread = Arc::clone(&slot);

        let spawned = std::thread::Builder::new()
            .name("ryll-bugreport-png".to_string())
            .spawn(move || {
                let result = encode_png(&pixels, width, height);
                // If the Mutex is poisoned the UI thread panicked;
                // nothing to write and the process is already
                // tearing down.
                if let Ok(mut guard) = slot_for_thread.lock() {
                    *guard = Some(result);
                }
            });
        if let Err(e) = spawned {
            // Thread spawn failed (OS resource pressure); leave
            // the slot empty so the submit path falls back to
            // live encoding.
            debug!("app: failed to spawn PNG encoder thread: {}", e);
        }

        self.pending_trigger = Some(TriggerSnapshot {
            triggered_at,
            triggered_uptime_secs,
            png_slot: slot,
        });
    }

    /// Drop the pending snapshot without consuming it. The
    /// encoder thread (if still running) keeps its Arc clone and
    /// eventually writes into a Mutex whose last reference it
    /// holds, which is then dropped. Safe when no snapshot is
    /// pending.
    pub(super) fn discard_trigger_snapshot(&mut self) {
        let _ = self.pending_trigger.take();
        self.window_shot = None;
    }

    /// Ask eframe for a screenshot of the frame being drawn. The bug
    /// report dialog is held back until it arrives (see
    /// `window_shot_holds_dialog`), so the frame shows what the user was
    /// reporting rather than the dialog.
    fn request_window_shot(&mut self) {
        let arrived = Arc::new(AtomicBool::new(false));
        let png_slot = Arc::new(std::sync::Mutex::new(None));
        let ctx = self.egui_ctx.clone();
        let (cb_arrived, cb_slot) = (Arc::clone(&arrived), Arc::clone(&png_slot));
        // The renderer calls this, possibly off the UI thread, and does
        // not ask for a frame, so it repaints to release the held dialog.
        self.egui_ctx.request_screenshot(move |image| {
            cb_arrived.store(true, Ordering::Release);
            encode_window_shot(image, cb_slot);
            ctx.request_repaint();
        });
        self.window_shot = Some(WindowShot {
            requested_at: Instant::now(),
            arrived,
            png_slot,
        });
        self.egui_ctx.request_repaint();
    }

    /// Whether the bug report dialog should wait another frame for the
    /// window screenshot. Not past `WINDOW_SHOT_WAIT`: a renderer that
    /// never delivers one must not keep the dialog from opening.
    pub(super) fn window_shot_holds_dialog(&self) -> bool {
        self.window_shot.as_ref().is_some_and(|s| {
            !s.arrived.load(Ordering::Acquire) && s.requested_at.elapsed() < WINDOW_SHOT_WAIT
        })
    }

    /// The window screenshot's PNG, if it arrived and has been encoded.
    /// `try_lock`, as for the guest screenshot, so a still-running
    /// encoder can't block the UI thread.
    pub(super) fn take_window_shot_png(&mut self) -> Option<Vec<u8>> {
        let shot = self.window_shot.take()?;
        let mut guard = shot.png_slot.try_lock().ok()?;
        guard.take().and_then(|r| r.ok())
    }

    /// Record which of ryll's panels, menus and dialogs this frame drew,
    /// and egui's claim on input, for bug reports. Called at the end of
    /// `ui`, so it sees what the frame ended with.
    pub(super) fn observe_ui_state(&mut self, ctx: &egui::Context) {
        let elements = [
            ("notifications_panel", self.show_notifications_panel),
            (
                "menu_or_popup",
                ctx.any_popup_open() || egui::Popup::is_any_open(ctx),
            ),
            ("traffic_viewer", self.show_traffic_viewer),
            ("usb_panel", self.show_usb_panel),
            ("folders_panel", self.show_webdav_panel),
            ("protocol_gaps", self.gaps_popup_open),
            ("bug_report_dialog", self.show_bug_dialog),
            ("region_select", self.region_select_active),
            ("paste_error_dialog", self.paste_error_message.is_some()),
            (
                "connection_modal",
                matches!(self.reconnect_state, ReconnectState::Modal(_)),
            ),
        ];
        self.ui_state.observe(&elements, EguiInputClaims::read(ctx));
    }

    /// Called from `finish_bug_report`. Returns the captured
    /// trigger timestamps (if any) and the encoded PNG bytes (if
    /// the worker has finished with `Ok`). Uses `try_lock` so a
    /// still-running encoder on a large surface can't block the
    /// UI thread — `None` falls back to live encoding inside
    /// `BugReport::assemble`.
    pub(super) fn take_trigger_for_submit(
        &mut self,
    ) -> (Option<TriggerTimestamps>, Option<Vec<u8>>) {
        let Some(snap) = self.pending_trigger.take() else {
            return (None, None);
        };
        // The `.ok()` intentionally discards any `Err` written
        // by the encoder thread: an encode failure (or the
        // no-surface-at-trigger-time sentinel seeded by
        // `begin_trigger_snapshot`) is indistinguishable here
        // from "the worker hadn't finished yet", and in both
        // cases we want the submit path to fall back to live
        // encoding of the submit-time surface. If we ever need
        // to surface the error to the user, it has to be logged
        // here *and* handled in the live-encode fallback.
        let png = match snap.png_slot.try_lock() {
            Ok(mut guard) => guard.take().and_then(|r| r.ok()),
            Err(_) => None,
        };
        let trigger = TriggerTimestamps {
            triggered_at: snap.triggered_at,
            triggered_uptime_secs: snap.triggered_uptime_secs,
        };
        (Some(trigger), png)
    }

    /// Enter region selection for a Display report whose dialog was
    /// just submitted.
    pub(super) fn begin_region_select(&mut self) {
        self.region_select_active = true;
        self.region_drag_start = None;
        self.region_drag_end = None;
        info!("app: bug report waiting for region selection");
        self.push_notification(
            NotifySeverity::Info,
            NotificationSource::BugReport,
            REGION_SELECT_PROMPT,
        );
    }

    /// Leave region selection and write the report. A no-op when no
    /// selection is in progress, so callers on teardown paths need
    /// not check.
    pub(super) fn end_region_select(&mut self, how: RegionSelectEnd) {
        if !self.region_select_active {
            return;
        }
        self.region_select_active = false;
        self.region_drag_start = None;
        self.region_drag_end = None;
        info!("app: region selection {}", how.describe());
        let report_type = self.bug_report_type.clone();
        let description = self.bug_description.clone();
        self.finish_bug_report(report_type, description, how.region());
    }
}

/// Build a `ReportRegion` from the raw drag-start / drag-end
/// coordinates produced by the region-select widget, iff the
/// resulting rectangle has strictly positive area. Returns
/// `None` for click-without-drag — the GUI handler uses this
/// to keep the user in region-select mode and surface a
/// "drag a non-zero region" notification rather than emitting
/// a degenerate `ReportRegion` into `report.json`.
///
/// "Strictly positive area" means `right > left AND
/// bottom > top` — a deliberate 1-pixel drag is allowed
/// since it points at a specific pixel, and rejecting it
/// would require a jitter threshold that no current data
/// justifies.
///
/// Reversed drags (bottom-right to top-left) are normalised
/// here so the produced region always satisfies
/// `left ≤ right && top ≤ bottom`.
pub(super) fn validate_region(sx: u32, sy: u32, ex: u32, ey: u32) -> Option<ReportRegion> {
    let left = sx.min(ex);
    let right = sx.max(ex);
    let top = sy.min(ey);
    let bottom = sy.max(ey);
    if right > left && bottom > top {
        Some(ReportRegion {
            left,
            top,
            right,
            bottom,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Region-select validation (K4 guard) ─────────────────

    #[test]
    fn validate_region_click_without_drag_returns_none() {
        // Press and release at the same point: degenerate 0×0
        // rectangle, must be rejected at the GUI layer so we
        // never serialise it into report.json.
        assert!(validate_region(100, 100, 100, 100).is_none());
    }

    #[test]
    fn validate_region_zero_width_returns_none() {
        // Same x, non-zero vertical drag — still degenerate.
        assert!(validate_region(50, 10, 50, 80).is_none());
    }

    #[test]
    fn validate_region_zero_height_returns_none() {
        // Non-zero horizontal drag, same y — still degenerate.
        assert!(validate_region(10, 40, 100, 40).is_none());
    }

    #[test]
    fn validate_region_one_by_one_returns_some() {
        // A deliberate 1-pixel drag points at a specific pixel
        // and must be allowed; rejecting it would require a
        // jitter floor without supporting data.
        let r = validate_region(7, 11, 8, 12).expect("1x1 region valid");
        assert_eq!(r.left, 7);
        assert_eq!(r.top, 11);
        assert_eq!(r.right, 8);
        assert_eq!(r.bottom, 12);
    }

    #[test]
    fn validate_region_normal_drag_returns_some() {
        // Happy path: a 30×40 region produced by a normal
        // top-left → bottom-right drag.
        let r = validate_region(10, 20, 40, 60).expect("normal region valid");
        assert_eq!(r.left, 10);
        assert_eq!(r.top, 20);
        assert_eq!(r.right, 40);
        assert_eq!(r.bottom, 60);
    }

    #[test]
    fn validate_region_reversed_drag_normalises() {
        // Drag from bottom-right to top-left must produce the
        // same canonical {left ≤ right, top ≤ bottom} region
        // as the forward drag — the bug-report consumers
        // assume this invariant.
        let r = validate_region(40, 60, 10, 20).expect("reversed drag valid");
        assert_eq!(r.left, 10);
        assert_eq!(r.top, 20);
        assert_eq!(r.right, 40);
        assert_eq!(r.bottom, 60);
    }

    // ── Region selection endings ───────────────────────────

    #[test]
    fn region_select_end_only_a_drag_carries_a_region() {
        let region = validate_region(10, 20, 40, 60).expect("valid region");
        let r = RegionSelectEnd::Selected(region)
            .region()
            .expect("a completed drag carries its region");
        assert_eq!((r.left, r.top, r.right, r.bottom), (10, 20, 40, 60));
        for how in [
            RegionSelectEnd::Skipped,
            RegionSelectEnd::Reconnecting,
            RegionSelectEnd::Disconnected,
            RegionSelectEnd::WindowClosed,
        ] {
            assert!(how.region().is_none(), "{:?}", how);
        }
    }

    #[test]
    fn region_select_end_says_unfinished_selections_still_save() {
        // The log line is the only record of how a selection ended,
        // and every ending must save.
        assert_eq!(
            RegionSelectEnd::Selected(validate_region(0, 0, 1, 1).unwrap()).describe(),
            "completed"
        );
        for how in [
            RegionSelectEnd::Skipped,
            RegionSelectEnd::Reconnecting,
            RegionSelectEnd::Disconnected,
            RegionSelectEnd::WindowClosed,
        ] {
            assert!(
                how.describe().ends_with("saving without a region"),
                "{:?}: {}",
                how,
                how.describe()
            );
        }
    }
}
