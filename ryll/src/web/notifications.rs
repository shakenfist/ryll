//! Renderer → notification store relay for `--web` mode.
//!
//! The renderer announces guest notifications, usbredir and
//! WebDAV failures as [`ChannelEvent::Notification`]. Each
//! frontend is responsible for getting them into the host's
//! [`NotificationStore`](crate::notifications::NotificationStore):
//! the GUI does it from its own event drain
//! (`app::events::handle_notification`), `run_headless` does it
//! through the [`NotificationSink`] it is handed, and web mode
//! does it here, because `run_connection` — the entry point web
//! mode uses — drains nothing on the host's behalf and takes no
//! sink.
//!
//! The store is not dead weight in web mode even though the
//! browser shell has no notification UI yet: a `--pedantic` bug
//! report includes it (`BugReport::register_pedantic_observer`),
//! so without this relay a web-mode pedantic zip carries only the
//! protocol-gap notifications that
//! `register_gap_notification_observer` synthesises on the host
//! side, and none of the ones the renderer raised.

use std::sync::Arc;

use shakenfist_spice_renderer::{ChannelEvent, NotificationSink};
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Push every [`ChannelEvent::Notification`] into `sink` until the
/// broadcast bus closes (i.e. the renderer's session orchestrator
/// has exited).
///
/// A `Lagged` receiver has lost notifications outright — unlike a
/// cursor delta there is no later event that restores them — so the
/// loss is logged at warn rather than debug, and the relay keeps
/// going: the notifications still on the bus are worth more than a
/// relay that stops at the first burst.
pub async fn run_notification_relay(
    mut event_rx: broadcast::Receiver<ChannelEvent>,
    sink: Arc<dyn NotificationSink>,
) {
    loop {
        match event_rx.recv().await {
            Ok(ChannelEvent::Notification(entry)) => sink.push(entry),
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    "web notifications: lagged by {} events; any notification among them is \
                     lost and will not be re-sent",
                    n
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("web notifications: event channel closed; relay exiting");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use shakenfist_spice_protocol::NotifySeverity;
    use shakenfist_spice_renderer::{NotificationEntry, NotificationSource};

    use super::*;

    /// Sink that records what it was pushed, in order.
    struct RecordingSink(Mutex<Vec<String>>);

    impl NotificationSink for RecordingSink {
        fn push(&self, entry: NotificationEntry) {
            self.0.lock().expect("sink lock").push(entry.message);
        }
    }

    fn entry(message: &str) -> NotificationEntry {
        NotificationEntry::new(
            NotifySeverity::Warn,
            NotificationSource::Internal,
            message.to_string(),
        )
    }

    /// The relay pushes notifications and ignores everything else
    /// on the bus.
    #[tokio::test(flavor = "current_thread")]
    async fn relays_notifications_and_ignores_other_events() {
        let (tx, rx) = broadcast::channel::<ChannelEvent>(16);
        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        let handle = tokio::spawn(run_notification_relay(rx, sink.clone()));

        tx.send(ChannelEvent::Notification(entry("disk full")))
            .expect("send notification");
        // An unrelated event the relay must not react to.
        tx.send(ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: 640,
            height: 480,
        })
        .expect("send surface");
        tx.send(ChannelEvent::Notification(entry("usb gone")))
            .expect("send notification");

        // Dropping the last sender closes the bus, which is the
        // relay's exit condition — so joining it is also how we wait
        // for the sends above to have been processed.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("relay should exit when the bus closes")
            .expect("relay should not panic");

        assert_eq!(
            sink.0.lock().expect("sink lock").as_slice(),
            ["disk full", "usb gone"],
            "both notifications should reach the sink, in order, and nothing else"
        );
    }

    /// The relay exits when the bus closes rather than spinning on
    /// the closed receiver.
    #[tokio::test(flavor = "current_thread")]
    async fn exits_when_the_bus_closes() {
        let (tx, rx) = broadcast::channel::<ChannelEvent>(4);
        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        let handle = tokio::spawn(run_notification_relay(rx, sink));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("relay should exit promptly")
            .expect("relay should not panic");
    }

    /// A lagged receiver does not stop the relay: it logs the loss
    /// and keeps delivering what is still on the bus.
    #[tokio::test(flavor = "current_thread")]
    async fn survives_a_lagged_receiver() {
        // Capacity 2, and nothing is consumed until the relay is
        // spawned below, so the first sends overflow the ring and the
        // receiver's next recv() returns Lagged.
        let (tx, rx) = broadcast::channel::<ChannelEvent>(2);
        for i in 0..6 {
            tx.send(ChannelEvent::Notification(entry(&format!("old {}", i))))
                .expect("send");
        }

        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        let handle = tokio::spawn(run_notification_relay(rx, sink.clone()));
        tx.send(ChannelEvent::Notification(entry("after the lag")))
            .expect("send");
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("relay should exit when the bus closes")
            .expect("relay should not panic");

        let seen = sink.0.lock().expect("sink lock").clone();
        assert!(
            seen.contains(&"after the lag".to_string()),
            "the relay should keep running after Lagged; saw {:?}",
            seen
        );
    }
}
