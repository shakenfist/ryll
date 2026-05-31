// Main application - egui App.
//
// eframe 0.34 deprecated a broad swath of the egui API we depend on
// (`Frame::none`, `CentralPanel`/`SidePanel`/`TopBottomPanel` as type
// aliases, `Panel::show*`, `Context::style` / `wants_pointer_input`,
// `InputState::screen_rect`, `Ui::close_menu`, `menu::menu_button`,
// and the `update()` trait method itself). The minimum-diff bump on
// the renovate/eframe-0.x branch leaves these call sites alone and
// silences the deprecation warnings module-wide so clippy's `-D
// warnings` policy still passes. The migration is tracked in
// `docs/plans/PLAN-egui-0.34-followups.md`.
#![allow(deprecated)]

use eframe::egui;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, error, info, warn};

use crate::auto_snapshot::AutoSnapshotState;
use crate::bugreport::{
    chrono_now, encode_png, format_size, AppSnapshot, BugReport, BugReportType, ChannelSnapshots,
    NotificationSnapshotState, PedanticConfig, ReportRegion, SurfaceInfo, TrafficBuffers,
    TrafficDirection, TrafficViewEntry, TriggerTimestamps,
};
use crate::capture::CaptureSession;
use crate::config::{Config, ShareDirConfig, VirtualDiskConfig};
use crate::display_gui::GuiSurface;
use crate::input_egui::{egui_key_to_logical, mouse_button_to_spice};
use crate::notifications::{
    self as notifications, register_gap_notification_observer, NotificationEntry,
    NotificationSource, NotificationStore, SharedNotifications,
};
use crate::settings;
use crate::streaming_state::{self, StreamingState};
use shakenfist_spice_protocol::{ChannelType, NotifySeverity, MOUSE_MODE_SERVER};
use shakenfist_spice_renderer::channels::inputs::scancode_for_logical_key;
use shakenfist_spice_renderer::channels::VolumeControl;
use shakenfist_spice_renderer::usb::{self, DeviceSource, UsbDeviceInfo};
use shakenfist_spice_renderer::{
    ChannelEvent, ClipboardBackend, CursorImage, InputEvent, UsbCommand, WebdavCommand,
};

use crate::clipboard_arboard::ArboardClipboard;

/// Channel buffer sizes
const EVENT_CHANNEL_SIZE: usize = 1024;
const INPUT_CHANNEL_SIZE: usize = 256;

/// Approximate height of the stats bar at the bottom of the window
const STATS_BAR_HEIGHT: f32 = 20.0;

/// Debounce window for resolution-change notifications.
/// A burst of SurfaceCreated events within this window
/// (boot mode probes, drag-resize storms) collapses to a
/// single notification carrying the latest resolution.
const RESOLUTION_NOTIFY_DEBOUNCE: Duration = Duration::from_millis(500);

/// Upper bound (logical pixels) for a primary surface
/// dimension that the auto-fit pipeline will honour. A
/// hostile or buggy SPICE server can announce
/// `SurfaceCreated { width: u32::MAX, height: u32::MAX }`;
/// without a bound we would forward that as
/// `ViewportCommand::InnerSize` to egui (platform-dependent
/// behaviour, possibly large internal allocations) and
/// emit a `"Display resolution: 4294967295x4294967295"`
/// notification. 16384 is `GL_MAX_TEXTURE_SIZE` on most
/// hardware and is comfortably above any realistic
/// display resolution.
const MAX_AUTO_FIT_DIMENSION: u32 = 16384;

/// Number of bandwidth samples to keep for the sparkline.
const BANDWIDTH_HISTORY_LEN: usize = 60;

/// Number of latency samples to keep for the sparkline.
const LATENCY_HISTORY_LEN: usize = 60;

/// Number of recent frame timestamps kept for the FPS sliding window.
const FPS_WINDOW_SIZE: usize = 120;

/// Phase-04: number of recent mpsc-queue lag samples (μs)
/// retained per event kind for render-side latency
/// diagnostics. Per-event cadence is typically several Hz to
/// hundreds of Hz; 32 entries cover seconds of recent activity
/// without bloating session.json. See
/// PLAN-video-keeping-up-phase-04.
const RECENT_LAG_RING_CAP: usize = 32;

/// Maximum entries shown in the traffic viewer.
const TRAFFIC_VIEWER_MAX_ENTRIES: usize = 200;

/// How often the traffic viewer refreshes from the ring buffers.
const TRAFFIC_VIEWER_REFRESH_MS: u64 = 250;

/// Auto-reconnect retry budget per disconnect cluster.
const MAX_RECONNECT_ATTEMPTS: u8 = 3;

/// Backoff (seconds) before each reconnect attempt within a
/// cluster. Index 0 is the wait before attempt 1, index 1 before
/// attempt 2, etc. Shape matches spice-gtk's reconnect policy:
/// short first attempt for blip recovery, longer windows for
/// server restarts. Total worst-case wait ~21 s before the modal.
const RECONNECT_BACKOFF_SECS: [u64; MAX_RECONNECT_ATTEMPTS as usize] = [1, 4, 16];

/// After the auto-reconnect budget is exhausted (Modal shown),
/// further disconnects within this window go straight back to
/// Modal without re-trying — a flapping server cannot make us
/// bang away forever. A fresh budget unlocks after this elapses.
const RECONNECT_CLUSTER_RESET: Duration = Duration::from_secs(5 * 60);

/// How long a notification's traffic-buffer snapshot is kept
/// before being treated as expired (Phase 10 / F2). The
/// "File bug report" button on a notification entry switches
/// to post-event-only mode after this window elapses.
const NOTIFICATION_SNAPSHOT_TTL: Duration = Duration::from_secs(60);

/// Maximum number of live notification snapshots retained at
/// any one time (Phase 10 / F2). Oldest is evicted when a
/// sixth notification fires.
const NOTIFICATION_SNAPSHOT_CAP: usize = 5;

/// Single entry in the notification-snapshot store. Owns a
/// captured `TrafficBuffers` (cheap thanks to Phase 07 /
/// Phase 08's Arc-shared payloads).
struct NotificationSnapshotEntry {
    captured_at: Instant,
    traffic: TrafficBuffers,
}

/// Bounded LRU+TTL store of traffic-buffer snapshots keyed by
/// `NotificationEntry::id`. Phase 10 (F2): every fresh
/// notification push captures one entry; entries are pruned
/// on overflow (cap) or expiry (TTL). The notifications panel
/// uses `has_live` to render the button's visual state, and
/// `take` to consume the snapshot when the user clicks.
struct NotificationSnapshotStore {
    by_id: HashMap<u64, NotificationSnapshotEntry>,
    /// Notification ids in insertion order, oldest first.
    /// Tracked separately so eviction-on-overflow is O(1).
    insertion_order: VecDeque<u64>,
    /// Tick-time prune is gated on this so we don't walk the
    /// map on every paint frame.
    last_prune: Instant,
}

impl NotificationSnapshotStore {
    fn new() -> Self {
        NotificationSnapshotStore {
            by_id: HashMap::new(),
            insertion_order: VecDeque::new(),
            last_prune: Instant::now(),
        }
    }

    /// Insert (or refresh) a snapshot for `id`. If the store
    /// is over capacity after this push, the oldest entry is
    /// evicted. Expired entries are pruned opportunistically.
    fn capture(&mut self, id: u64, traffic: TrafficBuffers, now: Instant) {
        self.prune_expired(now);

        if self.by_id.contains_key(&id) {
            // Refresh: replace the captured payload and bump
            // the captured_at timestamp. Don't touch
            // insertion_order — its position is unchanged by
            // a fold-refresh.
            if let Some(entry) = self.by_id.get_mut(&id) {
                entry.captured_at = now;
                entry.traffic = traffic;
            }
            return;
        }

        self.by_id.insert(
            id,
            NotificationSnapshotEntry {
                captured_at: now,
                traffic,
            },
        );
        self.insertion_order.push_back(id);
        while self.insertion_order.len() > NOTIFICATION_SNAPSHOT_CAP {
            if let Some(oldest_id) = self.insertion_order.pop_front() {
                self.by_id.remove(&oldest_id);
            }
        }
    }

    /// `true` iff a non-expired snapshot exists for this id.
    /// Prunes expired entries as a side effect so the answer
    /// is always current.
    fn has_live(&mut self, id: u64, now: Instant) -> bool {
        self.prune_expired(now);
        self.by_id.contains_key(&id)
    }

    /// Remove and return the snapshot for `id`, if present
    /// and non-expired. Used at button-click time to consume
    /// the snapshot for a report. Prunes expired entries on
    /// the way through.
    fn take(&mut self, id: u64, now: Instant) -> Option<TrafficBuffers> {
        self.prune_expired(now);
        let entry = self.by_id.remove(&id)?;
        self.insertion_order.retain(|other| *other != id);
        Some(entry.traffic)
    }

    /// Drop every entry older than the TTL. Cheap O(N) walk
    /// over `insertion_order`, at most `NOTIFICATION_SNAPSHOT_CAP`
    /// entries.
    fn prune_expired(&mut self, now: Instant) {
        while let Some(&oldest_id) = self.insertion_order.front() {
            let expired = self
                .by_id
                .get(&oldest_id)
                .map(|e| now.duration_since(e.captured_at) >= NOTIFICATION_SNAPSHOT_TTL)
                .unwrap_or(true);
            if !expired {
                break;
            }
            self.insertion_order.pop_front();
            self.by_id.remove(&oldest_id);
        }
    }

    /// Called from the GUI tick at most once per second so
    /// the notifications panel's button visuals reflect
    /// expiration in real time without polling on every
    /// repaint.
    fn maybe_prune(&mut self, now: Instant) {
        if now.duration_since(self.last_prune) >= Duration::from_secs(1) {
            self.last_prune = now;
            self.prune_expired(now);
        }
    }
}

/// Discriminator for the disconnect-modal. The variant
/// determines title, body copy, and which buttons render. The
/// `OneShotConsumed` and `TicketExpired` variants are entered
/// when the .vv file's `delete-this-file` / `ticket-valid-until`
/// keys (see `kerbside-wt-docs/docs/spice/console-vv-extensions.md`)
/// indicate that any further reconnect is doomed; the
/// `Generic` variant covers everything else.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModalVariant {
    /// Auto-reconnect budget exhausted on a reusable ticket.
    /// `latest_error` is the most recent attempt's failure
    /// string, shown in the modal body for context.
    Generic { latest_error: String },
    /// The .vv file's `delete-this-file=1` flag marked the
    /// ticket as single-use; the first link consumed it, and
    /// any reconnect would be rejected by the server. Reconnect
    /// button hidden.
    OneShotConsumed,
    /// `ticket-valid-until` has elapsed (wall-clock time). The
    /// server will reject any link from now on; auto-reconnect
    /// is suppressed and the modal explains why.
    TicketExpired { expired_at: SystemTime },
}

/// Auto-reconnect state machine. Replaces the implicit
/// `show_disconnect_dialog: bool` + `disconnect_reason` pair so
/// every disconnect path either auto-recovers or surfaces a
/// well-typed modal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReconnectState {
    /// Connected normally, or not yet attempted.
    Idle,
    /// Auto-reconnect is in progress. `attempt` ∈ 1..=MAX_ATTEMPTS;
    /// `next_at` is when the next `reconnect()` call should fire.
    Pending {
        attempt: u8,
        next_at: Instant,
        latest_error: String,
    },
    /// Budget exhausted (or ticket-related auto-suppression);
    /// the user takes over via the modal. Variant carries the
    /// reason and any context needed to render copy + buttons.
    Modal(ModalVariant),
}

/// Policy bits derived from the .vv file's ticket-related keys.
/// Bundled so the state-machine transition can take a single
/// argument rather than threading two unrelated booleans.
#[derive(Debug, Clone, Copy, Default)]
struct ReconnectPolicy {
    /// `delete-this-file=1` was set: the previous link consumed
    /// the ticket, so the server will reject any reconnect. All
    /// disconnects go straight to `Modal(OneShotConsumed)`.
    ticket_is_single_use: bool,
    /// `ticket-valid-until=<unix-ts>` was set: when this wall
    /// time has passed, any reconnect would be doomed. The
    /// state machine consults this on entry and at every
    /// `Pending` tick so the modal trips immediately rather
    /// than burning the 3-attempt budget on dead retries.
    ticket_valid_until: Option<SystemTime>,
}

impl ReconnectPolicy {
    /// If the ticket policy forbids any further reconnect, return
    /// the appropriate `ModalVariant`. `Ok(())` means the normal
    /// retry path is permitted.
    fn forbid_retry(&self, now_wall: SystemTime) -> Option<ModalVariant> {
        if self.ticket_is_single_use {
            return Some(ModalVariant::OneShotConsumed);
        }
        if let Some(expiry) = self.ticket_valid_until {
            if now_wall >= expiry {
                return Some(ModalVariant::TicketExpired { expired_at: expiry });
            }
        }
        None
    }
}

impl ReconnectState {
    /// Pure transition for a disconnect event. Returns the new
    /// state, or `None` if the event should be ignored (e.g. a
    /// duplicate channel-storm event while we're already in
    /// `Pending` or `Modal`).
    ///
    /// `awaiting_outcome` is `true` when the disconnect is the
    /// failure of an in-flight reconnect attempt (we previously
    /// called `reconnect()` from a `Pending` tick and are now
    /// hearing back). `false` for the initial disconnect or for
    /// duplicate storm events.
    ///
    /// `policy` derived from the .vv file's ticket-related
    /// keys. When it forbids retries (single-use ticket
    /// consumed, or `ticket-valid-until` elapsed) we skip the
    /// auto-retry path entirely and land in the matching
    /// `Modal` variant on the first disconnect.
    fn on_disconnect(
        &self,
        awaiting_outcome: bool,
        last_modal_at: Option<Instant>,
        now: Instant,
        now_wall: SystemTime,
        policy: ReconnectPolicy,
        latest_error: String,
    ) -> Option<Self> {
        // Ticket-bound deployments: any further reconnect would
        // be rejected, so trip the modal on the first disconnect
        // event of any kind — no point in burning the budget.
        if let Some(variant) = policy.forbid_retry(now_wall) {
            // Already in the matching Modal? Ignore the storm.
            if matches!(self, ReconnectState::Modal(v) if v == &variant) {
                return None;
            }
            return Some(ReconnectState::Modal(variant));
        }

        if awaiting_outcome {
            match self {
                ReconnectState::Pending { attempt, .. } => {
                    let next_attempt = attempt + 1;
                    if next_attempt > MAX_RECONNECT_ATTEMPTS {
                        Some(ReconnectState::Modal(ModalVariant::Generic {
                            latest_error,
                        }))
                    } else {
                        let backoff = Duration::from_secs(
                            RECONNECT_BACKOFF_SECS[(next_attempt - 1) as usize],
                        );
                        Some(ReconnectState::Pending {
                            attempt: next_attempt,
                            next_at: now + backoff,
                            latest_error,
                        })
                    }
                }
                // `awaiting_outcome` should imply we were in
                // Pending; defensively land in Generic Modal so
                // we don't silently re-arm a retry from a stale
                // state.
                _ => Some(ReconnectState::Modal(ModalVariant::Generic {
                    latest_error,
                })),
            }
        } else {
            match self {
                ReconnectState::Idle => {
                    if let Some(t) = last_modal_at {
                        if now.duration_since(t) < RECONNECT_CLUSTER_RESET {
                            return Some(ReconnectState::Modal(ModalVariant::Generic {
                                latest_error,
                            }));
                        }
                    }
                    let backoff = Duration::from_secs(RECONNECT_BACKOFF_SECS[0]);
                    Some(ReconnectState::Pending {
                        attempt: 1,
                        next_at: now + backoff,
                        latest_error,
                    })
                }
                // Already disconnected — ignore the duplicate.
                ReconnectState::Pending { .. } | ReconnectState::Modal(_) => None,
            }
        }
    }
}

/// Statistics tracking
#[derive(Default)]
struct Statistics {
    frames_received: u64,
    bytes_in: u64,
    bytes_out: u64,
    /// Inter-PING interval most recently observed on the main
    /// channel, in milliseconds.
    last_latency_ms: Option<f64>,
    /// Timestamps of recent DisplayMark events for sliding-window FPS.
    frame_times: Vec<Instant>,
}

// `ByteCounter` lives in the renderer crate; the app just
// re-exports it for backwards-compatible imports inside ryll.
pub use shakenfist_spice_renderer::ByteCounter;

/// Rolling bandwidth tracker — samples bytes/sec once per second.
struct BandwidthTracker {
    /// Shared counter incremented by all channels.
    counter: Arc<ByteCounter>,
    /// History of bytes-per-second samples (most recent last).
    /// VecDeque so eviction at capacity is O(1) (`pop_front`)
    /// instead of O(n) `Vec::remove(0)`.
    history: VecDeque<f32>,
    /// When the current second started.
    last_tick: Instant,
}

impl BandwidthTracker {
    fn new(counter: Arc<ByteCounter>) -> Self {
        BandwidthTracker {
            counter,
            history: VecDeque::with_capacity(BANDWIDTH_HISTORY_LEN),
            last_tick: Instant::now(),
        }
    }

    /// Tick the tracker — if a second has elapsed, read the
    /// counter and push a new sample.
    fn tick(&mut self) {
        let elapsed = self.last_tick.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let bytes = self.counter.take();
            let secs = elapsed.as_secs_f64();
            let bps = bytes as f64 / secs;
            self.history.push_back(bps as f32);
            if self.history.len() > BANDWIDTH_HISTORY_LEN {
                self.history.pop_front();
            }
            self.last_tick = Instant::now();
        }
    }

    /// Format the most recent bandwidth value for display.
    fn label(&self) -> String {
        match self.history.back() {
            Some(&bps) if bps >= 1_000_000.0 => format!("{:.1} MB/s", bps / 1_000_000.0),
            Some(&bps) if bps >= 1_000.0 => format!("{:.0} KB/s", bps / 1_000.0),
            Some(&bps) => format!("{:.0} B/s", bps),
            None => String::from("-- B/s"),
        }
    }
}

/// Rolling latency tracker — samples arrive when
/// `ChannelEvent::Latency` fires, driven by server PINGs
/// on the main channel.  Values are stored in milliseconds
/// (client-observed inter-PING interval) for sparkline
/// scaling.  Lower variance is better; spikes indicate a
/// network stall or server send-loop delay.
struct LatencyTracker {
    /// History of latency samples in ms (most recent last).
    /// VecDeque so eviction at capacity is O(1) (`pop_front`).
    history: VecDeque<f32>,
}

impl LatencyTracker {
    fn new() -> Self {
        LatencyTracker {
            history: VecDeque::with_capacity(LATENCY_HISTORY_LEN),
        }
    }

    /// Record a new latency sample in milliseconds.
    fn record(&mut self, sample_ms: f32) {
        self.history.push_back(sample_ms);
        if self.history.len() > LATENCY_HISTORY_LEN {
            self.history.pop_front();
        }
    }

    /// Format the most recent latency value for display.
    fn label(&self) -> String {
        match self.history.back() {
            Some(&v) => format!("{:.1}ms", v),
            None => String::from("--ms"),
        }
    }
}

/// Captured on the UI thread when a bug-report dialog opens.
///
/// The raw RGBA is cloned and handed to a background
/// `std::thread` that PNG-encodes into `png_slot`; the UI
/// thread holds another clone of the `Arc` and consumes the
/// bytes at submit time (or drops the `Arc` on cancel, letting
/// the worker write into what becomes garbage).
///
/// Timestamps are recorded synchronously on the UI thread at
/// dialog-open — they never depend on the encoder thread
/// finishing — so they always make it into metadata.json even
/// when the surface is missing or encoding fails.
struct TriggerSnapshot {
    /// ISO 8601 UTC timestamp of when the dialog opened. Same
    /// format as `ReportMetadata::timestamp`.
    triggered_at: String,
    /// Session uptime in seconds at the moment of dialog open.
    /// Same units as `AppSnapshot::uptime_secs`.
    triggered_uptime_secs: f64,
    /// Slot the encoder thread fills with either the PNG bytes
    /// or an `Err` on encode failure. `None` while the worker is
    /// still running.
    png_slot: Arc<std::sync::Mutex<Option<anyhow::Result<Vec<u8>>>>>,
}

