//! Trait surface for host-side clipboard access.
//!
//! The main channel uses this to ferry clipboard text between
//! the SPICE guest's vdagent and whatever clipboard subsystem
//! the host provides.
//!
//! ryll's GUI provides a host-clipboard-backed implementation.
//! [`MemoryClipboard`] is the one for sessions with no host
//! clipboard to use: headless and web mode give it to the main
//! channel when they run a control socket, which reads and writes
//! it with the `clipboard_get` and `clipboard_set` verbs.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

/// A host-side clipboard backend.
///
/// The renderer's main channel uses this to ferry clipboard
/// text between the SPICE guest's vdagent and whatever
/// clipboard subsystem the host provides.
///
/// Implementations are responsible for any internal caching of
/// the underlying handle. On Linux, opening a display-server
/// clipboard connection can be expensive, so implementations
/// should defer creation until first use and retry on error.
/// Errors are returned as `String`s to keep the trait
/// dyn-friendly without dragging in a specific error type.
pub trait ClipboardBackend: Send + Sync {
    /// Read the current clipboard text, if any.
    fn get_text(&self) -> Option<String>;

    /// Write `text` to the clipboard.
    ///
    /// On error the implementation should reset any cached
    /// handle so the next call retries from scratch.
    fn set_text(&self, text: &str) -> Result<(), String>;
}

/// A clipboard held in memory, for a session with no host clipboard.
///
/// The main channel sees it as any other backend: it polls
/// [`get_text`](ClipboardBackend::get_text) every 500 ms and grabs the
/// guest's clipboard when the text changes, and it stores text the
/// guest copies with [`set_text`](ClipboardBackend::set_text). The
/// session's other side -- the control socket -- writes with
/// [`set_host_text`](Self::set_host_text), standing in for whatever a
/// user would have copied on the host, and reads with
/// [`text`](Self::text).
///
/// The two writers are kept apart because only the guest's text is
/// news to the control socket's client: it wrote the other itself.
/// Each `set_text` is published on a watch, which the control
/// socket's `clipboard_changed` event is translated from. A watch
/// keeps only the latest value, so a client that falls behind sees
/// the newest copy rather than every one, which is what a clipboard
/// means anyway.
pub struct MemoryClipboard {
    text: Mutex<Option<String>>,
    guest_text: watch::Sender<Option<Arc<str>>>,
}

impl MemoryClipboard {
    pub fn new() -> Self {
        Self {
            text: Mutex::new(None),
            guest_text: watch::Sender::new(None),
        }
    }

    /// The clipboard's text, whichever side wrote it last, or `None`
    /// if neither has.
    pub fn text(&self) -> Option<String> {
        self.text.lock().ok()?.clone()
    }

    /// Set the clipboard as if the user had copied `text` on the host.
    ///
    /// The main channel notices on its next poll and grabs the guest's
    /// clipboard, provided the agent is connected and has announced
    /// its capabilities. Empty text is stored but never grabbed,
    /// because the poll ignores an empty clipboard. Not published as a
    /// guest change.
    pub fn set_host_text(&self, text: String) {
        if let Ok(mut guard) = self.text.lock() {
            *guard = Some(text);
        }
    }

    /// Subscribe to text arriving from the guest. The receiver starts
    /// with the current value marked seen, so it reports only copies
    /// made after this call.
    pub fn subscribe_guest_text(&self) -> watch::Receiver<Option<Arc<str>>> {
        self.guest_text.subscribe()
    }
}

impl Default for MemoryClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardBackend for MemoryClipboard {
    fn get_text(&self) -> Option<String> {
        self.text()
    }

    /// Store text the guest copied, and publish it.
    fn set_text(&self, text: &str) -> Result<(), String> {
        let mut guard = self.text.lock().map_err(|e| e.to_string())?;
        *guard = Some(text.to_string());
        drop(guard);
        // `send_replace` rather than `send`: it stores the value even
        // when nobody is subscribed, and marks it changed for every
        // receiver, including when the same text is copied twice.
        self.guest_text.send_replace(Some(Arc::from(text)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_empty() {
        let cb = MemoryClipboard::new();
        assert_eq!(cb.text(), None);
        assert_eq!(cb.get_text(), None);
    }

    #[test]
    fn host_text_is_read_back_by_the_main_channel_without_publishing() {
        let cb = MemoryClipboard::new();
        let rx = cb.subscribe_guest_text();
        cb.set_host_text("from the host".to_string());
        assert_eq!(cb.get_text().as_deref(), Some("from the host"));
        assert_eq!(cb.text().as_deref(), Some("from the host"));
        assert!(
            !rx.has_changed().unwrap(),
            "host text is not a guest change"
        );
    }

    #[test]
    fn guest_text_is_stored_and_published() {
        let cb = MemoryClipboard::new();
        let mut rx = cb.subscribe_guest_text();
        cb.set_text("from the guest").unwrap();
        assert_eq!(cb.text().as_deref(), Some("from the guest"));
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().as_deref(), Some("from the guest"));

        // The same text copied again is a change all the same.
        cb.set_text("from the guest").unwrap();
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn each_writer_replaces_the_other() {
        let cb = MemoryClipboard::new();
        cb.set_text("guest").unwrap();
        cb.set_host_text("host".to_string());
        assert_eq!(cb.text().as_deref(), Some("host"));
        cb.set_text("guest again").unwrap();
        assert_eq!(cb.text().as_deref(), Some("guest again"));
    }

    #[test]
    fn a_late_subscriber_sees_only_later_copies() {
        let cb = MemoryClipboard::new();
        cb.set_text("before").unwrap();
        let mut rx = cb.subscribe_guest_text();
        assert!(!rx.has_changed().unwrap());
        cb.set_text("after").unwrap();
        assert_eq!(rx.borrow_and_update().as_deref(), Some("after"));
    }
}
