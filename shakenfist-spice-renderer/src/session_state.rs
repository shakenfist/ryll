//! Session state that frontends must not lose, published as latest-value
//! `watch` channels rather than as `ChannelEvent`s.
//!
//! The event queue is the wrong carrier for state. The main channel gives
//! up on an event it cannot queue within `MAIN_EVENT_SEND_TIMEOUT`, and
//! web and headless mode fan events out over a `broadcast` bus whose
//! receivers skip what they lag past. Either way a dropped mouse-mode
//! change leaves a frontend sending the pointer messages the server
//! ignores until the session is reconnected, with nothing to show for it
//! but a warning in the log (issue #428).
//!
//! A `watch` channel holds only the latest value. Publishing never blocks
//! and never fails, whether or not anything is reading, and a reader can
//! fall behind but never miss the current value: several changes inside
//! one stall coalesce into the last of them. That is the right semantics
//! for state, and the reason a connect, disconnect and reconnect of the
//! agent within one UI stall reads as no change at all.
//!
//! The host creates a [`SessionState`] for each connection attempt, keeps
//! a [`SessionStateRx`] from [`SessionState::subscribe`], and hands the
//! `SessionState` to `run_connection`, which moves it into the main
//! channel. Because the receivers exist before the session does, there is
//! no subscribe-before-spawn ordering to get right. The senders are
//! dropped when the main channel's task ends, so `changed()` on a
//! receiver returns `Err` once the session is over.

use tokio::sync::watch;

/// The mouse mode before the server has announced one. Not a SPICE mode:
/// `MOUSE_MODE_SERVER` is 1 and `MOUSE_MODE_CLIENT` is 2. Frontends treat
/// anything other than server mode as absolute, which is what they did
/// before mouse mode was tracked at all.
pub const MOUSE_MODE_UNKNOWN: u32 = 0;

/// The publishing side of a session's state. Owned by the main channel.
#[derive(Debug)]
pub struct SessionState {
    mouse_mode: watch::Sender<u32>,
    agent_connected: watch::Sender<bool>,
}

/// The reading side of a session's state. Cheap to clone; each clone
/// tracks separately which value it has seen.
#[derive(Debug, Clone)]
pub struct SessionStateRx {
    /// The server's current mouse mode, from `MAIN_INIT` and then each
    /// `MOUSE_MODE` message. [`MOUSE_MODE_UNKNOWN`] until the first.
    pub mouse_mode: watch::Receiver<u32>,
    /// Whether the guest's vdagent is connected, from `MAIN_INIT` and
    /// then each `AGENT_CONNECTED` / `AGENT_DISCONNECTED` message.
    pub agent_connected: watch::Receiver<bool>,
}

impl SessionState {
    /// State for a session that has not connected yet: mouse mode
    /// unknown, no agent.
    pub fn new() -> Self {
        let (mouse_mode, _) = watch::channel(MOUSE_MODE_UNKNOWN);
        let (agent_connected, _) = watch::channel(false);
        Self {
            mouse_mode,
            agent_connected,
        }
    }

    /// A receiver bundle that has seen the current values, so its
    /// `changed()` resolves on the next publish.
    pub fn subscribe(&self) -> SessionStateRx {
        SessionStateRx {
            mouse_mode: self.mouse_mode.subscribe(),
            agent_connected: self.agent_connected.subscribe(),
        }
    }

    /// Publish the server's mouse mode. Every call marks the value
    /// changed, even when it repeats the last one: a repeat is what the
    /// server said, and a reader that only cares about transitions can
    /// compare values itself.
    pub(crate) fn publish_mouse_mode(&self, mode: u32) {
        self.mouse_mode.send_replace(mode);
    }

    /// Publish the vdagent connection state. Marks the value changed on
    /// every call, as [`Self::publish_mouse_mode`] does.
    pub(crate) fn publish_agent_connected(&self, connected: bool) {
        self.agent_connected.send_replace(connected);
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{ChannelEvent, EventSink};
    use shakenfist_spice_protocol::MOUSE_MODE_SERVER;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, Notify};

