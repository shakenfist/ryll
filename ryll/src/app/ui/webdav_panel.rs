use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use shakenfist_spice_renderer::WebdavCommand;
use tokio::sync::mpsc;
use tracing::{error, info};

/// State of the "Shared Folders" side panel, and the WebDAV command
/// sender it drives.
///
/// The sender belongs to one session: `new` takes the first one and
/// `reset_for_session` takes every one after it. `reset_for_session`
/// is also the only place that clears the state, so a field added
/// here is added in front of the code that has to decide whether a
/// new session keeps it. What it clears, and what it does not:
///
/// * Cleared for the new session: `channel_ready`, `shared_dir`,
///   `sharing_active`, `connected_at`, `error_message` and
///   `error_time`. The share belonged to the channel that went away,
///   and a stale error about it would outlive what it described.
/// * Installed: `tx`, the new session's sender.
/// * Carried across: `open`, because whether the user wants this panel
///   on screen is not the session's business; `read_only`, which
///   describes the share that has just been forgotten and is read only
///   while `sharing_active`, so clearing `sharing_active` is what makes
///   it unreachable rather than resetting it; and `pick_dir_rx` with
///   `pick_dir_readonly`, which belong to a directory picker the user
///   may still have open in front of them.
///
/// `channel_disconnected` clears less than `reset_for_session` does,
/// and deliberately: it leaves `error_message` and `error_time` alone
/// so the panel can still say why the channel dropped.
pub(in crate::app) struct WebdavPanel {
    open: bool,
    tx: Option<mpsc::Sender<WebdavCommand>>,
    channel_ready: bool,
    shared_dir: Option<String>,
    read_only: bool,
    sharing_active: bool,
    connected_at: Option<Instant>,
    error_message: Option<String>,
    error_time: Option<Instant>,
    pick_dir_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    pick_dir_readonly: bool,
}

impl WebdavPanel {
    pub(in crate::app) fn new(tx: mpsc::Sender<WebdavCommand>) -> Self {
        Self {
            open: false,
            tx: Some(tx),
            channel_ready: false,
            shared_dir: None,
            read_only: false,
            sharing_active: false,
            connected_at: None,
            error_message: None,
            error_time: None,
            pick_dir_rx: None,
            pick_dir_readonly: false,
        }
    }

    pub(in crate::app) fn is_open(&self) -> bool {
        self.open
    }

    // There is no `toggle` to go with these: the traffic viewer has
    // one because F11 flips it, and this panel has no hotkey, so one
    // here would be dead code, which `make lint` rejects.
    pub(in crate::app) fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    /// A new session: take its command sender and forget the previous
    /// session's share. The struct comment says what survives.
    pub(in crate::app) fn reset_for_session(&mut self, tx: mpsc::Sender<WebdavCommand>) {
        self.tx = Some(tx);
        self.channel_ready = false;
        self.shared_dir = None;
        self.sharing_active = false;
        self.connected_at = None;
        self.error_message = None;
        self.error_time = None;
    }

    /// The WebDAV channel connected.
    pub(in crate::app) fn channel_connected(&mut self) {
        info!("app: WebDAV channel connected");
        self.channel_ready = true;
    }

    /// WebDAV folder sharing started.
    pub(in crate::app) fn sharing_started(&mut self, path: String, read_only: bool) {
        info!("app: WebDAV sharing started: {} (ro={})", path, read_only);
        self.shared_dir = Some(path);
        self.read_only = read_only;
        self.sharing_active = true;
        self.connected_at = Some(Instant::now());
    }

    /// WebDAV folder sharing stopped.
    pub(in crate::app) fn sharing_stopped(&mut self) {
        info!("app: WebDAV sharing stopped");
        self.shared_dir = None;
        self.sharing_active = false;
        self.connected_at = None;
    }

    /// The WebDAV channel reported an error.
    pub(in crate::app) fn channel_error(&mut self, err: String) {
        error!("app: WebDAV error: {}", err);
        self.error_message = Some(err);
        self.error_time = Some(Instant::now());
    }

