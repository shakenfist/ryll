//! Trait surface for host-side clipboard access.
//!
//! The main channel uses this to ferry clipboard text between
//! the SPICE guest's vdagent and whatever clipboard subsystem
//! the host provides.
//!
//! ryll provides a host-clipboard-backed implementation. Other
//! consumers (e.g. a headless test harness or a web frontend)
//! can implement a no-op or a custom clipboard backend
//! independently.

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
