//! Byte-bounded LRU image cache.
//!
//! `BoundedImageCache` wraps [`lru::LruCache`] and adds a byte-level
//! cap on top of the crate's entry-count cap.  The inner `LruCache`
//! is created as *unbounded* (no entry limit); all capacity management
//! is done here in terms of total `Vec<u8>` length.
//!
//! On each [`BoundedImageCache::insert`] call the cache evicts the
//! least-recently-used entries until `total_bytes <= cap_bytes`.  If a
//! single entry is larger than the entire cap it is *refused* rather
//! than accepted (which would thrash the LRU evicting everything and
//! then immediately needing to re-fetch the giant frame anyway).
//!
//! Eviction counters (`evictions_total`, `evicted_bytes_total`) are
//! session-wide: they survive [`clear`][BoundedImageCache::clear] so
//! that an operator reading a bug report sees the cumulative eviction
//! pressure regardless of how many `inval_all` messages arrived.

use lru::LruCache;
use tracing::{debug, info};

/// Outcome of a [`BoundedImageCache::insert`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The entry was stored; no eviction was needed.
    Inserted,
    /// The entry was stored after one or more LRU entries were evicted
    /// to bring total bytes back within the cap.
    InsertedAfterEviction {
        /// Number of entries evicted.
        evicted: u32,
        /// Bytes freed by those evictions.
        freed_bytes: usize,
    },
    /// The entry was not stored; see [`RefusedReason`].
    Refused {
        /// Why the entry was refused.
        reason: RefusedReason,
    },
}

/// Why [`BoundedImageCache::insert`] refused to store an entry.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusedReason {
    /// The single entry's byte length exceeds `cap_bytes`.  Inserting
    /// it would require evicting everything and the entry would still
    /// not fit; refuse it so the caller can handle a cache miss.
    EntryLargerThanCap,
}

/// Byte-bounded LRU cache for decoded SPICE image RGBA data.
///
/// Keys are SPICE `image_id` values (`u64`); values are raw RGBA pixel
/// buffers (`Vec<u8>`).
///
/// ## Byte cap enforcement
///
/// After each successful insert the cache evicts the LRU entry
/// repeatedly until `bytes() <= cap_bytes()`.  A single entry whose
/// size exceeds `cap_bytes()` is refused up-front (see
/// [`InsertOutcome::Refused`]).
///
/// ## Eviction counters
///
/// `evictions_total` and `evicted_bytes_total` count cap-driven
/// evictions only.  Explicit [`remove`][Self::remove] calls (the
/// `inval_list` path) and [`clear`][Self::clear] calls (the `inval_all`
/// path) are *not* counted as evictions — they represent server-driven
/// invalidation, not memory pressure.
pub struct BoundedImageCache {
    inner: LruCache<u64, Vec<u8>>,
    bytes: usize,
    cap_bytes: usize,
    evictions_total: u64,
    evicted_bytes_total: u64,
    first_eviction_logged: bool,
}

impl BoundedImageCache {
    /// Create a new cache with the given byte cap.
    ///
    /// Panics if `cap_bytes` is 0.
    pub fn new(cap_bytes: usize) -> Self {
        assert!(cap_bytes > 0, "BoundedImageCache: cap_bytes must be > 0");
        Self {
            inner: LruCache::unbounded(),
            bytes: 0,
            cap_bytes,
            evictions_total: 0,
            evicted_bytes_total: 0,
            first_eviction_logged: false,
        }
    }

    /// Insert (or replace) an entry.
    ///
    /// Returns [`InsertOutcome::Refused`] if `value.len() > cap_bytes`.
    /// Otherwise stores the entry and evicts LRU entries until
    /// `bytes <= cap_bytes`, then returns either
    /// [`InsertOutcome::Inserted`] or
    /// [`InsertOutcome::InsertedAfterEviction`].
    pub fn insert(&mut self, key: u64, value: Vec<u8>) -> InsertOutcome {
        let incoming_bytes = value.len();

        // Refuse entries that can never fit.
        if incoming_bytes > self.cap_bytes {
            return InsertOutcome::Refused {
                reason: RefusedReason::EntryLargerThanCap,
            };
        }

        // `put` returns the previous value if the key already existed,
        // so we can subtract the old byte count from our running total.
        let displaced = self.inner.put(key, value);
        if let Some(old_val) = &displaced {
            self.bytes = self.bytes.saturating_sub(old_val.len());
        }
        self.bytes += incoming_bytes;

        // Evict LRU entries until we are within the cap.
        let mut evicted: u32 = 0;
        let mut freed_bytes: usize = 0;

        while self.bytes > self.cap_bytes {
            match self.inner.pop_lru() {
                // Should not happen; cache has the entry we just inserted.
                None => break,
                Some((_k, v)) => {
                    let n = v.len();
                    self.bytes = self.bytes.saturating_sub(n);
                    evicted += 1;
                    freed_bytes += n;
                    self.evictions_total += 1;
                    self.evicted_bytes_total += n as u64;
                }
            }
        }

        if evicted > 0 {
            if !self.first_eviction_logged {
                self.first_eviction_logged = true;
                let cap_mib = self.cap_bytes / (1024 * 1024);
                info!(
                    "image_cache: cap {} MiB reached; oldest entries will be evicted",
                    cap_mib,
                );
            } else {
                debug!(
                    evicted,
                    freed_bytes,
                    bytes = self.bytes,
                    cap_bytes = self.cap_bytes,
                    "image_cache: evicted LRU entries",
                );
            }

            InsertOutcome::InsertedAfterEviction {
                evicted,
                freed_bytes,
            }
        } else {
            InsertOutcome::Inserted
        }
    }

