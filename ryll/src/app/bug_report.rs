use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::bugreport::{
    BugReport, BugReportType, NotificationSnapshotState, ReportRegion, TrafficBuffers,
    TriggerTimestamps,
};
use crate::notifications::NotificationSource;
use shakenfist_spice_protocol::{ChannelType, NotifySeverity};
use shakenfist_spice_renderer::metrics::RuntimeMetrics;

use super::trigger_snapshot::REGION_SELECT_PENDING;
use super::RyllApp;

/// Window for the runtime-metrics sample taken when a bug report
/// is submitted from the GUI. Matches the window `BugReport::new`
/// uses on the non-interactive paths.
const BUG_REPORT_METRICS_WINDOW: Duration = Duration::from_secs(2);

/// How long `poll_pending_bug_report` waits for the sampler before
/// giving up on it and writing the report anyway.
///
/// Four times the sample window: long enough that a sampler merely
/// descheduled on a loaded host still lands its measurement, short
/// enough that a dead one does not strand the re-entrancy guard for
/// the rest of the session.
const BUG_REPORT_METRICS_DEADLINE: Duration = Duration::from_secs(8);

/// How long past the end of its sample window a bug report still
/// being sampled when the window closes is waited for, before it is
/// written without the sample. Far shorter than
/// `BUG_REPORT_METRICS_DEADLINE`: the user has asked to quit, and a
/// window that lingers for eight seconds after its close button
/// looks hung.
const BUG_REPORT_EXIT_GRACE: Duration = Duration::from_secs(1);

/// A disconnect report asked for while draining channel events, written
/// once the drain is done. See `App::request_disconnect_report`.
pub(super) struct PendingDisconnectReport {
    pub(super) channel: String,
    pub(super) message: String,
    /// As in `DisconnectCause::disconnect_kind`.
    pub(super) kind: String,
}

