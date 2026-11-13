//! Shared byte counter that channels increment from their read
//! loops. The host polls it periodically to derive a bandwidth
//! sparkline, and the inputs channel reads its running total to tell
//! whether the server has gone quiet on every channel. Channels store
//! an `Arc<ByteCounter>` directly — it is pure plumbing, not a
//! behavioural surface.

use std::sync::atomic::{AtomicU64, Ordering};

/// Byte counter shared between channels and the bandwidth tracker.
///
/// `add` is called from the channel read loops on every chunk.
/// `take` is called once per second by the host to read and reset
/// the bandwidth window atomically. `total` is never reset, so a
/// reader that compares two of its values is not disturbed by `take`.
pub struct ByteCounter {
    /// Bytes since the last `take`.
    window: AtomicU64,
    /// Bytes since the counter was made.
    total: AtomicU64,
}

impl ByteCounter {
    /// Construct a fresh counter at zero.
    pub fn new() -> Self {
        ByteCounter {
            window: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    /// Add bytes from a channel read.
    pub fn add(&self, bytes: u64) {
        self.window.fetch_add(bytes, Ordering::Relaxed);
        self.total.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Atomically read the bandwidth window and reset it to zero.
    pub fn take(&self) -> u64 {
        self.window.swap(0, Ordering::Relaxed)
    }

    /// Every byte received on any channel since the counter was made.
    /// Unaffected by [`take`](Self::take).
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

impl Default for ByteCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ByteCounter;

    #[test]
    fn take_resets_the_window_but_not_the_total() {
        let counter = ByteCounter::new();
        counter.add(10);
        counter.add(5);
        assert_eq!(counter.take(), 15);
        assert_eq!(counter.take(), 0);
        counter.add(1);
        assert_eq!(counter.total(), 16);
    }
}
