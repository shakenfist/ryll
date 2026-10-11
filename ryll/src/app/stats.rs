use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shakenfist_spice_renderer::ByteCounter;

/// Number of bandwidth samples to keep for the sparkline.
const BANDWIDTH_HISTORY_LEN: usize = 60;

/// Number of latency samples to keep for the sparkline.
const LATENCY_HISTORY_LEN: usize = 60;

/// Number of recent frame timestamps kept for the FPS sliding window.
pub(super) const FPS_WINDOW_SIZE: usize = 120;

/// Number of recent mpsc-queue lag samples (μs) retained per event
/// kind for render-side latency diagnostics. Per-event cadence is
/// typically several Hz to hundreds of Hz; 32 entries cover
/// seconds of recent activity without bloating session.json. See
/// `docs/plans/PLAN-video-keeping-up.md`.
const RECENT_LAG_RING_CAP: usize = 32;

/// Statistics tracking
#[derive(Default)]
pub(super) struct Statistics {
    pub(super) frames_received: u64,
    pub(super) bytes_in: u64,
    pub(super) bytes_out: u64,
    /// Inter-PING interval most recently observed on the main
    /// channel, in milliseconds.
    pub(super) last_latency_ms: Option<f64>,
    /// Timestamps of recent DisplayMark events for sliding-window FPS.
    pub(super) frame_times: Vec<Instant>,
}

/// Rolling bandwidth tracker — samples bytes/sec once per second.
pub(super) struct BandwidthTracker {
    /// Shared counter incremented by all channels.
    counter: Arc<ByteCounter>,
    /// History of bytes-per-second samples (most recent last).
    /// VecDeque so eviction at capacity is O(1) (`pop_front`)
    /// instead of O(n) `Vec::remove(0)`.
    pub(super) history: VecDeque<f32>,
    /// When the current second started.
    last_tick: Instant,
}

impl BandwidthTracker {
    pub(super) fn new(counter: Arc<ByteCounter>) -> Self {
        BandwidthTracker {
            counter,
            history: VecDeque::with_capacity(BANDWIDTH_HISTORY_LEN),
            last_tick: Instant::now(),
        }
    }

    /// Tick the tracker — if a second has elapsed, read the
    /// counter and push a new sample.
    pub(super) fn tick(&mut self) {
        let elapsed = self.last_tick.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let bytes = self.counter.take();
            let secs = elapsed.as_secs_f64();
            let bps = bytes as f64 / secs;
            self.history.push_back(bps as f32);
            if self.history.len() > BANDWIDTH_HISTORY_LEN {
                self.history.pop_front();
            }
            self.last_tick = Instant::now();
        }
    }

    /// Format the most recent bandwidth value for display.
    pub(super) fn label(&self) -> String {
        match self.history.back() {
            Some(&bps) if bps >= 1_000_000.0 => format!("{:.1} MB/s", bps / 1_000_000.0),
            Some(&bps) if bps >= 1_000.0 => format!("{:.0} KB/s", bps / 1_000.0),
            Some(&bps) => format!("{:.0} B/s", bps),
            None => String::from("-- B/s"),
        }
    }
}

/// Rolling latency tracker — samples arrive when
/// `ChannelEvent::Latency` fires, driven by server PINGs
/// on the main channel.  Values are stored in milliseconds
/// (client-observed inter-PING interval) for sparkline
/// scaling.  Lower variance is better; spikes indicate a
/// network stall or server send-loop delay.
pub(super) struct LatencyTracker {
    /// History of latency samples in ms (most recent last).
    /// VecDeque so eviction at capacity is O(1) (`pop_front`).
    pub(super) history: VecDeque<f32>,
}

impl LatencyTracker {
    pub(super) fn new() -> Self {
        LatencyTracker {
            history: VecDeque::with_capacity(LATENCY_HISTORY_LEN),
        }
    }

    /// Record a new latency sample in milliseconds.
    pub(super) fn record(&mut self, sample_ms: f32) {
        self.history.push_back(sample_ms);
        if self.history.len() > LATENCY_HISTORY_LEN {
            self.history.pop_front();
        }
    }