/// Wait until `deadline` for a bug report's metrics sampler to fill
/// its slot, and take the sample. `None` if it has not landed by
/// then. Used only at exit, where there is no next frame to poll
/// from.
fn wait_for_metrics_sample(
    slot: &std::sync::Mutex<Option<RuntimeMetrics>>,
    deadline: Instant,
) -> Option<RuntimeMetrics> {
    loop {
        if let Ok(mut guard) = slot.lock() {
            if let Some(metrics) = guard.take() {
                return Some(metrics);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}

/// A GUI-submitted bug report waiting on its runtime-metrics
/// sample.
///
/// `metrics::sample` blocks for `BUG_REPORT_METRICS_WINDOW`, so
/// the UI thread hands the sample to a worker and parks the rest
/// of the submission here. Everything is captured at submit time
/// rather than re-read when the sample lands, so the zip
/// describes the frame the user was looking at when they pressed
/// Capture and not the one two seconds later.
pub(super) struct PendingBugReport {
    report_type: BugReportType,
    description: String,
    region: Option<ReportRegion>,
    trigger: Option<TriggerTimestamps>,
    precomputed_png: Option<Vec<u8>>,
    /// Submit-time RGBA of the largest surface plus its
    /// dimensions. `None` for non-Display reports and when no
    /// surface exists yet.
    surface: Option<(Vec<u8>, u32, u32)>,
    /// PNG of the whole window from the frame the report started in.
    /// Only `BugReportType::Ui` reports keep it.
    window_png: Option<Vec<u8>>,
    /// Slot the sampler thread fills exactly once. `None` while
    /// it is still inside its sample window.
    metrics_slot: Arc<std::sync::Mutex<Option<RuntimeMetrics>>>,
    /// When the sample was started, so a sampler that never
    /// reports cannot hold the re-entrancy guard forever. Nothing
    /// in today's `metrics::sample` can fail to fill the slot --
    /// both platform paths return `unavailable` rather than
    /// panicking -- but the guard is the only thing standing
    /// between a dead worker and an F12 key that silently refuses
    /// for the rest of the session.
    started_at: Instant,
}

impl RyllApp {
    /// Assemble a parked bug report and write it to disk.
    /// Returns the path of the written zip file.
    ///
    /// Goes to `BugReport::assemble` rather than
    /// `BugReport::new` because the runtime-metrics sample has
    /// already been taken off the UI thread — see
    /// `finish_bug_report`.
    ///
    /// `pending.trigger` carries the trigger-time timestamps
    /// captured when the dialog opened; `pending.precomputed_png`
    /// carries the PNG that the background encoder produced from
    /// the trigger-time surface. Both are `None` if the dialog
    /// wasn't open when the report was started (e.g. a dev-only
    /// path that triggers reports without a dialog, though none
    /// exists today) — `BugReport` falls back to the submit-time
    /// behaviour in that case.
    fn generate_bug_report(
        &self,
        pending: PendingBugReport,
        runtime_metrics: RuntimeMetrics,
    ) -> anyhow::Result<std::path::PathBuf> {
        // Keep the surface-pixels fallback path. It's the safety net
        // when the background encoder wasn't spawned or hasn't finished,
        // and `BugReport::assemble` also crops it to produce the
        // submit-time region image.
        let surface_data = pending
            .surface
            .as_ref()
            .map(|(pixels, width, height)| (pixels.as_slice(), *width, *height));

        let mut report = BugReport::assemble(
            pending.report_type,
            pending.description,
            pending.region,
            &self.target,
            &self.traffic,
            &self.channel_snapshots,
            &self.app_snapshot,
            &self.notifications,
            surface_data,
            runtime_metrics,
            pending.trigger,
            pending.precomputed_png,
        )?;
        if let Some(png) = pending.window_png {
            report.set_window_screenshot(png);
        }

        let output_dir = self.manual_bug_report_dir();
        report.write_zip(&output_dir)
    }

    /// Resolve the output directory for a manual (F12 /
    /// Menu → Report) or auto-disconnect bug report.
    /// Priority:
    ///   1. --bug-report-dir if set
    ///   2. <--capture>/bug-reports/ if --capture is set
    ///   3. current working directory
    pub(super) fn manual_bug_report_dir(&self) -> PathBuf {
        if let Some(d) = &self.bug_report_dir {
            return d.clone();
        }
        match &self.capture {
            Some(cap) => cap.dir.join("bug-reports"),
            None => std::env::current_dir().unwrap_or_else(|_| ".".into()),
        }
    }

    /// Record that `channel` ended, and how, for the session's next
    /// disconnect report. The first end of a channel is kept: main's
    /// `DISCONNECTING` is followed by the close it announced.
    pub(super) fn note_channel_end(&mut self, channel: ChannelType, kind: &str) {
        let at_secs = self.traffic.elapsed().as_secs_f64();
        self.channel_ends
            .entry(channel.name().to_string())
            .or_insert_with(|| crate::bugreport::ChannelEnd {
                kind: kind.to_string(),
                at_secs,
            });
    }

    /// Ask for a disconnect report once this frame's events are
    /// handled. The first request of a frame names the report; later
    /// ones in the same frame show up in its per-channel states.
    pub(super) fn request_disconnect_report(&mut self, channel: &str, message: &str, kind: &str) {
        if self.pending_disconnect_report.is_none() {
            self.pending_disconnect_report = Some(PendingDisconnectReport {
                channel: channel.to_string(),
                message: message.to_string(),
                kind: kind.to_string(),
            });
        }
    }

    /// Auto-write a disconnect-snapshot bug report, best-effort.
    /// Subject to a 60 s cooldown to bound disk usage during a
    /// disconnect storm. Failures are logged but never block
    /// the disconnect modal. Runtime metrics are reported as
    /// unavailable here (a 1 s sample on the GUI thread would
    /// freeze the UI); pcap and snapshots are the load-bearing
    /// data for diagnosing the disconnect.
    pub(super) fn maybe_write_disconnect_snapshot(
        &mut self,
        channel: &str,
        message: &str,
        kind: &str,
    ) {
        if let Some(at) = self.last_disconnect_report_at {
            if at.elapsed() < Duration::from_secs(60) {
                debug!(
                    "app: disconnect snapshot cooldown active ({}s remaining), skipping for {}",
                    60u64.saturating_sub(at.elapsed().as_secs()),
                    channel
                );
                return;
            }
        }

        let keepalive_timeout_fired = self
            .channel_snapshots
            .main
            .lock()
            .map(|s| s.keepalive_timeout_fired)
            .unwrap_or(false);

        let cause = crate::bugreport::DisconnectCause {
            channel: channel.to_string(),
            error_message: message.to_string(),
            error_kind: None,
            disconnect_kind: kind.to_string(),
            keepalive_timeout_fired,
            session_uptime_secs: self.traffic.elapsed().as_secs_f64(),
            per_channel: crate::bugreport::DisconnectCause::collect_per_channel(
                &self.channel_snapshots,
                &self.channel_ends,
            ),
        };

        let runtime_metrics = shakenfist_spice_renderer::metrics::RuntimeMetrics::unavailable(
            "runtime metrics are not sampled on the GUI thread for auto-disconnect snapshots",
        );

        let output_dir = self.manual_bug_report_dir();
        match BugReport::write_disconnect(
            &output_dir,
            cause,
            &self.target,
            &self.traffic,
            &self.channel_snapshots,
            &self.app_snapshot,
            &self.notifications,
            runtime_metrics,
        ) {
            Ok(path) => {
                info!("app: disconnect snapshot saved to {}", path.display());
                self.push_notification(
                    NotifySeverity::Info,
                    NotificationSource::BugReport,
                    format!("Disconnect snapshot saved to {}", path.display()),
                );
                self.last_disconnect_report_at = Some(Instant::now());
            }
            Err(e) => {
                error!("app: failed to write disconnect snapshot: {}", e);
                // Still update the cooldown so a write that fails
                // for an environmental reason (no disk space, bad
                // dir) doesn't retry on every disconnect event.
                self.last_disconnect_report_at = Some(Instant::now());
            }
        }
    }

    /// The user clicked "File bug report" on a notification row.
    /// Looks up the notification, consumes any live snapshot for it,
    /// and writes a zip via `BugReport::write_notification`. Always
    /// produces a report — when no live snapshot is available, falls
    /// back to the current live `TrafficBuffers` and tags the report
    /// `PostEventOnly`.
    pub(super) fn file_notification_bug_report(&mut self, id: u64) {
        // Resolve the notification entry first so we can use
        // its severity/message in the report description.
        let entry = match self.notifications.lock() {
            Ok(guard) => guard.iter_newest_first().find(|e| e.id == id).cloned(),
            Err(_) => None,
        };
        let Some(entry) = entry else {
            warn!(
                "app: file_notification_bug_report({}) — entry no longer in store",
                id
            );
            return;
        };

        // Take the live snapshot (if any). If absent or
        // expired, fall back to the current live traffic.
        let now = Instant::now();
        let (traffic_owned, snapshot_state): (Option<TrafficBuffers>, NotificationSnapshotState) = {
            match self.notification_snapshots.lock() {
                Ok(mut store) => match store.take(id, now) {
                    Some(snap) => (Some(snap), NotificationSnapshotState::AtFire),
                    None => (None, NotificationSnapshotState::PostEventOnly),
                },
                Err(_) => (None, NotificationSnapshotState::PostEventOnly),
            }
        };
        let traffic_ref: &TrafficBuffers = match &traffic_owned {
            Some(s) => s,
            None => self.traffic.as_ref(),
        };

        let output_dir = self.manual_bug_report_dir();
        let runtime_metrics = shakenfist_spice_renderer::metrics::RuntimeMetrics::unavailable(
            "runtime metrics are not sampled on the GUI thread for notification \
                 bug reports — the pcap and channel snapshots are the load-bearing data",
        );

        match BugReport::write_notification(
            &output_dir,
            &entry,
            snapshot_state,
            &self.target,
            traffic_ref,
            &self.channel_snapshots,
            &self.app_snapshot,
            &self.notifications,
            runtime_metrics,
        ) {
            Ok(path) => {
                info!("app: notification bug report saved to {}", path.display());
                self.push_notification(
                    NotifySeverity::Info,
                    NotificationSource::BugReport,
                    format!("Bug report saved to {}", path.display()),
                );
                self.warn_if_too_big_to_attach(&path);
            }
            Err(e) => {
                error!("app: failed to write notification bug report: {}", e);
                self.push_notification(
                    NotifySeverity::Error,
                    NotificationSource::BugReport,
                    format!("Bug report failed: {}", e),
                );
            }
        }
    }

    /// Start a bug report submitted from the GUI.
    ///
    /// Captures everything only the UI thread can reach, then
    /// hands the runtime-metrics sample to a worker thread;
    /// `poll_pending_bug_report` assembles and writes the zip
    /// once the sample lands. Sampling inline would freeze the
    /// window for the whole sample window and stall the UI
    /// thread's draining of the bounded channel-event queue,
    /// back-pressuring every SPICE read loop behind it.
    pub(super) fn finish_bug_report(
        &mut self,
        report_type: BugReportType,
        description: String,
        region: Option<ReportRegion>,
    ) {
        if self.pending_bug_report.is_some() {
            // Nothing above this call guards re-entry: every
            // submit path clears `show_bug_dialog` /
            // `region_select_active` before the previous report
            // has been written, so F12 is live again immediately.
            self.discard_trigger_snapshot();
            self.push_notification(
                NotifySeverity::Warn,
                NotificationSource::BugReport,
                "A bug report is already being captured — ignoring this one",
            );
            return;
        }

        let (trigger, precomputed_png) = self.take_trigger_for_submit();
        let window_png = self
            .take_window_shot_png()
            .filter(|_| report_type == BugReportType::Ui);

        // Copying the largest surface is a deep copy of the whole
        // framebuffer -- 33 MB of RGBA at 3840x2160 -- on the UI thread,
        // held for the ~2 s the sample is in flight. `assemble` reads it in
        // exactly two places: as the screenshot when no PNG was precomputed
        // at trigger time, and as the source for a region crop. In the
        // common case (dialog open started the encoder, no region drawn)
        // nothing reads it, so the copy is pure cost on the one thread this
        // path exists to keep responsive.
        let surface_is_readable = report_type == BugReportType::Display
            && (precomputed_png.is_none() || region.is_some());
        let surface = if surface_is_readable {
            self.surfaces
                .surfaces
                .values()
                .max_by_key(|s| (s.width as u64) * (s.height as u64))
                .map(|s| (s.pixels().to_vec(), s.width, s.height))
        } else {
            None
        };

        let metrics_slot: Arc<std::sync::Mutex<Option<RuntimeMetrics>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_for_thread = Arc::clone(&metrics_slot);
        let ctx = self.egui_ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("ryll-bugreport-metrics".to_string())
            .spawn(move || {
                let metrics = shakenfist_spice_renderer::metrics::sample(BUG_REPORT_METRICS_WINDOW);
                // A poisoned Mutex means the UI thread panicked;
                // there is nobody left to hand the sample to.
                if let Ok(mut guard) = slot_for_thread.lock() {
                    *guard = Some(metrics);
                }
                ctx.request_repaint();
            });
        let sampling = spawned.is_ok();
        if let Err(e) = spawned {
            // Thread spawn failed (OS resource pressure). Seed the
            // slot so the next poll still produces a report, and
            // ask for the frame the sampler would have requested.
            debug!("app: failed to spawn bug-report metrics thread: {}", e);
            if let Ok(mut guard) = metrics_slot.lock() {
                *guard = Some(RuntimeMetrics::unavailable(format!(
                    "failed to spawn the metrics sampler thread: {}",
                    e
                )));
            }
            self.egui_ctx.request_repaint();
        }

        // The notification store is itself bug-report evidence, so it
        // must not claim a sample is running that never started.
        if sampling {
            self.push_notification(
                NotifySeverity::Info,
                NotificationSource::BugReport,
                format!(
                    "Capturing bug report — sampling runtime metrics for {} s",
                    BUG_REPORT_METRICS_WINDOW.as_secs()
                ),
            );
        } else {
            self.push_notification(
                NotifySeverity::Warn,
                NotificationSource::BugReport,
                "Capturing bug report — runtime metrics unavailable",
            );
        }

        self.pending_bug_report = Some(PendingBugReport {
            report_type,
            description,
            region,
            trigger,
            precomputed_png,
            surface,
            window_png,
            metrics_slot,
            started_at: Instant::now(),
        });
    }

    /// Write a parked bug report once its metrics sample lands.
    /// Called once per frame; a no-op until the sampler thread
    /// has filled the slot.
    pub(super) fn poll_pending_bug_report(&mut self) {
        let Some(pending) = self.pending_bug_report.as_ref() else {
            return;
        };
        // `try_lock`, not `lock`: losing the race against the
        // sampler's single write costs one frame, whereas blocking
        // here would reintroduce the stall this path exists to
        // avoid.
        let metrics = match pending.metrics_slot.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        // A sampler that never reports would otherwise hold the
        // re-entrancy guard for the life of the process, and every
        // later F12 would be refused with "already being captured"
        // after the user had typed a whole description. Past the
        // deadline, write the report with what we have: a degraded
        // report beats a permanently wedged key. The window is
        // generous -- four times the sample -- because expiring a
        // merely-slow sampler would lose a real measurement.
        let metrics = match metrics {
            Some(m) => m,
            None => {
                let waited = pending.started_at.elapsed();
                if waited <= BUG_REPORT_METRICS_DEADLINE {
                    return;
                }
                warn!(
                    "app: bug-report metrics sampler did not report within {:?}; \
                     writing the report without a sample",
                    waited
                );
                RuntimeMetrics::unavailable(format!(
                    "the metrics sampler thread did not report within {} s",
                    BUG_REPORT_METRICS_DEADLINE.as_secs()
                ))
            }
        };
        let pending = self
            .pending_bug_report
            .take()
            .expect("pending bug report checked immediately above");
        self.write_pending_bug_report(pending, metrics);
    }

    /// Reports keep their traffic within a budget that fits a GitHub
    /// attachment, but an unusually large screenshot can still push
    /// one over. Say so, rather than leave the user to find out when
    /// the upload fails.
    fn warn_if_too_big_to_attach(&mut self, path: &std::path::Path) {
        if let Some(size) = crate::bugreport::oversized_for_attachment(path) {
            let msg = format!(
                "Bug report is {:.1} MB, over GitHub's {} MB attachment limit; \
                 share it another way, or attach only the files inside it",
                size as f64 / 1e6,
                crate::bugreport::GITHUB_ATTACHMENT_LIMIT_BYTES / 1_000_000
            );
            warn!("app: {}", msg);
            self.push_notification(NotifySeverity::Warn, NotificationSource::BugReport, msg);
        }
    }

    fn write_pending_bug_report(&mut self, pending: PendingBugReport, metrics: RuntimeMetrics) {
        match self.generate_bug_report(pending, metrics) {
            Ok(path) => {
                let msg = format!("Bug report saved to {}", path.display());
                info!("app: {}", msg);
                self.push_notification(NotifySeverity::Info, NotificationSource::BugReport, msg);
                self.warn_if_too_big_to_attach(&path);
            }
            Err(e) => {
                let msg = format!("Bug report failed: {}", e);
                error!("app: {}", msg);
                self.push_notification(NotifySeverity::Error, NotificationSource::BugReport, msg);
            }
        }
    }

    /// Write a bug report that is still waiting for its metrics
    /// sample when the window closes. `poll_pending_bug_report`
    /// runs once per frame and there are no more frames, so a
    /// report submitted in the last couple of seconds -- including
    /// one whose region selection the close has just ended -- is
    /// otherwise lost without a log line.
    pub(super) fn flush_pending_bug_report_at_exit(&mut self) {
        let Some(pending) = self.pending_bug_report.take() else {
            return;
        };
        let deadline = pending.started_at + BUG_REPORT_METRICS_WINDOW + BUG_REPORT_EXIT_GRACE;
        info!("app: window closing; writing the bug report still being captured");
        let metrics =
            wait_for_metrics_sample(&pending.metrics_slot, deadline).unwrap_or_else(|| {
                RuntimeMetrics::unavailable(
                    "the window closed before the metrics sample completed".to_string(),
                )
            });
        self.write_pending_bug_report(pending, metrics);
    }

    /// Open the bug report dialog, unless a Display report is still
    /// waiting for its region. Opening it then would start a new
    /// trigger snapshot over the pending report's and restart the
    /// report the user is half way through.
    pub(super) fn open_bug_dialog(&mut self, report_type: BugReportType, description: String) {
        if self.region_select_active {
            self.push_notification(
                NotifySeverity::Warn,
                NotificationSource::BugReport,
                REGION_SELECT_PENDING,
            );
            return;
        }
        self.show_bug_dialog = true;
        self.bug_report_type = report_type;
        self.bug_description = description;
        self.begin_trigger_snapshot();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_metrics_sample_takes_a_sample_already_landed() {
        let slot = std::sync::Mutex::new(Some(RuntimeMetrics::unavailable("test".to_string())));
        // A deadline already past must still take a landed sample.
        let got = wait_for_metrics_sample(&slot, Instant::now());
        assert!(got.is_some());
        assert!(
            slot.lock().unwrap().is_none(),
            "the sample is taken, not copied"
        );
    }

    #[test]
    fn wait_for_metrics_sample_waits_for_a_late_sample() {
        let slot = Arc::new(std::sync::Mutex::new(None));
        let writer = Arc::clone(&slot);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            *writer.lock().unwrap() = Some(RuntimeMetrics::unavailable("late".to_string()));
        });
        let got = wait_for_metrics_sample(&slot, Instant::now() + Duration::from_secs(5));
        handle.join().unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn wait_for_metrics_sample_gives_up_at_the_deadline() {
        let slot: std::sync::Mutex<Option<RuntimeMetrics>> = std::sync::Mutex::new(None);
        let start = Instant::now();
        let got = wait_for_metrics_sample(&slot, start + Duration::from_millis(60));
        assert!(got.is_none());
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(60), "{:?}", waited);
        assert!(waited < Duration::from_secs(2), "{:?}", waited);
    }

    #[test]
    fn notification_bug_report_type_serialises() {
        // Round-trip a BugReportType::Notification through
        // serde_json to confirm both fields (notification_id
        // and snapshot_state) land in metadata.json.
        let rt = BugReportType::Notification {
            notification_id: 99,
            snapshot_state: NotificationSnapshotState::AtFire,
        };
        let json = serde_json::to_string(&rt).expect("serialise");
        assert!(json.contains("Notification"));
        assert!(json.contains("\"notification_id\":99"));
        assert!(json.contains("AtFire"));
    }
}
