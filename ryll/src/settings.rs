/// Global runtime settings shim for ryll-side callers.
///
/// Channels no longer read these globals — they receive a
/// `shakenfist_spice_renderer::LogConfig` value at construction
/// time. The globals remain for the few host-side callers that
/// have not yet been threaded with `LogConfig` (e.g. the GUI
/// stats panel and bug-report assembly), and as the source of
/// truth that `app.rs::reconnect()` reads to build a
/// `LogConfig` for the channels it spawns.
use std::sync::atomic::{AtomicBool, Ordering};

use shakenfist_spice_renderer::LogConfig;

/// Whether verbose protocol logging is enabled
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Whether intimate logging (keystrokes, mouse) is enabled
static INTIMATE: AtomicBool = AtomicBool::new(false);

/// Initialize settings from command line args
pub fn init(verbose: bool, intimate: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
    INTIMATE.store(intimate, Ordering::Relaxed);
}

/// Check if verbose logging is enabled
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Check if intimate logging is enabled
#[allow(dead_code)]
pub fn is_intimate() -> bool {
    INTIMATE.load(Ordering::Relaxed)
}

/// Snapshot the global flags into a `LogConfig` for handing to
/// renderer-side channel constructors.
pub fn log_config() -> LogConfig {
    LogConfig::new(is_verbose(), is_intimate())
}