    /// Format the most recent latency value for display.
    pub(super) fn label(&self) -> String {
        match self.history.back() {
            Some(&v) => format!("{:.1}ms", v),
            None => String::from("--ms"),
        }
    }
}

/// Push a lag sample into a bounded ring, evicting the oldest
/// entry when the cap is exceeded. Factored out of
/// `process_events` so the cap behaviour is unit-testable. See
/// `docs/plans/PLAN-video-keeping-up.md`.
pub(super) fn push_with_cap(ring: &mut VecDeque<u32>, value: u32) {
    ring.push_back(value);
    if ring.len() > RECENT_LAG_RING_CAP {
        ring.pop_front();
    }
}

/// Min / max / mean of the values in a lag ring. Returns
/// `(0, 0, 0)` when the ring is empty so a snapshot field
/// reads as "no samples yet" rather than carrying stale data.
pub(super) fn recent_lag_stats(ring: &VecDeque<u32>) -> (u32, u32, u32) {
    if ring.is_empty() {
        return (0, 0, 0);
    }
    let mut min = u32::MAX;
    let mut max = 0u32;
    let mut sum: u64 = 0;
    for &v in ring {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
        sum += v as u64;
    }
    let mean = u32::try_from(sum / ring.len() as u64).unwrap_or(u32::MAX);
    (min, max, mean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_tracker_label_empty() {
        let tracker = LatencyTracker::new();
        assert_eq!(tracker.label(), "--ms");
    }

    #[test]
    fn latency_tracker_label_non_empty() {
        let mut tracker = LatencyTracker::new();
        tracker.record(12.34);
        assert_eq!(tracker.label(), "12.3ms");
    }

    #[test]
    fn latency_tracker_record_trims_to_capacity() {
        let mut tracker = LatencyTracker::new();
        // Push 65 values: 0.0, 1.0, ..., 64.0
        for i in 0..65 {
            tracker.record(i as f32);
        }
        // Should be capped at LATENCY_HISTORY_LEN (60)
        assert_eq!(tracker.history.len(), LATENCY_HISTORY_LEN);
        // The first kept value should be the 6th original (index 5)
        assert_eq!(tracker.history[0], 5.0);
    }

    // ── Render-latency helpers ─────────────────────────────

    #[test]
    fn recent_lag_stats_empty_ring_returns_zeros() {
        let ring: VecDeque<u32> = VecDeque::new();
        assert_eq!(recent_lag_stats(&ring), (0, 0, 0));
    }

    #[test]
    fn recent_lag_stats_computes_min_max_mean() {
        let ring: VecDeque<u32> = [100u32, 300, 200].into_iter().collect();
        let (min, max, mean) = recent_lag_stats(&ring);
        assert_eq!(min, 100);
        assert_eq!(max, 300);
        assert_eq!(mean, 200);
    }

    #[test]
    fn recent_lag_stats_single_sample() {
        let ring: VecDeque<u32> = std::iter::once(42u32).collect();
        assert_eq!(recent_lag_stats(&ring), (42, 42, 42));
    }

    #[test]
    fn push_with_cap_caps_at_recent_lag_ring_cap() {
        let mut ring: VecDeque<u32> = VecDeque::new();
        // Push more than the cap; verify only the most recent
        // RECENT_LAG_RING_CAP entries survive, in order.
        for i in 0..(RECENT_LAG_RING_CAP as u32 + 8) {
            push_with_cap(&mut ring, i);
        }
        assert_eq!(ring.len(), RECENT_LAG_RING_CAP);
        // First retained value should be sample index 8.
        assert_eq!(ring.front().copied(), Some(8));
        // Last retained value should be the very last push.
        assert_eq!(ring.back().copied(), Some(RECENT_LAG_RING_CAP as u32 + 7));
    }

    #[test]
    fn push_with_cap_under_cap_retains_all() {
        let mut ring: VecDeque<u32> = VecDeque::new();
        for i in 0..5 {
            push_with_cap(&mut ring, i);
        }
        assert_eq!(ring.len(), 5);
        assert_eq!(ring.front().copied(), Some(0));
        assert_eq!(ring.back().copied(), Some(4));
    }
}
