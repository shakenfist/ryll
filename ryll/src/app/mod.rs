// Main application - egui App.
//
// This holds the `RyllApp` struct and its fields, its construction,
// and the `eframe::App` impl: `logic`, which keeps the session
// ticking whether or not the window is drawn, and `ui`, which is the
// sequence of calls that draws one frame. The drawing itself lives
// in `app/ui/`.

use eframe::egui;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::{error, info};

use crate::bugreport::{
    AppSnapshot, BugReport, BugReportType, ChannelSnapshots, PedanticConfig, TrafficBuffers,
    TrafficViewEntry,
};
use crate::capture::CaptureSession;
use crate::config::{Config, ShareDirConfig, VirtualDiskConfig};
use crate::display_gui::TextureCache;
use crate::input_egui::{HeldKeys, WheelAccumulator};
use crate::notifications::{
    register_gap_notification_observer, NotificationEntry, NotificationSource, NotificationStore,
    SharedNotifications,
};
use crate::reconnect_policy::MAX_RECONNECT_ATTEMPTS;
use crate::settings;
use shakenfist_spice_protocol::link::H264Offer;
use shakenfist_spice_protocol::NotifySeverity;
use shakenfist_spice_renderer::channels::{DisplaySize, VolumeControl};
use shakenfist_spice_renderer::usb::UsbDeviceInfo;
use shakenfist_spice_renderer::{
    ChannelEvent, ClipboardBackend, CursorImage, InputEvent, PointerTranslator, SessionState,
    SessionStateRx, SurfaceMirror, UsbCommand, WebdavCommand, MOUSE_MODE_UNKNOWN,
};

use crate::clipboard_arboard::ArboardClipboard;

mod agent_state;
mod bug_report;
mod connection;
mod events;
mod notification_snapshots;
mod reconnect;
mod resize;
mod screenshots;
mod session_init;
mod stats;
mod trigger_snapshot;
mod ui;
mod ui_state;

use agent_state::AgentState;
use bug_report::{PendingBugReport, PendingDisconnectReport};
use connection::build_connection_runtime;
use notification_snapshots::NotificationSnapshotStore;
use reconnect::{forbid_retry_modal, ReconnectState};
use resize::{resolution_notification_due, RESOLUTION_NOTIFY_DEBOUNCE};
use stats::{BandwidthTracker, LatencyTracker, Statistics};
use trigger_snapshot::{RegionSelectEnd, TriggerSnapshot, WindowShot};
use ui_state::UiStateTracker;

/// Channel buffer sizes
const EVENT_CHANNEL_SIZE: usize = 1024;
const INPUT_CHANNEL_SIZE: usize = 256;

// `ByteCounter` lives in the renderer crate; the app just
// re-exports it for backwards-compatible imports inside ryll.
pub use shakenfist_spice_renderer::ByteCounter;

/// The egui application
pub struct RyllApp {
    // Communication channels
    event_rx: mpsc::Receiver<ChannelEvent>,
    /// Mouse mode and agent state, as the current session's main
    /// channel publishes them. Replaced on every reconnect, together
    /// with `event_rx`; read once per frame by `sync_session_state`.
    session_state: SessionStateRx,
    input_tx: Option<mpsc::Sender<InputEvent>>,
    resize_tx: Option<Arc<mpsc::Sender<DisplaySize>>>,
    last_sent_resize: Option<(u32, u32)>,
    /// A size the window auto-fitted to, still to be told to the main
    /// channel as `DisplaySize::GuestHas` because the resize queue was
    /// full. Retried every frame until it goes, or until a requested
    /// size supersedes it. Dropping it would leave the main channel
    /// holding an older requested size, which it would send when the
    /// agent connects, resizing the guest away from the size the
    /// window has just fitted to.
    pending_guest_size: Option<(u32, u32)>,
    volume_control: Arc<VolumeControl>,

    // Display state: the surfaces, and the egui textures drawn from them
    surfaces: SurfaceMirror,
    textures: TextureCache,

    // Cursor state
    cursor_pos: (u16, u16),
    cursor_visible: bool,
    cursor_image: Option<CursorImage>,
    cursor_texture: Option<egui::TextureHandle>,

    // Screen-space rect of the rendered SPICE surface
    surface_rect: egui::Rect,

    // Statistics
    stats: Statistics,

    // Cadence mode
    cadence_enabled: bool,
    last_cadence_key: Instant,

    // Session state
    connected: bool,
    error_message: Option<String>,
    /// The mouse mode this app is acting on, copied from
    /// `session_state` each frame. Kept as a field so a bug report
    /// records the app's view beside the server's.
    mouse_mode: u32,
    /// Auto-reconnect state machine; supplants the old
    /// `show_disconnect_dialog` + `disconnect_reason` pair.
    reconnect_state: ReconnectState,
    /// `true` once a `Pending` tick has actually called
    /// `reconnect()` and is awaiting the new connection's
    /// outcome. Distinguishes "we just kicked off attempt N"
    /// from "we're seeing yet another channel-storm event for
    /// the disconnect that put us into Pending."
    awaiting_reconnect_outcome: bool,
    /// Wall time the most recent `Modal` was entered; used to
    /// gate the cluster-reset window. Cleared on manual
    /// reconnect from the modal (user intervention starts a
    /// fresh budget).
    last_modal_at: Option<Instant>,
    /// When the most recent main-channel server `NOTIFY` arrived, and
    /// its text: the reason a `DISCONNECTING` right after it gives in
    /// the `ServerEnded` modal.
    last_main_notice: Option<(Instant, String)>,
    /// Count of `Pending` entries (per-attempt) over the
    /// session's lifetime, surfaced in `session.json` for
    /// post-hoc analysis of how rocky the session was.
    auto_reconnect_count: u32,
    /// Has the T-30s `ticket-valid-until` pre-expiry warning
    /// been pushed for the current session? Latches to `true`
    /// once fired so the notification panel does not see a
    /// stream of duplicates as the deadline approaches.
    ticket_expiry_warned: bool,

    // Last mouse position sent (to avoid flooding with duplicates,
    // and to derive server-mode deltas)
    pointer: PointerTranslator,
    last_modifiers: Option<egui::Modifiers>,

