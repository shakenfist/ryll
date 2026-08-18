/// Auto-snapshot bug-report mode.
///
/// When `--auto-snapshot-interval N` is set, a background task fires a
/// complete `BugReport` every N seconds into a rolling
/// `<bug-report-dir>/auto-snapshots/` subdirectory. The rolling cap is
/// enforced by pruning the oldest zips once the count exceeds
/// `--auto-snapshot-cap` (default 20).
///
/// The task runs on its own tokio runtime in a dedicated std::thread so
/// the GUI thread is never blocked. All data it needs is Arc'd — the
/// same `Arc<TrafficBuffers>`, `ChannelSnapshots` (internally
/// Arc-backed), `Arc<Mutex<AppSnapshot>>`, and `SharedNotifications`
/// that the GUI and connection threads already share.
///
/// Error handling:
///   - write_zip failures: always `warn!`. A `NotifySeverity::Warn`
///     notification is pushed once per 5-minute window so the operator
///     knows something is wrong without being spammed.
///   - prune failures: `warn!` only; the interval loop continues.
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use crate::bugreport::{AppSnapshot, BugReport, BugReportType, ChannelSnapshots, TrafficBuffers};
use crate::notifications::{NotificationEntry, NotificationSource, SharedNotifications};
use shakenfist_spice_protocol::NotifySeverity;

/// Default rolling cap on the number of auto-snapshot zips kept on disk.
pub const DEFAULT_AUTO_SNAPSHOT_CAP: usize = 20;

/// Cool-down between failure notifications so a persistent error (e.g.
/// disk full) does not spam the operator notification panel.
const FAILURE_NOTIFY_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// State bundle for the auto-snapshot interval task.
///
/// Constructed at each session bring-up (after `SessionInitialized`)
/// and moved into the background thread. All handles are cheap to
/// clone (Arc) so no extra locking is required at construction time.
///
/// The `cancel` flag is owned by the spawning `RyllApp` and used to
/// retire this task on reconnect — the app stores the same `Arc`
/// alongside its `AutoSnapshotState`, sets it to `true` before
/// constructing a fresh state with new `traffic` / `channel_snapshots`
/// Arcs on reconnect, then re-spawns. Without the retire-and-respawn
/// pattern the first task would continue holding Arcs to the previous
/// session's TrafficBuffers and ChannelSnapshots (which `reconnect()`
/// replaces with fresh instances), so every subsequent zip would
/// capture empty / stale data from a session that no longer exists.
pub struct AutoSnapshotState {
    pub traffic: Arc<TrafficBuffers>,
    pub channel_snapshots: ChannelSnapshots,
    pub app_snapshot: Arc<Mutex<AppSnapshot>>,
    pub notifications: SharedNotifications,
    pub target_host: String,
    pub target_port: u16,
    /// Resolved output directory: `<bug_report_dir>/auto-snapshots/`.
    pub output_dir: PathBuf,
    pub interval: Duration,
    pub cap: usize,
    /// Shutdown signal. Polled between snapshot ticks; when set to
    /// `true`, the loop exits cleanly at the next interval boundary
    /// (or sooner — see `wait_with_cancel`). Set by `RyllApp` on
    /// reconnect to retire this task before respawning a fresh one
    /// with new state Arcs.
    pub cancel: Arc<AtomicBool>,
}

/// Generate a filesystem-safe filename for an auto-snapshot zip.
///
/// Format: `ryll-auto-snapshot-<utc-iso-safe>-T+<uptime_secs>.zip`
/// where `<utc-iso-safe>` is the current UTC time with colons replaced
/// by hyphens for filesystem portability.
///
/// Example: `ryll-auto-snapshot-2026-05-18T20-37-42Z-T+47.3s.zip`
pub fn auto_snapshot_filename(uptime_secs: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let ts = format_utc_iso_safe(now);
    format!("ryll-auto-snapshot-{}-T+{:.1}s.zip", ts, uptime_secs)
}

