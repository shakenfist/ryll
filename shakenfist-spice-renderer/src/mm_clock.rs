//! Shared monotonic SPICE `mm_time` clock.
//!
//! SPICE's `mm_time` is a server-side millisecond counter the
//! server seeds the client with via
//! `SPICE_MSG_MAIN_INIT::multi_media_time` and periodically
//! updates via `SPICE_MSG_MAIN_MULTI_MEDIA_TIME`. The display
//! channel needs to compute "current mm_time" at
//! `STREAM_REPORT` send time so the
//! `last_frame_delay = end_frame_mm_time - now_mm_time` field
//! is meaningful (matches spice-gtk's `channel-main.c`
//! `mm_time_set` / `_get` pair).
//!
//! `MmClock` stores `(base_mm_time, base_instant)` updated by
//! the main channel and a `now() -> u32` that combines them
//! with `Instant::now().duration_since(base_instant)`. The lock
//! is held for nanoseconds — `std::sync::Mutex` is fine and
//! avoids pulling in `parking_lot`.
//!
//! Wraparound at 2^32 ms (~49.7 days) is acceptable and
//! matches spice-gtk's semantics. The implementation uses a
//! `u128 -> u32` truncating cast which is the correct
//! wrapping behaviour: `base_mm_time.wrapping_add(elapsed_ms
//! as u32)` with the cast performed before the add.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Shared mm_time clock. Writer side is `MainChannel`, reader
/// side is `DisplayChannel`. See module docs.
pub struct MmClock {
    inner: Mutex<MmClockInner>,
}

struct MmClockInner {
    /// Server-provided `mm_time` value at the moment we received
    /// the last `MAIN_INIT` or `MULTI_MEDIA_TIME` message.
    base_mm_time: u32,
    /// Wall-clock instant captured at the same moment as
    /// `base_mm_time`. Combined with `base_mm_time` and the
    /// current `Instant::now()` to compute `now()`.
    base_instant: Instant,
    /// Number of `set(...)` calls since construction. Surfaced
    /// in bug reports so a stuck server (no MULTI_MEDIA_TIME
    /// updates) is visible.
    set_count: u64,
    /// Session-relative seconds (from the traffic clock) when
    /// the most recent `set(...)` was applied. `None` until
    /// the first `set`.
    last_set_ts_secs: Option<f64>,
}

impl MmClock {
    /// Construct a fresh clock with `base_mm_time = 0` and the
    /// current `Instant::now()` as the base. Initial `now()`
    /// reads will return millis-since-construction until the
    /// first `set()` arrives from the server.
    pub fn new() -> Self {
        MmClock {
            inner: Mutex::new(MmClockInner {
                base_mm_time: 0,
                base_instant: Instant::now(),
                set_count: 0,
                last_set_ts_secs: None,
            }),
        }
    }

    /// Update the base mm_time and base instant from a server
    /// message. `traffic_elapsed_secs` is the session-relative
    /// time used by the bug-report visibility fields; pass
    /// `traffic.elapsed().as_secs_f64()` from the caller.
    pub fn set(&self, mm_time: u32, traffic_elapsed_secs: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.base_mm_time = mm_time;
        inner.base_instant = Instant::now();
        inner.set_count = inner.set_count.saturating_add(1);
        inner.last_set_ts_secs = Some(traffic_elapsed_secs);
    }

    /// Internal `set` variant that takes the "current instant"
    /// as a parameter. Used by tests to inject wraparound /
    /// advance scenarios without sleeping. Not exposed publicly
    /// because production callers should always use the real
    /// `Instant::now()` (via `set`).
    #[cfg(test)]
    fn set_at(&self, mm_time: u32, base_instant: Instant, traffic_elapsed_secs: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.base_mm_time = mm_time;
        inner.base_instant = base_instant;
        inner.set_count = inner.set_count.saturating_add(1);
        inner.last_set_ts_secs = Some(traffic_elapsed_secs);
    }

    /// Compute the current `mm_time` as
    /// `base_mm_time + (Instant::now() - base_instant) ms`,
    /// wrapping at 2^32 ms (~49.7 days). The lock is held for
    /// the duration of a single load + a `Duration` subtract;
    /// no allocation, no I/O.
    pub fn now(&self) -> u32 {
        let inner = self.inner.lock().unwrap();
        Self::compute_now(inner.base_mm_time, inner.base_instant, Instant::now())
    }

    /// Pure helper used by `now()` and by the unit tests. The
    /// `now_instant` parameter is the wall-clock anchor; in
    /// production it's `Instant::now()`, in tests it's a
    /// fixture value.
    ///
    /// `as_millis()` returns u128; the truncating cast to u32
    /// implements the desired modular-wraparound behaviour.
    fn compute_now(base_mm_time: u32, base_instant: Instant, now_instant: Instant) -> u32 {
        let elapsed: Duration = now_instant.saturating_duration_since(base_instant);
        let elapsed_ms = elapsed.as_millis() as u32; // truncating cast = mod 2^32
        base_mm_time.wrapping_add(elapsed_ms)
    }