    // Keys forwarded to the inputs channel as pressed. Used to tell
    // auto-repeats from fresh presses and to send releases when input
    // forwarding is suppressed or the window loses focus.
    held_keys: HeldKeys,

    // Which of ryll's own panels, menus and dialogs are open, and when
    // they opened, for bug reports.
    ui_state: UiStateTracker,

    // A screenshot was asked for (F8 or the menu) and its save dialog
    // has not been opened yet. The dialog is opened after the frame's
    // input has been forwarded; see `open_screenshot_dialog`.
    screenshot_requested: bool,

    // Bitmask of mouse buttons we have forwarded as pressed to the
    // inputs channel.  Used to send synthetic releases when input
    // forwarding is suppressed (e.g. bug report dialog opens).
    forwarded_buttons: u32,

    // Wheel scroll not yet sent to the guest as whole clicks.
    wheel: WheelAccumulator,

    // Pending viewport resize from a new primary-surface event.
    // Only set for (display_channel_id, surface_id) == (0, 0).
    pending_resize: Option<(f32, f32)>,

    // (8-aligned width, height) of the last auto-resize we
    // issued. None until the first resize. Used to dedup so we
    // don't re-issue ViewportCommand::InnerSize every frame
    // while the window already matches the surface, and so
    // reconnect can re-fit by clearing it.
    last_auto_resize: Option<(u32, u32)>,

    // User-controlled opt-out of the always-fit behaviour.
    // True (default) => auto-fit the window to every primary
    // SurfaceCreated. False => leave the window alone; the
    // surface renders at native pixel size inside whatever
    // window the user has chosen (may overflow or letterbox).
    // Toggled live via the hamburger menu; initial value comes
    // from `--no-obey-guest-size` (inverted).
    obey_guest_size: bool,

    // Resolution-change notification state. The debounce
    // coalesces a burst of guest-side mode changes (boot
    // probes, drag-resize storms) into a single user-visible
    // notification per quiescent window so the panel does
    // not spam.
    //
    // pending_resolution_notify holds the latest (w, h) we
    // have seen on a primary SurfaceCreated paired with the
    // timestamp of that observation. The two are always set
    // and cleared together, so collapsing them into one
    // Option removes the representable-but-invalid state
    // where one is Some and the other is None. RyllApp::update
    // emits when the queued-at timestamp is at least
    // RESOLUTION_NOTIFY_DEBOUNCE old and the value differs
    // from last_notified_resolution. last_notified_resolution
    // suppresses re-emitting the same value (e.g. when the
    // guest re-confirms an existing mode after a fullscreen
    // toggle).
    pending_resolution_notify: Option<((u32, u32), Instant)>,
    last_notified_resolution: Option<(u32, u32)>,

    // Bandwidth tracking for the status bar sparkline
    bandwidth: BandwidthTracker,

    // Server PING interval, for the status bar sparkline
    ping_interval: LatencyTracker,

    // Inputs-channel round trip, for the status bar
    input_rtt: LatencyTracker,

    // Capture session (None when --capture is not specified)
    capture: Option<Arc<CaptureSession>>,

    // Override for bug-report output directory (--bug-report-dir).
    // None means fall back to capture/cwd; see manual_bug_report_dir().
    bug_report_dir: Option<PathBuf>,

    // Cooldown for auto-disconnect snapshots so a flapping
    // channel can't dump one zip per disconnect storm. 60 s
    // window; see maybe_write_disconnect_snapshot().
    last_disconnect_report_at: Option<Instant>,
    /// How and when each channel of this session ended, by channel
    /// name, for the next disconnect report. Cleared on reconnect.
    channel_ends: BTreeMap<String, crate::bugreport::ChannelEnd>,
    /// A disconnect report asked for while draining events. It is
    /// written once the drain is done, so that it sees every channel
    /// that ended in the same frame, not just the first.
    pending_disconnect_report: Option<PendingDisconnectReport>,

    // USB command sender and state
    usb_tx: Option<mpsc::Sender<UsbCommand>>,
    usb_channel_ready: bool,
    usb_connecting: bool,
    usb_disconnecting: bool,
    usb_error_message: Option<String>,
    usb_error_time: Option<Instant>,
    usb_device_description: Option<String>,
    usb_connected_at: Option<Instant>,

    // Traffic ring buffers (always active, for bug reports and traffic viewer)
    traffic: Arc<TrafficBuffers>,

    // In-app notification store (shared with all channels and producers).
    notifications: SharedNotifications,

    // Bounded snapshot store keyed by notification id. Captured on
    // every push_notification call so the "File bug report" button
    // on a notification entry can produce a report with pcap/state
    // from the moment the notification fired. Wrapped in Mutex for
    // the same multi-producer reasons SharedNotifications is.
    notification_snapshots: std::sync::Mutex<NotificationSnapshotStore>,

    // Channel state snapshots (always active, for bug reports)
    channel_snapshots: ChannelSnapshots,
    app_snapshot: Arc<std::sync::Mutex<AppSnapshot>>,

    // Count of display frames dropped because the encoder task's
    // queue was full at CaptureSession::frame call time. Mirrored
    // into AppSnapshot::video_drop_count by update_app_snapshot().
    // Stays zero unless --capture is active. See
    // `docs/plans/PLAN-video-keeping-up.md`.
    video_drop_count: u64,

    // Bounded rings of mpsc-queue lag samples (μs) for
    // renderer-to-app events. Mirrored into AppSnapshot by
    // update_app_snapshot() as min/max/mean aggregates. See
    // `docs/plans/PLAN-video-keeping-up.md`.
    recent_image_ready_lag_us: VecDeque<u32>,
    recent_display_mark_lag_us: VecDeque<u32>,

    // Connection target for bug report metadata
    target: String,

    // Bug report dialog state
    show_bug_dialog: bool,
    bug_report_type: BugReportType,
    bug_description: String,

