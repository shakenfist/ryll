//! Byte-bounded LRU map keyed `u64 -> Vec<u8>`.
//!
//! `ByteBoundedLru` wraps [`lru::LruCache`] and adds a byte-level
//! cap on top of the crate's entry-count cap.  The inner `LruCache`
//! is created as *unbounded* (no entry limit); all capacity management
//! is done here in terms of total `Vec<u8>` length.
//!
//! On each [`ByteBoundedLru::insert`] call the cache evicts the
//! least-recently-used entries until `bytes <= cap_bytes`.  If a
//! single entry is larger than the entire cap it is *refused* rather
//! than accepted (which would thrash the LRU evicting everything and
//! then immediately needing to re-fetch the giant value anyway).
//!
//! Eviction counters (`evictions_total`, `evicted_bytes_total`) are
//! session-wide: they survive [`clear`][ByteBoundedLru::clear] so
//! that an operator reading a bug report sees the cumulative eviction
//! pressure regardless of how many explicit invalidations arrived.
//!
//! This type is deliberately concrete (`u64` keys, `Vec<u8>` values)
//! rather than generic: both current consumers — the renderer's
//! `BoundedImageCache` and the GLZ dictionary in this crate — want
//! exactly that shape, and keeping it concrete avoids paying for
//! generic machinery we don't need.

use lru::LruCache;

/// Outcome of a [`ByteBoundedLru::insert`] call.
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

/// Why [`ByteBoundedLru::insert`] refused to store an entry.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusedReason {
    /// The single entry's byte length exceeds `cap_bytes`.  Inserting
    /// it would require evicting everything and the entry would still
    /// not fit; refuse it so the caller can handle a cache miss.
    EntryLargerThanCap,
}

/// Byte-bounded LRU cache keyed `u64 -> Vec<u8>`.
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
/// evictions only.  Explicit [`remove`][Self::remove] and
/// [`clear`][Self::clear] calls (server-driven invalidation) are
/// *not* counted as evictions.
pub struct ByteBoundedLru {
    inner: LruCache<u64, Vec<u8>>,
    bytes: usize,
    cap_bytes: usize,
    evictions_total: u64,
    evicted_bytes_total: u64,
}

impl ByteBoundedLru {
    /// Create a new cache with the given byte cap.
    ///
    /// Panics if `cap_bytes` is 0.
    pub fn new(cap_bytes: usize) -> Self {
        assert!(cap_bytes > 0, "ByteBoundedLru: cap_bytes must be > 0");
        Self {
            inner: LruCache::unbounded(),
            bytes: 0,
            cap_bytes,
            evictions_total: 0,
            evicted_bytes_total: 0,
        }
    }

    /// Insert (or replace) an entry.
    ///
    /// Returns [`InsertOutcome::Refused`] if `value.len() > cap_bytes`.
    /// Otherwise stores the entry and evicts LRU entries until
    /// `bytes <= cap_bytes`, then returns either
    /// [`InsertOutcome::Inserted`] or
    /// [`InsertOutcome::InsertedAfterEviction`].

