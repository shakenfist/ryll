/// Global runtime settings
///
/// These are set once at startup and read by channel handlers.
use std::sync::atomic::{AtomicBool, Ordering};

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
pub fn is_intimate() -> bool {
    INTIMATE.load(Ordering::Relaxed)
}