    // Snapshot of the display surface captured the moment a
    // bug-report dialog opens. Encoding runs in a background
    // thread; `take_trigger_for_submit` consumes both the
    // timestamps and (if ready) the PNG bytes at submit time.
    // `discard_trigger_snapshot` drops it on cancel.
    pending_trigger: Option<TriggerSnapshot>,

    // The window screenshot asked for when the pending trigger began.
    // `discard_trigger_snapshot` drops it with the trigger.
    window_shot: Option<WindowShot>,

    // A GUI-submitted bug report whose runtime-metrics sample is
    // still running on a worker thread; `poll_pending_bug_report`
    // writes the zip once it lands. Doubles as the re-entrancy
    // guard — `finish_bug_report` refuses a second submission
    // while this is `Some`.
    pending_bug_report: Option<PendingBugReport>,

    // Region selection state (Display bug reports)
    region_select_active: bool,
    region_drag_start: Option<(u32, u32)>,
    region_drag_end: Option<(u32, u32)>,

    // USB panel state
    show_usb_panel: bool,
    usb_available_devices: Vec<UsbDeviceInfo>,
    usb_virtual_disks: Vec<(PathBuf, bool)>,
    usb_devices_enumerated: bool,

    // File picker for adding virtual disks
    usb_add_disk_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    usb_add_disk_readonly: bool,
    usb_add_disk_message: Option<String>,

    // WebDAV panel state
    show_webdav_panel: bool,
    webdav_tx: Option<mpsc::Sender<WebdavCommand>>,
    webdav_channel_ready: bool,
    webdav_shared_dir: Option<String>,
    webdav_read_only: bool,
    webdav_sharing_active: bool,
    webdav_connected_at: Option<Instant>,
    webdav_error_message: Option<String>,
    webdav_error_time: Option<Instant>,
    webdav_pick_dir_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    webdav_pick_dir_readonly: bool,

    // Traffic viewer state
    show_traffic_viewer: bool,
    traffic_viewer_entries: Vec<TrafficViewEntry>,
    traffic_viewer_last_refresh: Instant,
    traffic_viewer_paused: bool,
    traffic_filter_main: bool,
    traffic_filter_display: bool,
    traffic_filter_inputs: bool,
    traffic_filter_cursor: bool,
    traffic_filter_usbredir: bool,
    traffic_filter_webdav: bool,
    traffic_filter_playback: bool,

    /// Whether the "Protocol gaps" floating window is currently open.
    gaps_popup_open: bool,

    // Notifications panel state
    show_notifications_panel: bool,
    notifications_panel_was_open_last_frame: bool,

    /// Whether paste-as-keystrokes is enabled.
    enable_paste: bool,

    /// Inter-character delay for paste-as-keystrokes in milliseconds.
    paste_char_delay_ms: u32,

    /// The guest agent as this session has seen it. A connected agent
    /// disables paste-as-keystrokes in favour of the clipboard path.
    agent_state: AgentState,

    /// Whether this session has already explained why Cmd+V does not
    /// paste without an agent. Reset on reconnect.
    paste_hint_shown: bool,

    /// `MainSnapshot::events_dropped_count` as of the last frame that
    /// looked, so a rise can be told apart from a count already
    /// reported. Reset to zero with the snapshots on reconnect.
    last_seen_events_dropped: u64,

    /// Cached clipboard instance for reading host clipboard.
    cached_clipboard: Option<arboard::Clipboard>,

    /// Error message for the paste error dialog (None = hidden).
    paste_error_message: Option<String>,

    // Reconnection state
    config: Config,
    monitors: u8,
    reconnect_virtual_disks: Vec<VirtualDiskConfig>,
    reconnect_share_dir: Option<ShareDirConfig>,
    egui_ctx: egui::Context,

    /// Per-connection cancel flag. `reconnect()` sets the previous
    /// attempt's flag before spawning the next, so a stale
    /// `run_connection` sees the flag in its 100 ms poll branch and
    /// exits cleanly, dropping its tokio runtime when the thread
    /// returns. Mirrors the cooperative-cancel shape of the global
    /// `SHUTDOWN_REQUESTED` flag, scoped per attempt.
    connection_cancel: Option<Arc<AtomicBool>>,

    /// Where the current attempt's connection thread reports an
    /// error from `run_connection`. That function returns an error
    /// only when it could not set a session up (after setup, channel
    /// failures arrive as `ChannelEvent::Error`), and nothing else
    /// tells the UI about it. Replaced per attempt like `event_rx`,
    /// so a superseded attempt's failure goes nowhere.
    connection_failure_rx: Option<oneshot::Receiver<String>>,
    /// A failure taken from `connection_failure_rx` but not yet
    /// acted on, because `event_rx` still held events the attempt
    /// sent before it failed. See `process_events`.
    pending_connection_failure: Option<String>,

    /// True while ryll's window is focused. Updated on every `update()`
    /// call from `ctx.input(|i| i.focused)`. Read by the
    /// `FocusGatedClipboard` decorator so the host pasteboard is only
    /// polled while the user is looking at ryll, following the
    /// spawn_blocking fix in commit 54155e99.
    app_focused: Arc<AtomicBool>,

    /// Persisted copy of the `--debug-single-thread-runtime`
    /// flag so reconnect can build a runtime of the same
    /// shape as the initial connect. Diagnostic-only.
    debug_single_thread_runtime: bool,

    /// Persisted copy of how the session offers H.264
    /// (`--experimental-h264`, `--no-h264`) so reconnect advertises
    /// the same display capabilities as the initial connect.
    h264: H264Offer,

    /// Byte cap for the display-channel image cache, converted
    /// from `--image-cache-cap-mib` at startup. Persisted here
    /// so reconnect passes the same value each time.
    image_cache_cap_bytes: usize,

    /// Byte cap for the shared GLZ dictionary, converted from
    /// `--glz-dictionary-cap-mib` at startup. Persisted here so
    /// reconnect passes the same value each time.
    glz_dictionary_cap_bytes: usize,

    /// `SPICE_IMAGE_COMPRESSION_*` wire value from
    /// `--preferred-compression`, sent at each display channel
    /// link-up, including on reconnect.
    preferred_compression: u8,