    // NOTE(mikal): while the implementation here looks like it would
    // temporarily overshoot the maximum size of the cache, the new entry
    // is passed value so any overshoot is accounted for in the memory
    // footprint of the caller of this method. This implementation is also
    // simpler and therefore safer than evicting items before insertion.
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
        //
        // NOTE(mikal): that is, the intent of this block is to reduce the
        // number of bytes recorded as in the cache by the size of the
        // evicted element (if any), and then increase it by the size of
        // the new element. saturating_sub() here ensures that we never
        // experience a negative number as part of the subtraction.
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
                // Should not happen: the cache still holds the entry we just
                // inserted and incoming_bytes <= cap_bytes, so running out of
                // entries while bytes > cap_bytes means the byte accounting
                // has drifted from the real cache contents.  Reset the
                // counter to match the (now empty) cache so the drift does
                // not become permanent.
                None => {
                    debug_assert!(
                        false,
                        "byte accounting drift: cache empty but bytes = {} (cap_bytes = {})",
                        self.bytes, self.cap_bytes
                    );
                    self.bytes = 0;
                    break;
                }
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
    /// regardless of how many invalidations arrived.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.bytes = 0;
    }

    /// Retain only entries for which `f(&key)` returns `true`.
    ///
    /// Returns the number of entries removed.  Adjusts the byte
    /// counter to match.  Removals via this method are not counted as
    /// cap-driven evictions.
    pub fn retain_keys<F: FnMut(&u64) -> bool>(&mut self, mut f: F) -> usize {
        // `LruCache` has no direct `retain`; collect victims first.
        let victims: Vec<u64> = self
            .inner
            .iter()
            .filter_map(|(k, _)| if !f(k) { Some(*k) } else { None })
            .collect();
        let mut removed = 0usize;
        for k in &victims {
            if let Some(v) = self.inner.pop(k) {
                self.bytes = self.bytes.saturating_sub(v.len());
                removed += 1;
            }
        }
        removed
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

    /// Cumulative count of entries evicted by the byte cap since the
    /// cache was constructed.
    pub fn evictions_total(&self) -> u64 {
        self.evictions_total
    }

    /// Cumulative bytes freed by cap-driven evictions since the cache
    /// was constructed.
    pub fn evicted_bytes_total(&self) -> u64 {
        self.evicted_bytes_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_200() -> ByteBoundedLru {
        ByteBoundedLru::new(200)
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
        c.insert(1, vec![0u8; 100]);
        c.insert(2, vec![0u8; 100]);
        assert_eq!(c.bytes(), 200);

        let outcome = c.insert(3, vec![0u8; 50]);

        match outcome {
            InsertOutcome::InsertedAfterEviction {
                evicted,
                freed_bytes,
            } => {
                assert_eq!(evicted, 1);
                assert_eq!(freed_bytes, 100);
            }
            other => panic!("expected InsertedAfterEviction, got {:?}", other),
        }

        assert!(c.get(&1).is_none());
        assert!(c.get(&2).is_some());
        assert!(c.get(&3).is_some());
        assert_eq!(c.bytes(), 150);
        assert_eq!(c.evictions_total(), 1);
        assert_eq!(c.evicted_bytes_total(), 100);
    }

    #[test]
    fn entry_larger_than_cap_refused() {
        let mut c = ByteBoundedLru::new(100);
        let outcome = c.insert(1, vec![0u8; 150]);
        assert_eq!(
            outcome,
            InsertOutcome::Refused {
                reason: RefusedReason::EntryLargerThanCap
            }
        );
        assert_eq!(c.len(), 0);
        assert_eq!(c.bytes(), 0);
    }

    #[test]
    fn remove_decrements_bytes() {
        let mut c = cache_200();
        c.insert(1, vec![0u8; 80]);
        c.insert(2, vec![0u8; 60]);
        assert!(c.remove(&1));
        assert_eq!(c.bytes(), 60);
        assert_eq!(c.len(), 1);
        assert_eq!(c.evictions_total(), 0);
        assert!(!c.remove(&99));
    }

    #[test]
    fn clear_resets_bytes_keeps_eviction_counters() {
        let mut c = cache_200();
        c.insert(1, vec![0u8; 100]);
        c.insert(2, vec![0u8; 100]);
        c.insert(3, vec![0u8; 100]); // evicts key=1
        let evictions_before = c.evictions_total();
        let evicted_bytes_before = c.evicted_bytes_total();
        assert!(evictions_before > 0);

        c.clear();
        assert_eq!(c.bytes(), 0);
        assert_eq!(c.len(), 0);
        assert_eq!(c.evictions_total(), evictions_before);
        assert_eq!(c.evicted_bytes_total(), evicted_bytes_before);
    }

    #[test]
    fn retain_keys_drops_matching_entries() {
        let mut c = cache_200();
        c.insert(1, vec![0u8; 30]);
        c.insert(2, vec![0u8; 40]);
        c.insert(3, vec![0u8; 50]);

        // Retain only odd keys; drops key=2.
        let removed = c.retain_keys(|k| k % 2 == 1);
        assert_eq!(removed, 1);
        assert_eq!(c.len(), 2);
        assert_eq!(c.bytes(), 80);
        assert!(c.get(&2).is_none());
        // Eviction counter must NOT be incremented by retain_keys.
        assert_eq!(c.evictions_total(), 0);
    }
}
