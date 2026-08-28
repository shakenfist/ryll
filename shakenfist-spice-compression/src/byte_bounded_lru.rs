//! Byte-bounded LRU map keyed `u64 -> Vec<u8>`.
//!
//! `ByteBoundedLru` wraps [`lru::LruCache`] and adds a byte-level
//! cap on top of the crate's entry-count cap.  The inner `LruCache`
//! is created as *unbounded* (no entry limit); all capacity management
//! is done here in terms of total `Vec<u8>` length.
//!
//! On each [`ByteBoundedLru::insert`] call the cache evicts the
//! least-recently-used entries until `bytes <= cap_bytes` *and*
//! `len <= max_entries`.  If a single entry is larger than the entire
//! cap it is *refused* rather than accepted (which would thrash the
//! LRU evicting everything and then immediately needing to re-fetch
//! the giant value anyway).
//!
//! The entry cap exists because `bytes` counts payload only.  Each
//! entry also costs a boxed LRU node, a hash-map slot and the
//! allocator header on its own `Vec` — see
//! [`PER_ENTRY_OVERHEAD_BYTES`] — none of which `bytes` can see.  A
//! stream of 1x1 RGBA images therefore accounts for 4 bytes apiece
//! while really costing around a hundred, so a payload-only cap of
//! 256 MiB would admit ~67 million entries and multiple gigabytes of
//! RSS.  Capping entries at `cap_bytes / PER_ENTRY_OVERHEAD_BYTES`
//! budgets container overhead at no more than the payload budget, so
//! the worst-case footprint is about twice `cap_bytes` rather than
//! twenty-five times it.
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

/// Per-entry container overhead charged when deriving the entry cap,
/// in bytes.
///
/// Not measured at runtime — a floor estimated from the layout an
/// entry actually occupies: the boxed `LruCache` node (key, `Vec`
/// header, two list pointers — 48 bytes, rounded up by the allocator),
/// the hash-map slot that points at it at hashbrown's load factor, and
/// the allocator header on the value's own heap buffer.  Around 96
/// bytes in total; the real figure varies with allocator and target,
/// and the point is that the accounting is now the right order of
/// magnitude rather than exact.
pub const PER_ENTRY_OVERHEAD_BYTES: usize = 96;

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
/// ## Capacity enforcement
///
/// After each successful insert the cache evicts the LRU entry
/// repeatedly until `bytes() <= cap_bytes()` *and* `len() <=
/// max_entries()`.  A single entry whose size exceeds `cap_bytes()` is
/// refused up-front (see [`InsertOutcome::Refused`]).
///
/// Entry-cap evictions are counted exactly like byte-cap evictions:
/// both are cap-driven, and a caller watching `evictions_total` wants
/// to see either.
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
    max_entries: usize,
    evictions_total: u64,
    evicted_bytes_total: u64,
}

impl ByteBoundedLru {
    /// Create a new cache with the given byte cap, and an entry cap
    /// derived from it as `cap_bytes / PER_ENTRY_OVERHEAD_BYTES`
    /// (never less than 1).
    ///
    /// Panics if `cap_bytes` is 0.
    pub fn new(cap_bytes: usize) -> Self {
        let max_entries = (cap_bytes / PER_ENTRY_OVERHEAD_BYTES).max(1);
        Self::with_caps(cap_bytes, max_entries)
    }