    // Auto-snapshot config. `None` means the mode is disabled. A fresh
    // task is spawned per session: on each `SessionInitialized` the
    // previous task (if any) is signalled via `auto_snapshot_cancel` to
    // retire, then a new task is spawned with fresh `traffic` /
    // `channel_snapshots` Arcs. The retire-and-respawn pattern is needed
    // because `reconnect()` replaces those Arcs wholesale; a long-lived
    // task would capture clones of the old Arcs and write zips full of
    // stale / empty data after the first reconnect.
    auto_snapshot_interval: Option<u64>,
    auto_snapshot_cap: usize,
    /// Cancel handle for the currently-running auto-snapshot task,
    /// if any. `None` before the first spawn and between retire
    /// and respawn. Setting to `true` causes the loop to exit at
    /// the next 500 ms poll.
    auto_snapshot_cancel: Option<Arc<AtomicBool>>,
    /// Join handle for the auto-snapshot thread currently running,
    /// if any. Retained so a respawn can move it to
    /// `auto_snapshot_retiring` and account for it there.
    auto_snapshot_thread: Option<std::thread::JoinHandle<()>>,
    /// Threads asked to retire that have not been observed exiting.
    ///
    /// A retiring thread is never joined: every caller runs on the
    /// eframe UI thread, and an outgoing loop that is already inside
    /// `spawn_blocking` does not see the cancel flag until its
    /// `BugReport::new` (documented as ~2 s of metric sampling) and
    /// zip write finish — so a join would freeze the GUI for seconds
    /// under exactly the trigger this bookkeeping exists to bound.
    /// They are reaped non-blockingly with `is_finished` instead, and
    /// [`connection::MAX_RETIRING_AUTO_SNAPSHOT_THREADS`] bounds how
    /// many may be outstanding, which is what actually bounds thread
    /// growth. It admits one more than its own value; see #716.
    auto_snapshot_retiring: Vec<std::thread::JoinHandle<()>>,
    /// When the most recent `SessionInitialized` was *accepted*.
    /// Drives [`session_init::MIN_SESSION_RESPAWN_INTERVAL`]; see the
    /// guard in `process_events`.
    last_session_initialized_at: Option<Instant>,
    /// Whether the one-shot "Auto-snapshot mode enabled" startup
    /// notification has been pushed. Latched on the first spawn
    /// so per-reconnect respawns don't repeat the notification.
    auto_snapshot_startup_notified: bool,

    /// Session id from the most recent `SessionInitialized`, or
    /// `None` before the first session. Lets `process_events` tell a
    /// repeated `SPICE_MSG_MAIN_INIT` for the session we already hold
    /// (a duplicate, to be dropped) from a genuinely new one.
    current_session_id: Option<u32>,

    /// Most recent time the streaming-flap notification fired. Used by
    /// `streaming_state::classify` to enforce the 60 s cool-down between
    /// repeat warnings. `None` until the first fire of the session; never
    /// cleared (reconnect resets the snapshot data the heuristic reads
    /// from, so a stale timestamp only suppresses the next legitimate
    /// flap during its remaining cool-down — acceptable for v1).
    last_flap_notification_ts: Option<Instant>,
}

