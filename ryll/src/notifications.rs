//! In-app notification store. See `docs/plans/PLAN-notifications-phase-01-store.md`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use eframe::egui;

use serde::{Deserialize, Serialize};
use shakenfist_spice_protocol::{ChannelType, NotifySeverity, SpiceVisibility};

/// Maximum entries before FIFO eviction.
pub const NOTIFICATION_STORE_CAP: usize = 500;

/// Window inside which a duplicate `(source, severity, message, visibility)`
/// tuple folds into the most recent matching entry rather than producing a
/// new one.
pub const NOTIFICATION_DEDUP_WINDOW: Duration = Duration::from_secs(30);

/// Origin of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationSource {
    /// Protocol gap registered via `warn_once!`.
    Gap,
    /// Bug-report writer success/failure status.
    BugReport,
    /// SPICE_MSG_NOTIFY received on a channel.
    Spice { channel: ChannelType, what: u32 },
    /// Internally generated notification.
    Internal,
}

/// A single notification entry held by the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationEntry {
    pub id: u64,
    pub when: SystemTime,
    pub severity: NotifySeverity,
    pub source: NotificationSource,
    pub message: String,
    pub count: u32,
    pub visibility: Option<SpiceVisibility>,
    pub read: bool,
}

impl NotificationEntry {
    /// Build a fresh entry. `id` is 0 until [`NotificationStore::push`] stamps it.
    pub fn new(
        severity: NotifySeverity,
        source: NotificationSource,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: 0,
            when: SystemTime::now(),
            severity,
            source,
            message: message.into(),
            count: 1,
            visibility: None,
            read: false,
        }
    }

    /// Builder-style setter for SPICE visibility.
    pub fn with_visibility(mut self, v: SpiceVisibility) -> Self {
        self.visibility = Some(v);
        self
    }
}

/// Bounded FIFO ring buffer of notifications with deduplication.
pub struct NotificationStore {
    entries: VecDeque<NotificationEntry>,
    next_id: AtomicU64,
    cap: usize,
    dedup_window: Duration,
}

impl NotificationStore {
    /// Construct a store with default cap and dedup window.
    pub fn new() -> Self {
        Self::with_config(NOTIFICATION_STORE_CAP, NOTIFICATION_DEDUP_WINDOW)
    }

    /// Construct with custom cap and dedup window. Used by tests.
    pub fn with_config(cap: usize, dedup_window: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: AtomicU64::new(1),
            cap,
            dedup_window,
        }
    }

    /// Insert an entry, applying the dedup rule. Returns the id of the entry
    /// the new one ended up belonging to (either a fresh id or the folded-into
    /// existing entry's id).
    pub fn push(&mut self, mut entry: NotificationEntry) -> u64 {
        let new_when = entry.when;

        // Walk newest -> oldest looking for a fold target. We collect the
        // index instead of mutating through the iterator because we cannot
        // hold an immutable borrow of `self.entries` and then mutate it.
        // A zero dedup window disables folding entirely.
        let mut fold_index: Option<usize> = None;
        if !self.dedup_window.is_zero() {
            for (idx, existing) in self.entries.iter().enumerate().rev() {
                let delta = existing
                    .when
                    .duration_since(new_when)
                    .or_else(|_| new_when.duration_since(existing.when))
                    .unwrap_or(Duration::ZERO);
                if delta > self.dedup_window {
                    break;
                }
                if existing.source == entry.source
                    && existing.severity == entry.severity
                    && existing.message == entry.message
                    && existing.visibility == entry.visibility
                {
                    fold_index = Some(idx);
                    break;
                }
            }
        }

        if let Some(idx) = fold_index {
            let existing = &mut self.entries[idx];
            existing.count = existing.count.saturating_add(1);
            existing.when = new_when;
            existing.read = false;
            return existing.id;
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        entry.id = id;
        self.entries.push_back(entry);
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
        id
    }

    /// Mark a single entry read. No-op if id is unknown.
    #[allow(dead_code)]
    pub fn mark_read(&mut self, id: u64) {
        for e in self.entries.iter_mut() {
            if e.id == id {
                e.read = true;
                return;
            }
        }
    }

    /// Mark every entry read.
    pub fn mark_all_read(&mut self) {
        for e in self.entries.iter_mut() {
            e.read = true;
        }
    }

    /// Drop every entry. `next_id` is preserved.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of unread entries.
    pub fn unread_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.read).count()
    }

    /// Total number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the store is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate unread entries, newest first.
    #[allow(dead_code)]
    pub fn iter_unread(&self) -> impl Iterator<Item = &NotificationEntry> {
        self.entries.iter().rev().filter(|e| !e.read)
    }

    /// Iterate every entry, newest first.
    pub fn iter_newest_first(&self) -> impl Iterator<Item = &NotificationEntry> {
        self.entries.iter().rev()
    }

    /// Highest severity among unread entries, or `None` when there are no
    /// unread entries.
    #[allow(dead_code)]
    pub fn highest_unread_severity(&self) -> Option<NotifySeverity> {
        self.entries
            .iter()
            .filter(|e| !e.read)
            .map(|e| e.severity)
            .max()
    }

    /// Snapshot the entries for serialisation.
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<NotificationEntry> {
        self.entries.iter().cloned().collect()
    }

    /// Remove the entry with the given id. No-op if id is unknown.
    pub fn remove(&mut self, id: u64) {
        self.entries.retain(|e| e.id != id);
    }

    /// Highest severity among unread entries that should flash the bell.
    /// Excludes entries with `visibility == Some(SpiceVisibility::Low)` per
    /// master-plan Q4 — low-visibility is informational and must not pull
    /// the operator's eye.
    pub fn highest_bell_severity(&self) -> Option<NotifySeverity> {
        self.entries
            .iter()
            .filter(|e| !e.read)
            .filter(|e| e.visibility != Some(SpiceVisibility::Low))
            .map(|e| e.severity)
            .max()
    }
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience type alias for the shared store.
pub type SharedNotifications = Arc<Mutex<NotificationStore>>;

