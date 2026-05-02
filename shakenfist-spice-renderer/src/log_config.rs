//! Verbose / intimate logging gate for the renderer.
//!
//! Replaces `ryll/src/settings.rs` for renderer consumers.
//! The host (ryll) constructs a `LogConfig` once at startup and
//! clones it into channel constructors. Channels read flags via
//! the value rather than a global, so a renderer embedded in a
//! different host can configure logging independently.

/// Logging configuration passed into channel constructors.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogConfig {
    /// Verbose protocol logging (every SPICE message logged).
    pub verbose: bool,
    /// Intimate logging (keystrokes, mouse coordinates).
    pub intimate: bool,
}

impl LogConfig {
    /// Construct a `LogConfig` with the given flags.
    pub fn new(verbose: bool, intimate: bool) -> Self {
        LogConfig { verbose, intimate }
    }
}