/// The egui application
pub struct RyllApp {
    // Communication channels
    event_rx: mpsc::Receiver<ChannelEvent>,
    input_tx: Option<mpsc::Sender<InputEvent>>,
    resize_tx: Option<Arc<mpsc::Sender<(u32, u32)>>>,
    last_sent_resize: Option<(u32, u32)>,
    volume_control: Arc<VolumeControl>,

    // Display state
    surfaces: HashMap<(u8, u32), GuiSurface>,

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
    /// Count of `Pending` entries (per-attempt) over the
    /// session's lifetime, surfaced in `session.json` for
    /// post-hoc analysis of how rocky the session was.
    auto_reconnect_count: u32,
    /// Has the T-30s `ticket-valid-until` pre-expiry warning
    /// been pushed for the current session? Latches to `true`
    /// once fired so the notification panel does not see a
    /// stream of duplicates as the deadline approaches.
    ticket_expiry_warned: bool,

    // Last mouse position sent (to avoid flooding with duplicates)
    last_mouse_pos: Option<(u32, u32)>,
    last_modifiers: Option<egui::Modifiers>,

    // Bitmask of mouse buttons we have forwarded as pressed to the
    // inputs channel.  Used to send synthetic releases when input
    // forwarding is suppressed (e.g. bug report dialog opens).
    forwarded_buttons: u32,

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

    // Latency tracking for the status bar sparkline
    latency: LatencyTracker,

    // Capture session (None when --capture is not specified)
    capture: Option<Arc<CaptureSession>>,

    // Override for bug-report output directory (--bug-report-dir).
    // None means fall back to capture/cwd; see manual_bug_report_dir().
    bug_report_dir: Option<PathBuf>,

    // Cooldown for auto-disconnect snapshots so a flapping
    // channel can't dump one zip per disconnect storm. 60 s
    // window; see maybe_write_disconnect_snapshot().
    last_disconnect_report_at: Option<Instant>,

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

    // Phase 10 (F2): bounded snapshot store keyed by
    // notification id. Captured on every push_notification
    // call so the "File bug report" button on a notification
    // entry can produce a report with pcap/state from the
    // moment the notification fired. Wrapped in Mutex for
    // the same multi-producer reasons SharedNotifications is.
    notification_snapshots: std::sync::Mutex<NotificationSnapshotStore>,

    // Channel state snapshots (always active, for bug reports)
    channel_snapshots: ChannelSnapshots,
    app_snapshot: Arc<std::sync::Mutex<AppSnapshot>>,

    // Phase-03: count of display frames dropped because the
    // encoder task's queue was full at CaptureSession::frame
    // call time. Mirrored into AppSnapshot::video_drop_count
    // by update_app_snapshot(). Stays zero unless --capture
    // is active. See PLAN-video-keeping-up-phase-03.
    video_drop_count: u64,

    // Phase-04: bounded rings of mpsc-queue lag samples (μs)
    // for renderer-to-app events. Mirrored into AppSnapshot
    // by update_app_snapshot() as min/max/mean aggregates.
    // See PLAN-video-keeping-up-phase-04.
    recent_image_ready_lag_us: VecDeque<u32>,
    recent_display_mark_lag_us: VecDeque<u32>,

    // Connection target for bug report metadata
    target_host: String,
    target_port: u16,

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

    /// Whether the guest has a vdagent connected (disables
    /// paste-as-keystrokes in favour of the clipboard path).
    agent_connected: bool,

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

    /// True while ryll's window is focused. Updated on every
    /// `update()` call from `ctx.input(|i| i.focused)`. Read by
    /// the `FocusGatedClipboard` decorator so the host
    /// pasteboard is only polled while the user is looking at
    /// ryll — a Phase 02 K1 follow-up to the spawn_blocking
    /// fix in commit 54155e99.
    app_focused: Arc<AtomicBool>,

    /// Persisted copy of the `--debug-single-thread-runtime`
    /// flag so reconnect can build a runtime of the same
    /// shape as the initial connect. Diagnostic-only.
    debug_single_thread_runtime: bool,

    /// Byte cap for the display-channel image cache, converted
    /// from `--image-cache-cap-mib` at startup. Persisted here
    /// so reconnect passes the same value each time.
    image_cache_cap_bytes: usize,

    /// Byte cap for the shared GLZ dictionary, converted from
    /// `--glz-dictionary-cap-mib` at startup. Persisted here so
    /// reconnect passes the same value each time. Phase 12E.
    glz_dictionary_cap_bytes: usize,

    // Phase 5 auto-snapshot config. `None` means the mode is
    // disabled. A fresh task is spawned per session: on each
    // `SessionInitialized` the previous task (if any) is signalled
    // via `auto_snapshot_cancel` to retire, then a new task is
    // spawned with fresh `traffic` / `channel_snapshots` Arcs. The
    // retire-and-respawn pattern is needed because `reconnect()`
    // replaces those Arcs wholesale; a long-lived task would
    // capture clones of the old Arcs and write zips full of stale
    // / empty data after the first reconnect.
    auto_snapshot_interval: Option<u64>,
    auto_snapshot_cap: usize,
    /// Cancel handle for the currently-running auto-snapshot task,
    /// if any. `None` before the first spawn and between retire
    /// and respawn. Setting to `true` causes the loop to exit at
    /// the next 500 ms poll.
    auto_snapshot_cancel: Option<Arc<AtomicBool>>,
    /// Whether the one-shot "Auto-snapshot mode enabled" startup
    /// notification has been pushed. Latched on the first spawn
    /// so per-reconnect respawns don't repeat the notification.
    auto_snapshot_startup_notified: bool,

    /// Phase 8: most recent time the streaming-flap notification
    /// fired. Used by `streaming_state::classify` to enforce the
    /// 60 s cool-down between repeat warnings. `None` until the
    /// first fire of the session; never cleared (reconnect resets
    /// the snapshot data the heuristic reads from, so a stale
    /// timestamp only suppresses the next legitimate flap during
    /// its remaining cool-down — acceptable for v1).
    last_flap_notification_ts: Option<Instant>,
}