impl RyllApp {
    #[allow(clippy::too_many_arguments)]
    #[expect(
        clippy::too_many_lines,
        reason = "the GUI's construction, where every long-lived handle is made \
         and cloned: the event, input, usb, webdav and resize channels, \
         the snapshot and notification stores, the repaint bridge and the \
         connection task, and then one struct literal naming every field. \
         The clones have to precede the moves, and splitting the literal \
         from the setup would mean a builder for a type built once"
    )]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        cadence: bool,
        enable_paste: bool,
        paste_char_delay_ms: u32,
        virtual_disks: Vec<VirtualDiskConfig>,
        share_dir: Option<ShareDirConfig>,
        capture: Option<Arc<CaptureSession>>,
        monitors: u8,
        pedantic_config: Option<PedanticConfig>,
        bug_report_dir: Option<PathBuf>,
        obey_guest_size: bool,
        debug_single_thread_runtime: bool,
        auto_snapshot_interval: Option<u64>,
        auto_snapshot_cap: Option<usize>,
        h264: H264Offer,
        image_cache_cap_bytes: usize,
        glz_dictionary_cap_bytes: usize,
        preferred_compression: u8,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let session_state = SessionState::new();
        let session_state_rx = session_state.subscribe();
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);
        let (usb_tx, usb_rx) = mpsc::channel(16);
        let (webdav_tx, webdav_rx) = mpsc::channel(16);
        let (resize_tx, resize_rx) = mpsc::channel(32);
        let resize_tx = Arc::new(resize_tx);
        let volume_control = VolumeControl::new();

        let byte_counter = Arc::new(ByteCounter::new());
        let traffic = Arc::new(TrafficBuffers::new());

        // In-app notification store (always active; all channels push,
        // GUI bell + side panel consume).
        let notifications: SharedNotifications =
            Arc::new(std::sync::Mutex::new(NotificationStore::new()));

        // Channel state snapshots (always active)
        let channel_snapshots = ChannelSnapshots::new();
        let app_snapshot = Arc::new(std::sync::Mutex::new(AppSnapshot::default()));
        // Built here so it is available for `target`, which feeds
        // bug-report metadata and the "Connected to" bell entry: under a
        // proxy tunnel `config.host` is a signed pseudo-hostname, and
        // `display_target()` is what keeps it out of both.
        let connection_config: shakenfist_spice_protocol::ConnectionConfig = (&config).into();
        let target = connection_config.display_target();

        // Register the --pedantic gap observer now that the live traffic,
        // channel-snapshot, and app-snapshot handles exist. The underlying
        // register_gap_observer has replay semantics, so any gaps fired
        // during the construction window before this call are delivered
        // when we register.
        if let Some(config) = pedantic_config {
            BugReport::register_pedantic_observer(
                config,
                target.clone(),
                traffic.clone(),
                channel_snapshots.clone(),
                app_snapshot.clone(),
                notifications.clone(),
            );
        }
        register_gap_notification_observer(notifications.clone());

        // Retain virtual disk paths for UI re-enumeration
        let usb_virtual_disks: Vec<(PathBuf, bool)> = virtual_disks
            .iter()
            .map(|d| (d.path.clone(), d.read_only))
            .collect();

        // Repaint when channel events arrive; 1s fallback for time-based UI.
        // The bridge task waits on the Arc<Notify> and pings egui any time a
        // channel handler signals it.  Every channel event goes out through
        // `EventSink::emit`, which signals as part of sending, so egui sleeps
        // when nothing is happening and wakes immediately when something is.
        let repaint_notify = Arc::new(Notify::new());
        let event_tx_clone = event_tx.clone();
        let resize_rx_for_conn = resize_rx;
        let ctx = cc.egui_ctx.clone();
        let bridge_ctx = cc.egui_ctx.clone();
        let bridge_notify = repaint_notify.clone();
        let conn_notify = repaint_notify.clone();
        let capture_clone: Option<Arc<dyn shakenfist_spice_renderer::CaptureSink>> = capture
            .clone()
            .map(|c| c as Arc<dyn shakenfist_spice_renderer::CaptureSink>);
        let counter_clone = byte_counter.clone();
        let traffic_clone: Arc<dyn shakenfist_spice_renderer::TrafficSink> =
            traffic.clone() as Arc<dyn shakenfist_spice_renderer::TrafficSink>;
        let log_config_clone = settings::log_config();
        let snaps_for_conn = channel_snapshots.clone();

        let vol_for_conn = volume_control.clone();
        let vd_clone = virtual_disks.clone();
        let sd_clone = share_dir.clone();
        let connection_cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_conn = connection_cancel.clone();
        // Initialise to true so a session connecting at startup
        // (before egui has fired its first focus event) polls the
        // clipboard normally. RyllApp::update overwrites this on
        // every frame.
        let app_focused = Arc::new(AtomicBool::new(true));
        let focused_for_conn = app_focused.clone();
        let single_thread_for_conn = debug_single_thread_runtime;
        let (failure_tx, failure_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let runtime = build_connection_runtime(single_thread_for_conn);
            runtime.block_on(async {
                // Repaint bridge: wake egui whenever a channel handler
                // signals notify_one() after pushing a ChannelEvent.
                tokio::spawn(async move {
                    loop {
                        bridge_notify.notified().await;
                        bridge_ctx.request_repaint();
                    }
                });

                let clipboard: Option<Arc<dyn ClipboardBackend>> = Some(Arc::new(
                    crate::clipboard_arboard::FocusGatedClipboard::new(
                        Arc::new(ArboardClipboard::new()),
                        focused_for_conn,
                    ),
                ));
                if let Err(e) = shakenfist_spice_renderer::run_connection(
                    connection_config,
                    event_tx_clone,
                    session_state,
                    conn_notify,
                    input_rx,
                    usb_rx,
                    webdav_rx,
                    vd_clone,
                    sd_clone,
                    capture_clone,
                    counter_clone,
                    traffic_clone,
                    snaps_for_conn,
                    monitors,
                    resize_rx_for_conn,
                    vol_for_conn,
                    enable_paste,
                    log_config_clone,
                    cancel_for_conn,
                    clipboard,
                    shakenfist_spice_renderer::PlaybackOutput::Local,
                    h264,
                    image_cache_cap_bytes,
                    glz_dictionary_cap_bytes,
                    preferred_compression,
                )
                .await
                {
                    // `{:#}`: the dial and TLS errors carry their
                    // cause as anyhow context, which `{}` drops.
                    let message = format!("{e:#}");
                    error!("app: connection error: {}", message);
                    // The receiver is gone if a newer attempt has
                    // superseded this one, which is fine.
                    let _ = failure_tx.send(message);
                }
            });
            ctx.request_repaint();
        });

        RyllApp {
            event_rx,
            session_state: session_state_rx,
            input_tx: Some(input_tx),
            resize_tx: Some(resize_tx),
            last_sent_resize: None,
            pending_guest_size: None,
            volume_control,
            surfaces: SurfaceMirror::new(),
            textures: TextureCache::new(),
            cursor_pos: (0, 0),
            cursor_visible: true,
            cursor_image: None,
            cursor_texture: None,
            surface_rect: egui::Rect::NOTHING,
            stats: Statistics::default(),
            cadence_enabled: cadence,
            last_cadence_key: Instant::now(),
            connected: false,
            error_message: None,
            mouse_mode: MOUSE_MODE_UNKNOWN,
            reconnect_state: ReconnectState::Idle,
            awaiting_reconnect_outcome: false,
            last_modal_at: None,
            last_main_notice: None,
            auto_reconnect_count: 0,
            ticket_expiry_warned: false,
            pointer: PointerTranslator::new(),
            last_modifiers: None,
            held_keys: HeldKeys::new(traffic.session_start()),
            ui_state: UiStateTracker::new(traffic.session_start()),
            screenshot_requested: false,
            forwarded_buttons: 0,
            wheel: WheelAccumulator::default(),
            pending_resize: None,
            last_auto_resize: None,
            obey_guest_size,
            pending_resolution_notify: None,
            last_notified_resolution: None,
            bandwidth: BandwidthTracker::new(byte_counter),
            ping_interval: LatencyTracker::new(),
            input_rtt: LatencyTracker::new(),
            capture,
            bug_report_dir,
            last_disconnect_report_at: None,
            channel_ends: BTreeMap::new(),
            pending_disconnect_report: None,
            usb_tx: Some(usb_tx),
            webdav_tx: Some(webdav_tx),
            usb_channel_ready: false,
            usb_connecting: false,
            usb_disconnecting: false,
            usb_error_message: None,
            usb_error_time: None,
            usb_device_description: None,
            usb_connected_at: None,
            traffic,
            notifications,
            notification_snapshots: std::sync::Mutex::new(NotificationSnapshotStore::new()),
            channel_snapshots,
            app_snapshot,
            video_drop_count: 0,
            recent_image_ready_lag_us: VecDeque::new(),
            recent_display_mark_lag_us: VecDeque::new(),
            target,
            show_bug_dialog: false,
            bug_report_type: BugReportType::Display,
            bug_description: String::new(),
            pending_trigger: None,
            window_shot: None,
            pending_bug_report: None,
            region_select_active: false,
            region_drag_start: None,
            region_drag_end: None,
            show_usb_panel: false,
            usb_available_devices: Vec::new(),
            usb_virtual_disks,
            usb_devices_enumerated: false,
            usb_add_disk_rx: None,
            usb_add_disk_readonly: false,
            usb_add_disk_message: None,
            show_webdav_panel: false,
            webdav_channel_ready: false,
            webdav_shared_dir: None,
            webdav_read_only: false,
            webdav_sharing_active: false,
            webdav_connected_at: None,
            webdav_error_message: None,
            webdav_error_time: None,
            webdav_pick_dir_rx: None,
            webdav_pick_dir_readonly: false,
            show_traffic_viewer: false,
            traffic_viewer_entries: Vec::new(),
            traffic_viewer_last_refresh: Instant::now(),
            traffic_viewer_paused: false,
            traffic_filter_main: true,
            traffic_filter_display: true,
            traffic_filter_inputs: true,
            traffic_filter_cursor: true,
            traffic_filter_usbredir: true,
            traffic_filter_webdav: true,
            traffic_filter_playback: true,
            gaps_popup_open: false,
            show_notifications_panel: false,
            notifications_panel_was_open_last_frame: false,
            enable_paste,
            paste_char_delay_ms,
            agent_state: AgentState::Unknown,
            paste_hint_shown: false,
            last_seen_events_dropped: 0,
            cached_clipboard: None,
            paste_error_message: None,
            config,
            monitors,
            reconnect_virtual_disks: virtual_disks,
            reconnect_share_dir: share_dir,
            egui_ctx: cc.egui_ctx.clone(),
            connection_cancel: Some(connection_cancel),
            connection_failure_rx: Some(failure_rx),
            pending_connection_failure: None,
            app_focused,
            debug_single_thread_runtime,
            h264,
            image_cache_cap_bytes,
            glz_dictionary_cap_bytes,
            preferred_compression,
            auto_snapshot_interval,
            auto_snapshot_cap: auto_snapshot_cap
                .unwrap_or(crate::auto_snapshot::DEFAULT_AUTO_SNAPSHOT_CAP),
            auto_snapshot_cancel: None,
            auto_snapshot_thread: None,
            auto_snapshot_retiring: Vec::new(),
            last_session_initialized_at: None,
            auto_snapshot_startup_notified: false,
            current_session_id: None,
            last_flap_notification_ts: None,
        }
    }

    /// Get or create the cached clipboard instance.
    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.cached_clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => self.cached_clipboard = Some(cb),
                Err(e) => {
                    tracing::warn!("app: failed to open clipboard: {}", e);
                    return None;
                }
            }
        }
        self.cached_clipboard.as_mut()
    }

    /// Attempt to paste the host clipboard as keystrokes.
    /// Returns true if a paste was triggered (or an error
    /// dialog was shown), false if there was nothing to do.
    fn trigger_paste(&mut self) -> bool {
        use shakenfist_spice_renderer::channels::{translate_paste, PasteError};

        if !self.enable_paste || self.agent_state.connected() {
            return false;
        }

        // Read clipboard
        let text = match self.clipboard() {
            Some(cb) => match cb.get_text() {
                Ok(t) if !t.is_empty() => t,
                Ok(_) => return false, // empty clipboard
                Err(e) => {
                    self.paste_error_message = Some(format!("Failed to read clipboard: {}", e));
                    return true;
                }
            },
            None => {
                self.paste_error_message = Some("No clipboard available".to_string());
                return true;
            }
        };

        // Pre-validate with the translator
        match translate_paste(&text) {
            Ok(_) => {
                // Translation will succeed — send to the inputs channel.
                if let Some(tx) = &self.input_tx {
                    let _ = tx.try_send(InputEvent::PasteText {
                        text,
                        char_delay_ms: self.paste_char_delay_ms,
                        request_id: None, // GUI path: no correlation token
                        cancel: None,     // GUI path: no cancellation token
                    });
                }
            }
            Err(PasteError::Unrepresentable { count, sample }) => {
                let sample_str: String = sample
                    .iter()
                    .map(|c| format!("U+{:04X}", *c as u32))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.paste_error_message = Some(format!(
                    "The clipboard contains {} character(s) that have no \
                     US-QWERTY scancode mapping: {}",
                    count, sample_str,
                ));
                return true;
            }
        }

        true
    }

    /// Push a notification entry into the shared store. After the
    /// push lands, also capture a `TrafficBuffers` snapshot keyed by
    /// the entry's id, so the "File bug report" button on the
    /// notification can later produce a report with pcap/state from
    /// the moment the notification fired.
    fn push_notification(
        &self,
        severity: NotifySeverity,
        source: NotificationSource,
        message: impl Into<String>,
    ) {
        let entry = NotificationEntry::new(severity, source, message);
        let id = match self.notifications.lock() {
            Ok(mut guard) => Some(guard.push(entry)),
            Err(_) => None,
        };
        if let Some(id) = id {
            // Capture under a separate lock so the
            // notifications mutex is already released — the
            // snapshot() call locks each per-channel ring
            // briefly, and we don't want to keep both
            // top-level locks acquired at once.
            if let Ok(mut store) = self.notification_snapshots.lock() {
                store.capture(id, self.traffic.snapshot(), Instant::now());
            }
        }
    }

    /// Push a connection-state transition. Wraps
    /// `push_notification` with `NotificationSource::Connection` so
    /// every connection event lands under a single label in the
    /// side panel.
    fn push_connection_event(&self, severity: NotifySeverity, message: impl Into<String>) {
        self.push_notification(severity, NotificationSource::Connection, message);
    }

    fn handle_cadence(&mut self) {
        if !self.cadence_enabled {
            return;
        }

        let now = Instant::now();
        if now.duration_since(self.last_cadence_key) >= Duration::from_secs(2) {
            if let Some(tx) = &self.input_tx {
                // Send space key
                let _ = tx.try_send(InputEvent::KeyDown(0x39)); // Space down
                let _ = tx.try_send(InputEvent::KeyUp(0xB9)); // Space up
                self.last_cadence_key = now;
            }
        }
    }
}

