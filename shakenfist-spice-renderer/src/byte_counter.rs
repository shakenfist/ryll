//! Shared byte counter that channels increment from their read
//! loops. The host polls it periodically to derive a bandwidth
//! sparkline. Channels store an `Arc<ByteCounter>` directly — it
//! is pure plumbing, not a behavioural surface.

use std::sync::atomic::{AtomicU64, Ordering};

/// Byte counter shared between channels and the bandwidth tracker.
///
/// `add` is called from the channel read loops on every chunk.
/// `take` is called once per second by the host to read and reset
/// the counter atomically.
pub struct ByteCounter(AtomicU64);

impl ByteCounter {
    /// Construct a fresh counter at zero.
    pub fn new() -> Self {
        ByteCounter(AtomicU64::new(0))
    }

    /// Add bytes from a channel read.
    pub fn add(&self, bytes: u64) {
        self.0.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Atomically read the counter and reset it to zero.
    pub fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

impl Default for ByteCounter {
    fn default() -> Self {
        Self::new()
    }
}