    /// The WebDAV channel went away, so there is no share any more.
    /// Unlike `reset_for_session` this leaves the error fields alone:
    /// whatever they hold is the best explanation the panel can offer
    /// for the channel having dropped.
    pub(in crate::app) fn channel_disconnected(&mut self) {
        self.channel_ready = false;
        self.shared_dir = None;
        self.sharing_active = false;
        self.connected_at = None;
    }

    pub(in crate::app) fn show(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // ── WebDAV Folders panel ─────────────────────────
        if self.open {
            // Poll directory picker result
            let mut picked_dir = None;
            if let Some(ref rx) = self.pick_dir_rx {
                if let Ok(result) = rx.try_recv() {
                    picked_dir = Some(result);
                }
            }
            if picked_dir.is_some() {
                self.pick_dir_rx = None;
            }

            let mut webdav_action = None;

            if let Some(Some(path)) = picked_dir {
                if path.is_dir() {
                    webdav_action = Some(WebdavCommand::ShareDirectory {
                        path,
                        read_only: self.pick_dir_readonly,
                    });
                }
            }

            // Auto-clear WebDAV errors after 10 seconds
            if let Some(error_time) = self.error_time {
                if error_time.elapsed() > Duration::from_secs(10) {
                    self.error_message = None;
                    self.error_time = None;
                }
            }

            // Request repaint for elapsed timer and error auto-clear
            if self.connected_at.is_some() || self.error_time.is_some() {
                ctx.request_repaint_after(Duration::from_secs(1));
            }

            egui::Panel::right("webdav_panel")
                .default_size(300.0)
                .show(ui, |ui| {
                    ui.heading("Shared Folders");
                    ui.separator();

                    // Channel status
                    if self.channel_ready {
                        ui.label("Channel: Ready");
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "Channel: Not available");
                    }

                    // Active share with elapsed timer
                    if self.sharing_active {
                        if let Some(ref dir) = self.shared_dir {
                            ui.separator();
                            let elapsed = self
                                .connected_at
                                .map(|t| t.elapsed().as_secs())
                                .unwrap_or(0);
                            let mins = elapsed / 60;
                            let secs = elapsed % 60;
                            let ro = if self.read_only { " [RO]" } else { "" };
                            ui.label(format!("Sharing: {}{} ({}m {}s)", dir, ro, mins, secs));
                            if ui.button("Stop Sharing").clicked() {
                                webdav_action = Some(WebdavCommand::StopSharing);
                            }
                        }
                    }

                    // Error display
                    if let Some(msg) = self.error_message.clone() {
                        ui.separator();
                        ui.colored_label(egui::Color32::RED, msg);
                        ui.horizontal(|ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.error_message = None;
                                self.error_time = None;
                            }
                        });
                    }

                    // Share controls (when not sharing)
                    if !self.sharing_active {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.pick_dir_readonly, "Read-only");
                            let picker_active = self.pick_dir_rx.is_some();
                            let enabled = self.channel_ready && !picker_active;
                            if ui
                                .add_enabled(enabled, egui::Button::new("Share Directory..."))
                                .clicked()
                            {
                                let (tx, rx) = std::sync::mpsc::channel();
                                std::thread::spawn(move || {
                                    let result = rfd::FileDialog::new()
                                        .set_title("Select directory to share")
                                        .pick_folder();
                                    let _ = tx.send(result);
                                });
                                self.pick_dir_rx = Some(rx);
                            }
                        });
                    }
                });

            // Execute WebDAV action outside the closure
            if let Some(cmd) = webdav_action {
                self.error_message = None;
                self.error_time = None;
                if let Some(ref tx) = self.tx {
                    if let Err(e) = tx.try_send(cmd) {
                        self.error_message = Some(format!("Failed to send command: {}", e));
                        self.error_time = Some(Instant::now());
                    }
                }
            }
        }
    }
}
