//! Byte-bounded LRU image cache.
//!
//! `BoundedImageCache` is a thin wrapper around
//! [`shakenfist_spice_compression::ByteBoundedLru`] that adds the
//! image-cache-specific first-eviction info log (so an operator
//! reading the log knows which cache hit its cap first — image cache
//! vs GLZ dictionary).  The byte-cap accounting, LRU eviction, refusal
//! of oversize entries, and the [`InsertOutcome`] / [`RefusedReason`]
//! enums are all provided by the shared `ByteBoundedLru` in the
//! compression crate so the GLZ dictionary can reuse them.
//!
//! See `docs/plans/PLAN-stream-caps-and-flap.md` for the refactor
//! that moved the underlying data structure into the compression
//! crate.

pub use shakenfist_spice_compression::byte_bounded_lru::{InsertOutcome, RefusedReason};

use shakenfist_spice_compression::byte_bounded_lru::ByteBoundedLru;
use tracing::{debug, info};

/// Byte-bounded LRU cache for decoded SPICE image RGBA data.
///
/// Keys are SPICE `image_id` values (`u64`); values are raw RGBA pixel
/// buffers (`Vec<u8>`).  All capacity management is delegated to
/// [`ByteBoundedLru`]; this type only adds the renderer-specific
/// first-eviction info log.
pub struct BoundedImageCache {
    inner: ByteBoundedLru,
    first_eviction_logged: bool,
}

impl BoundedImageCache {
    /// Create a new cache with the given byte cap.
    ///
    /// Panics if `cap_bytes` is 0.
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            inner: ByteBoundedLru::new(cap_bytes),
            first_eviction_logged: false,
        }
    }

    /// Insert (or replace) an entry.  See
    /// [`ByteBoundedLru::insert`] for the byte-cap semantics.
    pub fn insert(&mut self, key: u64, value: Vec<u8>) -> InsertOutcome {
        let outcome = self.inner.insert(key, value);
        if let InsertOutcome::InsertedAfterEviction {
            evicted,
            freed_bytes,
        } = outcome
        {
            if !self.first_eviction_logged {
                self.first_eviction_logged = true;
                let cap_mib = self.inner.cap_bytes() / (1024 * 1024);
                info!(
                    "image_cache: cap {} MiB reached; oldest entries will be evicted",
                    cap_mib,
                );
            } else {
                debug!(
                    evicted,
                    freed_bytes,
                    bytes = self.inner.bytes(),
                    cap_bytes = self.inner.cap_bytes(),
                    "image_cache: evicted LRU entries",
                );
            }
        }
        outcome
    }

    /// Return a reference to the value for `key`, bumping it to MRU.
    pub fn get(&mut self, key: &u64) -> Option<&Vec<u8>> {
        self.inner.get(key)
    }

    /// Remove `key` from the cache.  Returns `true` iff the key was
    /// present.
    pub fn remove(&mut self, key: &u64) -> bool {
        self.inner.remove(key)
    }

    /// Empty the cache, resetting `bytes` to 0.  Eviction counters
    /// survive (see [`ByteBoundedLru::clear`]).
    pub fn clear(&mut self) {
        self.inner.clear();
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
        self.inner.bytes()
    }

    /// The configured byte cap.
    pub fn cap_bytes(&self) -> usize {
        self.inner.cap_bytes()
    }

    /// Iterate over all keys in MRU→LRU order.
    pub fn keys(&self) -> impl Iterator<Item = &u64> + '_ {
        self.inner.keys()
    }

    /// Cumulative count of entries evicted by the byte cap since
    /// session start.
    pub fn evictions_total(&self) -> u64 {
        self.inner.evictions_total()
    }

    /// Cumulative bytes freed by cap-driven evictions since session
    /// start.
    pub fn evicted_bytes_total(&self) -> u64 {
        self.inner.evicted_bytes_total()
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
