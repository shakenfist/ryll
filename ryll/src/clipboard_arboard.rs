//! `arboard`-backed clipboard implementation for ryll.
//!
//! Wraps `arboard::Clipboard` with an internal `Mutex` for
//! thread safety and lazy initialisation. On Linux, creating
//! an `arboard::Clipboard` opens a connection to the display
//! server, so we defer creation until the first use and retry
//! on error rather than failing at construction time.

use std::sync::Mutex;

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