/// Build the per-connection tokio runtime, honouring the
/// `--debug-single-thread-runtime` flag. The current_thread
/// flavour runs every spawned task on the calling thread,
/// which lets us tell a real blocking call (still hangs
/// because there's nowhere else to make progress) from a
/// multi-threaded scheduler / Waker-registration anomaly
/// (does not hang). Used by both the initial-connect and
/// reconnect spawn sites.
fn build_connection_runtime(single_thread: bool) -> tokio::runtime::Runtime {
    if single_thread {
        info!("app: building current_thread tokio runtime (debug single-thread mode)");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build current_thread tokio runtime")
    } else {
        tokio::runtime::Runtime::new().expect("failed to build multi-threaded tokio runtime")
    }
}

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
/// Format a `SystemTime` as a `HH:MM:SS` clock string in UTC
/// for the `TicketExpired` modal body. UTC is unambiguous for a
/// modal that explains "the ticket expired at …" — the user is
/// usually in the same TZ as their issuing system anyway, and
/// pulling in a TZ-aware crate just for one line of modal copy
/// would be disproportionate. Falls back to the raw unix
/// timestamp if the time is before the epoch (impossible in
/// practice but cheap to handle).
fn format_expiry_local(t: SystemTime) -> String {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs() % 86400;
            let h = secs / 3600;
            let m = (secs % 3600) / 60;
            let s = secs % 60;
            format!("{:02}:{:02}:{:02} UTC", h, m, s)
        }
        Err(_) => "before 1970-01-01".to_string(),
    }
}

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
    #[allow(clippy::too_many_arguments)]
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
        image_cache_cap_bytes: usize,
        glz_dictionary_cap_bytes: usize,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
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
        let target_host = config.host.clone();
        let target_port = config.port;

        // Register the --pedantic gap observer now that the live traffic,
        // channel-snapshot, and app-snapshot handles exist. The underlying
        // register_gap_observer has replay semantics, so any gaps fired
        // during the construction window before this call are delivered
        // when we register.
        if let Some(config) = pedantic_config {
            BugReport::register_pedantic_observer(
                config,
                target_host.clone(),
                target_port,
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
        // channel handler signals it.  Channel handlers call notify_one()
        // after each event_tx.send(), so egui sleeps when nothing is happening
        // and wakes immediately when something is.
        let repaint_notify = Arc::new(Notify::new());
        let connection_config: shakenfist_spice_protocol::ConnectionConfig = (&config).into();
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
                    /* opus_sink */ None,
                    image_cache_cap_bytes,
                    glz_dictionary_cap_bytes,
                )
                .await
                {
                    error!("app: connection error: {}", e);
                }
            });
            ctx.request_repaint();
        });

        RyllApp {
            event_rx,
            input_tx: Some(input_tx),
            resize_tx: Some(resize_tx),
            last_sent_resize: None,
            volume_control,
            surfaces: HashMap::new(),
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
            mouse_mode: 0,
            reconnect_state: ReconnectState::Idle,
            awaiting_reconnect_outcome: false,
            last_modal_at: None,
            auto_reconnect_count: 0,
            ticket_expiry_warned: false,
            last_mouse_pos: None,
            last_modifiers: None,
            forwarded_buttons: 0,
            pending_resize: None,
            last_auto_resize: None,
            obey_guest_size,
            pending_resolution_notify: None,
            last_notified_resolution: None,
            bandwidth: BandwidthTracker::new(byte_counter),
            latency: LatencyTracker::new(),
            capture,
            bug_report_dir,
            last_disconnect_report_at: None,
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
            target_host,
            target_port,
            show_bug_dialog: false,
            bug_report_type: BugReportType::Display,
            bug_description: String::new(),
            pending_trigger: None,
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
            agent_connected: false,
            cached_clipboard: None,
            paste_error_message: None,
            config,
            monitors,
            reconnect_virtual_disks: virtual_disks,
            reconnect_share_dir: share_dir,
            egui_ctx: cc.egui_ctx.clone(),
            connection_cancel: Some(connection_cancel),
            app_focused,
            debug_single_thread_runtime,
            image_cache_cap_bytes,
            glz_dictionary_cap_bytes,
            auto_snapshot_interval,
            auto_snapshot_cap: auto_snapshot_cap
                .unwrap_or(crate::auto_snapshot::DEFAULT_AUTO_SNAPSHOT_CAP),
            auto_snapshot_cancel: None,
            auto_snapshot_startup_notified: false,
            last_flap_notification_ts: None,
        }
    }

    fn reconnect(&mut self) {
        // Signal the previous attempt (if any) to exit before
        // spawning the next. The previous run_connection sees the
        // flag in its 100 ms select branch and breaks out, which
        // drops its tokio runtime when the spawned thread returns.
        if let Some(prev) = self.connection_cancel.take() {
            prev.store(true, Ordering::Relaxed);
        }

        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);
        let (usb_tx, usb_rx) = mpsc::channel(16);
        let (webdav_tx, webdav_rx) = mpsc::channel(16);
        let (resize_tx, resize_rx) = mpsc::channel(32);
        let resize_tx = Arc::new(resize_tx);

        let byte_counter = Arc::new(ByteCounter::new());
        let traffic = Arc::new(TrafficBuffers::new());
        let channel_snapshots = ChannelSnapshots::new();

        self.event_rx = event_rx;
        self.input_tx = Some(input_tx);
        self.resize_tx = Some(resize_tx);
        self.last_sent_resize = None;
        // volume_control is intentionally NOT replaced — see the
        // `vol_for_conn` binding below for the rationale.
        self.surfaces.clear();
        self.cursor_pos = (0, 0);
        self.cursor_visible = true;
        self.cursor_image = None;
        self.cursor_texture = None;
        self.surface_rect = egui::Rect::NOTHING;
        self.stats = Statistics::default();
        self.last_cadence_key = Instant::now();
        self.connected = false;
        self.error_message = None;
        self.mouse_mode = 0;
        // Clear the main-channel keepalive-timeout flag so a
        // subsequent disconnect reports its own cause cleanly
        // rather than inheriting the previous attempt's state.
        if let Ok(mut snap) = self.channel_snapshots.main.lock() {
            snap.keepalive_timeout_fired = false;
        }
        self.last_mouse_pos = None;
        self.last_modifiers = None;
        self.forwarded_buttons = 0;
        self.pending_resize = None;
        self.last_auto_resize = None;
        self.pending_resolution_notify = None;
        self.last_notified_resolution = None;
        self.bandwidth = BandwidthTracker::new(byte_counter.clone());
        self.usb_tx = Some(usb_tx);
        self.webdav_tx = Some(webdav_tx);
        self.usb_channel_ready = false;
        self.usb_connecting = false;
        self.usb_disconnecting = false;
        self.usb_error_message = None;
        self.usb_error_time = None;
        self.usb_device_description = None;
        self.usb_connected_at = None;
        self.traffic = traffic.clone();
        self.channel_snapshots = channel_snapshots;
        self.webdav_channel_ready = false;
        self.webdav_shared_dir = None;
        self.webdav_sharing_active = false;
        self.webdav_connected_at = None;
        self.webdav_error_message = None;
        self.webdav_error_time = None;

        let repaint_notify = Arc::new(Notify::new());
        let connection_config: shakenfist_spice_protocol::ConnectionConfig = (&self.config).into();
        let event_tx_clone = event_tx.clone();
        let ctx = self.egui_ctx.clone();
        let bridge_ctx = self.egui_ctx.clone();
        let bridge_notify = repaint_notify.clone();
        let conn_notify = repaint_notify.clone();
        let capture_clone: Option<Arc<dyn shakenfist_spice_renderer::CaptureSink>> = self
            .capture
            .clone()
            .map(|c| c as Arc<dyn shakenfist_spice_renderer::CaptureSink>);
        let counter_clone = byte_counter;
        let traffic_clone: Arc<dyn shakenfist_spice_renderer::TrafficSink> =
            traffic as Arc<dyn shakenfist_spice_renderer::TrafficSink>;
        let snaps_for_conn = self.channel_snapshots.clone();
        let monitors = self.monitors;
        let virtual_disks = self.reconnect_virtual_disks.clone();
        let share_dir = self.reconnect_share_dir.clone();
        // Volume slider position and mute state are host-side
        // state (the cpal output gain), not session state. Hand
        // the *existing* Arc<VolumeControl> to the new connection
        // task so the user's prior choices survive the swap; the
        // old playback channel's clone drops as that task exits.
        let vol_for_conn = self.volume_control.clone();
        let enable_paste = self.enable_paste;
        let log_config_clone = settings::log_config();
        let connection_cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_conn = connection_cancel.clone();
        self.connection_cancel = Some(connection_cancel);
        let focused_for_conn = self.app_focused.clone();
        let single_thread_for_conn = self.debug_single_thread_runtime;
        let image_cache_cap_bytes = self.image_cache_cap_bytes;
        let glz_dictionary_cap_bytes = self.glz_dictionary_cap_bytes;

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
                    conn_notify,
                    input_rx,
                    usb_rx,
                    webdav_rx,
                    virtual_disks,
                    share_dir,
                    capture_clone,
                    counter_clone,
                    traffic_clone,
                    snaps_for_conn,
                    monitors,
                    resize_rx,
                    vol_for_conn,
                    enable_paste,
                    log_config_clone,
                    cancel_for_conn,
                    clipboard,
                    /* opus_sink */ None,
                    image_cache_cap_bytes,
                    glz_dictionary_cap_bytes,
                )
                .await
                {
                    error!("app: connection error: {}", e);
                }
            });
            ctx.request_repaint();
        });

        info!("app: reconnecting...");
    }

    /// Driven by the disconnect modal's Reconnect button. Counts
    /// as user intervention: clears the cluster-reset window so a
    /// failed manual attempt re-arms the full 3-attempt budget
    /// rather than going straight back to Modal.
    fn reconnect_manual(&mut self) {
        // Phase 09 (F1): surface the click in the bell history
        // so the user has visible confirmation that the button
        // registered before the connection actually completes.
        self.push_connection_event(NotifySeverity::Info, "Reconnecting (manual)…".to_string());
        self.reconnect_state = ReconnectState::Idle;
        self.awaiting_reconnect_outcome = false;
        self.last_modal_at = None;
        self.reconnect();
    }

    /// Drive the `ReconnectState` machine in response to a critical
    /// disconnect (Main/Display/Inputs going down, or any
    /// `ChannelEvent::Error`). Callers are responsible for writing
    /// the disconnect snapshot before invoking this — keeping the
    /// snapshot at the call site avoids double-writing when the
    /// `Disconnected` handler has already snapshotted for the
    /// channel-storm path.
    fn handle_critical_disconnect(&mut self, message: String) {
        let awaiting = self.awaiting_reconnect_outcome;
        let now = Instant::now();
        let now_wall = SystemTime::now();
        let policy = self.reconnect_policy();
        let new_state = self.reconnect_state.on_disconnect(
            awaiting,
            self.last_modal_at,
            now,
            now_wall,
            policy,
            message.clone(),
        );

        // Even if the state machine ignores the event, the
        // connection itself has gone — clear surfaces / cursor so
        // the next paint shows a clean canvas.
        self.connected = false;
        self.surfaces.clear();
        self.cursor_image = None;
        self.cursor_texture = None;

        let Some(new_state) = new_state else {
            // Duplicate storm event — state unchanged, no-op.
            return;
        };

        if awaiting {
            self.awaiting_reconnect_outcome = false;
        }

        if let ReconnectState::Pending { attempt, .. } = &new_state {
            self.auto_reconnect_count = self.auto_reconnect_count.saturating_add(1);
            if *attempt == 1 {
                // Initial disconnect — announce the start of the
                // auto-retry cycle. Phase 09 (F1).
                self.push_connection_event(
                    NotifySeverity::Warn,
                    "Connection lost — reconnecting…".to_string(),
                );
            } else {
                // attempt > 1: the previous attempt just failed.
                // Reclassified from NotificationSource::BugReport
                // to NotificationSource::Connection in Phase 09.
                self.push_connection_event(
                    NotifySeverity::Warn,
                    format!("Reconnect attempt {} failed: {}", attempt - 1, message),
                );
            }
        }
        if let ReconnectState::Modal(variant) = &new_state {
            self.last_modal_at = Some(now);
            // The transition into Modal{Generic} via the
            // awaiting-outcome path is itself an attempt failure —
            // the user's three retries have been spent.
            if awaiting && matches!(variant, ModalVariant::Generic { .. }) {
                self.push_connection_event(
                    NotifySeverity::Warn,
                    format!(
                        "Reconnect attempt {} failed: {}",
                        MAX_RECONNECT_ATTEMPTS, message,
                    ),
                );
            }
            // Phase 09 (F1): also surface the Modal entry itself
            // as a connection event so the bell history records
            // the cycle's terminal state. The modal pops in
            // parallel; users who dismiss it reflexively still
            // see the event in the notification log.
            let (severity, modal_msg) = modal_variant_notification(variant);
            self.push_connection_event(severity, modal_msg);
            // Clock-skew check: when we land in Generic Modal,
            // the ticket should have been good (otherwise we'd
            // have taken the OneShotConsumed/TicketExpired path).
            // If `ticket-valid-until` is set and still in the
            // future, the 3 failures are suspicious — log a hook
            // so future investigations have something to grep.
            if let (ModalVariant::Generic { .. }, Some(expiry)) =
                (variant, policy.ticket_valid_until)
            {
                if now_wall < expiry {
                    warn!(
                        "app: 3 reconnect attempts failed but ticket-valid-until \
                         is still in the future ({:?}); possible clock skew or \
                         server-side issue independent of ticket expiry",
                        expiry
                    );
                }
            }
        }

        self.reconnect_state = new_state;
    }

    /// Derive the `ReconnectPolicy` from the active `.vv`
    /// config. Read at every disconnect / tick — cheap, and
    /// avoids caching state that would have to be re-synced if
    /// Config ever became mutable.
    fn reconnect_policy(&self) -> ReconnectPolicy {
        ReconnectPolicy {
            ticket_is_single_use: self.config.ticket_is_single_use,
            ticket_valid_until: self.config.ticket_valid_until,
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            // Phase-04: record mpsc-queue lag from event emit
            // to app pickup for renderer-side latency
            // diagnostics. See PLAN-video-keeping-up-phase-04.
            // Within-batch correlation noted: events drained
            // in one process_events() call share this read,
            // so several samples from one egui frame will be
            // close in time.
            let consumed_at_secs = self.traffic.elapsed().as_secs_f64();
            match &event {
                ChannelEvent::ImageReady {
                    produced_at_secs, ..
                }
                | ChannelEvent::ImageReadyChroma {
                    produced_at_secs, ..
                }
                | ChannelEvent::ImageReadyAlpha {
                    produced_at_secs, ..
                } => {
                    let lag_us = ((consumed_at_secs - *produced_at_secs).max(0.0) * 1_000_000.0)
                        .min(u32::MAX as f64) as u32;
                    push_with_cap(&mut self.recent_image_ready_lag_us, lag_us);
                }
                ChannelEvent::DisplayMark { produced_at_secs } => {
                    let lag_us = ((consumed_at_secs - *produced_at_secs).max(0.0) * 1_000_000.0)
                        .min(u32::MAX as f64) as u32;
                    push_with_cap(&mut self.recent_display_mark_lag_us, lag_us);
                }
                _ => {}
            }
            match event {
                ChannelEvent::SessionInitialized(session_id) => {
                    info!("app: session {} initialized", session_id);
                    self.connected = true;
                    // A fresh successful link clears any pending
                    // reconnect state. Whether this resulted from
                    // an auto-retry or a user clicking Reconnect,
                    // the cycle is done.
                    if self.reconnect_state != ReconnectState::Idle {
                        info!("app: reconnect succeeded, returning to Idle");
                        self.reconnect_state = ReconnectState::Idle;
                    }
                    self.awaiting_reconnect_outcome = false;
                    // Phase 09 (F1): surface the link in the bell
                    // history. Fires on initial connect and on
                    // every reconnect success; the 30 s dedup
                    // collapses storm reconnects to a single entry.
                    self.push_connection_event(
                        NotifySeverity::Info,
                        format!("Connected to {}:{}", self.target_host, self.target_port),
                    );

                    // Phase 5: spawn the auto-snapshot interval task.
                    // Retire-and-respawn per session: `reconnect()`
                    // replaces `self.traffic` and `self.channel_snapshots`
                    // with fresh instances, so any task spawned for the
                    // previous session is holding stale Arcs. Signal it
                    // to retire (it will exit within ~500 ms), then
                    // spawn a fresh task with the current Arcs.
                    if let Some(interval_secs) = self.auto_snapshot_interval {
                        if let Some(prev_cancel) = self.auto_snapshot_cancel.take() {
                            prev_cancel.store(true, Ordering::Relaxed);
                        }
                        let output_dir = self.manual_bug_report_dir().join("auto-snapshots");
                        let cap = self.auto_snapshot_cap;
                        let cancel = Arc::new(AtomicBool::new(false));
                        self.auto_snapshot_cancel = Some(cancel.clone());

                        // One-shot startup notification — fire only on
                        // the first session so reconnects don't spam.
                        if !self.auto_snapshot_startup_notified {
                            self.auto_snapshot_startup_notified = true;
                            self.push_notification(
                                NotifySeverity::Info,
                                NotificationSource::Internal,
                                format!(
                                    "Auto-snapshot mode enabled \
                                     — every {}s, max {} snapshots, \
                                     saving to {}",
                                    interval_secs,
                                    cap,
                                    output_dir.display(),
                                ),
                            );
                        }

                        let state = AutoSnapshotState {
                            traffic: self.traffic.clone(),
                            channel_snapshots: self.channel_snapshots.clone(),
                            app_snapshot: self.app_snapshot.clone(),
                            notifications: self.notifications.clone(),
                            target_host: self.target_host.clone(),
                            target_port: self.target_port,
                            output_dir,
                            interval: Duration::from_secs(interval_secs),
                            cap,
                            cancel,
                        };
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("auto-snapshot: failed to build tokio runtime");
                            rt.block_on(crate::auto_snapshot::run_auto_snapshot_loop(state));
                        });
                    }
                }

                ChannelEvent::SurfaceCreated {
                    display_channel_id,
                    surface_id,
                    width,
                    height,
                } => {
                    info!(
                        "app: surface {}:{} created: {}x{}",
                        display_channel_id, surface_id, width, height
                    );
                    self.surfaces.insert(
                        (display_channel_id, surface_id),
                        GuiSurface::new(surface_id, width, height),
                    );
                    if is_primary_surface(display_channel_id, surface_id) {
                        if auto_fit_size_acceptable(width, height) {
                            self.pending_resize = Some((width as f32, height as f32));
                            self.pending_resolution_notify =
                                Some(((width, height), Instant::now()));
                        } else {
                            warn!(
                                "app: ignoring oversized primary surface {}x{} for auto-fit \
                                 (limit {}px per axis)",
                                width, height, MAX_AUTO_FIT_DIMENSION
                            );
                        }
                    }
                }

                ChannelEvent::SurfaceDestroyed {
                    display_channel_id,
                    surface_id,
                } => {
                    info!(
                        "app: surface {}:{} destroyed",
                        display_channel_id, surface_id
                    );
                    self.surfaces.remove(&(display_channel_id, surface_id));
                }

                ChannelEvent::ImageReady {
                    display_channel_id,
                    surface_id,
                    left,
                    top,
                    width,
                    height,
                    pixels,
                    ..
                } => {
                    // Auto-create surface if the server draws before sending
                    // SURFACE_CREATE (QEMU does this for the primary surface).
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        self.surfaces.entry((display_channel_id, surface_id))
                    {
                        let surf_w = left + width;
                        let surf_h = top + height;
                        info!(
                            "app: auto-creating surface {} ({}x{}) from draw at ({},{})+{}x{}",
                            surface_id, surf_w, surf_h, left, top, width, height
                        );
                        e.insert(GuiSurface::new(surface_id, surf_w, surf_h));
                        if is_primary_surface(display_channel_id, surface_id) {
                            if auto_fit_size_acceptable(surf_w, surf_h) {
                                self.pending_resize = Some((surf_w as f32, surf_h as f32));
                                self.pending_resolution_notify =
                                    Some(((surf_w, surf_h), Instant::now()));
                            } else {
                                warn!(
                                    "app: ignoring oversized auto-created primary surface \
                                     {}x{} for auto-fit (limit {}px per axis)",
                                    surf_w, surf_h, MAX_AUTO_FIT_DIMENSION
                                );
                            }
                        }
                    }

                    let surface = self
                        .surfaces
                        .get_mut(&(display_channel_id, surface_id))
                        .unwrap()
                        .surface_mut();
                    surface.blit(left, top, width, height, &pixels);
                    self.stats.frames_received += 1;
                    debug!(
                        "app: blit surface={}, pos=({},{}), size={}x{}",
                        surface_id, left, top, width, height
                    );
                }

                ChannelEvent::ImageReadyChroma {
                    display_channel_id,
                    surface_id,
                    left,
                    top,
                    width,
                    height,
                    pixels,
                    chroma_rgba,
                    ..
                } => {
                    if let Some(gs) = self.surfaces.get_mut(&(display_channel_id, surface_id)) {
                        gs.surface_mut().blit_chroma(
                            left,
                            top,
                            width,
                            height,
                            &pixels,
                            chroma_rgba,
                        );
                        self.stats.frames_received += 1;
                    } else {
                        debug!("app: ImageReadyChroma on unknown surface {}", surface_id);
                    }
                }

                ChannelEvent::ImageReadyAlpha {
                    display_channel_id,
                    surface_id,
                    left,
                    top,
                    width,
                    height,
                    pixels,
                    alpha,
                    ..
                } => {
                    if let Some(gs) = self.surfaces.get_mut(&(display_channel_id, surface_id)) {
                        gs.surface_mut()
                            .blit_alpha(left, top, width, height, &pixels, alpha);
                        self.stats.frames_received += 1;
                    } else {
                        debug!("app: ImageReadyAlpha on unknown surface {}", surface_id);
                    }
                }

                ChannelEvent::FillRect {
                    display_channel_id,
                    surface_id,
                    rect: (left, top, right, bottom),
                    colour,
                    clip,
                } => {
                    if let Some(gs) = self.surfaces.get_mut(&(display_channel_id, surface_id)) {
                        gs.surface_mut()
                            .fill_rect(left, top, right, bottom, colour, &clip);
                        self.stats.frames_received += 1;
                    } else {
                        debug!("app: FillRect on unknown surface {}", surface_id);
                    }
                }

                ChannelEvent::CopyBits {
                    display_channel_id,
                    surface_id,
                    src_x,
                    src_y,
                    dest_rect: (left, top, right, bottom),
                    clip,
                } => {
                    if let Some(gs) = self.surfaces.get_mut(&(display_channel_id, surface_id)) {
                        gs.surface_mut()
                            .copy_bits(src_x, src_y, left, top, right, bottom, &clip);
                        self.stats.frames_received += 1;
                    } else {
                        debug!("app: CopyBits on unknown surface {}", surface_id);
                    }
                }

                ChannelEvent::Invert {
                    display_channel_id,
                    surface_id,
                    rect: (left, top, right, bottom),
                    clip,
                } => {
                    if let Some(gs) = self.surfaces.get_mut(&(display_channel_id, surface_id)) {
                        gs.surface_mut()
                            .invert_rect(left, top, right, bottom, &clip);
                        self.stats.frames_received += 1;
                    } else {
                        debug!("app: Invert on unknown surface {}", surface_id);
                    }
                }

                ChannelEvent::DisplayMark { .. } => {
                    // Frame boundary — record timestamp for FPS calculation
                    let now = Instant::now();
                    self.stats.frame_times.push(now);
                    if self.stats.frame_times.len() > FPS_WINDOW_SIZE {
                        self.stats.frame_times.remove(0);
                    }

                    // Capture a video frame if enabled. Phase-03:
                    // frame() is a non-blocking enqueue returning
                    // bool; false means the encoder task's queue
                    // was full and the frame was dropped.
                    if let Some(ref capture) = self.capture {
                        if let Some(surface) = self
                            .surfaces
                            .values()
                            .map(|gs| gs.surface())
                            .max_by_key(|s| (s.width as u64) * (s.height as u64))
                        {
                            if !capture.frame(0, surface.pixels(), surface.width, surface.height) {
                                self.video_drop_count = self.video_drop_count.saturating_add(1);
                            }
                        }
                    }
                }

                ChannelEvent::CursorPosition { x, y, visible } => {
                    debug!("app: cursor position: ({},{}) visible={}", x, y, visible);
                    self.cursor_pos = (x, y);
                    self.cursor_visible = visible;
                }

                ChannelEvent::CursorShape(img) => {
                    debug!(
                        "app: cursor shape: {}x{}, hot=({},{})",
                        img.width, img.height, img.hot_spot_x, img.hot_spot_y
                    );
                    self.cursor_image = Some(img);
                    self.cursor_texture = None; // force recreation
                }

                ChannelEvent::MouseMode(mode) => {
                    info!(
                        "app: mouse mode: {} ({})",
                        mode,
                        if mode == 1 { "server" } else { "client" }
                    );
                    self.mouse_mode = mode;
                }

                ChannelEvent::MonitorsConfig { width, height } => {
                    debug!("app: requested monitors config {}x{}", width, height);
                }

                ChannelEvent::Statistics {
                    bytes_in,
                    bytes_out,
                    ..
                } => {
                    self.stats.bytes_in += bytes_in;
                    self.stats.bytes_out += bytes_out;
                }

                ChannelEvent::Latency { sample_ms } => {
                    self.stats.last_latency_ms = Some(sample_ms as f64);
                    self.latency.record(sample_ms);
                }

                ChannelEvent::Error { channel, message } => {
                    error!("app: {} channel error: {}", channel.name(), message);
                    // Phase 09 (F1): surface the raw error in the
                    // bell history before the state-machine path
                    // can swallow it into the modal. handle_*
                    // below also pushes the resulting connection-
                    // lost / modal-entry notifications.
                    self.push_connection_event(
                        NotifySeverity::Error,
                        format!("{} channel error: {}", channel.name(), message),
                    );
                    // Snapshot before driving the state machine so
                    // the zip captures the run-up to the failure.
                    self.maybe_write_disconnect_snapshot(channel.name(), &message);
                    let reason = format!("{} channel error: {}", channel.name(), message);
                    self.handle_critical_disconnect(reason);
                }

                ChannelEvent::UsbChannelReady => {
                    info!("app: USB redirection channel connected");
                    self.usb_channel_ready = true;
                }

                ChannelEvent::UsbDeviceConnected(desc) => {
                    info!("app: USB device connected: {}", desc);
                    self.usb_device_description = Some(desc);
                    self.clear_usb_operation_flags();
                    self.usb_connected_at = Some(Instant::now());
                }

                ChannelEvent::UsbDeviceDisconnected => {
                    info!("app: USB device disconnected");
                    self.usb_device_description = None;
                    self.clear_usb_operation_flags();
                    self.usb_connected_at = None;
                }

                ChannelEvent::UsbConnectFailed(err) => {
                    error!("app: USB connect failed: {}", err);
                    self.clear_usb_operation_flags();
                    self.usb_error_message = Some(err);
                    self.usb_error_time = Some(Instant::now());
                }

                ChannelEvent::WebdavChannelReady => {
                    info!("app: WebDAV channel connected");
                    self.webdav_channel_ready = true;
                }

                ChannelEvent::WebdavSharingStarted { path, read_only } => {
                    info!("app: WebDAV sharing started: {} (ro={})", path, read_only);
                    self.webdav_shared_dir = Some(path);
                    self.webdav_read_only = read_only;
                    self.webdav_sharing_active = true;
                    self.webdav_connected_at = Some(Instant::now());
                }

                ChannelEvent::WebdavSharingStopped => {
                    info!("app: WebDAV sharing stopped");
                    self.webdav_shared_dir = None;
                    self.webdav_sharing_active = false;
                    self.webdav_connected_at = None;
                }

                ChannelEvent::WebdavError(err) => {
                    error!("app: WebDAV error: {}", err);
                    self.webdav_error_message = Some(err);
                    self.webdav_error_time = Some(Instant::now());
                }

                ChannelEvent::PasteCompleted { chars, elapsed_ms } => {
                    info!("app: paste complete: {} chars in {}ms", chars, elapsed_ms);
                    self.push_notification(
                        NotifySeverity::Info,
                        NotificationSource::Internal,
                        format!("Pasted {} chars ({}ms)", chars, elapsed_ms),
                    );
                }

                ChannelEvent::PasteFailed { reason } => {
                    error!("app: paste failed: {}", reason);
                    self.paste_error_message = Some(reason);
                }

                ChannelEvent::Notification(entry) => {
                    if let Ok(mut guard) = self.notifications.lock() {
                        guard.push(entry);
                    }
                }

                ChannelEvent::AgentConnected(connected) => {
                    info!("app: vdagent connected={}", connected);
                    self.agent_connected = connected;
                    // Phase 09 (F1): record the agent-state
                    // transition. Affects clipboard sync, paste,
                    // and resolution updates — useful for the
                    // user to see when those features come or go.
                    self.push_connection_event(
                        NotifySeverity::Info,
                        if connected {
                            "Guest agent connected"
                        } else {
                            "Guest agent disconnected"
                        }
                        .to_string(),
                    );
                }

                ChannelEvent::Disconnected(channel) => {
                    info!("app: channel {} disconnected", channel.name());

                    // Snapshot for every channel disconnect, including
                    // non-critical ones. Under ticket-based deployments
                    // (oVirt, Kerbside) a dropped channel is permanently
                    // lost — the user silently loses audio / USB / etc.
                    // Even if the session keeps running, we want
                    // diagnostic data on why the channel went down.
                    self.maybe_write_disconnect_snapshot(
                        channel.name(),
                        &format!("channel {} disconnected", channel.name()),
                    );

                    // Channel-specific cleanup
                    if channel == ChannelType::Usbredir {
                        self.usb_channel_ready = false;
                        self.usb_device_description = None;
                        self.clear_usb_operation_flags();
                        self.usb_connected_at = None;
                    }
                    if channel == ChannelType::Webdav {
                        self.webdav_channel_ready = false;
                        self.webdav_shared_dir = None;
                        self.webdav_sharing_active = false;
                        self.webdav_connected_at = None;
                    }

                    // Only the critical channels drive the
                    // session-level reconnect state. Non-critical
                    // channels (USB, WebDAV, Cursor, Playback) have
                    // independent lifecycles; their disconnects
                    // are snapshotted above but do not tear down
                    // the session.
                    match channel {
                        ChannelType::Main | ChannelType::Display | ChannelType::Inputs => {
                            let reason = format!(
                                "Connection lost ({} channel disconnected)",
                                channel.name()
                            );
                            // Snapshot was already written above
                            // by the unconditional call near the
                            // top of this arm; the helper now only
                            // drives the state machine.
                            self.handle_critical_disconnect(reason);
                        }
                        _ => {
                            debug!(
                                "app: non-critical channel {} disconnected, session continues",
                                channel.name()
                            );
                            // Phase 09 (F1): non-critical
                            // disconnect goes to the bell as Info
                            // (was debug!-only before). Surfaces
                            // e.g. usbredir / webdav drops without
                            // disrupting the user.
                            self.push_connection_event(
                                NotifySeverity::Info,
                                format!("{} channel disconnected", channel.name()),
                            );
                        }
                    }
                }

                _ => {}
            }
        }

        self.update_app_snapshot();
    }

    /// Clear USB operation-in-progress flags.
    fn clear_usb_operation_flags(&mut self) {
        self.usb_connecting = false;
        self.usb_disconnecting = false;
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

        if !self.enable_paste || self.agent_connected {
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

    /// Sync app-level state to the shared snapshot.
    fn update_app_snapshot(&self) {
        let mut snap = self.app_snapshot.lock().unwrap();

        // FPS from sliding-window frame_times
        snap.fps = if self.stats.frame_times.len() >= 2 {
            let oldest = self.stats.frame_times.first().unwrap();
            let newest = self.stats.frame_times.last().unwrap();
            let elapsed = newest.duration_since(*oldest).as_secs_f64();
            if elapsed > 0.0 {
                (self.stats.frame_times.len() - 1) as f64 / elapsed
            } else {
                0.0
            }
        } else {
            0.0
        };

        snap.bandwidth_history = self.bandwidth.history.iter().copied().collect();
        snap.bandwidth_current = self.bandwidth.history.back().copied().unwrap_or(0.0);
        snap.last_latency_ms = self.stats.last_latency_ms;
        snap.frames_received = self.stats.frames_received;
        snap.surfaces = self
            .surfaces
            .values()
            .map(|gs| {
                let s = gs.surface();
                SurfaceInfo {
                    surface_id: s.id,
                    width: s.width,
                    height: s.height,
                }
            })
            .collect();
        snap.cursor_pos = self.cursor_pos;
        snap.cursor_visible = self.cursor_visible;
        snap.mouse_mode = self.mouse_mode;
        snap.connected = self.connected;
        snap.uptime_secs = self.traffic.elapsed().as_secs_f64();
        snap.auto_reconnect_count = self.auto_reconnect_count;
        snap.video_drop_count = self.video_drop_count;

        // Phase-04: render-side latency aggregates.
        let (img_min, img_max, img_mean) = recent_lag_stats(&self.recent_image_ready_lag_us);
        snap.image_ready_lag_recent_min_us = img_min;
        snap.image_ready_lag_recent_max_us = img_max;
        snap.image_ready_lag_recent_mean_us = img_mean;
        let (mark_min, mark_max, mark_mean) = recent_lag_stats(&self.recent_display_mark_lag_us);
        snap.display_mark_lag_recent_min_us = mark_min;
        snap.display_mark_lag_recent_max_us = mark_max;
        snap.display_mark_lag_recent_mean_us = mark_mean;
    }

    /// Clone the largest surface's RGBA pixels, capture trigger
    /// timestamps, and spawn a background thread to PNG-encode.
    ///
    /// Called from every dialog-open path. Idempotent — a no-op
    /// when a snapshot is already pending, so call sites can
    /// fire it unconditionally without worrying about a second
    /// open stomping the first.
    ///
    /// The operator's rule is to always capture here, regardless
    /// of which report type the dialog was opened with, so the
    /// artefact survives the form-filling delay. The decision to
    /// *include* the resulting PNG in the zip happens at submit
    /// time in `BugReport::assemble`, which drops it for
    /// non-Display submissions.
    fn begin_trigger_snapshot(&mut self) {
        if self.pending_trigger.is_some() {
            return;
        }
        let triggered_at = chrono_now();
        let triggered_uptime_secs = self.traffic.elapsed().as_secs_f64();

        // Pre-first-SURFACE_CREATE: no surface to snap. Record
        // timestamps anyway and seed an Err into the slot so the
        // submit path falls back cleanly to live encoding (which
        // will also produce None when there's no surface).
        let Some(surface) = self
            .surfaces
            .values()
            .map(|gs| gs.surface())
            .max_by_key(|s| (s.width as u64) * (s.height as u64))
        else {
            let slot = Arc::new(std::sync::Mutex::new(Some(Err(anyhow::anyhow!(
                "no surface available at trigger time"
            )))));
            self.pending_trigger = Some(TriggerSnapshot {
                triggered_at,
                triggered_uptime_secs,
                png_slot: slot,
            });
            return;
        };

        let pixels: Vec<u8> = surface.pixels().to_vec();
        let width = surface.width;
        let height = surface.height;
        let slot: Arc<std::sync::Mutex<Option<anyhow::Result<Vec<u8>>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_for_thread = Arc::clone(&slot);

        let spawned = std::thread::Builder::new()
            .name("ryll-bugreport-png".to_string())
            .spawn(move || {
                let result = encode_png(&pixels, width, height);
                // If the Mutex is poisoned the UI thread panicked;
                // nothing to write and the process is already
                // tearing down.
                if let Ok(mut guard) = slot_for_thread.lock() {
                    *guard = Some(result);
                }
            });
        if let Err(e) = spawned {
            // Thread spawn failed (OS resource pressure); leave
            // the slot empty so the submit path falls back to
            // live encoding.
            debug!("app: failed to spawn PNG encoder thread: {}", e);
        }

        self.pending_trigger = Some(TriggerSnapshot {
            triggered_at,
            triggered_uptime_secs,
            png_slot: slot,
        });
    }

    /// Drop the pending snapshot without consuming it. The
    /// encoder thread (if still running) keeps its Arc clone and
    /// eventually writes into a Mutex whose last reference it
    /// holds, which is then dropped. Safe when no snapshot is
    /// pending.
    fn discard_trigger_snapshot(&mut self) {
        let _ = self.pending_trigger.take();
    }

    /// Called from `finish_bug_report`. Returns the captured
    /// trigger timestamps (if any) and the encoded PNG bytes (if
    /// the worker has finished with `Ok`). Uses `try_lock` so a
    /// still-running encoder on a large surface can't block the
    /// UI thread — `None` falls back to live encoding inside
    /// `BugReport::assemble`.
    fn take_trigger_for_submit(&mut self) -> (Option<TriggerTimestamps>, Option<Vec<u8>>) {
        let Some(snap) = self.pending_trigger.take() else {
            return (None, None);
        };
        // The `.ok()` intentionally discards any `Err` written
        // by the encoder thread: an encode failure (or the
        // no-surface-at-trigger-time sentinel seeded by
        // `begin_trigger_snapshot`) is indistinguishable here
        // from "the worker hadn't finished yet", and in both
        // cases we want the submit path to fall back to live
        // encoding of the submit-time surface. If we ever need
        // to surface the error to the user, it has to be logged
        // here *and* handled in the live-encode fallback.
        let png = match snap.png_slot.try_lock() {
            Ok(mut guard) => guard.take().and_then(|r| r.ok()),
            Err(_) => None,
        };
        let trigger = TriggerTimestamps {
            triggered_at: snap.triggered_at,
            triggered_uptime_secs: snap.triggered_uptime_secs,
        };
        (Some(trigger), png)
    }

    /// Generate a bug report and write it to disk.
    /// Returns the path of the written zip file.
    ///
    /// `trigger` carries the trigger-time timestamps captured
    /// when the dialog opened; `precomputed_screenshot_png`
    /// carries the PNG that the background encoder produced
    /// from the trigger-time surface. Both are `None` if the
    /// dialog wasn't open when this was called (e.g. the
    /// dev-only F8 path used to trigger reports without a
    /// dialog, though that doesn't exist today) — `BugReport`
    /// falls back to the submit-time behaviour in that case.
    pub fn generate_bug_report(
        &self,
        report_type: BugReportType,
        description: String,
        region: Option<ReportRegion>,
        trigger: Option<TriggerTimestamps>,
        precomputed_screenshot_png: Option<Vec<u8>>,
    ) -> anyhow::Result<std::path::PathBuf> {
        // Keep the live surface-pixels fallback path. It's the
        // safety net when the background encoder wasn't spawned
        // or hasn't finished; phase 3 will also reuse this to
        // produce the submit-time region crop.
        let surface_data = if report_type == BugReportType::Display {
            self.surfaces
                .values()
                .map(|gs| gs.surface())
                .max_by_key(|s| (s.width as u64) * (s.height as u64))
                .map(|s| (s.pixels(), s.width, s.height))
        } else {
            None
        };

        let report = BugReport::new(
            report_type,
            description,
            region,
            &self.target_host,
            self.target_port,
            &self.traffic,
            &self.channel_snapshots,
            &self.app_snapshot,
            &self.notifications,
            surface_data,
            trigger,
            precomputed_screenshot_png,
        )?;

        let output_dir = self.manual_bug_report_dir();
        report.write_zip(&output_dir)
    }

    /// Resolve the output directory for a manual (F8) or
    /// auto-disconnect bug report. Priority:
    ///   1. --bug-report-dir if set
    ///   2. <--capture>/bug-reports/ if --capture is set
    ///   3. current working directory
    fn manual_bug_report_dir(&self) -> PathBuf {
        if let Some(d) = &self.bug_report_dir {
            return d.clone();
        }
        match &self.capture {
            Some(cap) => cap.dir.join("bug-reports"),
            None => std::env::current_dir().unwrap_or_else(|_| ".".into()),
        }
    }

    /// Auto-write a disconnect-snapshot bug report, best-effort.
    /// Subject to a 60 s cooldown to bound disk usage during a
    /// disconnect storm. Failures are logged but never block
    /// the disconnect modal. Runtime metrics are reported as
    /// unavailable here (a 1 s sample on the GUI thread would
    /// freeze the UI); pcap and snapshots are the load-bearing
    /// data for diagnosing the disconnect.
    fn maybe_write_disconnect_snapshot(&mut self, channel: &str, message: &str) {
        if let Some(at) = self.last_disconnect_report_at {
            if at.elapsed() < Duration::from_secs(60) {
                debug!(
                    "app: disconnect snapshot cooldown active ({}s remaining), skipping for {}",
                    60u64.saturating_sub(at.elapsed().as_secs()),
                    channel
                );
                return;
            }
        }

        let keepalive_timeout_fired = self
            .channel_snapshots
            .main
            .lock()
            .map(|s| s.keepalive_timeout_fired)
            .unwrap_or(false);

        let cause = crate::bugreport::DisconnectCause {
            channel: channel.to_string(),
            error_message: message.to_string(),
            error_kind: None,
            keepalive_timeout_fired,
            session_uptime_secs: self.traffic.elapsed().as_secs_f64(),
            per_channel: crate::bugreport::DisconnectCause::collect_per_channel(
                &self.channel_snapshots,
            ),
        };

        let runtime_metrics = shakenfist_spice_renderer::metrics::RuntimeMetrics::unavailable(
            "runtime metrics are not sampled on the GUI thread for auto-disconnect snapshots",
        );

        let output_dir = self.manual_bug_report_dir();
        match BugReport::write_disconnect(
            &output_dir,
            cause,
            &self.target_host,
            self.target_port,
            &self.traffic,
            &self.channel_snapshots,
            &self.app_snapshot,
            &self.notifications,
            runtime_metrics,
        ) {
            Ok(path) => {
                info!("app: disconnect snapshot saved to {}", path.display());
                self.push_notification(
                    NotifySeverity::Info,
                    NotificationSource::BugReport,
                    format!("Disconnect snapshot saved to {}", path.display()),
                );
                self.last_disconnect_report_at = Some(Instant::now());
            }
            Err(e) => {
                error!("app: failed to write disconnect snapshot: {}", e);
                // Still update the cooldown so a write that fails
                // for an environmental reason (no disk space, bad
                // dir) doesn't retry on every disconnect event.
                self.last_disconnect_report_at = Some(Instant::now());
            }
        }
    }

    /// Push a notification entry into the shared store. After
    /// the push lands, also capture a `TrafficBuffers`
    /// snapshot keyed by the entry's id (Phase 10 / F2) so
    /// the "File bug report" button on the notification can
    /// later produce a report with pcap/state from the
    /// moment the notification fired.
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

    /// Push a connection-state transition (Phase 09 / F1).
    /// Wraps `push_notification` with
    /// `NotificationSource::Connection` so every connection
    /// event lands under a single label in the side panel.
    fn push_connection_event(&self, severity: NotifySeverity, message: impl Into<String>) {
        self.push_notification(severity, NotificationSource::Connection, message);
    }

    /// Phase 10 (F2): user clicked "File bug report" on a
    /// notification row. Looks up the notification, consumes
    /// any live snapshot for it, and writes a zip via
    /// `BugReport::write_notification`. Always produces a
    /// report — when no live snapshot is available, falls
    /// back to the current live `TrafficBuffers` and tags the
    /// report `PostEventOnly`.
    fn file_notification_bug_report(&mut self, id: u64) {
        // Resolve the notification entry first so we can use
        // its severity/message in the report description.
        let entry = match self.notifications.lock() {
            Ok(guard) => guard.iter_newest_first().find(|e| e.id == id).cloned(),
            Err(_) => None,
        };
        let Some(entry) = entry else {
            warn!(
                "app: file_notification_bug_report({}) — entry no longer in store",
                id
            );
            return;
        };

        // Take the live snapshot (if any). If absent or
        // expired, fall back to the current live traffic.
        let now = Instant::now();
        let (traffic_owned, snapshot_state): (Option<TrafficBuffers>, NotificationSnapshotState) = {
            match self.notification_snapshots.lock() {
                Ok(mut store) => match store.take(id, now) {
                    Some(snap) => (Some(snap), NotificationSnapshotState::AtFire),
                    None => (None, NotificationSnapshotState::PostEventOnly),
                },
                Err(_) => (None, NotificationSnapshotState::PostEventOnly),
            }
        };
        let traffic_ref: &TrafficBuffers = match &traffic_owned {
            Some(s) => s,
            None => self.traffic.as_ref(),
        };

        let output_dir = self.manual_bug_report_dir();
        let runtime_metrics = shakenfist_spice_renderer::metrics::RuntimeMetrics::unavailable(
            "runtime metrics are not sampled on the GUI thread for notification \
                 bug reports — the pcap and channel snapshots are the load-bearing data",
        );

        match BugReport::write_notification(
            &output_dir,
            &entry,
            snapshot_state,
            &self.target_host,
            self.target_port,
            traffic_ref,
            &self.channel_snapshots,
            &self.app_snapshot,
            &self.notifications,
            runtime_metrics,
        ) {
            Ok(path) => {
                info!("app: notification bug report saved to {}", path.display());
                self.push_notification(
                    NotifySeverity::Info,
                    NotificationSource::BugReport,
                    format!("Bug report saved to {}", path.display()),
                );
            }
            Err(e) => {
                error!("app: failed to write notification bug report: {}", e);
                self.push_notification(
                    NotifySeverity::Error,
                    NotificationSource::BugReport,
                    format!("Bug report failed: {}", e),
                );
            }
        }
    }

    /// Run a bug report and set the status bar message from the result.
    fn finish_bug_report(
        &mut self,
        report_type: BugReportType,
        description: String,
        region: Option<ReportRegion>,
    ) {
        let (trigger, precomputed_png) = self.take_trigger_for_submit();
        match self.generate_bug_report(report_type, description, region, trigger, precomputed_png) {
            Ok(path) => {
                let msg = format!("Bug report saved to {}", path.display());
                info!("app: {}", msg);
                self.push_notification(NotifySeverity::Info, NotificationSource::BugReport, msg);
            }
            Err(e) => {
                let msg = format!("Bug report failed: {}", e);
                error!("app: {}", msg);
                self.push_notification(NotifySeverity::Error, NotificationSource::BugReport, msg);
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Don't forward input to the SPICE server when
        // the bug report dialog or region selection is active.
        if self.show_bug_dialog || self.region_select_active {
            return;
        }

        let input_tx = match &self.input_tx {
            Some(tx) => tx.clone(),
            None => return,
        };

        // Handle keyboard input — read from the global input state so
        // key events are captured regardless of which widget has focus.
        ctx.input(|i| {
            let mods = i.modifiers;
            let prev = self.last_modifiers.unwrap_or_default();

            if mods.ctrl != prev.ctrl {
                let code = 0x1D; // Left Ctrl
                if mods.ctrl {
                    let _ = input_tx.try_send(InputEvent::KeyDown(code));
                } else {
                    let _ = input_tx.try_send(InputEvent::KeyUp(code | 0x80));
                }
            }
            if mods.shift != prev.shift {
                let code = 0x2A; // Left Shift
                if mods.shift {
                    let _ = input_tx.try_send(InputEvent::KeyDown(code));
                } else {
                    let _ = input_tx.try_send(InputEvent::KeyUp(code | 0x80));
                }
            }
            if mods.alt != prev.alt {
                let code = 0x38; // Left Alt
                if mods.alt {
                    let _ = input_tx.try_send(InputEvent::KeyDown(code));
                } else {
                    let _ = input_tx.try_send(InputEvent::KeyUp(code | 0x80));
                }
            }

            self.last_modifiers = Some(mods);

            for event in &i.events {
                if let egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat: false,
                    ..
                } = event
                {
                    let lookup_key = physical_key.unwrap_or(*key);
                    if lookup_key == egui::Key::F11 || lookup_key == egui::Key::F12 {
                        continue;
                    }
                    if let Some((down_code, up_code)) =
                        egui_key_to_logical(lookup_key).and_then(scancode_for_logical_key)
                    {
                        let ev = if *pressed {
                            InputEvent::KeyDown(down_code)
                        } else {
                            InputEvent::KeyUp(up_code)
                        };
                        let _ = input_tx.try_send(ev);
                    }
                }
            }
        });
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

    fn maybe_send_monitors_resize(&mut self, ctx: &egui::Context) {
        let Some(tx) = &self.resize_tx else {
            return;
        };

        let viewport_size = ctx.input(|i| {
            i.viewport()
                .inner_rect
                .map(|rect| rect.size())
                .unwrap_or_else(|| i.screen_rect().size())
        });

        let is_max = ctx.input(|i| {
            i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)
        });

        let (width, height) = compute_outgoing_resize((viewport_size.x, viewport_size.y), is_max);

        if self.last_sent_resize == Some((width, height)) {
            return;
        }

        if tx.try_send((width, height)).is_ok() {
            self.last_sent_resize = Some((width, height));
        }
    }

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
        if self.surfaces.is_empty() {
            anyhow::bail!("No display surfaces to capture");
        }

        let mut sorted: Vec<_> = self.surfaces.iter().collect();
        sorted.sort_by_key(|(k, _)| *k);

        let paths = screenshot_paths(&base_path, sorted.len());

        let mut written = Vec::new();
        for ((_, gs), path) in sorted.into_iter().zip(paths) {
            let surface = gs.surface();
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
    fn open_screenshot_dialog(&mut self) {
        if self.surfaces.is_empty() {
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

impl eframe::App for RyllApp {
    // eframe 0.34 promoted `ui()` to the required trait method and
    // deprecated `update()`. We adopt the minimum-diff migration:
    // take the provided `Ui`, pull the `Context` out of it, and
    // run the original body unchanged. The existing panel/window
    // calls already wrap themselves on the `Context`, so the
    // `ui` parameter is intentionally unused at this layer. See
    // docs/plans/PLAN-egui-0.34-followups.md for the proper
    // restructure.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx();
        // Mirror egui's per-frame focus state into the shared
        // AtomicBool that the FocusGatedClipboard reads. The
        // value flips on the same frame egui sees the
        // platform-level focus event, so the next 500 ms
        // clipboard tick on the renderer side will see the
        // updated state.
        let focused = ctx.input(|i| i.focused);
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

        // Phase 10 (F2): prune expired notification snapshots
        // at most once per second so the per-row File-as-bug-
        // report button visual state honestly reflects the
        // 60 s TTL without walking the map on every paint.
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
                if let Some(variant) = self.reconnect_policy().forbid_retry(SystemTime::now()) {
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
                    // Phase 09 (F1): surface the attempt fire in
                    // the bell history. Per-attempt number in the
                    // message keeps successive attempts distinct
                    // across the 30 s dedup window.
                    self.push_connection_event(
                        NotifySeverity::Info,
                        format!("Reconnect attempt {}/{}…", attempt, MAX_RECONNECT_ATTEMPTS),
                    );
                    self.awaiting_reconnect_outcome = true;
                    self.reconnect();
                }
            }
        }

        // Resize viewport to match the remote surface (plus stats
        // bar) whenever a new primary surface differs from the
        // size we last fitted to. Maximised/fullscreen windows
        // are left alone — we cannot meaningfully change their
        // inner size, and the surface will render at native size
        // inside the available area.
        let pending = self.pending_resize.take();
        let is_max = ctx.input(|i| {
            i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)
        });
        if let Some((w, h, aw, ah)) =
            compute_auto_resize(pending, self.last_auto_resize, is_max, self.obey_guest_size)
        {
            let total_h = h + STATS_BAR_HEIGHT;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, total_h)));
            // Seed last_sent_resize so maybe_send_monitors_resize
            // doesn't echo our own resize back to the guest as a
            // VDAgentMonitorsConfig change.
            self.last_sent_resize = Some((aw, ah));
            self.last_auto_resize = Some((aw, ah));
            info!("app: window resize to {}x{} (surface)", w as u32, h as u32);
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

        self.maybe_send_monitors_resize(ctx);

        // Tick the bandwidth tracker
        self.bandwidth.tick();

        // Escape during region selection: skip and generate without region
        if self.region_select_active {
            let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc {
                let report_type = self.bug_report_type.clone();
                let description = self.bug_description.clone();
                self.finish_bug_report(report_type, description, None);
                self.region_select_active = false;
                self.region_drag_start = None;
                self.region_drag_end = None;
            }
        }

        // F12 toggles bug report dialog (not during region selection)
        if !self.region_select_active {
            let f12_pressed = ctx.input(|i| i.key_pressed(egui::Key::F12));
            if f12_pressed {
                if self.show_bug_dialog {
                    self.show_bug_dialog = false;
                    self.discard_trigger_snapshot();
                } else {
                    self.show_bug_dialog = true;
                    self.bug_report_type = BugReportType::Display;
                    self.bug_description.clear();
                    self.begin_trigger_snapshot();
                }
            }
        }

        // F11 toggles traffic viewer (not during region selection)
        if !self.region_select_active {
            let f11_pressed = ctx.input(|i| i.key_pressed(egui::Key::F11));
            if f11_pressed {
                self.show_traffic_viewer = !self.show_traffic_viewer;
            }
        }

        // F8 opens screenshot save dialog (not during region selection)
        if !self.region_select_active {
            let f8_pressed = ctx.input(|i| i.key_pressed(egui::Key::F8));
            if f8_pressed {
                self.open_screenshot_dialog();
            }
        }

        // Escape closes the dialog
        if self.show_bug_dialog {
            let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc {
                self.show_bug_dialog = false;
                self.discard_trigger_snapshot();
            }
        }

        // Ctrl+Alt+V triggers paste-as-keystrokes (not during
        // region selection or dialogs)
        let mut paste_triggered = false;
        if !self.region_select_active && !self.show_bug_dialog && self.paste_error_message.is_none()
        {
            let ctrl_alt_v =
                ctx.input(|i| i.modifiers.ctrl && i.modifiers.alt && i.key_pressed(egui::Key::V));
            if ctrl_alt_v {
                paste_triggered = self.trigger_paste();
            }
        }

        // Handle input (skip if paste was triggered this frame)
        if !paste_triggered {
            self.handle_input(ctx);
        }

        // Handle cadence mode
        self.handle_cadence();

        // Refresh traffic viewer entries periodically
        if self.show_traffic_viewer
            && !self.traffic_viewer_paused
            && self.traffic_viewer_last_refresh.elapsed()
                >= Duration::from_millis(TRAFFIC_VIEWER_REFRESH_MS)
        {
            self.traffic_viewer_entries =
                self.traffic.recent_view_entries(TRAFFIC_VIEWER_MAX_ENTRIES);
            self.traffic_viewer_last_refresh = Instant::now();
        }

        // Statistics panel (bottom) — rendered before CentralPanel
        // so egui reserves its space correctly.
        let is_maximized = ctx.input(|i| {
            i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)
        });

        let stats_frame = egui::Frame::none()
            .inner_margin(egui::Margin::symmetric(4, 2))
            .fill(ctx.style().visuals.panel_fill);
        // Phase 8: compute the live streaming-state classification
        // once per frame. The result drives the status-bar
        // indicator below; a fired notification (Flapping +
        // cool-down elapsed) is pushed before we render so the
        // bell can pick it up on the same frame.
        let (streaming_state, streams_active_for_tooltip) = {
            let snap = self.channel_snapshots.display.lock().unwrap();
            let (state, notif) = streaming_state::classify(
                &snap,
                Instant::now(),
                self.traffic.session_start(),
                self.last_flap_notification_ts,
            );
            let active = snap.streams_active.clone();
            drop(snap);
            if let Some(notification) = notif {
                self.last_flap_notification_ts = Some(Instant::now());
                self.push_notification(
                    NotifySeverity::Warn,
                    NotificationSource::Internal,
                    notification.message,
                );
            }
            (state, active)
        };

        egui::TopBottomPanel::bottom("stats")
            .frame(stats_frame)
            .show_animated(ctx, !is_maximized, |ui| {
                ui.horizontal(|ui| {
                    if !self.latency.history.is_empty() {
                        ui.label(format!("Latency: {}", self.latency.label()));
                    }
                    if self.latency.history.len() >= 2 {
                        let max_val = self.latency.history.iter().cloned().fold(1.0f32, f32::max);
                        let sparkline_w = 80.0;
                        let sparkline_h = 12.0;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(sparkline_w, sparkline_h),
                            egui::Sense::hover(),
                        );
                        let painter = ui.painter_at(rect);
                        let n = self.latency.history.len();
                        let bar_w = sparkline_w / n as f32;
                        for (i, &val) in self.latency.history.iter().enumerate() {
                            let h = (val / max_val) * sparkline_h;
                            let x = rect.min.x + i as f32 * bar_w;
                            let bar = egui::Rect::from_min_max(
                                egui::pos2(x, rect.max.y - h),
                                egui::pos2(x + bar_w - 0.5, rect.max.y),
                            );
                            painter.rect_filled(bar, 0.0, egui::Color32::from_rgb(180, 140, 80));
                        }
                    }
                    if !self.latency.history.is_empty() {
                        ui.separator();
                    }

                    // Sliding-window FPS from DisplayMark timestamps
                    if self.stats.frame_times.len() >= 2 {
                        let oldest = self.stats.frame_times.first().unwrap();
                        let newest = self.stats.frame_times.last().unwrap();
                        let elapsed = newest.duration_since(*oldest).as_secs_f64();
                        if elapsed > 0.0 {
                            let fps = (self.stats.frame_times.len() - 1) as f64 / elapsed;
                            ui.label(format!("FPS: {:.1}", fps));
                        }
                    }

                    if self.cadence_enabled {
                        ui.separator();
                        ui.label("Cadence: ON");
                    }

                    // Phase 5: show auto-snapshot counter only when the mode
                    // is active (hiding the line avoids visual noise in normal
                    // sessions).
                    if let Some(_interval_secs) = self.auto_snapshot_interval {
                        let (saved, _pruned) = self
                            .app_snapshot
                            .lock()
                            .map(|s| (s.auto_snapshots_saved, s.auto_snapshots_pruned))
                            .unwrap_or((0, 0));
                        ui.separator();
                        ui.label(format!(
                            "Auto-snapshot: {}/{}",
                            saved, self.auto_snapshot_cap
                        ));
                    }

                    if let ReconnectState::Pending { attempt, .. } = &self.reconnect_state {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 60),
                            format!("Reconnecting… ({}/{})", attempt, MAX_RECONNECT_ATTEMPTS),
                        );
                    }

                    if let Some(ref desc) = self.usb_device_description {
                        ui.separator();
                        ui.label(format!("USB: {}", desc));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let vol = &self.volume_control;
                        let mut muted = vol.muted();
                        if ui.small_button(if muted { "🔇" } else { "🔊" }).clicked() {
                            muted = !muted;
                            vol.set_muted(muted);
                        }
                        let mut v = vol.volume() as f32;
                        let slider = egui::Slider::new(&mut v, 0.0..=100.0).show_value(false);
                        if ui
                            .add_sized([80.0, ui.available_height()], slider)
                            .changed()
                        {
                            vol.set_volume(v as u8);
                        }

                        // Phase 8: live streaming indicator. Sits to
                        // the left of the volume controls (which stay
                        // rightmost as the operator-action zone) and
                        // to the right of the bandwidth sparkline.
                        // Glyph: ▶ (U+25B6) — a Unicode triangle that
                        // egui's default font renders cleanly on
                        // every platform we ship to. 📹 is a colour
                        // emoji and renders as a tofu box in egui's
                        // monochrome font; ▶ keeps the visual cue
                        // (something "playing") without the font
                        // problem.
                        let (icon_colour, tooltip_lines): (egui::Color32, Vec<String>) =
                            match &streaming_state {
                                StreamingState::Off => (
                                    egui::Color32::from_rgb(120, 120, 120),
                                    vec!["No streams active".to_string()],
                                ),
                                StreamingState::Active => {
                                    let mut lines =
                                        Vec::with_capacity(streams_active_for_tooltip.len() + 1);
                                    lines.push(format!(
                                        "{} active stream{}",
                                        streams_active_for_tooltip.len(),
                                        if streams_active_for_tooltip.len() == 1 {
                                            ""
                                        } else {
                                            "s"
                                        },
                                    ));
                                    let now_secs = self.traffic.elapsed().as_secs_f64();
                                    for s in &streams_active_for_tooltip {
                                        let codec = match s.codec_type {
                                            1 => "MJPEG".to_string(),
                                            2 => "VP8".to_string(),
                                            3 => "H264".to_string(),
                                            4 => "VP9".to_string(),
                                            5 => "H265".to_string(),
                                            other => format!("codec{}", other),
                                        };
                                        let lifetime = (now_secs - s.created_at_secs).max(0.0);
                                        lines.push(format!(
                                            "codec={} {}x{} frames={} lifetime={:.0}s",
                                            codec,
                                            s.stream_width,
                                            s.stream_height,
                                            s.frames_decoded_ok,
                                            lifetime,
                                        ));
                                    }
                                    (egui::Color32::from_rgb(60, 180, 60), lines)
                                }
                                StreamingState::RecentlyDestroyed { secs_since } => (
                                    egui::Color32::from_rgb(220, 160, 60),
                                    vec![format!("Last stream destroyed {:.1}s ago", secs_since)],
                                ),
                                StreamingState::Flapping {
                                    destroys_in_window,
                                    window_secs,
                                    mean_lifetime_secs,
                                } => (
                                    egui::Color32::from_rgb(220, 60, 60),
                                    vec![format!(
                                        "Streams flapping: {} destroys in {:.0} s, \
                                         mean lifetime {:.1} s",
                                        destroys_in_window, window_secs, mean_lifetime_secs,
                                    )],
                                ),
                            };
                        let icon_text = egui::RichText::new("\u{25B6}").color(icon_colour);
                        let icon_resp = ui.label(icon_text);
                        icon_resp.on_hover_ui(|ui| {
                            for line in &tooltip_lines {
                                ui.label(line);
                            }
                        });

                        ui.separator();

                        ui.allocate_ui_with_layout(
                            egui::vec2(75.0, ui.available_height()),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(self.bandwidth.label());
                            },
                        );
                        if self.bandwidth.history.len() >= 2 {
                            let max_val = self
                                .bandwidth
                                .history
                                .iter()
                                .cloned()
                                .fold(1.0f32, f32::max);
                            let sparkline_w = 80.0;
                            let sparkline_h = 12.0;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(sparkline_w, sparkline_h),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter_at(rect);
                            let n = self.bandwidth.history.len();
                            let bar_w = sparkline_w / n as f32;
                            for (i, &val) in self.bandwidth.history.iter().enumerate() {
                                let h = (val / max_val) * sparkline_h;
                                let x = rect.min.x + i as f32 * bar_w;
                                let bar = egui::Rect::from_min_max(
                                    egui::pos2(x, rect.max.y - h),
                                    egui::pos2(x + bar_w - 0.5, rect.max.y),
                                );
                                painter.rect_filled(bar, 0.0, egui::Color32::from_rgb(80, 180, 80));
                            }
                        }

                        // Bell notification button
                        let (unread_count, bell_severity) = self
                            .notifications
                            .lock()
                            .map(|s| (s.unread_count(), s.highest_bell_severity()))
                            .unwrap_or((0, None));

                        let mut bell_text = egui::RichText::new("\u{1F514}");
                        if let Some(sev) = bell_severity {
                            let (_, colour) = notifications::severity_visuals(sev);
                            if let Some(c) = colour {
                                bell_text = bell_text.color(c);
                            }
                        }
                        let bell_button = ui.add(egui::Button::new(bell_text));
                        let bell_button = if unread_count > 0 {
                            bell_button.on_hover_text(format!(
                                "{} unread notification{}",
                                unread_count,
                                if unread_count == 1 { "" } else { "s" },
                            ))
                        } else {
                            bell_button
                        };
                        if bell_button.clicked() {
                            self.show_notifications_panel = !self.show_notifications_panel;
                        }

                        ui.separator();
                        egui::menu::menu_button(ui, "☰", |ui| {
                            ui.checkbox(&mut self.obey_guest_size, "Obey guest size hints");
                            ui.separator();
                            ui.checkbox(&mut self.show_traffic_viewer, "Traffic");
                            ui.checkbox(&mut self.show_usb_panel, "USB");
                            ui.checkbox(&mut self.show_webdav_panel, "Folders");
                            if ui
                                .add(egui::Button::new("Screenshot").shortcut_text("F8"))
                                .clicked()
                            {
                                self.open_screenshot_dialog();
                                ui.close_menu();
                            }
                            if ui
                                .add(egui::Button::new("Report").shortcut_text("F12"))
                                .clicked()
                            {
                                self.show_bug_dialog = true;
                                self.bug_report_type = BugReportType::Display;
                                self.bug_description.clear();
                                self.begin_trigger_snapshot();
                                ui.close_menu();
                            }
                            if self.enable_paste {
                                ui.separator();
                                let label = egui::Button::new("Paste").shortcut_text("Ctrl+Alt+V");
                                let enabled = !self.agent_connected;
                                let response = ui.add_enabled(enabled, label);
                                if response.clicked() {
                                    self.trigger_paste();
                                    ui.close_menu();
                                }
                                if !enabled {
                                    response.on_disabled_hover_text(
                                        "vdagent is connected — use Ctrl+V instead",
                                    );
                                }
                            }
                        });
                    });
                });
            });

        // Traffic viewer side panel (conditional)
        if self.show_traffic_viewer {
            egui::SidePanel::right("traffic_viewer")
                .default_width(350.0)
                .show(ctx, |ui| {
                    // Header
                    ui.horizontal(|ui| {
                        ui.heading("Traffic");
                        if ui
                            .small_button(if self.traffic_viewer_paused {
                                "Resume"
                            } else {
                                "Pause"
                            })
                            .clicked()
                        {
                            self.traffic_viewer_paused = !self.traffic_viewer_paused;
                        }
                        ui.label(format!("{} msgs", self.traffic_viewer_entries.len()));
                    });

                    // Channel filters
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.traffic_filter_main, "Main");
                        ui.checkbox(&mut self.traffic_filter_display, "Display");
                        ui.checkbox(&mut self.traffic_filter_inputs, "Inputs");
                        ui.checkbox(&mut self.traffic_filter_cursor, "Cursor");
                        ui.checkbox(&mut self.traffic_filter_usbredir, "USB");
                        ui.checkbox(&mut self.traffic_filter_webdav, "WebDAV");
                        ui.checkbox(&mut self.traffic_filter_playback, "Playback");
                    });
                    ui.separator();

                    // Scrollable message list
                    let stick = !self.traffic_viewer_paused;
                    let now_elapsed = self.traffic.elapsed();
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(stick)
                        .show(ui, |ui| {
                            for entry in &self.traffic_viewer_entries {
                                // Apply channel filter
                                let visible = match entry.channel {
                                    "main" => self.traffic_filter_main,
                                    "display" => self.traffic_filter_display,
                                    "inputs" => self.traffic_filter_inputs,
                                    "cursor" => self.traffic_filter_cursor,
                                    "usbredir" => self.traffic_filter_usbredir,
                                    "webdav" => self.traffic_filter_webdav,
                                    "playback" => self.traffic_filter_playback,
                                    _ => true,
                                };
                                if !visible {
                                    continue;
                                }

                                let relative =
                                    entry.timestamp.as_secs_f64() - now_elapsed.as_secs_f64();
                                let dir = match entry.direction {
                                    TrafficDirection::Sent => "\u{2192}",
                                    TrafficDirection::Received => "\u{2190}",
                                };
                                let channel_color = match entry.channel {
                                    "main" => egui::Color32::from_rgb(120, 160, 255),
                                    "display" => egui::Color32::from_rgb(100, 200, 100),
                                    "inputs" => egui::Color32::from_rgb(255, 180, 80),
                                    "cursor" => egui::Color32::from_rgb(200, 130, 255),
                                    "usbredir" => egui::Color32::from_rgb(255, 100, 100),
                                    "webdav" => egui::Color32::from_rgb(100, 200, 200),
                                    _ => egui::Color32::GRAY,
                                };
                                let size_str = format_size(entry.wire_size);

                                ui.horizontal(|ui| {
                                    ui.monospace(format!("{:>7.1}s", relative));
                                    ui.colored_label(
                                        channel_color,
                                        format!("{:<8}", entry.channel),
                                    );
                                    ui.monospace(dir);
                                    ui.label(entry.message_name);
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.monospace(size_str);
                                        },
                                    );
                                });
                            }
                        });
                });
        }

        // Notifications side panel (conditional)
        if self.show_notifications_panel {
            egui::SidePanel::right("notifications")
                .default_width(360.0)
                .show(ctx, |ui| {
                    // Header: title + actions
                    ui.horizontal(|ui| {
                        ui.heading("Notifications");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Clear all").clicked() {
                                if let Ok(mut s) = self.notifications.lock() {
                                    s.clear();
                                }
                            }
                            if ui.small_button("Mark all read").clicked() {
                                if let Ok(mut s) = self.notifications.lock() {
                                    s.mark_all_read();
                                }
                            }
                        });
                    });

                    // Snapshot the state under one lock so rendering is off-lock
                    let (total, unread, snapshot) = match self.notifications.lock() {
                        Ok(s) => (
                            s.len(),
                            s.unread_count(),
                            s.iter_newest_first().cloned().collect::<Vec<_>>(),
                        ),
                        Err(_) => (0, 0, Vec::new()),
                    };
                    ui.label(format!("{} total / {} unread", total, unread));
                    ui.separator();

                    // Phase 10 (F2): collect notification-snapshot
                    // live-state flags under one lock outside the
                    // per-row render, so each row's button knows
                    // whether to render in the at-fire (solid) or
                    // post-event-only (dim) state without
                    // re-locking per entry.
                    let live_now = Instant::now();
                    let live_ids: std::collections::HashSet<u64> = match self
                        .notification_snapshots
                        .lock()
                    {
                        Ok(mut store) => {
                            store.prune_expired(live_now);
                            snapshot
                                .iter()
                                .filter(|e| store.has_live(e.id, live_now))
                                .map(|e| e.id)
                                .collect()
                        }
                        Err(_) => std::collections::HashSet::new(),
                    };

                    let mut to_remove: Vec<u64> = Vec::new();
                    let mut pending_bug_report_id: Option<u64> = None;
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if snapshot.is_empty() {
                            ui.label("No notifications.");
                        }
                        for entry in &snapshot {
                            ui.horizontal(|ui| {
                                let (glyph, colour) =
                                    notifications::severity_visuals(entry.severity);
                                let mut g = egui::RichText::new(glyph);
                                if let Some(c) = colour {
                                    g = g.color(c);
                                }
                                if entry.read {
                                    g = g.weak();
                                }
                                ui.label(g);

                                ui.monospace(notifications::format_relative(entry.when));

                                ui.colored_label(egui::Color32::GRAY, entry.source.label());

                                let mut msg_text = egui::RichText::new(&entry.message);
                                if entry.read {
                                    msg_text = msg_text.weak();
                                }
                                ui.label(msg_text);

                                if entry.count > 1 {
                                    ui.label(
                                        egui::RichText::new(format!("[{}\u{00D7}]", entry.count))
                                            .weak(),
                                    );
                                }

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("Dismiss").clicked() {
                                            to_remove.push(entry.id);
                                        }
                                        // Phase 10 (F2): File-as-bug-report
                                        // button. Always present; visual
                                        // state and tooltip vary by whether
                                        // a live snapshot still exists for
                                        // this notification.
                                        let snapshot_live = live_ids.contains(&entry.id);
                                        let (label, tooltip) = if snapshot_live {
                                            (
                                                egui::RichText::new("File…"),
                                                "File bug report (at-fire snapshot available)",
                                            )
                                        } else {
                                            (
                                                egui::RichText::new("File…").weak(),
                                                "File bug report (post-event context only — snapshot expired)",
                                            )
                                        };
                                        if ui
                                            .small_button(label)
                                            .on_hover_text(tooltip)
                                            .clicked()
                                        {
                                            pending_bug_report_id = Some(entry.id);
                                        }
                                    },
                                );
                            });
                        }
                    });

                    if !to_remove.is_empty() {
                        if let Ok(mut s) = self.notifications.lock() {
                            for id in to_remove {
                                s.remove(id);
                            }
                        }
                    }
                    if let Some(id) = pending_bug_report_id {
                        self.file_notification_bug_report(id);
                    }
                });
        }

        // Mark all read on the panel-open → panel-closed transition,
        // so the bell dot clears and the user gets one chance to triage
        // before the unread state resets.
        if !self.show_notifications_panel && self.notifications_panel_was_open_last_frame {
            if let Ok(mut s) = self.notifications.lock() {
                s.mark_all_read();
            }
        }
        self.notifications_panel_was_open_last_frame = self.show_notifications_panel;

        // USB device management panel (conditional)
        if self.show_usb_panel {
            // Auto-enumerate on first open
            if !self.usb_devices_enumerated {
                self.usb_available_devices = usb::enumerate_devices(&self.usb_virtual_disks);
                self.usb_devices_enumerated = true;
            }

            // Poll for file picker result
            let mut picked_path = None;
            if let Some(ref rx) = self.usb_add_disk_rx {
                if let Ok(result) = rx.try_recv() {
                    picked_path = Some(result);
                }
            }
            if picked_path.is_some() {
                self.usb_add_disk_rx = None;
            }
            if let Some(Some(path)) = picked_path {
                self.usb_add_disk_message = None;
                match std::fs::metadata(&path) {
                    Ok(meta) => {
                        if !meta.is_file() {
                            self.usb_add_disk_message =
                                Some("Selected path is not a regular file.".to_string());
                        } else if meta.len() < 512 {
                            self.usb_add_disk_message =
                                Some("File is too small (< 512 bytes).".to_string());
                        } else {
                            let read_only = self.usb_add_disk_readonly;
                            self.usb_virtual_disks.push((path.clone(), read_only));
                            self.usb_available_devices =
                                usb::enumerate_devices(&self.usb_virtual_disks);
                            let warn = if meta.len() % 512 != 0 {
                                " (warning: size not a multiple of 512)"
                            } else {
                                ""
                            };
                            let ro = if read_only { " [RO]" } else { "" };
                            self.usb_add_disk_message =
                                Some(format!("Added: {}{}{}", path.display(), ro, warn));
                        }
                    }
                    Err(e) => {
                        self.usb_add_disk_message = Some(format!("Cannot read file: {}", e));
                    }
                }
            }

            // Auto-clear USB errors after 10 seconds
            if let Some(error_time) = self.usb_error_time {
                if error_time.elapsed() > Duration::from_secs(10) {
                    self.usb_error_message = None;
                    self.usb_error_time = None;
                }
            }

            // Request repaint for elapsed timer and error auto-clear
            if self.usb_connected_at.is_some() || self.usb_error_time.is_some() {
                ctx.request_repaint_after(Duration::from_secs(1));
            }

            let mut usb_action = None;
            let mut open_usb_bug_report = false;

            egui::SidePanel::right("usb_panel")
                .default_width(300.0)
                .show(ctx, |ui| {
                    // Header with refresh button
                    ui.horizontal(|ui| {
                        ui.heading("USB Devices");
                        if ui.small_button("Refresh").clicked() {
                            self.usb_available_devices =
                                usb::enumerate_devices(&self.usb_virtual_disks);
                        }
                    });
                    ui.separator();

                    // Channel status
                    if self.usb_channel_ready {
                        ui.label("Channel: Ready");
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "Channel: Not available");
                    }

                    // Connected device with elapsed time
                    if let Some(ref desc) = self.usb_device_description {
                        ui.separator();
                        let elapsed = self
                            .usb_connected_at
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let mins = elapsed / 60;
                        let secs = elapsed % 60;
                        ui.label(format!("Connected: {} ({}m {}s)", desc, mins, secs));
                    }

                    // Error message with dismiss and bug report buttons
                    if self.usb_error_message.is_some() {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::RED,
                            self.usb_error_message.as_ref().unwrap(),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.usb_error_message = None;
                                self.usb_error_time = None;
                            }
                            if ui.small_button("Report this as a bug").clicked() {
                                open_usb_bug_report = true;
                            }
                        });
                    }

                    // Operation in progress indicator
                    if self.usb_connecting {
                        ui.separator();
                        ui.label("Connecting...");
                    } else if self.usb_disconnecting {
                        ui.separator();
                        ui.label("Disconnecting...");
                    }

                    ui.separator();

                    // Device list with connect/disconnect buttons
                    let buttons_disabled =
                        !self.usb_channel_ready || self.usb_connecting || self.usb_disconnecting;
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if self.usb_available_devices.is_empty() {
                            ui.colored_label(egui::Color32::GRAY, "No USB devices found.");
                        } else {
                            for device in &self.usb_available_devices {
                                let label = device.label();
                                let is_connected = self
                                    .usb_device_description
                                    .as_ref()
                                    .is_some_and(|d| *d == label);

                                ui.horizontal(|ui| {
                                    if is_connected {
                                        ui.colored_label(egui::Color32::GREEN, "\u{25CF}");
                                        ui.label(&label);
                                        if ui
                                            .add_enabled(
                                                !buttons_disabled,
                                                egui::Button::new("Disconnect"),
                                            )
                                            .clicked()
                                        {
                                            usb_action = Some(UsbCommand::DisconnectDevice);
                                        }
                                    } else {
                                        ui.label(&label);
                                        let connect_enabled = !buttons_disabled
                                            && self.usb_device_description.is_none();
                                        if ui
                                            .add_enabled(
                                                connect_enabled,
                                                egui::Button::new("Connect"),
                                            )
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

                    // Add virtual disk section
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.usb_add_disk_readonly, "Read-only");
                        let picker_active = self.usb_add_disk_rx.is_some();
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
                            self.usb_add_disk_rx = Some(rx);
                        }
                    });

                    // Add-disk message
                    if let Some(ref msg) = self.usb_add_disk_message {
                        if msg.starts_with("Added:") {
                            ui.label(msg);
                        } else {
                            ui.colored_label(egui::Color32::RED, msg);
                        }
                    }
                });

            // Execute USB action outside the closure
            if let Some(cmd) = usb_action {
                self.usb_error_message = None;
                self.usb_error_time = None;
                let is_disconnect = matches!(cmd, UsbCommand::DisconnectDevice);
                if is_disconnect {
                    self.usb_disconnecting = true;
                } else {
                    self.usb_connecting = true;
                }
                if let Some(ref tx) = self.usb_tx {
                    if let Err(e) = tx.try_send(cmd) {
                        self.usb_connecting = false;
                        self.usb_disconnecting = false;
                        self.usb_error_message = Some(format!("Failed to send command: {}", e));
                        self.usb_error_time = Some(Instant::now());
                    }
                }
            }

            // Open bug report dialog for USB error (two-pass)
            if open_usb_bug_report {
                self.show_bug_dialog = true;
                self.bug_report_type = BugReportType::Usb;
                self.bug_description = self.usb_error_message.clone().unwrap_or_default();
                self.begin_trigger_snapshot();
            }
        }

        // ── WebDAV Folders panel ─────────────────────────
        if self.show_webdav_panel {
            // Poll directory picker result
            let mut picked_dir = None;
            if let Some(ref rx) = self.webdav_pick_dir_rx {
                if let Ok(result) = rx.try_recv() {
                    picked_dir = Some(result);
                }
            }
            if picked_dir.is_some() {
                self.webdav_pick_dir_rx = None;
            }

            let mut webdav_action = None;

            if let Some(Some(path)) = picked_dir {
                if path.is_dir() {
                    webdav_action = Some(WebdavCommand::ShareDirectory {
                        path,
                        read_only: self.webdav_pick_dir_readonly,
                    });
                }
            }

            // Auto-clear WebDAV errors after 10 seconds
            if let Some(error_time) = self.webdav_error_time {
                if error_time.elapsed() > Duration::from_secs(10) {
                    self.webdav_error_message = None;
                    self.webdav_error_time = None;
                }
            }

            // Request repaint for elapsed timer and error auto-clear
            if self.webdav_connected_at.is_some() || self.webdav_error_time.is_some() {
                ctx.request_repaint_after(Duration::from_secs(1));
            }

            egui::SidePanel::right("webdav_panel")
                .default_width(300.0)
                .show(ctx, |ui| {
                    ui.heading("Shared Folders");
                    ui.separator();

                    // Channel status
                    if self.webdav_channel_ready {
                        ui.label("Channel: Ready");
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "Channel: Not available");
                    }

                    // Active share with elapsed timer
                    if self.webdav_sharing_active {
                        if let Some(ref dir) = self.webdav_shared_dir {
                            ui.separator();
                            let elapsed = self
                                .webdav_connected_at
                                .map(|t| t.elapsed().as_secs())
                                .unwrap_or(0);
                            let mins = elapsed / 60;
                            let secs = elapsed % 60;
                            let ro = if self.webdav_read_only { " [RO]" } else { "" };
                            ui.label(format!("Sharing: {}{} ({}m {}s)", dir, ro, mins, secs));
                            if ui.button("Stop Sharing").clicked() {
                                webdav_action = Some(WebdavCommand::StopSharing);
                            }
                        }
                    }

                    // Error display
                    if self.webdav_error_message.is_some() {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::RED,
                            self.webdav_error_message.as_ref().unwrap(),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.webdav_error_message = None;
                                self.webdav_error_time = None;
                            }
                        });
                    }

                    // Share controls (when not sharing)
                    if !self.webdav_sharing_active {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.webdav_pick_dir_readonly, "Read-only");
                            let picker_active = self.webdav_pick_dir_rx.is_some();
                            let enabled = self.webdav_channel_ready && !picker_active;
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
                                self.webdav_pick_dir_rx = Some(rx);
                            }
                        });
                    }
                });

            // Execute WebDAV action outside the closure
            if let Some(cmd) = webdav_action {
                self.webdav_error_message = None;
                self.webdav_error_time = None;
                if let Some(ref tx) = self.webdav_tx {
                    if let Err(e) = tx.try_send(cmd) {
                        self.webdav_error_message = Some(format!("Failed to send command: {}", e));
                        self.webdav_error_time = Some(Instant::now());
                    }
                }
            }
        }

        // Main display area (no margin so the surface fills edge-to-edge)
        let mut open_channel_bug_report = false;
        let panel_frame = egui::Frame::none().inner_margin(0.0);
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                if self.error_message.is_some() {
                    ui.colored_label(
                        egui::Color32::RED,
                        format!("Error: {}", self.error_message.as_ref().unwrap()),
                    );
                    ui.horizontal(|ui| {
                        if ui.small_button("Dismiss").clicked() {
                            self.error_message = None;
                        }
                        if ui.small_button("Report this as a bug").clicked() {
                            open_channel_bug_report = true;
                        }
                    });
                    ui.separator();
                }

                if !self.connected {
                    ui.centered_and_justified(|ui| {
                        ui.label("Connecting...");
                    });
                    return;
                }

                let mut keys: Vec<(u8, u32)> = self.surfaces.keys().copied().collect();
                keys.sort_unstable();
                let primary_key = keys
                    .iter()
                    .copied()
                    .find(|(_, sid)| *sid == 0)
                    .or_else(|| keys.first().copied());

                if let Some(primary_key) = primary_key {
                    if let Some(gs) = self.surfaces.get_mut(&primary_key) {
                        let (width, height) = (gs.surface().width, gs.surface().height);
                        let texture = gs.texture(ctx);
                        let size = egui::vec2(width as f32, height as f32);

                        let response = ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .sense(egui::Sense::click_and_drag()),
                        );

                        self.surface_rect = response.rect;

                        let input_suppressed = self.show_bug_dialog || self.region_select_active;
                        if !input_suppressed {
                            if let Some(tx) = &self.input_tx {
                                if let Some(pos) = response.hover_pos() {
                                    let x = (pos.x - response.rect.min.x).max(0.0) as u32;
                                    let y = (pos.y - response.rect.min.y).max(0.0) as u32;
                                    if self.last_mouse_pos != Some((x, y)) {
                                        if self.mouse_mode == MOUSE_MODE_SERVER {
                                            // Server mode: send relative deltas.
                                            let (prev_x, prev_y) =
                                                self.last_mouse_pos.unwrap_or((x, y));
                                            let dx = x as i32 - prev_x as i32;
                                            let dy = y as i32 - prev_y as i32;
                                            let _ = tx.try_send(InputEvent::MouseMotion { dx, dy });
                                        } else {
                                            // Client mode: send absolute position.
                                            let _ = tx.try_send(InputEvent::MouseMove { x, y });
                                        }
                                        self.last_mouse_pos = Some((x, y));
                                    }
                                }

                                // Gate button-press / scroll-wheel forwarding on
                                // whether the pointer is over the SPICE surface,
                                // so clicks on the status-bar widgets (volume,
                                // mute, reconnect indicator, etc.) do not leak
                                // phantom clicks into the guest at
                                // `last_mouse_pos`. Button-release is forwarded
                                // unconditionally when the corresponding bit is
                                // set in `forwarded_buttons`, so a press inside
                                // the image followed by a release outside it
                                // does not leave a stuck button in the guest
                                // (symmetric with the `input_suppressed` path
                                // below).
                                let pointer_on_surface = response.contains_pointer();
                                ctx.input(|i| {
                                    let pos = self.last_mouse_pos.unwrap_or((0, 0));
                                    for button in [
                                        egui::PointerButton::Primary,
                                        egui::PointerButton::Secondary,
                                        egui::PointerButton::Middle,
                                    ] {
                                        let spice_btn = mouse_button_to_spice(button);
                                        if pointer_on_surface && i.pointer.button_pressed(button) {
                                            self.forwarded_buttons |= spice_btn;
                                            let _ = tx.try_send(InputEvent::MouseDown {
                                                button: spice_btn,
                                                x: pos.0,
                                                y: pos.1,
                                            });
                                        }
                                        if i.pointer.button_released(button)
                                            && self.forwarded_buttons & spice_btn != 0
                                        {
                                            self.forwarded_buttons &= !spice_btn;
                                            let _ = tx.try_send(InputEvent::MouseUp {
                                                button: spice_btn,
                                                x: pos.0,
                                                y: pos.1,
                                            });
                                        }
                                    }

                                    if pointer_on_surface {
                                        let scroll_y = i.smooth_scroll_delta.y;
                                        if scroll_y.abs() > 0.5 {
                                            let btn = if scroll_y > 0.0 { 0x08 } else { 0x10 };
                                            let _ = tx.try_send(InputEvent::MouseDown {
                                                button: btn,
                                                x: pos.0,
                                                y: pos.1,
                                            });
                                            let _ = tx.try_send(InputEvent::MouseUp {
                                                button: btn,
                                                x: pos.0,
                                                y: pos.1,
                                            });
                                        }
                                    }
                                });
                            }
                        } else if self.forwarded_buttons != 0 {
                            if let Some(tx) = &self.input_tx {
                                let pos = self.last_mouse_pos.unwrap_or((0, 0));
                                for bit in 0..5u32 {
                                    let mask = 1 << bit;
                                    if self.forwarded_buttons & mask != 0 {
                                        let _ = tx.try_send(InputEvent::MouseUp {
                                            button: mask,
                                            x: pos.0,
                                            y: pos.1,
                                        });
                                    }
                                }
                            }
                            self.forwarded_buttons = 0;
                        }
                    }
                }

                if self.surfaces.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("Waiting for display...");
                    });
                }
            });

        // Open bug report dialog for channel error (two-pass)
        if open_channel_bug_report {
            self.show_bug_dialog = true;
            self.bug_report_type = BugReportType::Connection;
            self.bug_description = self.error_message.clone().unwrap_or_default();
            self.begin_trigger_snapshot();
        }

        // Bug report dialog (two-pass: render then act)
        let mut dialog_action = None;
        if self.show_bug_dialog {
            egui::Window::new("Bug Report")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(350.0);

                    ui.label(
                        "Bug reports may contain sensitive data including \
                         screen contents, typed keystrokes, and protocol \
                         traffic. Review the report before sharing and \
                         ensure no confidential information is visible on \
                         screen or was recently typed.",
                    );
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label("Report type:");
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Display,
                        "Display (screenshot + image state)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Input,
                        "Input (keyboard + mouse state)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Cursor,
                        "Cursor (cursor cache + position)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Connection,
                        "Connection (session + main channel)",
                    );
                    ui.radio_value(
                        &mut self.bug_report_type,
                        BugReportType::Usb,
                        "USB (usbredir channel + device state)",
                    );

                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label("Description (optional):");
                    ui.text_edit_singleline(&mut self.bug_description);

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Capture").clicked() {
                            dialog_action = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            dialog_action = Some(false);
                        }
                    });
                });
        }

        // Execute dialog action outside the closure
        match dialog_action {
            Some(true) => {
                if self.bug_report_type == BugReportType::Display {
                    // Enter region selection mode for display reports
                    self.region_select_active = true;
                    self.region_drag_start = None;
                    self.region_drag_end = None;
                } else {
                    // Non-display: generate immediately
                    let report_type = self.bug_report_type.clone();
                    let description = self.bug_description.clone();
                    self.finish_bug_report(report_type, description, None);
                }
                self.show_bug_dialog = false;
            }
            Some(false) => {
                self.show_bug_dialog = false;
                self.discard_trigger_snapshot();
            }
            None => {}
        }

        // Paste error dialog (two-pass: render then act)
        let mut paste_dialog_action = None;
        if let Some(ref msg) = self.paste_error_message {
            egui::Window::new("Cannot paste as keystrokes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(350.0);
                    ui.label(msg);
                    ui.add_space(8.0);
                    if ui.button("OK").clicked() {
                        paste_dialog_action = Some(());
                    }
                });
        }
        if paste_dialog_action.is_some() {
            self.paste_error_message = None;
        }

        // Escape also dismisses the paste error dialog
        if self.paste_error_message.is_some() {
            let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc {
                self.paste_error_message = None;
            }
        }

        // Region selection mode: crosshair, drag tracking, overlays
        if self.region_select_active && self.surface_rect != egui::Rect::NOTHING {
            // Show crosshair cursor over the surface
            ctx.output_mut(|o| o.cursor_icon = egui::CursorIcon::Crosshair);

            // Get surface dimensions for clamping
            let surf_w = self.surface_rect.width() as u32;
            let surf_h = self.surface_rect.height() as u32;

            // Track drag (two-pass: collect action, execute outside)
            let mut region_completed = false;
            ctx.input(|i| {
                if i.pointer.primary_pressed() {
                    if let Some(pos) = i.pointer.interact_pos() {
                        let x = ((pos.x - self.surface_rect.min.x).max(0.0) as u32).min(surf_w);
                        let y = ((pos.y - self.surface_rect.min.y).max(0.0) as u32).min(surf_h);
                        self.region_drag_start = Some((x, y));
                        self.region_drag_end = Some((x, y));
                    }
                }
                if i.pointer.primary_down() {
                    if let Some(pos) = i.pointer.interact_pos() {
                        let x = ((pos.x - self.surface_rect.min.x).max(0.0) as u32).min(surf_w);
                        let y = ((pos.y - self.surface_rect.min.y).max(0.0) as u32).min(surf_h);
                        self.region_drag_end = Some((x, y));
                    }
                }
                if i.pointer.primary_released() && self.region_drag_start.is_some() {
                    region_completed = true;
                }
            });

            // Draw instruction banner
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("region_select_banner"),
            ));
            let banner_rect = egui::Rect::from_min_size(
                self.surface_rect.min,
                egui::vec2(self.surface_rect.width(), 28.0),
            );
            painter.rect_filled(
                banner_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180),
            );
            painter.text(
                banner_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Click and drag to select the affected region. Press Escape to skip.",
                egui::FontId::proportional(13.0),
                egui::Color32::WHITE,
            );

            // Draw selection rectangle while dragging
            if let (Some((sx, sy)), Some((ex, ey))) = (self.region_drag_start, self.region_drag_end)
            {
                let left = sx.min(ex) as f32 + self.surface_rect.min.x;
                let top = sy.min(ey) as f32 + self.surface_rect.min.y;
                let right = sx.max(ex) as f32 + self.surface_rect.min.x;
                let bottom = sy.max(ey) as f32 + self.surface_rect.min.y;
                let sel_rect =
                    egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(right, bottom));
                let sel_painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("region_select_rect"),
                ));
                sel_painter.rect_filled(
                    sel_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(255, 0, 0, 60),
                );
                sel_painter.rect_stroke(
                    sel_rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 0, 0)),
                    egui::StrokeKind::Middle,
                );
            }

            // Generate report on drag release
            if region_completed {
                let (sx, sy) = self.region_drag_start.unwrap();
                let (ex, ey) = self.region_drag_end.unwrap();
                match validate_region(sx, sy, ex, ey) {
                    Some(region) => {
                        let report_type = self.bug_report_type.clone();
                        let description = self.bug_description.clone();
                        self.finish_bug_report(report_type, description, Some(region));
                        self.region_select_active = false;
                        self.region_drag_start = None;
                        self.region_drag_end = None;
                    }
                    None => {
                        // Click without drag, or any other
                        // degenerate input (K4 guard). Stay in
                        // region-select mode and tell the user
                        // what went wrong; reset the drag state
                        // so they can try again without having
                        // to re-enter region-select.
                        self.push_notification(
                            NotifySeverity::Warn,
                            NotificationSource::BugReport,
                            "Drag a region with non-zero area, or press \
                             Escape to cancel."
                                .to_string(),
                        );
                        self.region_drag_start = None;
                        self.region_drag_end = None;
                    }
                }
            }
        }

        // Create a default cursor if the server hasn't sent one yet
        if self.cursor_image.is_none() && self.connected {
            self.cursor_image = Some(CursorImage {
                width: 12,
                height: 19,
                hot_spot_x: 0,
                hot_spot_y: 0,
                pixels: default_arrow_cursor(),
            });
            self.cursor_texture = None;
        }

        // Create cursor texture if we have a new shape
        if self.cursor_image.is_some() && self.cursor_texture.is_none() {
            if let Some(ref img) = self.cursor_image {
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [img.width as usize, img.height as usize],
                    &img.pixels,
                );
                let options = egui::TextureOptions {
                    magnification: egui::TextureFilter::Nearest,
                    minification: egui::TextureFilter::Nearest,
                    ..Default::default()
                };
                self.cursor_texture = Some(ctx.load_texture("spice_cursor", color_image, options));
            }
        }

        // Draw cursor overlay using the painter so it doesn't
        // interfere with mouse input on the surface below.
        // (hidden during region selection — crosshair cursor is shown instead)
        if self.cursor_visible
            && !self.region_select_active
            && self.surface_rect != egui::Rect::NOTHING
        {
            if let (Some(ref tex), Some(ref img)) = (&self.cursor_texture, &self.cursor_image) {
                // In client mode (2) the host controls cursor position, so
                // use last_mouse_pos for immediate feedback.  In server mode
                // (1) the guest is the authority — use cursor_pos reported by
                // the cursor channel to stay in sync with the guest.
                let mode = self.mouse_mode;
                let (cx, cy) = if mode == 1 {
                    // Server mode: guest-reported position is authoritative
                    (self.cursor_pos.0 as f32, self.cursor_pos.1 as f32)
                } else {
                    // Client mode: use host-tracked position for responsiveness
                    self.last_mouse_pos
                        .map(|(x, y)| (x as f32, y as f32))
                        .unwrap_or((self.cursor_pos.0 as f32, self.cursor_pos.1 as f32))
                };

                let x = self.surface_rect.min.x + cx - img.hot_spot_x as f32;
                let y = self.surface_rect.min.y + cy - img.hot_spot_y as f32;
                let size = egui::vec2(img.width as f32, img.height as f32);
                let rect = egui::Rect::from_min_size(egui::pos2(x, y), size);

                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("spice_cursor"),
                ));
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                painter.image(tex.id(), rect, uv, egui::Color32::WHITE);
            }
        }

        // Protocol gaps floating window (toggled by the Gaps: N button)
        egui::Window::new("Protocol gaps")
            .open(&mut self.gaps_popup_open)
            .resizable(true)
            .default_width(400.0)
            .default_height(300.0)
            .show(ctx, |ui| {
                let mut keys = shakenfist_spice_protocol::logging::warn_once_keys();
                keys.sort();
                if keys.is_empty() {
                    ui.label("No protocol gaps seen this session.");
                } else {
                    ui.label(format!("{} distinct gaps:", keys.len()));
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for key in &keys {
                            ui.monospace(*key);
                        }
                    });
                }
            });

        // The modal only appears when auto-reconnect has given
        // up. Pending state shows the status-bar indicator
        // (rendered in the bottom stats panel) instead, so the
        // user is not interrupted while we are still trying.
        let mut wants_reconnect = false;
        if let ReconnectState::Modal(variant) = &self.reconnect_state {
            let (title, body, allow_reconnect) = match variant {
                ModalVariant::Generic { latest_error } => (
                    "Connection lost",
                    format!(
                        "Three automatic reconnect attempts failed: {}",
                        latest_error
                    ),
                    true,
                ),
                ModalVariant::OneShotConsumed => (
                    "Session ended — cannot reconnect",
                    "This connection used a single-use ticket. \
                     Request a new connection from the system that \
                     issued the original link."
                        .to_string(),
                    false,
                ),
                ModalVariant::TicketExpired { expired_at } => (
                    "Session ended — ticket expired",
                    format!(
                        "The ticket for this session expired at {}. \
                         Request a new connection.",
                        format_expiry_local(*expired_at),
                    ),
                    false,
                ),
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(body);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if allow_reconnect && ui.button("Reconnect").clicked() {
                            wants_reconnect = true;
                        }
                        if ui.button("Close").clicked() {
                            if let Some(ref capture) = self.capture {
                                capture.close();
                            }
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }
        if wants_reconnect {
            self.reconnect_manual();
        }

        if self.cursor_image.is_some()
            && !self.region_select_active
            && self.surface_rect != egui::Rect::NOTHING
            && !ctx.wants_pointer_input()
            && ctx.input(|i| {
                i.pointer
                    .hover_pos()
                    .is_some_and(|p| self.surface_rect.contains(p))
            })
        {
            ctx.output_mut(|o| o.cursor_icon = egui::CursorIcon::None);
        }

        // Repaint when channel events arrive; 1s fallback for time-based UI
        // (bandwidth/latency sparklines, status-message expiry, cadence-mode
        // keystroke injection).  The bridge task wakes us immediately when
        // an event arrives via the Arc<Notify>; this fallback only ensures
        // anything that polls Instant::elapsed() updates roughly once a
        // second.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

/// True when an inbound display-channel surface refers to
/// the primary surface — i.e. display channel 0, surface
/// id 0. The phase 1 plan picked this literal pair (rather
/// than tracking "the renderer's current primary key")
/// because the primary surface key is fixed by the SPICE
/// protocol; centralising the check here keeps the trigger
/// sites in sync if that ever changes.
fn is_primary_surface(display_channel_id: u8, surface_id: u32) -> bool {
    display_channel_id == 0 && surface_id == 0
}

/// Push a lag sample into a bounded ring, evicting the oldest
/// entry when the cap is exceeded. Factored out of
/// `process_events` so the cap behaviour is unit-testable.
/// See PLAN-video-keeping-up-phase-04.
fn push_with_cap(ring: &mut VecDeque<u32>, value: u32) {
    ring.push_back(value);
    if ring.len() > RECENT_LAG_RING_CAP {
        ring.pop_front();
    }
}

/// Min / max / mean of the values in a lag ring. Returns
/// `(0, 0, 0)` when the ring is empty so a snapshot field
/// reads as "no samples yet" rather than carrying stale data.
fn recent_lag_stats(ring: &VecDeque<u32>) -> (u32, u32, u32) {
    if ring.is_empty() {
        return (0, 0, 0);
    }
    let mut min = u32::MAX;
    let mut max = 0u32;
    let mut sum: u64 = 0;
    for &v in ring {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
        sum += v as u64;
    }
    let mean = u32::try_from(sum / ring.len() as u64).unwrap_or(u32::MAX);
    (min, max, mean)
}

/// True when an announced surface size is small enough to
/// safely drive the auto-fit and resolution-notification
/// pipelines. See `MAX_AUTO_FIT_DIMENSION` for the
/// rationale; the trigger sites use this to refuse to
/// arm `pending_resize` / `pending_resolution_notify` for
/// nonsense-sized surfaces (which a hostile server can
/// announce trivially) without affecting the SPICE
/// renderer's own surface bookkeeping.
fn auto_fit_size_acceptable(width: u32, height: u32) -> bool {
    width <= MAX_AUTO_FIT_DIMENSION && height <= MAX_AUTO_FIT_DIMENSION
}

/// Decide whether to issue a `ViewportCommand::InnerSize`
/// to fit the remote surface, and what size to ask for.
///
/// `pending` is the surface size pulled from
/// `pending_resize` (logical pixels, f32). `last_auto`
/// is the (8-aligned) size of the last auto-resize we
/// issued, or None if we have not auto-resized yet. `is_max`
/// is true when the viewport is maximised or fullscreen
/// and we should not change the inner size. `obey` is the
/// user-controlled toggle: when false the function always
/// returns None so the window is never auto-fitted.
///
/// Returns Some((width, height, aligned_w, aligned_h))
/// where `(width, height)` are the values to pass to
/// `ViewportCommand::InnerSize` (with `STATS_BAR_HEIGHT`
/// added to the height — see the call site) and
/// `(aligned_w, aligned_h)` are the values to store in
/// `last_auto_resize` and seed into `last_sent_resize`.
/// Returns None when no resize should fire.
fn compute_auto_resize(
    pending: Option<(f32, f32)>,
    last_auto: Option<(u32, u32)>,
    is_max: bool,
    obey: bool,
) -> Option<(f32, f32, u32, u32)> {
    let (w, h) = pending?;
    if !obey || is_max {
        return None;
    }
    let aligned_w = ((w as u32).max(8) / 8) * 8;
    let aligned_h = ((h as u32).max(8) / 8) * 8;
    if last_auto == Some((aligned_w, aligned_h)) {
        return None;
    }
    Some((w, h, aligned_w, aligned_h))
}

/// Decide what `(width, height)` to send to the guest as a
/// `VDAgentMonitorsConfig` from a given viewport size.
///
/// `viewport` is the live inner-rect size in logical
/// pixels (typically `egui::ViewportInputState::inner_rect`).
/// `is_max` is true when the viewport is maximised or
/// fullscreen — in that case we do not subtract
/// `STATS_BAR_HEIGHT` from the height because the stats
/// bar overlays inside the maximised area rather than
/// adding to it.
///
/// The result is 8-pixel aligned (rounded down, matching
/// what the SPICE display-channel mode-set machinery
/// expects) and clamped to a minimum of 8 on each axis so
/// we never send a degenerate `(0, 0)` resize during a
/// pathological viewport report.
fn compute_outgoing_resize(viewport: (f32, f32), is_max: bool) -> (u32, u32) {
    let bar_height = if is_max { 0.0 } else { STATS_BAR_HEIGHT };
    let w_raw = viewport.0.max(0.0) as u32;
    let h_raw = (viewport.1 - bar_height).max(0.0) as u32;
    let aligned_w = (w_raw.max(8) / 8) * 8;
    let aligned_h = (h_raw.max(8) / 8) * 8;
    (aligned_w, aligned_h)
}

/// Decide whether the pending resolution-change
/// notification has been quiet long enough to emit, and
/// what value to emit.
///
/// `pending` pairs the latest queued (w, h) with its
/// observation timestamp; the two are always set and
/// cleared together at every call site.
///
/// Returns Some((w, h)) when:
/// * a value is pending,
/// * at least `debounce` has elapsed since the value
///   was queued, and
/// * the value differs from `last_notified` (so we do
///   not re-emit a confirmation of the existing mode).
///
/// Returns None to leave the pending state in place
/// (still inside the debounce window) or to drop it
/// silently (matches last_notified — the caller should
/// also clear the pending field in that case; see the
/// call site).
///
/// Pure for unit-testability — `now` is injected.
fn resolution_notification_due(
    pending: Option<((u32, u32), Instant)>,
    last_notified: Option<(u32, u32)>,
    now: Instant,
    debounce: Duration,
) -> Option<(u32, u32)> {
    let (target, queued_at) = pending?;
    if now.saturating_duration_since(queued_at) < debounce {
        return None;
    }
    if last_notified == Some(target) {
        return None;
    }
    Some(target)
}

/// Phase 09 (F1): map an auto-reconnect `ModalVariant` to the
/// `(severity, message)` pair that surfaces in the notification
/// pane when the state machine lands in Modal. Pure for
/// unit-testability — `modal_variant_notification` is the
/// only piece of new business logic the connection-event
/// push sites depend on.
fn modal_variant_notification(variant: &ModalVariant) -> (NotifySeverity, String) {
    match variant {
        ModalVariant::Generic { .. } => (
            NotifySeverity::Error,
            "Auto-reconnect failed after 3 attempts".to_string(),
        ),
        ModalVariant::OneShotConsumed => (
            NotifySeverity::Error,
            "Connection ended — single-use ticket consumed".to_string(),
        ),
        ModalVariant::TicketExpired { .. } => (
            NotifySeverity::Error,
            "Connection ended — ticket expired".to_string(),
        ),
    }
}

/// Build a `ReportRegion` from the raw drag-start / drag-end
/// coordinates produced by the region-select widget, iff the
/// resulting rectangle has strictly positive area. Returns
/// `None` for click-without-drag (the K4 case in Phase 04) —
/// the GUI handler uses this to keep the user in
/// region-select mode and surface a "drag a non-zero region"
/// notification rather than emitting a degenerate
/// `ReportRegion` into `report.json`.
///
/// "Strictly positive area" means `right > left AND
/// bottom > top` — a deliberate 1-pixel drag is allowed
/// since it points at a specific pixel, and rejecting it
/// would require a jitter threshold that no current data
/// justifies.
///
/// Reversed drags (bottom-right to top-left) are normalised
/// here so the produced region always satisfies
/// `left ≤ right && top ≤ bottom`.
fn validate_region(sx: u32, sy: u32, ex: u32, ey: u32) -> Option<ReportRegion> {
    let left = sx.min(ex);
    let right = sx.max(ex);
    let top = sy.min(ey);
    let bottom = sy.max(ey);
    if right > left && bottom > top {
        Some(ReportRegion {
            left,
            top,
            right,
            bottom,
        })
    } else {
        None
    }
}

/// Generate a simple 12x19 white arrow cursor with a black outline (RGBA).
fn default_arrow_cursor() -> Vec<u8> {
    #[rustfmt::skip]
    let shape: &[&[u8]] = &[
        &[1,0,0,0,0,0,0,0,0,0,0,0],
        &[1,1,0,0,0,0,0,0,0,0,0,0],
        &[1,2,1,0,0,0,0,0,0,0,0,0],
        &[1,2,2,1,0,0,0,0,0,0,0,0],
        &[1,2,2,2,1,0,0,0,0,0,0,0],
        &[1,2,2,2,2,1,0,0,0,0,0,0],
        &[1,2,2,2,2,2,1,0,0,0,0,0],
        &[1,2,2,2,2,2,2,1,0,0,0,0],
        &[1,2,2,2,2,2,2,2,1,0,0,0],
        &[1,2,2,2,2,2,2,2,2,1,0,0],
        &[1,2,2,2,2,2,2,2,2,2,1,0],
        &[1,2,2,2,2,2,2,2,2,2,2,1],
        &[1,2,2,2,2,2,2,1,1,1,1,1],
        &[1,2,2,2,1,2,2,1,0,0,0,0],
        &[1,2,2,1,0,1,2,2,1,0,0,0],
        &[1,2,1,0,0,1,2,2,1,0,0,0],
        &[1,1,0,0,0,0,1,2,2,1,0,0],
        &[1,0,0,0,0,0,1,2,2,1,0,0],
        &[0,0,0,0,0,0,0,1,1,0,0,0],
    ];

    let mut pixels = vec![0u8; 12 * 19 * 4];
    for (y, row) in shape.iter().enumerate() {
        for (x, &val) in row.iter().enumerate() {
            let idx = (y * 12 + x) * 4;
            match val {
                1 => {
                    // Black outline
                    pixels[idx] = 0;
                    pixels[idx + 1] = 0;
                    pixels[idx + 2] = 0;
                    pixels[idx + 3] = 255;
                }
                2 => {
                    // White fill
                    pixels[idx] = 255;
                    pixels[idx + 1] = 255;
                    pixels[idx + 2] = 255;
                    pixels[idx + 3] = 255;
                }
                _ => {} // transparent (already 0)
            }
        }
    }
    pixels
}

// ── Unit tests ──────────────────────────────────────────────────────────────

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

    #[test]
    fn latency_tracker_label_empty() {
        let tracker = LatencyTracker::new();
        assert_eq!(tracker.label(), "--ms");
    }

    #[test]
    fn latency_tracker_label_non_empty() {
        let mut tracker = LatencyTracker::new();
        tracker.record(12.34);
        assert_eq!(tracker.label(), "12.3ms");
    }

    #[test]
    fn compute_auto_resize_decisions() {
        // No pending event => no resize.
        assert_eq!(compute_auto_resize(None, None, false, true), None);

        // Pending size, never resized before, not maximised =>
        // resize to the aligned target.
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), None, false, true),
            Some((1024.0, 768.0, 1024, 768)),
        );

        // Same target as last_auto => skip (dedup).
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), Some((1024, 768)), false, true),
            None,
        );

        // Maximised => skip even when target differs.
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), Some((640, 480)), true, true),
            None,
        );

        // Non-aligned size gets aligned to 8 px boundary.
        assert_eq!(
            compute_auto_resize(Some((1366.0, 770.0)), None, false, true),
            Some((1366.0, 770.0, 1360, 768)),
        );

        // Differs after alignment from last_auto => resize.
        assert_eq!(
            compute_auto_resize(Some((1280.0, 800.0)), Some((1024, 768)), false, true),
            Some((1280.0, 800.0, 1280, 800)),
        );

        // obey = false short-circuits even when the target
        // differs and the window is not maximised.
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), None, false, false,),
            None,
        );

        // obey = false short-circuits even when the target equals
        // last_auto (would dedup anyway, but we want the obey
        // gate to be the reason).
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), Some((1024, 768)), false, false,),
            None,
        );
    }

    #[test]
    fn compute_outgoing_resize_decisions() {
        // Even-aligned input passes through unchanged
        // (height has STATS_BAR_HEIGHT subtracted first).
        assert_eq!(
            compute_outgoing_resize((1024.0, 768.0 + STATS_BAR_HEIGHT), false),
            (1024, 768),
        );

        // Non-aligned widths round DOWN to the 8 px grid
        // (matches the historical `w -= w % 8` form).
        assert_eq!(
            compute_outgoing_resize((1366.0, 768.0 + STATS_BAR_HEIGHT), false),
            (1360, 768),
        );

        // Non-aligned heights round DOWN too.
        assert_eq!(
            compute_outgoing_resize((1024.0, 770.0 + STATS_BAR_HEIGHT), false),
            (1024, 768),
        );

        // Sub-8 viewport dims clamp to the 8 px floor on each
        // axis. Use 4×4 (after bar subtraction) — both axes
        // hit the clamp.
        assert_eq!(
            compute_outgoing_resize((4.0, 4.0 + STATS_BAR_HEIGHT), false),
            (8, 8),
        );

        // Negative viewport dims (f32 < 0) clamp to zero
        // before the 8 px floor — must not panic on the
        // `as u32` conversion. f32 -> u32 is saturating in
        // Rust, but we still rely on the .max(0.0) up front.
        assert_eq!(compute_outgoing_resize((-100.0, -50.0), false), (8, 8),);

        // is_max = true skips the STATS_BAR_HEIGHT
        // subtraction. Same viewport, different is_max ->
        // different height.
        assert_eq!(compute_outgoing_resize((1024.0, 768.0), true), (1024, 768),);
        // is_max = false subtracts STATS_BAR_HEIGHT (20) then rounds down to
        // the 8-px grid: (768 - 20) = 748, rounded down to 744.
        assert_eq!(compute_outgoing_resize((1024.0, 768.0), false), (1024, 744),);
    }

    /// After auto-fitting to a fresh guest surface, the next
    /// frame's outgoing-resize computation must produce the
    /// same (aligned_w, aligned_h) so `last_sent_resize`
    /// dedupes and we do not echo our own resize back to the
    /// guest as a fresh VDAgentMonitorsConfig.
    #[test]
    fn round_trip_no_echo() {
        // Guest sends SurfaceCreated 1024x768. compute_auto_resize
        // returns the (w, h, aligned_w, aligned_h) we will fit
        // to and seed into last_sent_resize.
        let auto = compute_auto_resize(Some((1024.0, 768.0)), None, false, true)
            .expect("auto-fit should fire on first surface");
        let (fit_w, fit_h, aligned_w, aligned_h) = auto;
        assert_eq!((fit_w as u32, fit_h as u32), (1024, 768));
        assert_eq!((aligned_w, aligned_h), (1024, 768));

        // egui then reports the new viewport inner-rect: the
        // surface size plus STATS_BAR_HEIGHT (we asked for
        // total_h = h + STATS_BAR_HEIGHT in the resize block).
        let viewport = (fit_w, fit_h + STATS_BAR_HEIGHT);
        let outgoing = compute_outgoing_resize(viewport, false);

        // The outgoing computation must match the seeded
        // last_sent_resize, so maybe_send_monitors_resize
        // dedupes and does NOT fire.
        assert_eq!(outgoing, (aligned_w, aligned_h));
    }

    /// If the guest answers a ryll-driven resize request with
    /// a *different* size (e.g. ryll asked for 1280x800, guest
    /// can only do 1024x768), auto-fit re-seeds last_sent_resize
    /// to the guest's choice. The next outgoing computation
    /// against the new viewport must dedupe so ryll does not
    /// then ask for 1024x768 again as if it were a user-driven
    /// resize.
    #[test]
    fn round_trip_guest_overrides_request() {
        // State at the start of the test: last_sent_resize
        // was (1280, 800) because the user dragged the window
        // to that size and we sent a VDAgentMonitorsConfig
        // accordingly. last_auto_resize is None because no
        // auto-fit has fired yet this session.
        let last_sent = Some((1280u32, 800u32));
        let last_auto: Option<(u32, u32)> = None;

        // Guest replies with a 1024x768 SurfaceCreated.
        let auto = compute_auto_resize(Some((1024.0, 768.0)), last_auto, false, true)
            .expect("auto-fit should fire — surface differs from last_auto");
        let (_, _, aligned_w, aligned_h) = auto;
        assert_eq!((aligned_w, aligned_h), (1024, 768));

        // Caller seeds both last_sent_resize and
        // last_auto_resize from the auto-fit result.
        let new_last_sent = Some((aligned_w, aligned_h));
        assert_ne!(
            new_last_sent, last_sent,
            "last_sent must update — we did not request 1024x768"
        );

        // egui reports the new viewport size. Outgoing
        // computation against it must match new_last_sent so
        // we do NOT then fire a fresh VDAgentMonitorsConfig
        // asking the guest for a size it just gave us.
        let viewport = (aligned_w as f32, aligned_h as f32 + STATS_BAR_HEIGHT);
        let outgoing = compute_outgoing_resize(viewport, false);
        assert_eq!(Some(outgoing), new_last_sent);
    }

    #[test]
    fn latency_tracker_record_trims_to_capacity() {
        let mut tracker = LatencyTracker::new();
        // Push 65 values: 0.0, 1.0, ..., 64.0
        for i in 0..65 {
            tracker.record(i as f32);
        }
        // Should be capped at LATENCY_HISTORY_LEN (60)
        assert_eq!(tracker.history.len(), LATENCY_HISTORY_LEN);
        // The first kept value should be the 6th original (index 5)
        assert_eq!(tracker.history[0], 5.0);
    }

    #[test]
    fn auto_fit_size_acceptable_bounds() {
        // Anchors the cap so a typo (`>` vs `>=`, `||` vs
        // `&&`) cannot silently let an attacker-controlled
        // dimension through. The cap matches
        // GL_MAX_TEXTURE_SIZE on common hardware and is
        // comfortably above any realistic display.
        assert!(auto_fit_size_acceptable(0, 0));
        assert!(auto_fit_size_acceptable(1024, 768));
        assert!(auto_fit_size_acceptable(
            MAX_AUTO_FIT_DIMENSION,
            MAX_AUTO_FIT_DIMENSION
        ));
        assert!(!auto_fit_size_acceptable(MAX_AUTO_FIT_DIMENSION + 1, 768));
        assert!(!auto_fit_size_acceptable(1024, MAX_AUTO_FIT_DIMENSION + 1));
        assert!(!auto_fit_size_acceptable(u32::MAX, u32::MAX));
    }

    #[test]
    fn is_primary_surface_only_zero_zero() {
        // Anchors the gating predicate so a typo
        // (`||` for `&&`, or a non-zero default) cannot
        // silently widen the set of surfaces that drive
        // auto-fit and resolution notifications.
        assert!(is_primary_surface(0, 0));
        assert!(!is_primary_surface(0, 1));
        assert!(!is_primary_surface(1, 0));
        assert!(!is_primary_surface(1, 1));
    }

    #[test]
    fn resolution_notification_due_nothing_pending() {
        let now = Instant::now();
        assert_eq!(
            resolution_notification_due(None, None, now, Duration::from_millis(500),),
            None,
        );
    }

    #[test]
    fn resolution_notification_due_inside_debounce() {
        let now = Instant::now();
        let queued_at = now - Duration::from_millis(100);
        assert_eq!(
            resolution_notification_due(
                Some(((1024, 768), queued_at)),
                None,
                now,
                Duration::from_millis(500),
            ),
            None,
            "100 ms < 500 ms debounce — must not fire",
        );
    }

    #[test]
    fn resolution_notification_due_past_window_emits() {
        let now = Instant::now();
        let queued_at = now - Duration::from_millis(600);
        assert_eq!(
            resolution_notification_due(
                Some(((1024, 768), queued_at)),
                None,
                now,
                Duration::from_millis(500),
            ),
            Some((1024, 768)),
        );
    }

    #[test]
    fn resolution_notification_due_past_window_dedupes() {
        // Pending value matches last_notified — caller
        // should suppress so we do not announce the same
        // resolution twice in a row.
        let now = Instant::now();
        let queued_at = now - Duration::from_millis(600);
        assert_eq!(
            resolution_notification_due(
                Some(((1024, 768), queued_at)),
                Some((1024, 768)),
                now,
                Duration::from_millis(500),
            ),
            None,
        );
    }

    // ── ReconnectState transitions ──────────────────────────

    fn err(s: &str) -> String {
        s.to_string()
    }

    fn no_policy() -> ReconnectPolicy {
        ReconnectPolicy::default()
    }

    fn epoch() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    #[test]
    fn reconnect_idle_to_pending_on_first_disconnect() {
        let now = Instant::now();
        let next =
            ReconnectState::Idle.on_disconnect(false, None, now, epoch(), no_policy(), err("eof"));
        match next {
            Some(ReconnectState::Pending {
                attempt,
                next_at,
                latest_error,
            }) => {
                assert_eq!(attempt, 1);
                assert_eq!(
                    next_at - now,
                    Duration::from_secs(RECONNECT_BACKOFF_SECS[0])
                );
                assert_eq!(latest_error, "eof");
            }
            other => panic!("expected Pending(1), got {:?}", other),
        }
    }

    #[test]
    fn reconnect_full_failure_cluster_lands_in_modal() {
        let now = Instant::now();
        // Initial disconnect: Idle → Pending(1).
        let s1 = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), no_policy(), err("e1"))
            .unwrap();
        // Attempt 1 failure: Pending(1) → Pending(2).
        let s2 = s1
            .on_disconnect(
                true,
                None,
                now + Duration::from_secs(1),
                epoch(),
                no_policy(),
                err("e2"),
            )
            .unwrap();
        assert!(matches!(s2, ReconnectState::Pending { attempt: 2, .. }));
        // Attempt 2 failure: Pending(2) → Pending(3).
        let s3 = s2
            .on_disconnect(
                true,
                None,
                now + Duration::from_secs(5),
                epoch(),
                no_policy(),
                err("e3"),
            )
            .unwrap();
        assert!(matches!(s3, ReconnectState::Pending { attempt: 3, .. }));
        // Attempt 3 failure: Pending(3) → Modal(Generic) with latest error.
        let s4 = s3
            .on_disconnect(
                true,
                None,
                now + Duration::from_secs(20),
                epoch(),
                no_policy(),
                err("final"),
            )
            .unwrap();
        match s4 {
            ReconnectState::Modal(ModalVariant::Generic { latest_error }) => {
                assert_eq!(latest_error, "final");
            }
            other => panic!("expected Modal(Generic), got {:?}", other),
        }
    }

    #[test]
    fn reconnect_storm_event_during_pending_is_ignored() {
        let now = Instant::now();
        let pending = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), no_policy(), err("first"))
            .unwrap();
        // A non-awaiting second event (channel storm) must not
        // advance the attempt counter — that would burn budget
        // for a single underlying failure.
        let next = pending.on_disconnect(
            false,
            None,
            now + Duration::from_millis(10),
            epoch(),
            no_policy(),
            err("dup"),
        );
        assert!(next.is_none());
    }

    #[test]
    fn reconnect_cluster_reset_window_blocks_retry() {
        let now = Instant::now();
        // Recent modal: a fresh disconnect must skip Pending and
        // land directly in Modal. Otherwise a flapping server
        // would have us banging away forever.
        let modal_at = now - Duration::from_secs(60);
        let next = ReconnectState::Idle.on_disconnect(
            false,
            Some(modal_at),
            now,
            epoch(),
            no_policy(),
            err("flap"),
        );
        assert!(matches!(
            next,
            Some(ReconnectState::Modal(ModalVariant::Generic { .. }))
        ));
    }

    #[test]
    fn reconnect_cluster_reset_window_expires() {
        let now = Instant::now();
        // Beyond the 5-min reset window, a fresh budget unlocks.
        let modal_at = now - RECONNECT_CLUSTER_RESET - Duration::from_secs(1);
        let next = ReconnectState::Idle
            .on_disconnect(
                false,
                Some(modal_at),
                now,
                epoch(),
                no_policy(),
                err("later"),
            )
            .unwrap();
        assert!(matches!(next, ReconnectState::Pending { attempt: 1, .. }));
    }

    #[test]
    fn reconnect_modal_ignores_extra_storm_events() {
        // Once we're in Modal, additional non-awaiting events
        // must not change state — the user is in control.
        let now = Instant::now();
        let modal = ReconnectState::Modal(ModalVariant::Generic {
            latest_error: "x".into(),
        });
        let next = modal.on_disconnect(false, None, now, epoch(), no_policy(), err("y"));
        assert!(next.is_none());
    }

    #[test]
    fn reconnect_awaiting_outcome_from_idle_lands_in_modal_defensively() {
        // Defensive arm of on_disconnect: awaiting_outcome=true
        // implies we were in Pending (we just called
        // reconnect() from the GUI tick). If a stale event
        // somehow arrives while state is Idle, the state
        // machine doesn't silently re-arm a retry — it lands
        // in Modal(Generic) so the user takes over. Pin the
        // safety-net behaviour so a future refactor can't
        // strip it without the test catching the change.
        let now = Instant::now();
        let next = ReconnectState::Idle
            .on_disconnect(
                true, // awaiting_outcome from Idle: shouldn't happen, defensive
                None,
                now,
                epoch(),
                no_policy(),
                err("stale"),
            )
            .unwrap();
        assert!(
            matches!(
                &next,
                ReconnectState::Modal(ModalVariant::Generic { latest_error }) if latest_error == "stale"
            ),
            "awaiting_outcome from non-Pending must land in Modal(Generic), got {:?}",
            next
        );
    }

    #[test]
    fn reconnect_awaiting_outcome_from_modal_lands_in_modal_defensively() {
        // Same defensive arm, entering from Modal. Replaces
        // the existing Modal with a fresh Generic carrying
        // the new error — no silent re-arm, no panic.
        let now = Instant::now();
        let modal = ReconnectState::Modal(ModalVariant::OneShotConsumed);
        let next = modal
            .on_disconnect(true, None, now, epoch(), no_policy(), err("stale"))
            .unwrap();
        assert!(
            matches!(
                &next,
                ReconnectState::Modal(ModalVariant::Generic { latest_error }) if latest_error == "stale"
            ),
            "awaiting_outcome from Modal must produce Modal(Generic), got {:?}",
            next
        );
    }

    #[test]
    fn reconnect_backoff_progression_matches_spec() {
        // The backoffs published in plan §A.1 are 1s/4s/16s.
        // Lock them in with a direct check so a typo in the
        // constant array is caught by the test suite.
        assert_eq!(RECONNECT_BACKOFF_SECS, [1, 4, 16]);
        assert_eq!(MAX_RECONNECT_ATTEMPTS, 3);
    }

    // ── Ticket-policy paths ─────────────────────────────────

    fn one_shot_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            ticket_is_single_use: true,
            ticket_valid_until: None,
        }
    }

    fn expiring_at(t: SystemTime) -> ReconnectPolicy {
        ReconnectPolicy {
            ticket_is_single_use: false,
            ticket_valid_until: Some(t),
        }
    }

    #[test]
    fn ticket_single_use_skips_pending_and_lands_in_oneshot_modal() {
        // delete-this-file=1: a fresh disconnect must short-circuit
        // straight to OneShotConsumed without ever entering
        // Pending. Auto-retry would only produce server-side
        // ticket-validation failures.
        let now = Instant::now();
        let next = ReconnectState::Idle
            .on_disconnect(
                false,
                None,
                now,
                epoch(),
                one_shot_policy(),
                err("anything"),
            )
            .unwrap();
        assert!(matches!(
            next,
            ReconnectState::Modal(ModalVariant::OneShotConsumed)
        ));
    }

    #[test]
    fn ticket_single_use_storm_event_is_noop_in_modal() {
        // After landing in OneShotConsumed, a duplicate disconnect
        // (channel storm) must not refire the modal transition.
        let now = Instant::now();
        let modal = ReconnectState::Modal(ModalVariant::OneShotConsumed);
        let next = modal.on_disconnect(false, None, now, epoch(), one_shot_policy(), err("storm"));
        assert!(next.is_none());
    }

    #[test]
    fn ticket_expired_in_past_lands_in_ticket_expired_modal() {
        let now = Instant::now();
        let expiry = epoch() - Duration::from_secs(60);
        let policy = expiring_at(expiry);
        // Disconnect arrives after the ticket has already expired
        // (per our wall clock). Skip Pending entirely.
        let next = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), policy, err("dead"))
            .unwrap();
        match next {
            ReconnectState::Modal(ModalVariant::TicketExpired { expired_at }) => {
                assert_eq!(expired_at, expiry);
            }
            other => panic!("expected Modal(TicketExpired), got {:?}", other),
        }
    }

    #[test]
    fn ticket_valid_in_future_takes_normal_pending_path() {
        let now = Instant::now();
        // Ticket good for another hour: a disconnect should enter
        // Pending(1) as usual.
        let expiry = epoch() + Duration::from_secs(3600);
        let next = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), expiring_at(expiry), err("blip"))
            .unwrap();
        assert!(matches!(next, ReconnectState::Pending { attempt: 1, .. }));
    }

    // ── VolumeControl round-trip (K3 guard) ─────────────────

    #[test]
    fn volume_control_round_trip() {
        // K3 (Phase 03) fixed RyllApp::reconnect() leaving the
        // user's volume slider at 80% / unmuted after every
        // reconnect. The fix relies on the existing
        // Arc<VolumeControl> surviving the swap, with the same
        // get/set semantics on both sides of the boundary. Pin
        // the contract so a future refactor of VolumeControl's
        // storage cannot quietly re-introduce the regression.
        let vc = shakenfist_spice_renderer::channels::playback::VolumeControl::new();
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

    // ── Region-select validation (K4 guard) ─────────────────

    #[test]
    fn validate_region_click_without_drag_returns_none() {
        // Press and release at the same point: degenerate 0×0
        // rectangle, must be rejected at the GUI layer so we
        // never serialise it into report.json.
        assert!(validate_region(100, 100, 100, 100).is_none());
    }

    #[test]
    fn validate_region_zero_width_returns_none() {
        // Same x, non-zero vertical drag — still degenerate.
        assert!(validate_region(50, 10, 50, 80).is_none());
    }

    #[test]
    fn validate_region_zero_height_returns_none() {
        // Non-zero horizontal drag, same y — still degenerate.
        assert!(validate_region(10, 40, 100, 40).is_none());
    }

    #[test]
    fn validate_region_one_by_one_returns_some() {
        // A deliberate 1-pixel drag points at a specific pixel
        // and must be allowed; rejecting it would require a
        // jitter floor without supporting data.
        let r = validate_region(7, 11, 8, 12).expect("1x1 region valid");
        assert_eq!(r.left, 7);
        assert_eq!(r.top, 11);
        assert_eq!(r.right, 8);
        assert_eq!(r.bottom, 12);
    }

    #[test]
    fn validate_region_normal_drag_returns_some() {
        // Happy path: a 30×40 region produced by a normal
        // top-left → bottom-right drag.
        let r = validate_region(10, 20, 40, 60).expect("normal region valid");
        assert_eq!(r.left, 10);
        assert_eq!(r.top, 20);
        assert_eq!(r.right, 40);
        assert_eq!(r.bottom, 60);
    }

    #[test]
    fn validate_region_reversed_drag_normalises() {
        // Drag from bottom-right to top-left must produce the
        // same canonical {left ≤ right, top ≤ bottom} region
        // as the forward drag — the bug-report consumers
        // assume this invariant.
        let r = validate_region(40, 60, 10, 20).expect("reversed drag valid");
        assert_eq!(r.left, 10);
        assert_eq!(r.top, 20);
        assert_eq!(r.right, 40);
        assert_eq!(r.bottom, 60);
    }

    // ── Phase 09 (F1) connection-event message formats ──────

    #[test]
    fn connection_event_message_format_attempt_fire() {
        // The attempt-fire notification template embeds the
        // attempt number so the 30 s dedup window doesn't
        // collapse successive attempts of one cluster. Catches
        // a typo in the format string and pins the embedded
        // MAX_RECONNECT_ATTEMPTS reference.
        let expected = format!("Reconnect attempt {}/{}…", 2, MAX_RECONNECT_ATTEMPTS);
        assert_eq!(expected, "Reconnect attempt 2/3…");
    }

    #[test]
    fn connection_event_message_format_modal_variants() {
        // Pin the (severity, message) pair each ModalVariant
        // maps to. Catches drift between modal copy and
        // notification copy.
        let (sev, msg) = modal_variant_notification(&ModalVariant::Generic {
            latest_error: "ignored".into(),
        });
        assert_eq!(sev, NotifySeverity::Error);
        assert_eq!(msg, "Auto-reconnect failed after 3 attempts");

        let (sev, msg) = modal_variant_notification(&ModalVariant::OneShotConsumed);
        assert_eq!(sev, NotifySeverity::Error);
        assert_eq!(msg, "Connection ended — single-use ticket consumed");

        let (sev, msg) = modal_variant_notification(&ModalVariant::TicketExpired {
            expired_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        });
        assert_eq!(sev, NotifySeverity::Error);
        assert_eq!(msg, "Connection ended — ticket expired");
    }

    // ── Phase 10 (F2) notification-snapshot store ───────────

    fn fresh_traffic() -> TrafficBuffers {
        TrafficBuffers::new()
    }

    #[test]
    fn snapshot_store_evicts_oldest_when_over_cap() {
        // Six pushes against a five-entry cap: the first id
        // should no longer be live.
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        for id in 1u64..=6 {
            store.capture(id, fresh_traffic(), t0);
        }
        assert!(!store.has_live(1, t0), "oldest id should have been evicted");
        for id in 2u64..=6 {
            assert!(store.has_live(id, t0), "id {} should still be live", id);
        }
    }

    #[test]
    fn snapshot_store_drops_expired_entries_on_prune() {
        // Two snapshots, then advance "now" past the TTL.
        // Both should be pruned.
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        store.capture(1, fresh_traffic(), t0);
        store.capture(2, fresh_traffic(), t0);
        let later = t0 + NOTIFICATION_SNAPSHOT_TTL + Duration::from_secs(1);
        store.prune_expired(later);
        assert!(!store.has_live(1, later));
        assert!(!store.has_live(2, later));
    }

    #[test]
    fn snapshot_store_replaces_on_same_id_fold() {
        // A re-fire of the same notification id (within the
        // dedup window) refreshes the captured_at timestamp
        // and does not append a new entry.
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        store.capture(42, fresh_traffic(), t0);
        let later = t0 + Duration::from_secs(10);
        store.capture(42, fresh_traffic(), later);
        // Only one entry tracked.
        assert_eq!(store.insertion_order.len(), 1);
        // After 51 s from later (= t0 + 61), the entry must
        // still be live (the refresh extended its lifetime).
        let check = later + Duration::from_secs(51);
        assert!(store.has_live(42, check));
    }

    #[test]
    fn snapshot_store_lookup_returns_none_after_ttl() {
        let mut store = NotificationSnapshotStore::new();
        let t0 = Instant::now();
        store.capture(7, fresh_traffic(), t0);
        assert!(store.has_live(7, t0));
        let later = t0 + NOTIFICATION_SNAPSHOT_TTL + Duration::from_secs(1);
        assert!(!store.has_live(7, later));
        // take() also prunes; second call returns None.
        assert!(store.take(7, later).is_none());
    }

    #[test]
    fn notification_bug_report_type_serialises() {
        // Round-trip a BugReportType::Notification through
        // serde_json to confirm both fields (notification_id
        // and snapshot_state) land in metadata.json.
        let rt = BugReportType::Notification {
            notification_id: 99,
            snapshot_state: NotificationSnapshotState::AtFire,
        };
        let json = serde_json::to_string(&rt).expect("serialise");
        assert!(json.contains("Notification"));
        assert!(json.contains("\"notification_id\":99"));
        assert!(json.contains("AtFire"));
    }

    // ── Phase-04 render-latency helpers ──────────────────

    #[test]
    fn recent_lag_stats_empty_ring_returns_zeros() {
        let ring: VecDeque<u32> = VecDeque::new();
        assert_eq!(recent_lag_stats(&ring), (0, 0, 0));
    }

    #[test]
    fn recent_lag_stats_computes_min_max_mean() {
        let ring: VecDeque<u32> = [100u32, 300, 200].into_iter().collect();
        let (min, max, mean) = recent_lag_stats(&ring);
        assert_eq!(min, 100);
        assert_eq!(max, 300);
        assert_eq!(mean, 200);
    }

    #[test]
    fn recent_lag_stats_single_sample() {
        let ring: VecDeque<u32> = std::iter::once(42u32).collect();
        assert_eq!(recent_lag_stats(&ring), (42, 42, 42));
    }

    #[test]
    fn push_with_cap_caps_at_recent_lag_ring_cap() {
        let mut ring: VecDeque<u32> = VecDeque::new();
        // Push more than the cap; verify only the most recent
        // RECENT_LAG_RING_CAP entries survive, in order.
        for i in 0..(RECENT_LAG_RING_CAP as u32 + 8) {
            push_with_cap(&mut ring, i);
        }
        assert_eq!(ring.len(), RECENT_LAG_RING_CAP);
        // First retained value should be sample index 8.
        assert_eq!(ring.front().copied(), Some(8));
        // Last retained value should be the very last push.
        assert_eq!(ring.back().copied(), Some(RECENT_LAG_RING_CAP as u32 + 7));
    }

    #[test]
    fn push_with_cap_under_cap_retains_all() {
        let mut ring: VecDeque<u32> = VecDeque::new();
        for i in 0..5 {
            push_with_cap(&mut ring, i);
        }
        assert_eq!(ring.len(), 5);
        assert_eq!(ring.front().copied(), Some(0));
        assert_eq!(ring.back().copied(), Some(4));
    }
}
