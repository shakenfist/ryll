use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use shakenfist_spice_renderer::usb::{self, DeviceSource, UsbDeviceInfo};
use shakenfist_spice_renderer::UsbCommand;
use tokio::sync::mpsc;
use tracing::{error, info};

// What the USB panel produces that its caller needs. Frame-scoped: it
// is recomputed from scratch on every draw.
pub(in crate::app) struct UsbPanelOutcome {
    pub(in crate::app) open_bug_report: bool,
}

/// State of the "USB Devices" side panel, and the USB command sender
/// it drives.
///
/// The sender belongs to one session: `new` takes the first one and
/// `reset_for_session` takes every one after it. `reset_for_session`
/// is also the only place that clears the state, so a field added
/// here is added in front of the code that has to decide whether a
/// new session keeps it. What it clears, and what it does not:
///
/// * Cleared for the new session: `channel_ready`, `connecting`,
///   `disconnecting`, `device_description`, `connected_at`,
///   `error_message` and `error_time`. All seven describe a
///   redirection that belonged to the channel that went away — no
///   device is attached to the new guest, no operation is still
///   running, and a stale error about either would outlive what it
///   described.
/// * Installed: `tx`, the new session's sender.
/// * Carried across: `open`, because whether the user wants this panel
///   on screen is not the session's business; `available_devices` with
///   `devices_enumerated`, because the devices plugged into this host
///   are the host's and re-reading them is the Refresh button's job;
///   `virtual_disks`, because the disk images the user added by hand
///   are theirs, and dropping them on a reconnect would make them add
///   every one again; and `add_disk_rx` with `add_disk_readonly` and
///   `add_disk_message`, which belong to a file picker the user may
///   still have open in front of them.
///
/// `channel_disconnected` clears less than `reset_for_session` does,
/// and deliberately: it leaves `error_message` and `error_time` alone
/// so the panel can still say why the channel dropped.
pub(in crate::app) struct UsbPanel {
    open: bool,
    tx: Option<mpsc::Sender<UsbCommand>>,
    channel_ready: bool,
    connecting: bool,
    disconnecting: bool,
    error_message: Option<String>,
    error_time: Option<Instant>,
    device_description: Option<String>,
    connected_at: Option<Instant>,
    available_devices: Vec<UsbDeviceInfo>,
    virtual_disks: Vec<(PathBuf, bool)>,
    devices_enumerated: bool,
    add_disk_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    add_disk_readonly: bool,
    add_disk_message: Option<String>,
}