    /// Return a reference to the value for `key`, bumping it to MRU.
    ///
    /// Returns `None` on a cache miss.
    pub fn get(&mut self, key: &u64) -> Option<&Vec<u8>> {
        self.inner.get(key)
    }

    /// Remove `key` from the cache.
    ///
    /// Returns `true` iff the key was present.  Decrements the byte
    /// counter by the removed value's length.  Does **not** increment
    /// `evictions_total` — explicit removes are server-driven
    /// invalidation, not cap-driven eviction.
    pub fn remove(&mut self, key: &u64) -> bool {
        match self.inner.pop(key) {
            None => false,
            Some(v) => {
                self.bytes = self.bytes.saturating_sub(v.len());
                true
            }
        }
    }

    /// Empty the cache, resetting `bytes` to 0.
    ///
    /// `evictions_total` and `evicted_bytes_total` are **not** reset —
    /// those are session-wide counters that survive cache resets so that
    /// an operator reading a bug report sees cumulative eviction pressure
    /// regardless of how many `inval_all` messages arrived.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.bytes = 0;
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Total bytes currently held by all cached values.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The configured byte cap.
    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    /// Iterate over all keys in MRU→LRU order.
    pub fn keys(&self) -> impl Iterator<Item = &u64> + '_ {
        self.inner.iter().map(|(k, _)| k)
    }

    /// Cumulative count of entries evicted by the byte cap since session
    /// start.
    pub fn evictions_total(&self) -> u64 {
        self.evictions_total
    }

    /// Cumulative bytes freed by cap-driven evictions since session
    /// start.
    pub fn evicted_bytes_total(&self) -> u64 {
        self.evicted_bytes_total
    }

    /// Whether the first-eviction info log has been emitted this
    /// session.  Exposed for unit-test visibility; production callers
    /// should ignore this.
    #[cfg(test)]
    pub fn was_first_eviction_logged(&self) -> bool {
        self.first_eviction_logged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: a cache with a 200-byte cap.
    fn cache_200() -> BoundedImageCache {
        BoundedImageCache::new(200)
    }

    #[test]
    fn insert_under_cap_no_eviction() {
        let mut c = cache_200();
        let r1 = c.insert(1, vec![0u8; 60]);
        let r2 = c.insert(2, vec![0u8; 40]);

        assert_eq!(r1, InsertOutcome::Inserted);
        assert_eq!(r2, InsertOutcome::Inserted);
        assert_eq!(c.len(), 2);
        assert_eq!(c.bytes(), 100);
        assert_eq!(c.evictions_total(), 0);
        assert_eq!(c.evicted_bytes_total(), 0);
        assert!(c.get(&1).is_some());
        assert!(c.get(&2).is_some());
    }

    #[test]
    fn insert_over_cap_evicts_oldest() {
        let mut c = cache_200();
        // Insert A (key=1, 100 B), B (key=2, 100 B) — fills cap exactly.
        c.insert(1, vec![0u8; 100]);
        c.insert(2, vec![0u8; 100]);
        assert_eq!(c.bytes(), 200);

        // Insert C (key=3, 50 B) — pushes over cap; oldest (key=1) must go.
        let outcome = c.insert(3, vec![0u8; 50]);

        match outcome {
            InsertOutcome::InsertedAfterEviction {
                evicted,
                freed_bytes,
            } => {
                assert_eq!(evicted, 1, "expected 1 eviction");
                assert_eq!(freed_bytes, 100, "expected 100 bytes freed");
            }
            other => panic!("expected InsertedAfterEviction, got {:?}", other),
        }

        assert!(c.get(&1).is_none(), "key=1 (oldest) must have been evicted");
        assert!(c.get(&2).is_some(), "key=2 must still be present");
        assert!(c.get(&3).is_some(), "key=3 must be present");
        assert_eq!(c.bytes(), 150);
        assert_eq!(c.evictions_total(), 1);
        assert_eq!(c.evicted_bytes_total(), 100);
    }

    #[test]
    fn repeated_insert_same_key_replaces_bytes() {
        let mut c = cache_200();
        c.insert(1, vec![0u8; 100]);
        assert_eq!(c.bytes(), 100);

        // Replace with a smaller value — bytes must shrink, not grow.
        let outcome = c.insert(1, vec![0u8; 50]);
        assert_eq!(outcome, InsertOutcome::Inserted);
        assert_eq!(c.len(), 1, "still one entry");
        assert_eq!(c.bytes(), 50, "bytes must be 50, not 150");
        assert_eq!(c.evictions_total(), 0);
    }

    #[test]
    fn get_bumps_to_mru() {
        // Cap = 200 B; insert A(100B), B(100B).
        //
        // Layout after inserts A, B:
        //   MRU → B → A → LRU     (bytes = 200, exactly at cap)
        //
        // `get(A)` bumps A to MRU:
        //   MRU → A → B → LRU
        //
        // Insert C(100B) → evict B (LRU), not A.
        let mut c = cache_200();
        c.insert(10, vec![0u8; 100]); // A
        c.insert(20, vec![0u8; 100]); // B  —  MRU→B→A→LRU

        // Touch A → MRU→A→B→LRU.
        assert!(c.get(&10).is_some());

        // Insert C — B should be the eviction victim.
        let outcome = c.insert(30, vec![0u8; 100]);
        match outcome {
            InsertOutcome::InsertedAfterEviction { evicted, .. } => {
                assert_eq!(evicted, 1);
            }
            other => panic!("expected InsertedAfterEviction, got {:?}", other),
        }

        assert!(c.get(&10).is_some(), "A must survive (was MRU)");
        assert!(c.get(&20).is_none(), "B must be evicted (was LRU)");
        assert!(c.get(&30).is_some(), "C must be present");
    }

    #[test]
    fn entry_larger_than_cap_refused() {
        let mut c = BoundedImageCache::new(100);
        let outcome = c.insert(1, vec![0u8; 150]);

        assert_eq!(
            outcome,
            InsertOutcome::Refused {
                reason: RefusedReason::EntryLargerThanCap
            }
        );
        assert_eq!(c.len(), 0, "cache must be unchanged after refusal");
        assert_eq!(c.bytes(), 0);
        assert_eq!(c.evictions_total(), 0);
    }

    #[test]
    fn clear_resets_bytes_keeps_eviction_counters() {
        let mut c = cache_200();
        // Fill to trigger eviction.
        c.insert(1, vec![0u8; 100]);
        c.insert(2, vec![0u8; 100]);
        c.insert(3, vec![0u8; 100]); // evicts key=1

        let evictions_before = c.evictions_total();
        let evicted_bytes_before = c.evicted_bytes_total();
        assert!(
            evictions_before > 0,
            "sanity: at least one eviction happened"
        );

        c.clear();

        assert_eq!(c.bytes(), 0, "bytes must be 0 after clear");
        assert_eq!(c.len(), 0, "len must be 0 after clear");
        assert_eq!(
            c.evictions_total(),
            evictions_before,
            "evictions_total must survive clear",
        );
        assert_eq!(
            c.evicted_bytes_total(),
            evicted_bytes_before,
            "evicted_bytes_total must survive clear",
        );
    }

    #[test]
    fn remove_decrements_bytes_not_eviction_counter() {
        let mut c = cache_200();
        c.insert(1, vec![0u8; 80]);
        c.insert(2, vec![0u8; 60]);
        assert_eq!(c.bytes(), 140);

        let removed = c.remove(&1);
        assert!(removed, "remove must return true for a present key");
        assert_eq!(c.bytes(), 60);
        assert_eq!(c.len(), 1);
        assert_eq!(c.evictions_total(), 0, "remove is not an eviction");

        let not_removed = c.remove(&99);
        assert!(!not_removed, "remove must return false for a missing key");
    }

    #[test]
    fn first_eviction_sets_logged_flag() {
        let mut c = BoundedImageCache::new(100);
        assert!(!c.was_first_eviction_logged());

        c.insert(1, vec![0u8; 60]);
        assert!(!c.was_first_eviction_logged(), "no eviction yet");

        c.insert(2, vec![0u8; 60]); // evicts key=1
        assert!(
            c.was_first_eviction_logged(),
            "flag must be set after first eviction"
        );

        // Subsequent evictions do not change the flag (it stays true).
        c.insert(3, vec![0u8; 60]);
        assert!(c.was_first_eviction_logged());
    }
}
