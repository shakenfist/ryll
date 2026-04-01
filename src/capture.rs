/// Capture session for protocol and display debugging.
///
/// When `--capture <DIR>` is specified, all SPICE protocol
/// traffic and display frames are written to files in the
/// given directory. When not enabled, all methods are no-ops.
use std::fs;
use std::path::PathBuf;
use std::time::Instant;
use tracing::info;

/// Holds state for an active capture session.
#[allow(dead_code)] // fields and methods used by phases 2 and 3
pub struct CaptureSession {
    /// Output directory for capture files.
    pub dir: PathBuf,
    /// Timestamp of session start, for relative timing.
    pub start: Instant,
}

#[allow(dead_code)] // stub methods used by phases 2 and 3
impl CaptureSession {
    /// Create a new capture session writing to `dir`.
    pub fn new(dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir)?;
        info!("capture: writing to {}", dir.display());
        Ok(CaptureSession {
            dir,
            start: Instant::now(),
        })
    }

    /// Record a packet sent by the client on the given channel.
    /// Phase 2 will write this to a pcap file.
    pub fn packet_sent(&self, _channel: &str, _data: &[u8]) {
        // Stub — implemented in phase 2
    }

    /// Record a packet received from the server on the given channel.
    /// Phase 2 will write this to a pcap file.
    pub fn packet_received(&self, _channel: &str, _data: &[u8]) {
        // Stub — implemented in phase 2
    }

    /// Record a display frame after a MARK boundary.
    /// Phase 3 will encode this as a video frame.
    pub fn frame(&self, _surface_id: u32, _pixels: &[u8], _width: u32, _height: u32) {
        // Stub — implemented in phase 3
    }

    /// Finalise and close the capture session.
    pub fn close(&mut self) {
        info!("capture: session closed ({})", self.dir.display());
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.close();
    }
}