impl NotificationSource {
    /// Compact human label for the side panel.
    pub fn label(&self) -> String {
        match self {
            NotificationSource::Gap => "Gap".to_string(),
            NotificationSource::BugReport => "BugReport".to_string(),
            NotificationSource::Internal => "Internal".to_string(),
            NotificationSource::Spice { channel, .. } => {
                format!("SPICE/{}", channel.name())
            }
        }
    }
}

/// Format a `SystemTime` as a human-readable relative timestamp.
pub fn format_relative(when: SystemTime) -> String {
    let now = SystemTime::now();
    let delta = now.duration_since(when).unwrap_or(Duration::ZERO);
    let secs = delta.as_secs();
    if secs < 1 {
        "now".to_string()
    } else if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Returns (glyph, optional colour). `None` colour means "default text colour".
pub(crate) fn severity_visuals(s: NotifySeverity) -> (&'static str, Option<egui::Color32>) {
    match s {
        NotifySeverity::Info => ("\u{2139}", Some(egui::Color32::from_rgb(120, 170, 230))),
        NotifySeverity::Warn => ("\u{26A0}", Some(egui::Color32::from_rgb(255, 180, 80))),
        NotifySeverity::Error => ("\u{2716}", Some(egui::Color32::from_rgb(220, 90, 90))),
    }
}

/// Register a gap-observer callback that pushes a Warn-severity Gap
/// notification for each new warn_once! key.
pub fn register_gap_notification_observer(notifications: SharedNotifications) {
    shakenfist_spice_protocol::logging::register_gap_observer(Arc::new(
        move |key: &'static str| {
            let entry = NotificationEntry::new(NotifySeverity::Warn, NotificationSource::Gap, key);
            if let Ok(mut guard) = notifications.lock() {
                guard.push(entry);
            }
            // Lock poison is unrecoverable here; observer is best-effort,
            // matching the --pedantic observer's stance in bugreport.rs.
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakenfist_spice_protocol::warn_once;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn entry(
        severity: NotifySeverity,
        source: NotificationSource,
        message: &str,
        when: SystemTime,
    ) -> NotificationEntry {
        let mut e = NotificationEntry::new(severity, source, message);
        e.when = when;
        e
    }

    #[test]
    fn new_store_is_empty() {
        let s = NotificationStore::new();
        assert_eq!(s.len(), 0);
        assert_eq!(s.unread_count(), 0);
        assert!(s.highest_unread_severity().is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn push_assigns_monotonic_ids() {
        let mut s = NotificationStore::new();
        let id1 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "a",
            at(0),
        ));
        let id2 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "b",
            at(60),
        ));
        let id3 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "c",
            at(120),
        ));
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn push_evicts_at_cap() {
        let mut s = NotificationStore::with_config(3, Duration::from_secs(30));
        for i in 0..5u64 {
            // Each entry has a distinct message and `when` 60s apart so dedup
            // never folds.
            s.push(entry(
                NotifySeverity::Info,
                NotificationSource::Gap,
                &format!("msg{}", i),
                at(i * 60),
            ));
        }
        assert_eq!(s.len(), 3);
        let ids: Vec<u64> = s.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn dedup_within_window_folds() {
        let mut s = NotificationStore::with_config(10, Duration::from_secs(30));
        let id1 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(100),
        ));
        s.mark_read(id1);
        let id2 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(105),
        ));
        assert_eq!(id1, id2);
        assert_eq!(s.len(), 1);
        let e = &s.entries[0];
        assert_eq!(e.count, 2);
        assert_eq!(e.when, at(105));
        assert!(!e.read);
    }

    #[test]
    fn dedup_outside_window_does_not_fold() {
        let mut s = NotificationStore::with_config(10, Duration::from_secs(30));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(100),
        ));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(131),
        ));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn dedup_zero_window_never_folds() {
        let mut s = NotificationStore::with_config(10, Duration::ZERO);
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(100),
        ));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(100),
        ));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn dedup_distinct_visibility_does_not_fold() {
        let mut s = NotificationStore::with_config(10, Duration::from_secs(30));
        let mut a = entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(100),
        );
        a.visibility = Some(SpiceVisibility::Low);
        let mut b = entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(105),
        );
        b.visibility = Some(SpiceVisibility::High);
        s.push(a);
        s.push(b);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn dedup_distinct_source_does_not_fold() {
        let mut s = NotificationStore::with_config(10, Duration::from_secs(30));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "same",
            at(100),
        ));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::BugReport,
            "same",
            at(105),
        ));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn mark_read_clears_unread() {
        let mut s = NotificationStore::new();
        let id = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "a",
            at(0),
        ));
        assert_eq!(s.unread_count(), 1);
        s.mark_read(id);
        assert_eq!(s.unread_count(), 0);
    }

    #[test]
    fn mark_read_unknown_id_is_noop() {
        let mut s = NotificationStore::new();
        s.mark_read(999);
        assert_eq!(s.len(), 0);
        assert_eq!(s.unread_count(), 0);
    }

    #[test]
    fn mark_all_read() {
        let mut s = NotificationStore::new();
        for i in 0..3u64 {
            s.push(entry(
                NotifySeverity::Info,
                NotificationSource::Gap,
                &format!("m{}", i),
                at(i * 60),
            ));
        }
        s.mark_all_read();
        assert_eq!(s.unread_count(), 0);
        assert!(s.entries.iter().all(|e| e.read));
    }

    #[test]
    fn clear_drops_everything() {
        let mut s = NotificationStore::new();
        for i in 0..3u64 {
            s.push(entry(
                NotifySeverity::Info,
                NotificationSource::Gap,
                &format!("m{}", i),
                at(i * 60),
            ));
        }
        s.clear();
        assert_eq!(s.len(), 0);
        let id = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "after",
            at(1000),
        ));
        assert_eq!(id, 4);
    }

    #[test]
    fn iter_unread_skips_read() {
        let mut s = NotificationStore::new();
        let id1 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "a",
            at(0),
        ));
        let id2 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "b",
            at(60),
        ));
        let id3 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "c",
            at(120),
        ));
        let _ = (id1, id3);
        s.mark_read(id2);
        let collected: Vec<&NotificationEntry> = s.iter_unread().collect();
        assert_eq!(collected.len(), 2);
        assert!(collected.iter().all(|e| !e.read));
    }

    #[test]
    fn iter_newest_first_orders_correctly() {
        let mut s = NotificationStore::new();
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "a",
            at(0),
        ));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "b",
            at(60),
        ));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "c",
            at(120),
        ));
        let ids: Vec<u64> = s.iter_newest_first().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    #[test]
    fn highest_unread_severity_picks_max() {
        let mut s = NotificationStore::new();
        let _info = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "i",
            at(0),
        ));
        let _warn = s.push(entry(
            NotifySeverity::Warn,
            NotificationSource::Gap,
            "w",
            at(60),
        ));
        let err = s.push(entry(
            NotifySeverity::Error,
            NotificationSource::Gap,
            "e",
            at(120),
        ));
        assert_eq!(s.highest_unread_severity(), Some(NotifySeverity::Error));
        s.mark_read(err);
        assert_eq!(s.highest_unread_severity(), Some(NotifySeverity::Warn));
    }

    #[test]
    fn serde_round_trip_entries() {
        let mut entries: Vec<NotificationEntry> = Vec::new();

        let mut e = entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "gap-info",
            at(10),
        );
        e.id = 1;
        entries.push(e);

        let mut e = entry(
            NotifySeverity::Warn,
            NotificationSource::BugReport,
            "bug-warn",
            at(20),
        );
        e.id = 2;
        e.visibility = Some(SpiceVisibility::Medium);
        entries.push(e);

        let mut e = entry(
            NotifySeverity::Error,
            NotificationSource::Spice {
                channel: ChannelType::Main,
                what: 1,
            },
            "spice-main-error",
            at(30),
        );
        e.id = 3;
        e.count = 5;
        e.read = true;
        entries.push(e);

        let mut e = entry(
            NotifySeverity::Info,
            NotificationSource::Spice {
                channel: ChannelType::Display,
                what: 2,
            },
            "spice-display-info",
            at(40),
        );
        e.id = 4;
        e.visibility = Some(SpiceVisibility::High);
        entries.push(e);

        let mut e = entry(
            NotifySeverity::Warn,
            NotificationSource::Internal,
            "internal-warn",
            at(50),
        );
        e.id = 5;
        entries.push(e);

        let json = serde_json::to_string(&entries).expect("serialize");
        let round: Vec<NotificationEntry> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round, entries);
    }

    #[test]
    fn gap_observer_pushes_warn_entry() {
        let store: SharedNotifications = Arc::new(Mutex::new(NotificationStore::new()));
        register_gap_notification_observer(store.clone());
        warn_once!("phase3_test:k1", "msg");
        let guard = store.lock().unwrap();
        let found = guard.iter_newest_first().any(|e| {
            e.message == "phase3_test:k1"
                && e.severity == NotifySeverity::Warn
                && e.source == NotificationSource::Gap
        });
        assert!(
            found,
            "expected a Warn/Gap entry with message 'phase3_test:k1'"
        );
    }

    #[test]
    fn gap_observer_replay_pushes_existing_keys() {
        warn_once!("phase3_test:k_replay", "pre-registration key");
        let store: SharedNotifications = Arc::new(Mutex::new(NotificationStore::new()));
        register_gap_notification_observer(store.clone());
        let guard = store.lock().unwrap();
        let found = guard
            .iter_newest_first()
            .any(|e| e.message == "phase3_test:k_replay");
        assert!(found, "expected replay to push 'phase3_test:k_replay'");
    }

    #[test]
    fn gap_observer_distinct_keys_produce_distinct_entries() {
        let store: SharedNotifications = Arc::new(Mutex::new(NotificationStore::new()));
        register_gap_notification_observer(store.clone());
        warn_once!("phase3_test:distinct_a", "msg a");
        warn_once!("phase3_test:distinct_b", "msg b");
        let guard = store.lock().unwrap();
        let has_a = guard
            .iter_newest_first()
            .any(|e| e.message == "phase3_test:distinct_a");
        let has_b = guard
            .iter_newest_first()
            .any(|e| e.message == "phase3_test:distinct_b");
        assert!(
            has_a,
            "expected entry with message 'phase3_test:distinct_a'"
        );
        assert!(
            has_b,
            "expected entry with message 'phase3_test:distinct_b'"
        );
    }

    #[test]
    fn remove_drops_entry() {
        let mut s = NotificationStore::with_config(10, Duration::ZERO);
        let id1 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "a",
            at(0),
        ));
        let id2 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "b",
            at(60),
        ));
        let id3 = s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "c",
            at(120),
        ));
        let _ = (id1, id3);
        s.remove(id2);
        assert_eq!(s.len(), 2);
        assert!(!s.entries.iter().any(|e| e.id == id2));
    }

    #[test]
    fn remove_unknown_id_is_noop() {
        let mut s = NotificationStore::with_config(10, Duration::ZERO);
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "a",
            at(0),
        ));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "b",
            at(60),
        ));
        s.push(entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "c",
            at(120),
        ));
        s.remove(999);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn highest_bell_severity_skips_low_visibility() {
        let mut s = NotificationStore::with_config(10, Duration::ZERO);
        let mut warn_low = entry(
            NotifySeverity::Warn,
            NotificationSource::Gap,
            "warn-low",
            at(0),
        );
        warn_low.visibility = Some(SpiceVisibility::Low);
        let mut info_high = entry(
            NotifySeverity::Info,
            NotificationSource::Gap,
            "info-high",
            at(60),
        );
        info_high.visibility = Some(SpiceVisibility::High);
        let mut err_low = entry(
            NotifySeverity::Error,
            NotificationSource::Gap,
            "err-low",
            at(120),
        );
        err_low.visibility = Some(SpiceVisibility::Low);
        s.push(warn_low);
        s.push(info_high);
        s.push(err_low);
        assert_eq!(s.highest_bell_severity(), Some(NotifySeverity::Info));
    }

    #[test]
    fn source_label_static_variants() {
        assert_eq!(NotificationSource::Gap.label(), "Gap");
        assert_eq!(NotificationSource::BugReport.label(), "BugReport");
        assert_eq!(NotificationSource::Internal.label(), "Internal");
    }

    #[test]
    fn source_label_spice_variant() {
        let src = NotificationSource::Spice {
            channel: ChannelType::Display,
            what: 0,
        };
        assert_eq!(src.label(), "SPICE/display");
    }

    #[test]
    fn format_relative_seconds() {
        let when = SystemTime::now()
            .checked_sub(Duration::from_secs(5))
            .unwrap();
        let s = format_relative(when);
        assert!(s == "5s ago" || s == "6s ago", "unexpected result: {s}");
    }

    #[test]
    fn format_relative_in_future_returns_now() {
        let when = SystemTime::now() + Duration::from_secs(1);
        assert_eq!(format_relative(when), "now");
    }
}
