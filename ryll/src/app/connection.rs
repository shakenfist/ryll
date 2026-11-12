use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::{error, info, warn};

use crate::auto_snapshot::AutoSnapshotState;
use crate::bugreport::{ChannelSnapshots, TrafficBuffers};
use crate::clipboard_arboard::ArboardClipboard;
use crate::input_egui::HeldKeys;
use crate::notifications::NotificationSource;
use crate::reconnect_policy::{ReconnectPolicy, MAX_RECONNECT_ATTEMPTS};
use crate::settings;
use shakenfist_spice_protocol::NotifySeverity;
use shakenfist_spice_renderer::{ByteCounter, ClipboardBackend, SessionState, MOUSE_MODE_UNKNOWN};

use super::agent_state::AgentState;
use super::reconnect::{modal_variant_notification, server_notice, ModalVariant, ReconnectState};
use super::stats::{BandwidthTracker, Statistics};
use super::trigger_snapshot::RegionSelectEnd;
use super::ui_state::UiStateTracker;
use super::{RyllApp, EVENT_CHANNEL_SIZE, INPUT_CHANNEL_SIZE};

/// How many retiring auto-snapshot threads may be outstanding before
/// a new session declines to spawn another.
///
/// Nothing joins these threads — every retire runs on the eframe UI
/// thread — so this cap, not a join, is what bounds thread growth
/// under a server repeating `SPICE_MSG_MAIN_INIT`. A retiring thread
/// exits within one 500 ms poll unless it is inside a zip write, so
/// four outstanding means the host is already far behind; refusing the
/// spawn costs the new session its auto-snapshots (logged at warn)
/// rather than letting threads accumulate.
///
/// `MIN_SESSION_RESPAWN_INTERVAL` makes reaching this cap unlikely in
/// the first place; the two bounds are deliberately independent, since
/// the rate limit is about how fast respawns are requested and this is
/// about how fast the previous ones actually finish.
pub(super) const MAX_RETIRING_AUTO_SNAPSHOT_THREADS: usize = 4;