    #[test]
    fn a_new_session_starts_unknown_and_without_an_agent() {
        let rx = SessionState::new().subscribe();
        assert_eq!(*rx.mouse_mode.borrow(), MOUSE_MODE_UNKNOWN);
        assert!(!*rx.agent_connected.borrow());
    }

    /// The case issue #428 is about. The UI has stopped draining the
    /// event queue, so the main channel's next event is abandoned on its
    /// send timeout -- but a mouse-mode change published as state still
    /// reaches the reader, and the renderer is still woken to look.
    #[tokio::test(start_paused = true)]
    async fn state_published_while_the_event_queue_is_wedged_still_arrives() {
        let (tx, _rx) = mpsc::channel(1);
        let repaint = Arc::new(Notify::new());
        let sink =
            EventSink::new(tx, Arc::clone(&repaint)).with_send_timeout(Duration::from_millis(50));
        let state = SessionState::new();
        let mut state_rx = state.subscribe();

        // Fill the only slot; `_rx` is held and never polled.
        sink.emit(ChannelEvent::SessionInitialized(1)).await;
        sink.emit(ChannelEvent::Latency { sample_ms: 1.0 }).await;
        assert_eq!(sink.drop_stats().total, 1, "the queue should be wedged");
        // Discard the permits those emits left, so the check below is
        // about the wake that goes with the publish.
        tokio::time::timeout(Duration::from_millis(10), repaint.notified())
            .await
            .expect("emit wakes the renderer");

        // What `MainChannel` does on MOUSE_MODE.
        state.publish_mouse_mode(MOUSE_MODE_SERVER);
        sink.wake();

        tokio::time::timeout(Duration::from_secs(1), state_rx.mouse_mode.changed())
            .await
            .expect("changed() must resolve while the event queue is full")
            .expect("the sender is still alive");
        assert_eq!(*state_rx.mouse_mode.borrow_and_update(), MOUSE_MODE_SERVER);
        tokio::time::timeout(Duration::from_millis(10), repaint.notified())
            .await
            .expect("publishing state must wake the renderer");
    }

    /// A reader that falls behind sees the latest value, not a backlog
    /// and not a gap: the property a `broadcast` receiver lacks.
    #[test]
    fn a_reader_that_falls_behind_sees_only_the_latest_value() {
        let state = SessionState::new();
        let mut rx = state.subscribe();

        for _ in 0..5000 {
            state.publish_agent_connected(true);
            state.publish_agent_connected(false);
        }
        state.publish_agent_connected(true);

        let seen = rx.agent_connected.borrow_and_update();
        assert!(seen.has_changed());
        assert!(*seen);
    }

    /// A repeated value is still published as a change, so a frontend
    /// that announces every report from the server keeps doing so.
    #[test]
    fn a_repeated_value_still_counts_as_a_change() {
        let state = SessionState::new();
        let mut rx = state.subscribe();

        state.publish_agent_connected(false);
        assert!(rx.agent_connected.has_changed().expect("sender alive"));
        rx.agent_connected.borrow_and_update();
        assert!(!rx.agent_connected.has_changed().expect("sender alive"));
    }

    /// Dropping the state is how a reader learns the session is over.
    #[tokio::test]
    async fn readers_see_the_session_end_when_the_state_is_dropped() {
        let state = SessionState::new();
        let mut rx = state.subscribe();
        state.publish_mouse_mode(MOUSE_MODE_SERVER);
        drop(state);

        // The last value survives the sender, for a reader that reads
        // after the session ended.
        assert_eq!(*rx.mouse_mode.borrow(), MOUSE_MODE_SERVER);
        rx.mouse_mode.borrow_and_update();
        assert!(rx.mouse_mode.changed().await.is_err());
    }
}
