//! `arboard`-backed clipboard implementation for ryll.
//!
//! Wraps `arboard::Clipboard` with an internal `Mutex` for
//! thread safety and lazy initialisation. On Linux, creating
//! an `arboard::Clipboard` opens a connection to the display
//! server, so we defer creation until the first use and retry
//! on error rather than failing at construction time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use shakenfist_spice_renderer::ClipboardBackend;

/// Host clipboard backed by `arboard`.
///
/// Passed into `MainChannel::new` as
/// `Some(Arc::new(ArboardClipboard::new()))` for GUI sessions.
/// Headless sessions pass `None`.
pub struct ArboardClipboard {
    inner: Mutex<Option<arboard::Clipboard>>,
}

impl ArboardClipboard {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Acquire (or lazily create) the inner `arboard::Clipboard`
    /// and call `f` on it, returning `Some(result)`.
    ///
    /// Returns `None` if the mutex is poisoned or if creating
    /// the clipboard fails (e.g. no display server).
    fn with_clipboard<R>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Option<R> {
        let mut guard = self.inner.lock().ok()?;
        if guard.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => *guard = Some(cb),
                Err(_) => return None,
            }
        }
        Some(f(guard
            .as_mut()
            .expect("clipboard was just initialised above")))
    }
}

impl ClipboardBackend for ArboardClipboard {
    fn get_text(&self) -> Option<String> {
        self.with_clipboard(|cb| cb.get_text().ok())?
    }

    fn set_text(&self, text: &str) -> Result<(), String> {
        // Acquire the lock and ensure we have a live clipboard handle.
        let mut guard = self.inner.lock().map_err(|e| e.to_string())?;
        if guard.is_none() {
            *guard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
        }
        match guard
            .as_mut()
            .expect("clipboard was just initialised above")
            .set_text(text)
        {
            Ok(()) => Ok(()),
            Err(e) => {
                // Reset the cached handle so the next call retries.
                *guard = None;
                Err(e.to_string())
            }
        }
    }
}

/// `ClipboardBackend` decorator that suppresses host-side
/// `get_text()` calls when ryll's window is not focused.
///
/// Why: macOS's pasteboard server (`pboard`) serializes
/// access system-wide, and a backgrounded process is heavily
/// deprioritised. The synchronous `arboard::Clipboard::get_text()`
/// call from main's 500 ms `clipboard_interval` tick has been
/// observed (session-001b/c/d/f) to block long enough on
/// macOS to wedge main's tokio worker, starving SPICE PONGs
/// and tripping the server's 30 s rcc connectivity timeout.
///
/// Step 2e (commit 54155e99) made the call cancel-safe via
/// `spawn_blocking` + 1 s timeout. This wrapper goes a step
/// further: when ryll isn't focused, the user is by definition
/// not interacting with the guest, so polling the host
/// pasteboard adds no value — and contending against
/// foreground apps' pasteboard activity is exactly what
/// triggers the throttling. Returning `None` early avoids
/// the underlying call entirely.
///
/// `set_text` is unchanged: guest→host clipboard pushes are
/// always honoured because they only fire on explicit guest
/// action, and writing to the pasteboard is fast and
/// non-throttled.
pub struct FocusGatedClipboard {
    inner: Arc<dyn ClipboardBackend>,
    focused: Arc<AtomicBool>,
}

impl FocusGatedClipboard {
    pub fn new(inner: Arc<dyn ClipboardBackend>, focused: Arc<AtomicBool>) -> Self {
        Self { inner, focused }
    }
}

impl ClipboardBackend for FocusGatedClipboard {
    fn get_text(&self) -> Option<String> {
        if !self.focused.load(Ordering::Relaxed) {
            return None;
        }
        self.inner.get_text()
    }

    fn set_text(&self, text: &str) -> Result<(), String> {
        self.inner.set_text(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysHello;
    impl ClipboardBackend for AlwaysHello {
        fn get_text(&self) -> Option<String> {
            Some("hello".to_string())
        }
        fn set_text(&self, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn focus_gated_returns_none_when_unfocused() {
        let inner: Arc<dyn ClipboardBackend> = Arc::new(AlwaysHello);
        let focused = Arc::new(AtomicBool::new(false));
        let cb = FocusGatedClipboard::new(inner, focused.clone());
        assert_eq!(cb.get_text(), None);

        focused.store(true, Ordering::Relaxed);
        assert_eq!(cb.get_text(), Some("hello".to_string()));
    }

    #[test]
    fn focus_gated_set_text_always_passes_through() {
        let inner: Arc<dyn ClipboardBackend> = Arc::new(AlwaysHello);
        let focused = Arc::new(AtomicBool::new(false));
        let cb = FocusGatedClipboard::new(inner, focused);
        // set_text bypasses the focus gate intentionally.
        assert!(cb.set_text("ignored").is_ok());
    }
}