/// Format a unix timestamp as a filesystem-safe ISO-8601 UTC string
/// (colons replaced with hyphens so the name is valid on every OS).
///
/// Example: 1747600662 → "2026-05-18T20-37-42Z"
fn format_utc_iso_safe(unix_secs: u64) -> String {
    let s = unix_secs % 60;
    let m = (unix_secs / 60) % 60;
    let h = (unix_secs / 3600) % 24;
    let total_days = unix_secs / 86400;
    let (y, mo, d) = days_to_ymd(total_days);
    format!("{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z", y, mo, d, h, m, s)
}

/// Convert a count of days since the Unix epoch (1970-01-01) to a
/// (year, month, day) tuple in the proleptic Gregorian calendar.
/// Month is 1-based (1 = January).
///
/// Algorithm from http://www.howardhinnant.com/date_algorithms.html
/// "Civil from days" — public domain.
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

/// Prune the oldest auto-snapshot zips in `dir` to keep at most `cap`
/// files. Zips are sorted lexicographically by filename; by construction
/// the filename embeds a UTC timestamp so lex order equals chronological
/// order.
///
/// Returns the number of files deleted.
///
/// Errors from `std::fs::remove_file` are logged at `warn` level but do
/// not abort the prune — we delete as many as we can.
pub fn prune_to_cap(dir: &std::path::Path, cap: usize) -> usize {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("ryll-auto-snapshot-") && n.ends_with(".zip"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            warn!("auto-snapshot: cannot read output dir for pruning: {}", e);
            return 0;
        }
    };

    // Lex sort = chronological sort by construction (timestamp is the
    // leading component of the filename).
    files.sort();

    if files.len() <= cap {
        return 0;
    }

    let excess = files.len() - cap;
    let mut deleted = 0usize;
    for path in files.iter().take(excess) {
        match std::fs::remove_file(path) {
            Ok(()) => {
                debug!("auto-snapshot: pruned {}", path.display());
                deleted += 1;
            }
            Err(e) => {
                warn!("auto-snapshot: failed to prune {}: {}", path.display(), e);
            }
        }
    }
    deleted
}

