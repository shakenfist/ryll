//! Trait for receiving notifications produced by the renderer.
//!
//! Channels emit `ChannelEvent::Notification(entry)`; the host
//! drains the event channel and decides what to do with each
//! entry. `run_headless` exposes a small `NotificationSink` so
//! the host can plug its store in without the renderer having to
//! know what shape that store has — ryll uses
//! `Arc<Mutex<NotificationStore>>`, the planned `--web` frontend
//! will use whatever fits its lifecycle.

use crate::notification::NotificationEntry;

/// Receives notifications produced by the renderer's headless
/// session loop. Implementations are expected to be cheap and
/// non-blocking; `run_headless` calls `push` directly from its
/// event-drain branch without holding any other locks.
pub trait NotificationSink: Send + Sync {
    /// Record a notification.
    fn push(&self, entry: NotificationEntry);
}