    /// Cumulative number of `set` calls since construction.
    /// Surfaced in `MainSnapshot::mm_time_set_count`.
    pub fn set_count(&self) -> u64 {
        self.inner.lock().unwrap().set_count
    }

    /// Session-relative seconds at the most recent `set`. None
    /// until the first `set` lands. Surfaced in
    /// `MainSnapshot::last_mm_time_set_ts_secs`.
    pub fn last_set_ts_secs(&self) -> Option<f64> {
        self.inner.lock().unwrap().last_set_ts_secs
    }
}

impl Default for MmClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `now()` should track `base_mm_time + elapsed` as the
    /// wall-clock advances. Uses real `Instant::now()`; we
    /// assert with a generous tolerance because OS scheduling
    /// jitter on a loaded CI host can easily add a millisecond
    /// or two between the `set` and the `now` reads.
    #[test]
    fn set_then_now_returns_base_plus_elapsed() {
        let clock = MmClock::new();
        clock.set(1_000_000, 0.0);
        // Sleep a bit then read.
        std::thread::sleep(Duration::from_millis(20));
        let observed = clock.now();
        let diff = observed.wrapping_sub(1_000_000);
        // 20 ms +/- generous slack for jitter.
        assert!(
            (15..=200).contains(&diff),
            "expected ~20 ms past 1_000_000, got base+{} ms (observed={})",
            diff,
            observed
        );
    }

    /// Inject a base instant in the past via `set_at` so the
    /// observed `now()` is deterministic without sleeping.
    #[test]
    fn advance_via_injected_instant() {
        let clock = MmClock::new();
        let in_the_past = Instant::now() - Duration::from_millis(500);
        clock.set_at(2_000, in_the_past, 0.0);
        let observed = clock.now();
        let diff = observed.wrapping_sub(2_000);
        // We injected a 500 ms-old base; allow slack for the
        // time spent between set_at and now().
        assert!(
            (495..=600).contains(&diff),
            "expected ~500 ms past 2_000, got base+{} ms",
            diff
        );
    }

    /// Wraparound at 2^32 ms: seed `base_mm_time` near
    /// `u32::MAX` with a base instant in the past, then assert
    /// the computed `now` wraps modulo 2^32.
    #[test]
    fn wraparound_at_2_pow_32_ms() {
        let clock = MmClock::new();
        // base = MAX - 50, elapsed ~= 100 ms → expected wrap to ~50.
        let base_instant = Instant::now() - Duration::from_millis(100);
        clock.set_at(u32::MAX - 50, base_instant, 0.0);
        let observed = clock.now();
        // observed is u32; with a wrap, it should be a small
        // positive value (the elapsed minus 50).
        assert!(
            observed < 200,
            "expected wraparound to a small u32 value, got {}",
            observed
        );
        // And specifically not the un-wrapped sum (saturated).
        assert_ne!(observed, u32::MAX);
    }

    /// Direct test of the pure `compute_now` helper for the
    /// wraparound boundary — no sleeps, no real clock.
    #[test]
    fn compute_now_wraps_exactly() {
        let base_instant = Instant::now();
        // Elapsed of exactly 100 ms.
        let now_instant = base_instant + Duration::from_millis(100);
        // base = MAX - 30; expected = MAX - 30 + 100 mod 2^32 = 69.
        let result = MmClock::compute_now(u32::MAX - 30, base_instant, now_instant);
        assert_eq!(result, 69);
    }

    /// Calling `set` with the same value twice should not panic
    /// or destabilise the clock. The second call rebases the
    /// instant; subsequent `now()` reads continue to be
    /// monotone in wall time (modulo wraparound). We assert
    /// that `set_count` advances by one per call and the clock
    /// remains usable.
    #[test]
    fn idempotent_set_with_same_value() {
        let clock = MmClock::new();
        clock.set(42, 0.0);
        let first_now = clock.now();
        clock.set(42, 0.1);
        let second_now = clock.now();
        assert_eq!(clock.set_count(), 2);
        // Both reads should be at or just after 42; with the
        // rebase, second_now should be ~42 again (modulo a few
        // ms of elapsed time since the second `set`).
        assert!((42..=42 + 100).contains(&first_now));
        assert!((42..=42 + 100).contains(&second_now));
    }

    /// `set_count` and `last_set_ts_secs` accessors should
    /// reflect the most recent `set` call.
    #[test]
    fn set_count_and_last_set_ts_track_calls() {
        let clock = MmClock::new();
        assert_eq!(clock.set_count(), 0);
        assert_eq!(clock.last_set_ts_secs(), None);
        clock.set(1, 1.5);
        assert_eq!(clock.set_count(), 1);
        assert_eq!(clock.last_set_ts_secs(), Some(1.5));
        clock.set(2, 2.75);
        assert_eq!(clock.set_count(), 2);
        assert_eq!(clock.last_set_ts_secs(), Some(2.75));
    }
}