/// Whether the window has keyboard focus, read so that it is current in a
/// logic-only pass too.
///
/// While the window is hidden eframe calls `App::logic` without running an
/// egui pass, so `InputState::focused` keeps the value from the last frame
/// that was drawn. `Context::run_logic` does refresh the raw input's focus,
/// which is what `InputState::focused` is built from in a normal pass.
fn window_focused(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.raw.focused)
}

impl eframe::App for RyllApp {
    // A bug report the user has started must not be lost to the
    // window closing: a Display report waiting for its region is
    // written without one, and a report whose metrics sample is still
    // running is waited for briefly and written.
    fn on_exit(&mut self) {
        self.end_region_select(RegionSelectEnd::WindowClosed);
        self.flush_pending_bug_report_at_exit();
    }

    // Work that must keep ticking whether or not the window is drawn.
    //
    // eframe calls this before every `ui` pass, and on its own when the
    // window is minimised or occluded (behind other windows, on another
    // Space, or under the macOS lock screen): then it runs no egui pass at
    // all, so `ui` is never called. Everything that drains the channel-event
    // queue or advances a timer lives here, or a hidden window stops
    // reading the queue, the renderer's producers back up behind it, and
    // the main channel starts dropping events (issue #444). Anything that
    // needs a `Ui`, or this pass's keyboard and pointer input (stale while
    // hidden), stays in `ui`.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Mirror the window's focus state into the shared
        // AtomicBool that the FocusGatedClipboard reads. The
        // value flips on the same pass egui sees the
        // platform-level focus event, so the next 500 ms
        // clipboard tick on the renderer side will see the
        // updated state.
        let focused = window_focused(ctx);
        self.app_focused
            .store(focused, std::sync::atomic::Ordering::Relaxed);