    /// Create a new cache with an explicit entry cap as well as a byte
    /// cap.
    ///
    /// [`new`][Self::new] derives the entry cap from the byte cap,
    /// which is what production callers want. This constructor exists
    /// for callers that must set the two independently — notably tests
    /// that exercise the byte-cap arithmetic at small caps, where the
    /// derived entry cap would bind first and hide it.
    ///
    /// Panics if either cap is 0.
    pub fn with_caps(cap_bytes: usize, max_entries: usize) -> Self {
        assert!(cap_bytes > 0, "ByteBoundedLru: cap_bytes must be > 0");
        assert!(max_entries > 0, "ByteBoundedLru: max_entries must be > 0");
        Self {
            inner: LruCache::unbounded(),
            bytes: 0,
            cap_bytes,
            max_entries,
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
    pub fn insert(&mut self, key: u64, value: Vec<u8>) -> InsertOutcome {
        let mut evicted_keys = Vec::new();
        self.insert_recording_evicted(key, value, &mut evicted_keys)
    }

    /// Insert (or replace) an entry, reporting which keys the byte
    /// cap evicted.
    ///
    /// `evicted_keys` is cleared, then filled with the keys evicted
    /// by this insert in eviction order (least-recently-used first).
    /// It stays empty when nothing was evicted, including on
    /// [`InsertOutcome::Refused`].
    ///
    /// Callers that must know *which* entries went away use this:
    /// the GLZ dictionary tombstones evicted image ids so a later
    /// cross-frame reference to one can fail immediately instead of
    /// waiting out its timeout for an image that is never coming
    /// back. Everyone else wants [`insert`][Self::insert], which is
    /// this method with a throwaway vector.
    pub fn insert_recording_evicted(
        &mut self,
        key: u64,
        value: Vec<u8>,
        evicted_keys: &mut Vec<u64>,
    ) -> InsertOutcome {
        evicted_keys.clear();
        // NOTE(mikal): while the implementation here looks like it would
        // temporarily overshoot the maximum size of the cache, the new entry
        // is passed value so any overshoot is accounted for in the memory
        // footprint of the caller of this method. This implementation is also
        // simpler and therefore safer than evicting items before insertion.
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

        // Evict LRU entries until we are within both caps.
        let mut evicted: u32 = 0;
        let mut freed_bytes: usize = 0;

        while self.bytes > self.cap_bytes || self.inner.len() > self.max_entries {
            match self.inner.pop_lru() {
                // Should not happen: the cache still holds the entry we just
                // inserted, incoming_bytes <= cap_bytes and max_entries >= 1,
                // so running out of entries while still over a cap means the
                // byte accounting has drifted from the real cache contents.
                // Reset the counter to match the (now empty) cache so the
                // drift does not become permanent.
                None => {
                    debug_assert!(
                        false,
                        "byte accounting drift: cache empty but bytes = {} (cap_bytes = {})",
                        self.bytes, self.cap_bytes
                    );
                    self.bytes = 0;
                    break;
                }
                Some((k, v)) => {
                    let n = v.len();
                    self.bytes = self.bytes.saturating_sub(n);
                    evicted += 1;
                    freed_bytes += n;
                    evicted_keys.push(k);
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

    /// The configured byte cap.  Governs payload bytes only; see
    /// [`max_entries`][Self::max_entries] for the container-overhead
    /// half.
    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    /// The configured entry cap.
    pub fn max_entries(&self) -> usize {
        self.max_entries
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

    /// A 200-byte cache with the entry cap set out of the way, so the
    /// byte-cap arithmetic these tests assert on is what actually
    /// drives eviction.  `ByteBoundedLru::new(200)` would derive an
    /// entry cap of 2 and evict on entry count first.
    fn cache_200() -> ByteBoundedLru {
        ByteBoundedLru::with_caps(200, 8)
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

    /// Sum the lengths of every live value, independently of the
    /// cache's own `bytes` counter, to prove the counter has not
    /// drifted from reality. Reads through `get`, so it reorders
    /// recency — only call it where the assertion is about byte
    /// accounting rather than about which key is evicted next.
    fn live_bytes(c: &mut ByteBoundedLru) -> usize {
        let keys: Vec<u64> = c.keys().copied().collect();
        keys.iter().map(|k| c.get(k).map_or(0, |v| v.len())).sum()
    }

    #[test]
    fn insert_can_cascade_multiple_evictions() {
        // Fill the cache with four small entries, then insert one
        // large enough that a single eviction is not enough to get
        // back under the cap.
        let mut c = cache_200();
        for k in 1..=4u64 {
            c.insert(k, vec![0u8; 50]);
        }
        assert_eq!(c.bytes(), 200);
        assert_eq!(c.len(), 4);

        // 200 + 150 = 350; three evictions (50 each) bring it to 200.
        let outcome = c.insert(5, vec![0u8; 150]);
        match outcome {
            InsertOutcome::InsertedAfterEviction {
                evicted,
                freed_bytes,
            } => {
                assert_eq!(evicted, 3, "one insert should cascade three evictions");
                assert_eq!(freed_bytes, 150);
            }
            other => panic!("expected InsertedAfterEviction, got {:?}", other),
        }

        // The three least-recently-used keys are gone; key 4 and the
        // new key 5 survive.
        assert!(c.get(&1).is_none());
        assert!(c.get(&2).is_none());
        assert!(c.get(&3).is_none());
        assert!(c.get(&4).is_some());
        assert!(c.get(&5).is_some());
        assert_eq!(c.len(), 2);
        assert_eq!(c.bytes(), 200);
        assert_eq!(live_bytes(&mut c), 200);
        assert_eq!(c.evictions_total(), 3);
        assert_eq!(c.evicted_bytes_total(), 150);
    }

    #[test]
    fn insert_recording_evicted_reports_cascade_keys() {
        let mut c = cache_200();
        for k in 1..=4u64 {
            c.insert(k, vec![0u8; 50]);
        }

        let mut evicted_keys = Vec::new();
        // Pre-fill with junk to prove the callee clears it.
        evicted_keys.push(999);
        let outcome = c.insert_recording_evicted(5, vec![0u8; 150], &mut evicted_keys);

        assert!(matches!(
            outcome,
            InsertOutcome::InsertedAfterEviction { evicted: 3, .. }
        ));
        assert_eq!(
            evicted_keys,
            vec![1, 2, 3],
            "evicted keys should be reported LRU-first"
        );

        // An insert that evicts nothing leaves the vector empty.
        let mut c2 = cache_200();
        let outcome = c2.insert_recording_evicted(1, vec![0u8; 10], &mut evicted_keys);
        assert_eq!(outcome, InsertOutcome::Inserted);
        assert!(evicted_keys.is_empty());

        // A refused entry evicts nothing either.
        let outcome = c2.insert_recording_evicted(2, vec![0u8; 500], &mut evicted_keys);
        assert_eq!(
            outcome,
            InsertOutcome::Refused {
                reason: RefusedReason::EntryLargerThanCap
            }
        );
        assert!(evicted_keys.is_empty());
    }

    #[test]
    fn sustained_churn_keeps_byte_accounting_exact() {
        // Dozens of insert/evict cycles with varying entry sizes,
        // interleaved with the other mutators, to catch counter drift
        // that a single-step test would miss.
        let mut c = ByteBoundedLru::new(1000);
        let mut expected_evicted_bytes: u64 = 0;
        let mut expected_evictions: u64 = 0;

        for i in 0..200u64 {
            // Sizes cycle 40..=240 so some inserts evict one entry and
            // some cascade several.
            let size = 40 + ((i % 6) as usize) * 40;
            let outcome = c.insert(i, vec![0u8; size]);
            if let InsertOutcome::InsertedAfterEviction {
                evicted,
                freed_bytes,
            } = outcome
            {
                expected_evictions += u64::from(evicted);
                expected_evicted_bytes += freed_bytes as u64;
            }

            // Every so often, exercise the non-eviction removal paths
            // as well: they adjust `bytes` on a different code path.
            if i % 17 == 0 {
                c.remove(&i.saturating_sub(3));
            }
            if i % 41 == 0 {
                c.retain_keys(|k| k % 2 == 0);
            }

            assert!(
                c.bytes() <= c.cap_bytes(),
                "cache overshot its cap at iteration {i}: {} > {}",
                c.bytes(),
                c.cap_bytes()
            );
            assert_eq!(
                c.bytes(),
                live_bytes(&mut c),
                "byte counter drifted from live contents at iteration {i}"
            );
        }

        assert!(
            expected_evictions > 0,
            "the churn loop must actually have evicted something"
        );
        assert_eq!(c.evictions_total(), expected_evictions);
        assert_eq!(c.evicted_bytes_total(), expected_evicted_bytes);
        assert_eq!(
            c.bytes(),
            live_bytes(&mut c),
            "final byte counter must equal the sum of the live entries"
        );
    }
}

#[cfg(test)]
mod entry_cap_tests {
    use super::*;

    #[test]
    fn entry_cap_is_derived_from_the_byte_cap() {
        let c = ByteBoundedLru::new(256 * 1024 * 1024);
        assert_eq!(
            c.max_entries(),
            256 * 1024 * 1024 / PER_ENTRY_OVERHEAD_BYTES
        );

        // A cap too small to pay the overhead for even one entry still
        // permits one, so a cache is never unusable.
        let c = ByteBoundedLru::new(1);
        assert_eq!(c.max_entries(), 1);
    }

    #[test]
    fn tiny_entries_are_bounded_by_the_entry_cap_not_the_byte_cap() {
        // The 1x1-RGBA case: 4 accounted bytes per entry against a
        // cap that would otherwise admit hundreds of thousands of
        // them, each really costing ~PER_ENTRY_OVERHEAD_BYTES.
        let cap_bytes = 4096;
        let mut c = ByteBoundedLru::new(cap_bytes);
        let max_entries = c.max_entries();
        for key in 0..(max_entries as u64 * 4) {
            c.insert(key, vec![0u8; 4]);
        }

        assert_eq!(c.len(), max_entries, "entry cap must bound the cache");
        assert!(
            c.bytes() < cap_bytes,
            "the byte cap was never the binding constraint: {} vs {}",
            c.bytes(),
            cap_bytes,
        );
        assert!(
            c.evictions_total() > 0,
            "entry-cap evictions must be counted like byte-cap ones",
        );
    }

    #[test]
    fn entry_cap_evictions_are_reported_to_insert_recording_evicted() {
        let mut c = ByteBoundedLru::with_caps(1_000_000, 2);
        c.insert(1, vec![0u8; 10]);
        c.insert(2, vec![0u8; 10]);

        let mut evicted_keys = Vec::new();
        let outcome = c.insert_recording_evicted(3, vec![0u8; 10], &mut evicted_keys);

        assert_eq!(
            outcome,
            InsertOutcome::InsertedAfterEviction {
                evicted: 1,
                freed_bytes: 10,
            },
        );
        assert_eq!(evicted_keys, vec![1]);
        assert_eq!(c.len(), 2);
    }
}