/// Build the per-connection tokio runtime, honouring the
/// `--debug-single-thread-runtime` flag. The current_thread
/// flavour runs every spawned task on the calling thread,
/// which lets us tell a real blocking call (still hangs
/// because there's nowhere else to make progress) from a
/// multi-threaded scheduler / Waker-registration anomaly
/// (does not hang). Used by both the initial-connect and
/// reconnect spawn sites.
pub(super) fn build_connection_runtime(single_thread: bool) -> tokio::runtime::Runtime {
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

impl RyllApp {
    pub(super) fn reconnect(&mut self) {
        // The surfaces are cleared below, which would leave a region
        // selection running with nothing to select from: its overlay
        // is drawn over the surface, so it vanishes, while guest
        // input and F8/F11/F12 stay suppressed. Write the report now,
        // while the submit-time surface still exists.
        self.end_region_select(RegionSelectEnd::Reconnecting);
        // Signal the previous attempt (if any) to exit before
        // spawning the next. The previous run_connection sees the
        // flag in its 100 ms select branch and breaks out, which
        // drops its tokio runtime when the spawned thread returns.
        if let Some(prev) = self.connection_cancel.take() {
            prev.store(true, Ordering::Relaxed);
        }
        // The auto-snapshot task about to be replaced holds Arcs to
        // the `traffic` / `channel_snapshots` this function is about
        // to swap out, so its remaining zips would describe a session
        // that no longer exists. Signal now; the respawn moves the
        // handle to the retiring set without waiting for it.
        self.signal_auto_snapshot_retire();

        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
        // A fresh state for the new session, subscribed before it is
        // spawned. Keeping the old receivers would leave this app
        // reading a finished session's last mouse mode.
        let session_state = SessionState::new();
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);
        let (usb_tx, usb_rx) = mpsc::channel(16);
        let (webdav_tx, webdav_rx) = mpsc::channel(16);
        let (resize_tx, resize_rx) = mpsc::channel(32);
        let resize_tx = Arc::new(resize_tx);

        let byte_counter = Arc::new(ByteCounter::new());
        let traffic = Arc::new(TrafficBuffers::new());
        let channel_snapshots = ChannelSnapshots::new();

        self.event_rx = event_rx;
        self.channel_ends.clear();
        self.pending_disconnect_report = None;
        self.session_state = session_state.subscribe();
        self.agent_state = AgentState::Unknown;
        self.paste_hint_shown = false;
        self.input_tx = Some(input_tx);
        self.resize_tx = Some(resize_tx);
        self.last_sent_resize = None;
        self.pending_guest_size = None;
        // volume_control is intentionally NOT replaced — see the
        // `vol_for_conn` binding below for the rationale.
        self.surfaces.surfaces.clear();
        self.textures.clear();
        self.cursor_pos = (0, 0);
        self.cursor_visible = true;
        self.cursor_image = None;
        self.cursor_texture = None;
        self.surface_rect = egui::Rect::NOTHING;
        self.stats = Statistics::default();
        self.last_cadence_key = Instant::now();
        self.connected = false;
        self.error_message = None;
        self.mouse_mode = MOUSE_MODE_UNKNOWN;
        // Clear the main-channel keepalive-timeout flag so a
        // subsequent disconnect reports its own cause cleanly
        // rather than inheriting the previous attempt's state.
        if let Ok(mut snap) = self.channel_snapshots.main.lock() {
            snap.keepalive_timeout_fired = false;
        }
        self.pointer.reset();
        self.last_modifiers = None;
        self.held_keys = HeldKeys::new(traffic.session_start());
        self.ui_state = UiStateTracker::new(traffic.session_start());
        self.forwarded_buttons = 0;
        self.wheel.reset();
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
        // The new session's drop counter starts at zero.
        self.last_seen_events_dropped = 0;
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
        let h264 = self.h264;
        let image_cache_cap_bytes = self.image_cache_cap_bytes;
        let glz_dictionary_cap_bytes = self.glz_dictionary_cap_bytes;
        let preferred_compression = self.preferred_compression;
        let (failure_tx, failure_rx) = oneshot::channel();
        self.connection_failure_rx = Some(failure_rx);
        self.pending_connection_failure = None;

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

        info!("app: reconnecting...");
    }

    /// Driven by the disconnect modal's Reconnect button. Counts
    /// as user intervention: clears the cluster-reset window so a
    /// failed manual attempt re-arms the full 3-attempt budget
    /// rather than going straight back to Modal.
    pub(super) fn reconnect_manual(&mut self) {
        // Surface the click in the bell history so the user has visible
        // confirmation that the button registered before the connection
        // actually completes.
        self.push_connection_event(NotifySeverity::Info, "Reconnecting (manual)…".to_string());
        self.reconnect_state = ReconnectState::Idle;
        self.awaiting_reconnect_outcome = false;
        self.last_modal_at = None;
        self.reconnect();
    }

    /// Ask the running auto-snapshot task to retire, without waiting
    /// for it.
    ///
    /// Called on every critical disconnect. Until this existed the
    /// cancel flag was only raised by the *next* `SessionInitialized`,
    /// so a server that accepted one session and then refused every
    /// reconnect left the last task writing full-size zips of a frozen
    /// pcap for as long as ryll stayed open. Nothing anywhere joins
    /// these threads — every caller runs on the egui UI thread and an
    /// in-flight zip write can take seconds; `retire_auto_snapshot_task`
    /// bounds them by counting instead.
    fn signal_auto_snapshot_retire(&self) {
        if let Some(cancel) = &self.auto_snapshot_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Drop the handles of any retiring auto-snapshot threads that
    /// have finished.
    ///
    /// `is_finished` never blocks, so this is safe to call from the
    /// UI thread. Dropping a `JoinHandle` detaches rather than waits,
    /// so the only thing reaping buys is an accurate count for
    /// [`MAX_RETIRING_AUTO_SNAPSHOT_THREADS`] — which is the point:
    /// without it the cap would eventually refuse a respawn on the
    /// strength of threads that exited long ago.
    fn reap_auto_snapshot_threads(&mut self) {
        self.auto_snapshot_retiring
            .retain(|handle| !handle.is_finished());
    }

    /// Ask the running auto-snapshot task to retire and move its
    /// handle to the retiring set, without waiting for it.
    ///
    /// Called immediately before a respawn. Returns `false` when too
    /// many previous threads are still winding down, in which case the
    /// caller must not spawn another: that cap is what bounds thread
    /// growth now that nothing joins. A no-op returning `true` when
    /// the mode is disabled or nothing is running.
    pub(super) fn retire_auto_snapshot_task(&mut self) -> bool {
        self.signal_auto_snapshot_retire();
        self.auto_snapshot_cancel = None;
        self.reap_auto_snapshot_threads();
        if let Some(handle) = self.auto_snapshot_thread.take() {
            self.auto_snapshot_retiring.push(handle);
        }
        if self.auto_snapshot_retiring.len() > MAX_RETIRING_AUTO_SNAPSHOT_THREADS {
            warn!(
                "app: {} auto-snapshot threads are still retiring (cap {}); \
                 not spawning another for this session",
                self.auto_snapshot_retiring.len(),
                MAX_RETIRING_AUTO_SNAPSHOT_THREADS,
            );
            return false;
        }
        true
    }

    /// Spawn the auto-snapshot interval task for the session just
    /// initialized, retiring the outgoing one first.
    ///
    /// Returns whether it spawned: `false` when the retire refused,
    /// which is the caller's cue to stop rather than carry on as if a
    /// task were running.
    pub(super) fn spawn_auto_snapshot_task(&mut self, interval_secs: u64) -> bool {
        // Retire the outgoing task before spawning, so at
        // most one auto-snapshot thread is *running* at any
        // moment. Nothing is joined — this is the UI thread
        // — so the retire can refuse when too many previous
        // threads are still winding down; honour that rather
        // than spawning anyway.
        if !self.retire_auto_snapshot_task() {
            return false;
        }
        let output_dir = self.manual_bug_report_dir().join("auto-snapshots");
        let cap = self.auto_snapshot_cap;
        let byte_cap = crate::auto_snapshot::DEFAULT_AUTO_SNAPSHOT_BYTE_CAP;
        let cancel = Arc::new(AtomicBool::new(false));
        self.auto_snapshot_cancel = Some(cancel.clone());

        // One-shot startup notification — fire only on
        // the first session so reconnects don't spam.
        // Both budgets are quoted: the count alone tells
        // the operator nothing about how much disk the
        // mode is authorised to use.
        if !self.auto_snapshot_startup_notified {
            self.auto_snapshot_startup_notified = true;
            self.push_notification(
                NotifySeverity::Info,
                NotificationSource::Internal,
                format!(
                    "Auto-snapshot mode enabled \
                     — every {}s, max {} snapshots / {} MiB, \
                     saving to {}",
                    interval_secs,
                    cap,
                    byte_cap / (1024 * 1024),
                    output_dir.display(),
                ),
            );
        }

        let state = AutoSnapshotState {
            traffic: self.traffic.clone(),
            channel_snapshots: self.channel_snapshots.clone(),
            app_snapshot: self.app_snapshot.clone(),
            notifications: self.notifications.clone(),
            target: self.target.clone(),
            output_dir,
            interval: Duration::from_secs(interval_secs),
            cap,
            byte_cap,
            cancel,
        };
        let handle = std::thread::spawn(move || {
            // A runtime that will not build means an
            // opt-in diagnostic feature does not start;
            // it is not grounds to abort ryll. The
            // previous `.expect()` here panicked the
            // auto-snapshot thread, losing the mode for
            // the session without saying why.
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    warn!(
                        "auto-snapshot: cannot build tokio runtime, mode \
                         inactive for this session: {}",
                        e
                    );
                    return;
                }
            };
            rt.block_on(crate::auto_snapshot::run_auto_snapshot_loop(state));
        });
        self.auto_snapshot_thread = Some(handle);
        true
    }

    /// Drive the `ReconnectState` machine in response to a critical
    /// disconnect (Main/Display/Inputs going down, or any
    /// `ChannelEvent::Error`). Callers are responsible for writing
    /// the disconnect snapshot before invoking this — keeping the
    /// snapshot at the call site avoids double-writing when the
    /// `Disconnected` handler has already snapshotted for the
    /// channel-storm path.
    /// Forget the session that has just ended: clear surfaces and
    /// cursor so the next paint shows a clean canvas, and tell the
    /// auto-snapshot task to stop capturing a dead session.
    fn clear_ended_session(&mut self) {
        self.connected = false;
        self.signal_auto_snapshot_retire();
        // As in `reconnect`: end any region selection while the surface
        // it was started over still exists, rather than leave it running
        // over an empty canvas until a reconnect or Escape ends it.
        self.end_region_select(RegionSelectEnd::Disconnected);
        self.surfaces.surfaces.clear();
        self.textures.clear();
        self.cursor_image = None;
        self.cursor_texture = None;
    }

    /// The server sent the main channel's `DISCONNECTING`, so it ended
    /// the session on purpose. Show the `ServerEnded` modal rather than
    /// auto-reconnecting, which against a server where the newest
    /// client wins would take the session straight back from the
    /// client that has just been given it. The display and inputs
    /// channels the server closes next are then ignored, because the
    /// state machine keeps the modal (`ReconnectState::on_disconnect`).
    /// Callers write the disconnect snapshot first, as for
    /// `handle_critical_disconnect`.
    pub(super) fn handle_server_ended(&mut self) {
        let now = Instant::now();
        let notice = server_notice(self.last_main_notice.as_ref(), now);
        self.clear_ended_session();
        let Some(new_state) = self.reconnect_state.on_server_ended(notice) else {
            return;
        };
        info!("app: the server ended the session; not reconnecting");
        self.awaiting_reconnect_outcome = false;
        self.last_modal_at = Some(now);
        if let ReconnectState::Modal(variant) = &new_state {
            let (severity, message) = modal_variant_notification(variant);
            self.push_connection_event(severity, message);
        }
        self.reconnect_state = new_state;
    }

    pub(super) fn handle_critical_disconnect(&mut self, message: String) {
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
        // connection itself has gone.
        self.clear_ended_session();

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
                // Initial disconnect — announce the start of the auto-retry cycle.
                self.push_connection_event(
                    NotifySeverity::Warn,
                    "Connection lost — reconnecting…".to_string(),
                );
            } else {
                // attempt > 1: the previous attempt just failed. Recorded under
                // NotificationSource::Connection.
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
            // Also surface the Modal entry itself as a connection event so the
            // bell history records the cycle's terminal state. The modal pops
            // in parallel; users who dismiss it reflexively still see the
            // event in the notification log.
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
    pub(super) fn reconnect_policy(&self) -> ReconnectPolicy {
        ReconnectPolicy {
            ticket_is_single_use: self.config.ticket_is_single_use,
            ticket_valid_until: self.config.ticket_valid_until,
        }
    }

    /// The current attempt's connection task returned an error: it
    /// could not set a session up. Before this existed the error was
    /// only logged, which left a failed first connect showing
    /// "Connecting..." and a failed auto-retry showing "Reconnecting…"
    /// for as long as ryll stayed open.
    pub(super) fn handle_connection_failed(&mut self, message: String) {
        self.push_connection_event(
            NotifySeverity::Error,
            format!("Connection attempt failed: {}", message),
        );

        if self.connected || self.awaiting_reconnect_outcome {
            // Either a session was set up before the failure (a
            // secondary channel would not open), so this is a
            // disconnect and the ticket policy applies, or this was an
            // auto-retry, whose budget the state machine keeps.
            self.maybe_write_disconnect_snapshot("connect", &message, "ConnectFailed");
            self.handle_critical_disconnect(message);
            return;
        }

        // A first attempt or a manual Reconnect that never reached a
        // session. That is usually a mistyped port or a server that is
        // down rather than a ryll bug, and the zip would hold no
        // traffic, so only write one when the user asked for bug
        // reports or a capture.
        if self.bug_report_dir.is_some() || self.capture.is_some() {
            self.maybe_write_disconnect_snapshot("connect", &message, "ConnectFailed");
        }

        // Routing this through `handle_critical_disconnect` would retry
        // silently in the background and, for a `delete-this-file=1`
        // ticket, claim the ticket was consumed by a session that never
        // existed. Say what happened instead.
        self.reconnect_state =
            ReconnectState::Modal(ModalVariant::ConnectFailed { error: message });
        self.last_modal_at = Some(Instant::now());
    }
}