/// Run the auto-snapshot interval loop.
///
/// This function is async and is intended to be called from a
/// `tokio::spawn`ed task within a dedicated std::thread. It loops until
/// the tokio runtime shuts down.
///
/// Each tick:
///   1. Assemble a `BugReport` via `BugReport::new` (blocks ~2 s on a
///      `spawn_blocking` metrics sample — the tokio runtime stays
///      responsive because the sampling happens off the async executor).
///   2. Write the zip into `state.output_dir` with the auto-snapshot
///      filename scheme.
///   3. Prune the directory to `state.cap`.
///   4. Bump `auto_snapshots_saved` / `auto_snapshots_pruned` in
///      `AppSnapshot`.
///
/// On write failure: `warn!` always; push a `NotifySeverity::Warn`
/// notification at most once per `FAILURE_NOTIFY_COOLDOWN`.
/// Yield until `cancel` becomes `true`. Polls every 500 ms so a
/// retire signal is acted on quickly even when `interval` is much
/// longer (the default auto-snapshot cadence is 30 s but operators
/// may use longer values for long-running sessions).
async fn wait_for_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub async fn run_auto_snapshot_loop(state: AutoSnapshotState) {
    let mut interval = tokio::time::interval(state.interval);
    // Skip the first immediate tick so the operator gets the first
    // snapshot after a full interval (they just saw the startup
    // notification and the mode is freshly armed).
    interval.tick().await;

    let mut last_failure_notified_at: Option<Instant> = None;

    loop {
        // Race the interval tick against a cancel poll so retire
        // latency is bounded by the polling interval rather than
        // by `state.interval` (which can be 30 s or more).
        tokio::select! {
            _ = interval.tick() => {}
            _ = wait_for_cancel(&state.cancel) => {
                info!("auto-snapshot: cancel signalled — exiting loop");
                return;
            }
        }
        if state.cancel.load(Ordering::Relaxed) {
            info!("auto-snapshot: cancel signalled — exiting loop");
            return;
        }

        // Derive uptime from the traffic buffer's elapsed() clock
        // (same baseline used by the rest of the bug-report
        // infrastructure).
        let uptime_secs = state.traffic.elapsed().as_secs_f64();
        let description = format!("auto-snapshot T+{:.1}s", uptime_secs);
        let filename = auto_snapshot_filename(uptime_secs);

        let traffic_snap = state.traffic.clone();
        let channel_snap = state.channel_snapshots.clone();
        let app_snap_arc = state.app_snapshot.clone();
        let notifications_snap = state.notifications.clone();
        let host = state.target_host.clone();
        let port = state.target_port;
        let output_dir = state.output_dir.clone();
        let cap = state.cap;
        let filename_clone = filename.clone();

        // Assemble and write inside spawn_blocking so the 2-second
        // runtime-metrics sample doesn't stall the tokio executor.
        let result = tokio::task::spawn_blocking(move || {
            let report = BugReport::new(
                BugReportType::AutoSnapshot,
                description,
                None, // no region
                &host,
                port,
                &traffic_snap,
                &channel_snap,
                &app_snap_arc,
                &notifications_snap,
                None, // no surface pixels (screenshot out of scope for auto)
                None, // no explicit trigger timestamps
                None, // no precomputed screenshot PNG
            )?;

            std::fs::create_dir_all(&output_dir)?;
            report.write_zip_named(&output_dir, &filename_clone)
        })
        .await;

        let zip_path = match result {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!("auto-snapshot: write failed: {}", e);
                let should_notify = last_failure_notified_at
                    .map(|t| t.elapsed() >= FAILURE_NOTIFY_COOLDOWN)
                    .unwrap_or(true);
                if should_notify {
                    if let Ok(mut guard) = state.notifications.lock() {
                        guard.push(NotificationEntry::new(
                            NotifySeverity::Warn,
                            NotificationSource::Internal,
                            format!("Auto-snapshot write failed: {}", e),
                        ));
                    }
                    last_failure_notified_at = Some(Instant::now());
                }
                continue;
            }
            Err(e) => {
                warn!("auto-snapshot: spawn_blocking panicked: {}", e);
                continue;
            }
        };
        debug!("auto-snapshot: wrote {}", zip_path.display());

        // Prune to cap, then update counters in AppSnapshot.
        let pruned = prune_to_cap(&state.output_dir, cap);
        if let Ok(mut snap) = state.app_snapshot.lock() {
            snap.auto_snapshots_saved = snap.auto_snapshots_saved.saturating_add(1);
            snap.auto_snapshots_pruned = snap.auto_snapshots_pruned.saturating_add(pruned as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    // ── filename generator ──────────────────────────────────

    #[test]
    fn auto_snapshot_filename_has_expected_prefix_and_suffix() {
        let name = auto_snapshot_filename(47.3);
        assert!(
            name.starts_with("ryll-auto-snapshot-"),
            "bad prefix: {}",
            name
        );
        // The uptime suffix must encode the value supplied.
        assert!(name.contains("T+47.3s"), "missing uptime in: {}", name);
        assert!(name.ends_with(".zip"), "bad suffix: {}", name);
    }

    #[test]
    fn auto_snapshot_filename_has_no_colons() {
        // Colons are illegal in Windows filenames; the format must use
        // hyphens instead.
        let name = auto_snapshot_filename(0.0);
        assert!(!name.contains(':'), "colon found in filename: {}", name);
    }

    // ── format_utc_iso_safe ─────────────────────────────────

    #[test]
    fn format_utc_iso_safe_known_timestamp() {
        // 2026-05-18 20:37:42 UTC = 1779136662 unix seconds.
        let s = format_utc_iso_safe(1779136662);
        // Should be 20 chars: "YYYY-MM-DDTHH-MM-SSZ"
        assert_eq!(s.len(), 20, "unexpected length: {}", s);
        assert!(s.ends_with('Z'), "missing Z suffix: {}", s);
        // No colons.
        assert!(!s.contains(':'), "colon in: {}", s);
        // Verify the specific value.
        assert_eq!(s, "2026-05-18T20-37-42Z", "wrong timestamp: {}", s);
    }

    // ── days_to_ymd ─────────────────────────────────────────

    #[test]
    fn days_to_ymd_epoch() {
        // Day 0 = 1970-01-01
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2026-05-18: 1779136662 / 86400 = 20591 days (truncated).
        let (y, mo, d) = days_to_ymd(20591);
        assert_eq!((y, mo, d), (2026, 5, 18));
    }

    // ── prune_to_cap ────────────────────────────────────────

    fn write_fake_zip(dir: &std::path::Path, name: &str) {
        let path = dir.join(name);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(b"fake").unwrap();
    }

    #[test]
    fn prune_to_cap_removes_oldest_excess() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Create 25 fake zips (cap is 20, so 5 should be pruned).
        // Names are sortable: lexical order = chronological.
        for i in 0..25u32 {
            let name = format!("ryll-auto-snapshot-2026-05-18T00-00-{:02}Z-T+0.0s.zip", i);
            write_fake_zip(dir, &name);
        }

        let deleted = prune_to_cap(dir, 20);
        assert_eq!(deleted, 5, "expected 5 pruned, got {}", deleted);

        // The 5 oldest (seconds 00..04) should be gone.
        for i in 0..5u32 {
            let name = format!("ryll-auto-snapshot-2026-05-18T00-00-{:02}Z-T+0.0s.zip", i);
            assert!(!dir.join(&name).exists(), "should have pruned {}", name);
        }
        // The 20 newest (seconds 05..24) should remain.
        for i in 5..25u32 {
            let name = format!("ryll-auto-snapshot-2026-05-18T00-00-{:02}Z-T+0.0s.zip", i);
            assert!(dir.join(&name).exists(), "should have kept {}", name);
        }
    }

    #[test]
    fn prune_to_cap_no_op_when_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        for i in 0..5u32 {
            let name = format!("ryll-auto-snapshot-2026-05-18T00-00-{:02}Z-T+0.0s.zip", i);
            write_fake_zip(dir, &name);
        }

        let deleted = prune_to_cap(dir, 20);
        assert_eq!(deleted, 0);
    }

    #[test]
    fn prune_to_cap_ignores_non_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Non-matching files should not count toward the cap or be pruned.
        write_fake_zip(dir, "other-file.zip");
        write_fake_zip(dir, "ryll-bugreport-something.zip");
        for i in 0..5u32 {
            let name = format!("ryll-auto-snapshot-2026-05-18T00-00-{:02}Z-T+0.0s.zip", i);
            write_fake_zip(dir, &name);
        }

        // Cap of 3: 5 matching → 2 deleted.
        let deleted = prune_to_cap(dir, 3);
        assert_eq!(deleted, 2);
        // Non-matching files must still exist.
        assert!(dir.join("other-file.zip").exists());
        assert!(dir.join("ryll-bugreport-something.zip").exists());
    }

    // ── retire-and-respawn helper ───────────────────────────

    /// Bookkeeping check: `wait_for_cancel` is the polling primitive
    /// the auto-snapshot loop uses to bound retire latency below
    /// `state.interval` (which can be 30 s+). Verify it returns
    /// promptly when the flag is already set, and that it waits
    /// when the flag is unset (with a short timeout so the test
    /// doesn't hang on regression).
    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_cancel_returns_when_already_set() {
        let cancel = Arc::new(AtomicBool::new(true));
        tokio::time::timeout(Duration::from_millis(100), wait_for_cancel(&cancel))
            .await
            .expect("wait_for_cancel must return promptly when flag already set");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_cancel_returns_within_one_poll_after_set() {
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let waiter = tokio::spawn(async move { wait_for_cancel(&c).await });
        // Let one poll cycle start, set the flag, then assert the
        // waiter completes within the next poll interval + slack.
        // The 500 ms poll interval is hard-coded in wait_for_cancel;
        // 750 ms total gives one full poll + buffer for scheduling.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_millis(750), waiter)
            .await
            .expect("waiter task must complete within one poll after cancel set")
            .expect("waiter task must not panic");
    }
}