        // Graceful shutdown on Ctrl+C: close capture session (flushes
        // the MP4 moov atom) then ask eframe to exit. Also flip the
        // per-connection cancel flag so the renderer's session
        // orchestrator unwinds its channel tasks promptly instead of
        // waiting for the read loops to time out.
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            info!("app: shutdown requested (SIGINT)");
            if let Some(ref cancel) = self.connection_cancel {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(ref capture) = self.capture {
                capture.close();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Process incoming events
        self.process_events();

        // Prune expired notification snapshots at most once per second
        // so the per-row File-as-bug-report button visual state
        // honestly reflects the 60 s TTL without walking the map on
        // every paint.
        if let Ok(mut store) = self.notification_snapshots.lock() {
            store.maybe_prune(Instant::now());
        }

        // Pre-expiry warning: if `ticket-valid-until` is set and
        // we are within 30 s of expiry, push a one-shot warn
        // notification so the user knows the session is about
        // to end. Latched via `ticket_expiry_warned` to fire
        // exactly once per session.
        if !self.ticket_expiry_warned {
            if let Some(expiry) = self.config.ticket_valid_until {
                if let Ok(remaining) = expiry.duration_since(SystemTime::now()) {
                    if remaining <= Duration::from_secs(30) {
                        self.push_notification(
                            NotifySeverity::Warn,
                            NotificationSource::BugReport,
                            "Session ticket expires in 30 seconds.".to_string(),
                        );
                        self.ticket_expiry_warned = true;
                    }
                }
            }
        }

        // Auto-reconnect tick: when the Pending deadline has
        // passed AND we are not already awaiting the outcome of
        // a prior fire, kick off the next attempt. The flag
        // doubles as a "don't re-fire on the next frame" gate;
        // it clears only when the outcome arrives
        // (SessionInitialized → success, or a disconnect →
        // advance/Modal).
        if !self.awaiting_reconnect_outcome {
            // Re-check the ticket policy at the moment of fire —
            // a long Pending window can outlive
            // `ticket-valid-until`, and there is no point firing
            // a reconnect we know the server will reject. This
            // tripwire fires from either Pending(N) directly to
            // Modal{TicketExpired}, bypassing the remaining
            // attempts.
            if matches!(self.reconnect_state, ReconnectState::Pending { .. }) {
                if let Some(variant) =
                    forbid_retry_modal(&self.reconnect_policy(), SystemTime::now())
                {
                    info!(
                        "app: ticket policy now forbids retry mid-Pending; \
                         transitioning to Modal directly"
                    );
                    self.last_modal_at = Some(Instant::now());
                    self.reconnect_state = ReconnectState::Modal(variant);
                }
            }
            if let ReconnectState::Pending {
                attempt, next_at, ..
            } = &self.reconnect_state
            {
                if Instant::now() >= *next_at {
                    let attempt = *attempt;
                    info!("app: auto-reconnect attempt {} firing", attempt);
                    // Surface the attempt fire in the bell history. Per-attempt number
                    // in the message keeps successive attempts distinct across the 30 s
                    // dedup window.
                    self.push_connection_event(
                        NotifySeverity::Info,
                        format!("Reconnect attempt {}/{}…", attempt, MAX_RECONNECT_ATTEMPTS),
                    );
                    self.awaiting_reconnect_outcome = true;
                    self.reconnect();
                }
            }
        }

        let now = Instant::now();
        if let Some((w, h)) = resolution_notification_due(
            self.pending_resolution_notify,
            self.last_notified_resolution,
            now,
            RESOLUTION_NOTIFY_DEBOUNCE,
        ) {
            self.push_notification(
                NotifySeverity::Info,
                NotificationSource::Internal,
                format!("Display resolution: {}x{}", w, h),
            );
            self.last_notified_resolution = Some((w, h));
            self.pending_resolution_notify = None;
        } else if let Some((_, at)) = self.pending_resolution_notify {
            // Still inside the debounce window. Ask egui for a
            // repaint right at the deadline so the notification
            // fires promptly even if no other events arrive.
            let elapsed = now.saturating_duration_since(at);
            if elapsed < RESOLUTION_NOTIFY_DEBOUNCE {
                ctx.request_repaint_after(RESOLUTION_NOTIFY_DEBOUNCE - elapsed);
            } else {
                // Past the deadline but skipped (target matches
                // last_notified). Drop the pending state so we
                // do not keep retrying.
                self.pending_resolution_notify = None;
            }
        }

        // Tick the bandwidth tracker
        self.bandwidth.tick();

        self.poll_pending_bug_report();

        // Handle cadence mode
        self.handle_cadence();

        // Repaint when channel events arrive; 1s fallback for time-based UI
        // (bandwidth/latency sparklines, status-message expiry, cadence-mode
        // keystroke injection).  The bridge task wakes us immediately when
        // an event arrives via the Arc<Notify>; this fallback only ensures
        // anything that polls Instant::elapsed() updates roughly once a
        // second. While the window is hidden eframe answers both with a
        // logic-only pass, so the reconnect and debounce timers above keep
        // firing.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }

    // eframe's App entry point: it hands us a root `&mut Ui`
    // filling the viewport, which the panels below borrow.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35 unified the panel types and made every panel's
        // `show` take `&mut Ui` instead of `&Context`. We own a
        // cloned `Context` (cheap: it is `Arc`-backed) and bind
        // `ctx` to a borrow of it, rather than to `ui.ctx()`. That
        // keeps `ctx: &Context` — so all the existing `ctx.method()`
        // and `Window::show(ctx, …)` calls are unchanged — while
        // leaving `ui` free to be borrowed mutably by the panels.
        let ctx_owned = ui.ctx().clone();
        let ctx = &ctx_owned;
        // `logic` has already started the Ctrl+C shutdown; draw nothing
        // while the close goes through.
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        // The call order below is the frame: egui reserves each
        // panel's space in the order the panels are drawn, which is
        // why the status bar comes before the central panel that
        // fills what is left of the window, and the windows are
        // drawn in z-order, back to front.
        self.gate_keyboard_focus(ctx);
        self.fit_viewport_to_surface(ctx);
        self.handle_hotkeys(ctx);
        self.refresh_traffic_entries();

        self.draw_stats_bar(ui, ctx);
        self.draw_traffic_panel(ui);
        self.draw_notifications_panel(ui);
        self.draw_usb_panel(ui, ctx);
        self.draw_webdav_panel(ui, ctx);
        let outcome = self.draw_surface(ui, ctx);

        // Open bug report dialog for channel error (two-pass)
        if outcome.open_channel_bug_report {
            let description = self.error_message.clone().unwrap_or_default();
            self.open_bug_dialog(BugReportType::Connection, description);
        }

        self.draw_bug_report_dialog(ctx);
        self.draw_paste_error_dialog(ctx);
        self.draw_region_select(ctx);
        self.draw_cursor_overlay(ctx);
        self.draw_protocol_gaps_window(ctx);
        self.draw_reconnect_modal(ctx);
        self.hide_host_cursor(ctx, outcome.pointer_on_surface);
        self.observe_ui_state(ctx);
    }
}

// ── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    // -------------------------------------------------------------------------
    // Hidden-window logic passes (issue #444)
    //
    // While the window is minimised or occluded, eframe runs `App::logic`
    // through `Context::run_logic` and never calls `ui`. These pin the egui
    // behaviour `RyllApp::logic` relies on to keep draining events then.
    // -------------------------------------------------------------------------

