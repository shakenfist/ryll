use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::bugreport::TrafficBuffers;

/// How long a notification's traffic-buffer snapshot is kept
/// before being treated as expired. The "File bug report" button
/// on a notification entry switches to post-event-only mode
/// after this window elapses.
pub(super) const NOTIFICATION_SNAPSHOT_TTL: Duration = Duration::from_secs(60);

/// Maximum number of live notification snapshots retained at any
/// one time. Oldest is evicted when a sixth notification fires.
pub(super) const NOTIFICATION_SNAPSHOT_CAP: usize = 5;

/// Single entry in the notification-snapshot store. Owns a
/// captured `TrafficBuffers` (cheap thanks to its Arc-shared
/// payloads).
pub(super) struct NotificationSnapshotEntry {
    captured_at: Instant,
    traffic: TrafficBuffers,
}

/// Bounded LRU+TTL store of traffic-buffer snapshots keyed by
/// `NotificationEntry::id`. Every fresh notification push
/// captures one entry; entries are pruned on overflow (cap) or
/// expiry (TTL). The notifications panel uses `has_live` to
/// render the button's visual state, and `take` to consume the
/// snapshot when the user clicks.
pub(super) struct NotificationSnapshotStore {
    by_id: HashMap<u64, NotificationSnapshotEntry>,
    /// Notification ids in insertion order, oldest first.
    /// Tracked separately so eviction-on-overflow is O(1).
    insertion_order: VecDeque<u64>,
    /// Tick-time prune is gated on this so we don't walk the
    /// map on every paint frame.
    last_prune: Instant,
}

impl NotificationSnapshotStore {
    pub(super) fn new() -> Self {
        NotificationSnapshotStore {
            by_id: HashMap::new(),
            insertion_order: VecDeque::new(),
            last_prune: Instant::now(),
        }
    }

    /// Insert (or refresh) a snapshot for `id`. If the store
    /// is over capacity after this push, the oldest entry is
    /// evicted. Expired entries are pruned opportunistically.
    pub(super) fn capture(&mut self, id: u64, traffic: TrafficBuffers, now: Instant) {
        self.prune_expired(now);

        if self.by_id.contains_key(&id) {
            // Refresh: replace the captured payload and bump
            // the captured_at timestamp. Don't touch
            // insertion_order — its position is unchanged by
            // a fold-refresh.
            if let Some(entry) = self.by_id.get_mut(&id) {
                entry.captured_at = now;
                entry.traffic = traffic;
            }
            return;
        }

        self.by_id.insert(
            id,
            NotificationSnapshotEntry {
                captured_at: now,
                traffic,
            },
        );
        self.insertion_order.push_back(id);
        while self.insertion_order.len() > NOTIFICATION_SNAPSHOT_CAP {
            if let Some(oldest_id) = self.insertion_order.pop_front() {
                self.by_id.remove(&oldest_id);
            }
        }
    }

    /// `true` iff a non-expired snapshot exists for this id.
    /// Prunes expired entries as a side effect so the answer
    /// is always current.
    pub(super) fn has_live(&mut self, id: u64, now: Instant) -> bool {
        self.prune_expired(now);
        self.by_id.contains_key(&id)
    }

    /// Remove and return the snapshot for `id`, if present
    /// and non-expired. Used at button-click time to consume
    /// the snapshot for a report. Prunes expired entries on
    /// the way through.
    pub(super) fn take(&mut self, id: u64, now: Instant) -> Option<TrafficBuffers> {
        self.prune_expired(now);
        let entry = self.by_id.remove(&id)?;
        self.insertion_order.retain(|other| *other != id);
        Some(entry.traffic)
    }

    /// Drop every entry older than the TTL. Cheap O(N) walk
    /// over `insertion_order`, at most `NOTIFICATION_SNAPSHOT_CAP`
    /// entries.
    pub(super) fn prune_expired(&mut self, now: Instant) {
        while let Some(&oldest_id) = self.insertion_order.front() {
            let expired = self
                .by_id
                .get(&oldest_id)
                .map(|e| now.duration_since(e.captured_at) >= NOTIFICATION_SNAPSHOT_TTL)
                .unwrap_or(true);
            if !expired {
                break;
            }
            self.insertion_order.pop_front();
            self.by_id.remove(&oldest_id);
        }
    }

    /// Called from the GUI tick at most once per second so
    /// the notifications panel's button visuals reflect
    /// expiration in real time without polling on every
    /// repaint.
    pub(super) fn maybe_prune(&mut self, now: Instant) {
        if now.duration_since(self.last_prune) >= Duration::from_secs(1) {
            self.last_prune = now;
            self.prune_expired(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Notification-snapshot store ────────────────────────

    fn fresh_traffic() -> TrafficBuffers {
        TrafficBuffers::new()
    }

    #[test]
    fn snapshot_store_evicts_oldest_when_over_cap() {
        // Six pushes against a five-entry cap: the first id
        // should no longer be live.
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        for id in 1u64..=6 {
            store.capture(id, fresh_traffic(), t0);
        }
        assert!(!store.has_live(1, t0), "oldest id should have been evicted");
        for id in 2u64..=6 {
            assert!(store.has_live(id, t0), "id {} should still be live", id);
        }
    }

    #[test]
    fn snapshot_store_drops_expired_entries_on_prune() {
        // Two snapshots, then advance "now" past the TTL.
        // Both should be pruned.
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        store.capture(1, fresh_traffic(), t0);
        store.capture(2, fresh_traffic(), t0);
        let later = t0 + NOTIFICATION_SNAPSHOT_TTL + Duration::from_secs(1);
        store.prune_expired(later);
        assert!(!store.has_live(1, later));
        assert!(!store.has_live(2, later));
    }

    #[test]
    fn snapshot_store_replaces_on_same_id_fold() {
        // A re-fire of the same notification id (within the
        // dedup window) refreshes the captured_at timestamp
        // and does not append a new entry.
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        store.capture(42, fresh_traffic(), t0);
        let later = t0 + Duration::from_secs(10);
        store.capture(42, fresh_traffic(), later);
        // Only one entry tracked.
        assert_eq!(store.insertion_order.len(), 1);
        // After 51 s from later (= t0 + 61), the entry must
        // still be live (the refresh extended its lifetime).
        let check = later + Duration::from_secs(51);
        assert!(store.has_live(42, check));
    }

    #[test]
    fn snapshot_store_lookup_returns_none_after_ttl() {
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        store.capture(7, fresh_traffic(), t0);
        assert!(store.has_live(7, t0));
        let later = t0 + NOTIFICATION_SNAPSHOT_TTL + Duration::from_secs(1);
        assert!(!store.has_live(7, later));
        // take() also prunes; second call returns None.
        assert!(store.take(7, later).is_none());
    }
}
