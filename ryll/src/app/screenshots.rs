use std::path::PathBuf;
use tracing::{error, info};

use crate::notifications::NotificationSource;
use shakenfist_spice_protocol::NotifySeverity;

use super::RyllApp;

// ── Screenshot path helpers ─────────────────────────────────────────────────

/// Derive per-surface output paths from a user-chosen base path.
///
/// * If `count <= 1`, returns `vec![base.to_path_buf()]` unchanged.
///   Callers are expected to validate non-empty surfaces before calling
///   (`save_screenshots` does); a `count == 0` invocation will silently
///   produce a single path equal to `base`, never an empty vector.
/// * If `count > 1`, strips the last extension from `base` (if any) and
///   appends `-1.png`, `-2.png`, … `-{count}.png`.  Parent directory
///   components in `base` are preserved.
///
/// Examples:
/// ```text
/// ("foo.png",          1) → ["foo.png"]
/// ("foo.png",          3) → ["foo-1.png", "foo-2.png", "foo-3.png"]
/// ("foo",              2) → ["foo-1.png", "foo-2.png"]
/// ("foo.bar.png",      2) → ["foo.bar-1.png", "foo.bar-2.png"]
/// ("/tmp/foo.png",     2) → ["/tmp/foo-1.png", "/tmp/foo-2.png"]
/// ```
fn screenshot_paths(base: &std::path::Path, count: usize) -> Vec<PathBuf> {
    if count <= 1 {
        return vec![base.to_path_buf()];
    }

    // Strip only the last extension.
    let stem: std::path::PathBuf = if base.extension().is_some() {
        base.with_extension("")
    } else {
        base.to_path_buf()
    };

    (1..=count)
        .map(|n| {
            let mut name = stem
                .file_name()
                .map(|s| s.to_os_string())
                .unwrap_or_default();
            name.push(format!("-{}.png", n));
            stem.with_file_name(name)
        })
        .collect()
}

// ── End screenshot path helpers ─────────────────────────────────────────────

impl RyllApp {
    /// Save the current display surface(s) as PNG file(s).
    ///
    /// If there is exactly one surface, the file is written directly to
    /// `base_path`.  With multiple surfaces the last extension is stripped
    /// from `base_path` and each surface gets its own file with a `-N.png`
    /// suffix (e.g. `foo-1.png`, `foo-2.png`).  Surfaces are visited in
    /// deterministic order (sorted by their `(channel_id, surface_id)` key).
    ///
    /// Returns the list of paths that were successfully written.
    fn save_screenshots(&self, base_path: PathBuf) -> anyhow::Result<Vec<PathBuf>> {
        if self.surfaces.surfaces.is_empty() {
            anyhow::bail!("No display surfaces to capture");
        }

        let mut sorted: Vec<_> = self.surfaces.surfaces.iter().collect();
        sorted.sort_by_key(|(k, _)| *k);

        let paths = screenshot_paths(&base_path, sorted.len());

        let mut written = Vec::new();
        for ((_, surface), path) in sorted.into_iter().zip(paths) {
            let png_bytes =
                crate::bugreport::encode_png(surface.pixels(), surface.width, surface.height)?;
            std::fs::write(&path, &png_bytes)?;
            written.push(path);
        }

        Ok(written)
    }

    /// Open a native save dialog and write the current surface(s) as PNG(s).
    ///
    /// If the dialog is cancelled, nothing happens.
    ///
    /// The dialog is synchronous: it runs the native panel's modal loop
    /// inside `ui()`, and any key released while it is open goes to the
    /// panel, so egui never sees the release. Every key the guest holds
    /// is therefore released once the dialog returns. The caller must
    /// open the dialog only after the frame's input has been forwarded,
    /// or a press from this frame would be sent after that release and
    /// stick.
    pub(super) fn open_screenshot_dialog(&mut self) {
        if self.surfaces.surfaces.is_empty() {
            self.push_notification(
                NotifySeverity::Warn,
                NotificationSource::Internal,
                "No display surface to capture yet",
            );
            return;
        }

        let default_name = format!(
            "ryll-screenshot-{}.png",
            crate::bugreport::filename_timestamp()
        );

        let picked = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .save_file();
        self.release_guest_keys();

        if let Some(path) = picked {
            match self.save_screenshots(path) {
                Ok(paths) => {
                    let msg = if paths.len() == 1 {
                        format!("Saved screenshot to {}", paths[0].display())
                    } else {
                        let names: Vec<String> =
                            paths.iter().map(|p| p.display().to_string()).collect();
                        format!("Saved {} screenshots to {}", paths.len(), names.join(", "))
                    };
                    info!("app: {}", msg);
                    self.push_notification(NotifySeverity::Info, NotificationSource::Internal, msg);
                }
                Err(e) => {
                    let msg = format!("Screenshot failed: {}", e);
                    error!("app: {}", msg);
                    self.push_notification(
                        NotifySeverity::Error,
                        NotificationSource::Internal,
                        msg,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_paths_single() {
        let paths = screenshot_paths(std::path::Path::new("foo.png"), 1);
        assert_eq!(paths, vec![PathBuf::from("foo.png")]);
    }

    #[test]
    fn screenshot_paths_multi() {
        let paths = screenshot_paths(std::path::Path::new("foo.png"), 3);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("foo-1.png"),
                PathBuf::from("foo-2.png"),
                PathBuf::from("foo-3.png"),
            ]
        );
    }

    #[test]
    fn screenshot_paths_no_extension() {
        let paths = screenshot_paths(std::path::Path::new("foo"), 2);
        assert_eq!(
            paths,
            vec![PathBuf::from("foo-1.png"), PathBuf::from("foo-2.png"),]
        );
    }

    #[test]
    fn screenshot_paths_multi_extension() {
        let paths = screenshot_paths(std::path::Path::new("foo.bar.png"), 2);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("foo.bar-1.png"),
                PathBuf::from("foo.bar-2.png"),
            ]
        );
    }

    #[test]
    fn screenshot_paths_with_directory() {
        let paths = screenshot_paths(std::path::Path::new("/tmp/captures/foo.png"), 2);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/captures/foo-1.png"),
                PathBuf::from("/tmp/captures/foo-2.png"),
            ]
        );
    }
}
