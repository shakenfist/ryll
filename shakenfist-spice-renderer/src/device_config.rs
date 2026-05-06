//! Device configuration types passed to the usbredir and webdav
//! channel constructors.
//!
//! These shapes describe what the renderer needs to attach a
//! virtual disk or shared directory; the host (ryll) constructs
//! them from CLI flags, but a different consumer of the renderer
//! could construct them from any source. Path validation lives
//! in the host (it is policy, not protocol).

use std::path::PathBuf;

/// Parsed virtual disk configuration for a USB MSC backend.
#[derive(Debug, Clone)]
pub struct VirtualDiskConfig {
    pub path: PathBuf,
    pub read_only: bool,
}

/// Parsed shared directory configuration for the WebDAV channel.
#[derive(Debug, Clone)]
pub struct ShareDirConfig {
    pub path: PathBuf,
    pub read_only: bool,
}