    #[test]
    fn logic_only_passes_keep_answering_repaint_requests() {
        let ctx = egui::Context::default();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = wakes.clone();
        ctx.set_request_repaint_callback(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let wakes_now = || wakes.load(Ordering::SeqCst);

        // The repaint bridge wakes egui for each burst of channel events.
        // With no pass in between, repeat requests are already covered.
        ctx.request_repaint();
        let after_first = wakes_now();
        assert!(after_first > 0);
        ctx.request_repaint();
        assert_eq!(wakes_now(), after_first);

        // eframe serves the wake-ups with logic-only passes. Once they have
        // consumed the request, the next burst must reach eframe again, or a
        // hidden window would drain the queue once and then never again.
        for _ in 0..3 {
            let _ = ctx.run_logic(&egui::RawInput::default(), |_| {});
        }
        let before = wakes_now();
        ctx.request_repaint();
        assert!(wakes_now() > before);
    }

    #[test]
    fn window_focused_is_current_in_a_logic_only_pass() {
        let ctx = egui::Context::default();
        let focused = egui::RawInput {
            focused: true,
            ..Default::default()
        };
        ctx.run_ui(focused, |ui| assert!(window_focused(ui.ctx())))
            .drop_without_applying_deltas();

        // The window loses focus as it is hidden. `InputState::focused`
        // still says true, from the last drawn frame; `window_focused` must
        // not, or the clipboard poll keeps running for an unfocused window.
        let blurred = egui::RawInput {
            focused: false,
            ..Default::default()
        };
        let _ = ctx.run_logic(&blurred, |ctx| {
            assert!(ctx.input(|i| i.focused));
            assert!(!window_focused(ctx));
        });
    }

    // ── VolumeControl round-trip (K3 guard) ─────────────────

    #[test]
    fn volume_control_round_trip() {
        // RyllApp::reconnect() must not leave the user's volume slider
        // back at 80% / unmuted. Not resetting it relies on the existing
        // Arc<VolumeControl> surviving the swap, with the same get/set
        // semantics on both sides of the boundary. Pin the contract so a
        // future refactor of VolumeControl's storage cannot quietly
        // re-introduce the regression.
        let vc = shakenfist_spice_renderer::channels::VolumeControl::new();
        assert_eq!(vc.volume(), 80);
        assert!(!vc.muted());
        vc.set_volume(25);
        vc.set_muted(true);
        assert_eq!(vc.volume(), 25);
        assert!(vc.muted());
        // The same Arc must reflect updates from any clone — the
        // app holds one Arc and hands a clone to the playback
        // channel, so reads via either must see the same value.
        let other = vc.clone();
        assert_eq!(other.volume(), 25);
        assert!(other.muted());
        other.set_volume(60);
        assert_eq!(vc.volume(), 60);
    }
}