impl UsbPanel {
    pub(in crate::app) fn new(
        tx: mpsc::Sender<UsbCommand>,
        virtual_disks: Vec<(PathBuf, bool)>,
    ) -> Self {
        Self {
            open: false,
            tx: Some(tx),
            channel_ready: false,
            connecting: false,
            disconnecting: false,
            error_message: None,
            error_time: None,
            device_description: None,
            connected_at: None,
            available_devices: Vec::new(),
            virtual_disks,
            devices_enumerated: false,
            add_disk_rx: None,
            add_disk_readonly: false,
            add_disk_message: None,
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

    /// The device redirected to the guest, if there is one. The status
    /// bar names it beside the rest of the session's state.
    pub(in crate::app) fn device_description(&self) -> Option<&str> {
        self.device_description.as_deref()
    }

    /// The error the panel is showing, if it is showing one. The
    /// caller uses it as the description when the user asks for that
    /// error to be reported as a bug.
    pub(in crate::app) fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// A new session: take its command sender and forget the previous
    /// session's redirection. The struct comment says what survives.
    pub(in crate::app) fn reset_for_session(&mut self, tx: mpsc::Sender<UsbCommand>) {
        self.tx = Some(tx);
        self.channel_ready = false;
        self.connecting = false;
        self.disconnecting = false;
        self.error_message = None;
        self.error_time = None;
        self.device_description = None;
        self.connected_at = None;
    }

    /// The USB redirection channel connected.
    pub(in crate::app) fn channel_connected(&mut self) {
        info!("app: USB redirection channel connected");
        self.channel_ready = true;
    }

    /// A USB device was connected.
    pub(in crate::app) fn device_connected(&mut self, desc: String) {
        info!("app: USB device connected: {}", desc);
        self.device_description = Some(desc);
        self.clear_operation_flags();
        self.connected_at = Some(Instant::now());
    }

    /// A USB device was disconnected.
    pub(in crate::app) fn device_disconnected(&mut self) {
        info!("app: USB device disconnected");
        self.device_description = None;
        self.clear_operation_flags();
        self.connected_at = None;
    }

    /// A USB device connection attempt failed.
    pub(in crate::app) fn connect_failed(&mut self, err: String) {
        error!("app: USB connect failed: {}", err);
        self.clear_operation_flags();
        self.error_message = Some(err);
        self.error_time = Some(Instant::now());
    }

    /// The USB channel went away, so nothing is redirected any more.
    /// Unlike `reset_for_session` this leaves the error fields alone:
    /// whatever they hold is the best explanation the panel can still
    /// offer for the channel having dropped.
    pub(in crate::app) fn channel_disconnected(&mut self) {
        self.channel_ready = false;
        self.device_description = None;
        self.clear_operation_flags();
        self.connected_at = None;
    }

    /// Clear USB operation-in-progress flags.
    fn clear_operation_flags(&mut self) {
        self.connecting = false;
        self.disconnecting = false;
    }

    pub(in crate::app) fn show(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
    ) -> UsbPanelOutcome {
        let mut outcome = UsbPanelOutcome {
            open_bug_report: false,
        };

        // USB device management panel (conditional)
        if self.open {
            self.poll_state(ctx);

            let mut usb_action = None;

            egui::Panel::right("usb_panel")
                .default_size(300.0)
                .show(ui, |ui| {
                    self.draw_header(ui);
                    self.draw_channel_status(ui);
                    self.draw_connected_device(ui);
                    self.draw_error_message(ui, &mut outcome);
                    self.draw_operation_status(ui);

                    ui.separator();

                    usb_action = self.draw_device_list(ui);
                    self.draw_add_disk(ui);
                });

            // Execute USB action outside the closure. Still deferred
            // now that every field the execution touches is in this
            // struct: `Panel::show` holds the closure for as long as
            // the panel is drawn, so the send cannot happen inside it.
            if let Some(cmd) = usb_action {
                self.error_message = None;
                self.error_time = None;
                let is_disconnect = matches!(cmd, UsbCommand::DisconnectDevice);
                if is_disconnect {
                    self.disconnecting = true;
                } else {
                    self.connecting = true;
                }
                if let Some(ref tx) = self.tx {
                    if let Err(e) = tx.try_send(cmd) {
                        self.connecting = false;
                        self.disconnecting = false;
                        self.error_message = Some(format!("Failed to send command: {}", e));
                        self.error_time = Some(Instant::now());
                    }
                }
            }
        }

        outcome
    }

    fn poll_state(&mut self, ctx: &egui::Context) {
        // Auto-enumerate on first open
        if !self.devices_enumerated {
            self.available_devices = usb::enumerate_devices(&self.virtual_disks);
            self.devices_enumerated = true;
        }

        // Poll for file picker result
        let mut picked_path = None;
        if let Some(ref rx) = self.add_disk_rx {
            if let Ok(result) = rx.try_recv() {
                picked_path = Some(result);
            }
        }
        if picked_path.is_some() {
            self.add_disk_rx = None;
        }
        if let Some(Some(path)) = picked_path {
            self.add_disk_message = None;
            match std::fs::metadata(&path) {
                Ok(meta) => {
                    if !meta.is_file() {
                        self.add_disk_message =
                            Some("Selected path is not a regular file.".to_string());
                    } else if meta.len() < 512 {
                        self.add_disk_message =
                            Some("File is too small (< 512 bytes).".to_string());
                    } else {
                        let read_only = self.add_disk_readonly;
                        self.virtual_disks.push((path.clone(), read_only));
                        self.available_devices = usb::enumerate_devices(&self.virtual_disks);
                        let warn = if meta.len() % 512 != 0 {
                            " (warning: size not a multiple of 512)"
                        } else {
                            ""
                        };
                        let ro = if read_only { " [RO]" } else { "" };
                        self.add_disk_message =
                            Some(format!("Added: {}{}{}", path.display(), ro, warn));
                    }
                }
                Err(e) => {
                    self.add_disk_message = Some(format!("Cannot read file: {}", e));
                }
            }
        }

        // Auto-clear USB errors after 10 seconds
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
    }

    fn draw_header(&mut self, ui: &mut egui::Ui) {
        // Header with refresh button
        ui.horizontal(|ui| {
            ui.heading("USB Devices");
            if ui.small_button("Refresh").clicked() {
                self.available_devices = usb::enumerate_devices(&self.virtual_disks);
            }
        });
        ui.separator();
    }

    fn draw_channel_status(&self, ui: &mut egui::Ui) {
        // Channel status
        if self.channel_ready {
            ui.label("Channel: Ready");
        } else {
            ui.colored_label(egui::Color32::GRAY, "Channel: Not available");
        }
    }

    fn draw_connected_device(&self, ui: &mut egui::Ui) {
        // Connected device with elapsed time
        if let Some(ref desc) = self.device_description {
            ui.separator();
            let elapsed = self
                .connected_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let mins = elapsed / 60;
            let secs = elapsed % 60;
            ui.label(format!("Connected: {} ({}m {}s)", desc, mins, secs));
        }
    }

    fn draw_error_message(&mut self, ui: &mut egui::Ui, outcome: &mut UsbPanelOutcome) {
        // Error message with dismiss and bug report buttons
        if let Some(msg) = self.error_message.clone() {
            ui.separator();
            ui.colored_label(egui::Color32::RED, msg);
            ui.horizontal(|ui| {
                if ui.small_button("Dismiss").clicked() {
                    self.error_message = None;
                    self.error_time = None;
                }
                if ui.small_button("Report this as a bug").clicked() {
                    outcome.open_bug_report = true;
                }
            });
        }
    }

    fn draw_operation_status(&self, ui: &mut egui::Ui) {
        // Operation in progress indicator
        if self.connecting {
            ui.separator();
            ui.label("Connecting...");
        } else if self.disconnecting {
            ui.separator();
            ui.label("Disconnecting...");
        }
    }

    /// Draw the device list and return the command its buttons asked
    /// for. One frame can only carry one click, so returning the
    /// command keeps the last-write-wins behaviour this had while it
    /// wrote through an out-param.
    fn draw_device_list(&self, ui: &mut egui::Ui) -> Option<UsbCommand> {
        let mut usb_action = None;

        // Device list with connect/disconnect buttons
        let buttons_disabled = !self.channel_ready || self.connecting || self.disconnecting;
        egui::ScrollArea::vertical().show(ui, |ui| {
            if self.available_devices.is_empty() {
                ui.colored_label(egui::Color32::GRAY, "No USB devices found.");
            } else {
                for device in &self.available_devices {
                    let label = device.label();
                    let is_connected = self
                        .device_description
                        .as_ref()
                        .is_some_and(|d| *d == label);

                    ui.horizontal(|ui| {
                        if is_connected {
                            ui.colored_label(egui::Color32::GREEN, "\u{25CF}");
                            ui.label(&label);
                            if ui
                                .add_enabled(!buttons_disabled, egui::Button::new("Disconnect"))
                                .clicked()
                            {
                                usb_action = Some(UsbCommand::DisconnectDevice);
                            }
                        } else {
                            ui.label(&label);
                            let connect_enabled =
                                !buttons_disabled && self.device_description.is_none();
                            if ui
                                .add_enabled(connect_enabled, egui::Button::new("Connect"))
                                .clicked()
                            {
                                usb_action = Some(match &device.source {
                                    #[cfg(target_os = "linux")]
                                    DeviceSource::Physical { bus, address } => {
                                        UsbCommand::ConnectPhysical {
                                            bus: *bus,
                                            address: *address,
                                        }
                                    }
                                    DeviceSource::VirtualDisk { path, read_only } => {
                                        UsbCommand::ConnectVirtualDisk {
                                            path: path.clone(),
                                            read_only: *read_only,
                                        }
                                    }
                                });
                            }
                        }
                    });
                }
            }
        });

        usb_action
    }

    fn draw_add_disk(&mut self, ui: &mut egui::Ui) {
        // Add virtual disk section
        ui.separator();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.add_disk_readonly, "Read-only");
            let picker_active = self.add_disk_rx.is_some();
            if ui
                .add_enabled(!picker_active, egui::Button::new("Add Disk..."))
                .clicked()
            {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let result = rfd::FileDialog::new()
                        .set_title("Select RAW disk image")
                        .add_filter("Disk images", &["raw", "img"])
                        .add_filter("All files", &["*"])
                        .pick_file();
                    let _ = tx.send(result);
                });
                self.add_disk_rx = Some(rx);
            }
        });

        // Add-disk message
        if let Some(ref msg) = self.add_disk_message {
            if msg.starts_with("Added:") {
                ui.label(msg);
            } else {
                ui.colored_label(egui::Color32::RED, msg);
            }
        }
    }
}
