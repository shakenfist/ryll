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

use std::sync::Mutex;

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

    /// A number that changes whenever the clipboard's text may have,
    /// or `None` if the backend cannot tell.
    ///
    /// The main channel polls the clipboard every 500 ms. With a
    /// generation it skips the read -- and the copy, normalisation and
    /// hash that follow it -- while the number is unchanged, which for
    /// a large clipboard is most of the poll's cost. A host clipboard
    /// that other programs write cannot know when they do, so the
    /// default is `None`, and the main channel then reads every time.
    fn generation(&self) -> Option<u64> {
        None
    }
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
/// Each `set_text` publishes the copy's length in bytes on a watch,
/// which the control socket's `clipboard_changed` event is translated
/// from. The text itself is not published: a copy can be 100 MiB, and
/// a client reads it with `clipboard_get` when it wants it. A watch
/// keeps only the latest value, so a client that falls behind hears of
/// the newest copy rather than every one, which is what a clipboard
/// means anyway.
pub struct MemoryClipboard {
    contents: Mutex<Contents>,
    guest_copies: watch::Sender<Option<usize>>,
}

/// The clipboard's text, and its generation: bumped on every write,
/// from either side, under the same lock as the text it counts.
#[derive(Default)]
struct Contents {
    text: Option<String>,
    generation: u64,
}

impl MemoryClipboard {
    pub fn new() -> Self {
        Self {
            contents: Mutex::new(Contents::default()),
            guest_copies: watch::Sender::new(None),
        }
    }

    /// Lock the contents. A poisoned lock is taken anyway: the guarded
    /// value is plain data with no invariant a panicking writer could
    /// have left half-kept, and failing every later read and write
    /// over it would lose the clipboard for the rest of the session.
    fn lock(&self) -> std::sync::MutexGuard<'_, Contents> {
        self.contents.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the text and bump the generation.
    fn store(&self, text: String) {
        let mut contents = self.lock();
        contents.text = Some(text);
        contents.generation = contents.generation.wrapping_add(1);
    }

    /// The clipboard's text, whichever side wrote it last, or `None`
    /// if neither has.
    pub fn text(&self) -> Option<String> {
        self.lock().text.clone()
    }

    /// Set the clipboard as if the user had copied `text` on the host.
    ///
    /// The main channel notices on its next poll and grabs the guest's
    /// clipboard, provided the agent is connected and has announced
    /// its capabilities. Empty text is stored but never grabbed,
    /// because the poll ignores an empty clipboard. Not published as a
    /// guest change.
    pub fn set_host_text(&self, text: String) {
        self.store(text);
    }

    /// Subscribe to text arriving from the guest: the value is the
    /// latest copy's length in bytes. The receiver starts with the
    /// current value marked seen, so it reports only copies made after
    /// this call.
    pub fn subscribe_guest_copies(&self) -> watch::Receiver<Option<usize>> {
        self.guest_copies.subscribe()
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
        self.store(text.to_string());
        // `send_replace` rather than `send`: it stores the value even
        // when nobody is subscribed, and marks it changed for every
        // receiver, including when the same text is copied twice.
        self.guest_copies.send_replace(Some(text.len()));
        Ok(())
    }

    fn generation(&self) -> Option<u64> {
        Some(self.lock().generation)
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
        let rx = cb.subscribe_guest_copies();
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
        let mut rx = cb.subscribe_guest_copies();
        cb.set_text("from the guest").unwrap();
        assert_eq!(cb.text().as_deref(), Some("from the guest"));
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), Some("from the guest".len()));

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
    fn every_write_from_either_side_bumps_the_generation() {
        let cb = MemoryClipboard::new();
        let start = cb.generation().unwrap();
        cb.set_host_text("host".to_string());
        let after_host = cb.generation().unwrap();
        assert_ne!(after_host, start);
        // Reading changes nothing.
        let _ = cb.get_text();
        assert_eq!(cb.generation(), Some(after_host));
        // The same text again is still a write.
        cb.set_text("host").unwrap();
        assert_ne!(cb.generation(), Some(after_host));
    }

    #[test]
    fn a_poisoned_lock_still_reads_and_writes() {
        let cb = std::sync::Arc::new(MemoryClipboard::new());
        cb.set_host_text("before".to_string());
        let poisoner = cb.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.contents.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(cb.contents.is_poisoned());
        assert_eq!(cb.text().as_deref(), Some("before"));
        cb.set_host_text("after".to_string());
        assert_eq!(cb.text().as_deref(), Some("after"));
        assert!(cb.set_text("guest").is_ok());
        assert_eq!(cb.get_text().as_deref(), Some("guest"));
    }

    #[test]
    fn a_late_subscriber_sees_only_later_copies() {
        let cb = MemoryClipboard::new();
        cb.set_text("before").unwrap();
        let mut rx = cb.subscribe_guest_copies();
        assert!(!rx.has_changed().unwrap());
        cb.set_text("after").unwrap();
        assert_eq!(*rx.borrow_and_update(), Some("after".len()));
    }
}
