use std::time::Instant;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::bugreport::SurfaceInfo;
use crate::notifications::NotificationSource;
use shakenfist_spice_protocol::{ChannelType, NotifySeverity, MOUSE_MODE_SERVER};
use shakenfist_spice_renderer::channels::{DisconnectReason, PasteFailureKind};
use shakenfist_spice_renderer::{ChannelEvent, DrawOutcome};

use super::reconnect::ReconnectState;
use super::resize::{auto_fit_size_acceptable, is_primary_surface, MAX_AUTO_FIT_DIMENSION};
use super::session_init::{
    classify_session_initialized, SessionInitDecision, MIN_SESSION_RESPAWN_INTERVAL,
};
use super::stats::{push_with_cap, recent_lag_stats, FPS_WINDOW_SIZE};
use super::RyllApp;

// Two `ChannelEvent` variants reach the GUI and are deliberately
// ignored, and the dispatch below names both rather than ending in
// `_`, so that a new variant fails to compile here until someone
// decides whether the GUI should act on it.
//
// `ChannelsAvailable` is consumed by nobody: `session::connect` in
// the renderer crate learns the channel list from a oneshot the main
// channel fires immediately before it emits the event, so the event
// is informational. `DigestUpdated`, which exists only under the
// `digest-decode` feature, is turned into the control socket's
// `digest_updated` wire event by the renderer's control server.

impl RyllApp {
    pub(super) fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            let consumed_at_secs = self.traffic.elapsed().as_secs_f64();
            self.sample_event_lag(&event, consumed_at_secs);
            match event {
                ChannelEvent::SessionInitialized(session_id) => {
                    self.handle_session_initialized(session_id)
                }
                ChannelEvent::SurfaceCreated { .. }
                | ChannelEvent::SurfaceDestroyed { .. }
                | ChannelEvent::ImageReady { .. }
                | ChannelEvent::ImageReadyChroma { .. }
                | ChannelEvent::ImageReadyAlpha { .. }
                | ChannelEvent::FillRect { .. }
                | ChannelEvent::CopyBits { .. }
                | ChannelEvent::Invert { .. } => self.handle_draw_event(&event),
                ChannelEvent::DisplayMark { .. } => self.handle_display_mark(),
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
                ChannelEvent::MonitorsConfig { width, height } => {
                    debug!("app: requested monitors config {}x{}", width, height);
                }
                ChannelEvent::PingInterval { interval_ms } => {
                    self.stats.last_ping_interval_ms = Some(interval_ms as f64);
                    self.ping_interval.record(interval_ms);
                }
                ChannelEvent::InputRoundTrip { rtt_ms } => {
                    self.stats.last_input_rtt_ms = Some(rtt_ms as f64);
                    self.input_rtt.record(rtt_ms);
                }
                ChannelEvent::Error { channel, message } => {
                    self.handle_channel_error(channel, message)
                }
                ChannelEvent::UsbChannelReady => self.handle_usb_channel_ready(),
                ChannelEvent::UsbDeviceConnected(desc) => self.handle_usb_device_connected(desc),
                ChannelEvent::UsbDeviceDisconnected => self.handle_usb_device_disconnected(),
                ChannelEvent::UsbConnectFailed(err) => self.handle_usb_connect_failed(err),
                ChannelEvent::WebdavChannelReady => self.handle_webdav_channel_ready(),
                ChannelEvent::WebdavSharingStarted { path, read_only } => {
                    self.handle_webdav_sharing_started(path, read_only)
                }
                ChannelEvent::WebdavSharingStopped => self.handle_webdav_sharing_stopped(),
                ChannelEvent::WebdavError(err) => self.handle_webdav_error(err),
                ChannelEvent::PasteCompleted {
                    chars, elapsed_ms, ..
                } => self.handle_paste_completed(chars, elapsed_ms),
                ChannelEvent::PasteFailed {
                    kind: PasteFailureKind::InProgress,
                    reason,
                    ..
                } => self.handle_paste_refused(reason),
                ChannelEvent::PasteFailed { reason, .. } => self.handle_paste_failed(reason),
                ChannelEvent::Notification(entry) => {
                    if let Ok(mut guard) = self.notifications.lock() {
                        guard.push(entry);
                    }
                }
                ChannelEvent::Disconnected(channel, reason) => {
                    self.handle_disconnected(channel, reason)
                }
                ChannelEvent::ChannelsAvailable(_) => {}
                #[cfg(feature = "digest-decode")]
                ChannelEvent::DigestUpdated { .. } => {}
            }
        }

        self.finish_event_drain();
    }

    // Record mpsc-queue lag from event emit to app pickup for
    // renderer-side latency diagnostics. See
    // `docs/plans/PLAN-video-keeping-up.md`. Within-batch
    // correlation noted: events drained in one process_events()
    // call share this read, so several samples from one egui frame
    // will be close in time.
    fn sample_event_lag(&mut self, event: &ChannelEvent, consumed_at_secs: f64) {
        match event {
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
    }

    /// Announce a newly initialized session and respawn the
    /// per-session auto-snapshot task.
    fn handle_session_initialized(&mut self, session_id: u32) {
        // Nothing on the wire obliges a server to send
        // SPICE_MSG_MAIN_INIT exactly once, and the renderer
        // emits this event for every one it decodes. Acting on
        // every one would re-announce the connection and
        // respawn the auto-snapshot task per ~36-byte message.
        // See `classify_session_initialized` for why the
        // duplicate check alone is not enough.
        match classify_session_initialized(
            self.connected,
            self.current_session_id,
            session_id,
            self.last_session_initialized_at.map(|at| at.elapsed()),
        ) {
            SessionInitDecision::IgnoreDuplicate => {
                debug!(
                    "app: ignoring duplicate SessionInitialized for session {}",
                    session_id
                );
                return;
            }
            SessionInitDecision::IgnoreTooSoon => {
                debug!(
                    "app: ignoring SessionInitialized for session {} — under \
                     the {:?} minimum since the last accepted one",
                    session_id, MIN_SESSION_RESPAWN_INTERVAL
                );
                return;
            }
            SessionInitDecision::Accept => {}
        }
        self.last_session_initialized_at = Some(Instant::now());
        info!("app: session {} initialized", session_id);
        self.current_session_id = Some(session_id);
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
        // Surface the link in the bell history. Fires on initial
        // connect and on every reconnect success; the 30 s dedup
        // collapses storm reconnects to a single entry.
        // `target` already carries the port (or, under
        // a proxy tunnel, the redacted target) via
        // `display_target()`, so it is printed as is.
        self.push_connection_event(
            NotifySeverity::Info,
            format!("Connected to {}", self.target),
        );

        // Spawn the auto-snapshot interval task. Retire-and-respawn
        // per session: `reconnect()` replaces `self.traffic` and
        // `self.channel_snapshots` with fresh instances, so any
        // task spawned for the previous session is holding stale
        // Arcs.
        if let Some(interval_secs) = self.auto_snapshot_interval {
            if !self.spawn_auto_snapshot_task(interval_secs) {
                // The refusal is already logged at warn. Nothing
                // follows this today, so the return is redundant; it
                // is kept so that a statement added after it is not
                // reached on a refused retire, which is what the
                // `continue` this replaces guaranteed while the arm
                // was inline. `expect` rather than `allow`, so that
                // adding one fails the build here until someone
                // decides whether it belongs before this check.
                #[expect(
                    clippy::needless_return,
                    reason = "keeps the escape the inline continue guaranteed"
                )]
                return;
            }
        }
    }

    /// Apply a draw op to the surface mirror and layer the GUI's
    /// auto-fit, texture invalidation and frame stats on the outcome.
    fn handle_draw_event(&mut self, event: &ChannelEvent) {
        // The mirror applies (and logs) the draw op exactly
        // as headless and web mode do; the GUI layers
        // auto-fit, texture invalidation and frame stats on
        // what it reports.
        let outcome = self.surfaces.apply_event(event);
        match outcome {
            DrawOutcome::Created { key, width, height }
            | DrawOutcome::AutoCreated { key, width, height } => {
                // A new surface must not inherit a texture
                // sized for the one it replaced.
                self.textures.remove(key);
                if is_primary_surface(key.0, key.1) {
                    if auto_fit_size_acceptable(width, height) {
                        self.pending_resize = Some((width as f32, height as f32));
                        self.pending_resolution_notify = Some(((width, height), Instant::now()));
                    } else {
                        let how = if matches!(outcome, DrawOutcome::AutoCreated { .. }) {
                            "auto-created"
                        } else {
                            "created"
                        };
                        warn!(
                            "app: ignoring oversized {} primary surface {}x{} for \
                             auto-fit (limit {}px per axis)",
                            how, width, height, MAX_AUTO_FIT_DIMENSION
                        );
                    }
                }
            }
            DrawOutcome::Destroyed { key } => self.textures.remove(key),
            DrawOutcome::Drawn { .. }
            | DrawOutcome::UnknownSurface { .. }
            | DrawOutcome::NotDisplay => {}
        }
        // Every draw that lands counts, including the one
        // that auto-created its surface.
        if matches!(
            outcome,
            DrawOutcome::AutoCreated { .. } | DrawOutcome::Drawn { .. }
        ) {
            self.stats.frames_received += 1;
        }
    }

    /// Record a frame boundary, and capture a video frame from it when
    /// capture is enabled.
    fn handle_display_mark(&mut self) {
        // Frame boundary — record timestamp for FPS calculation
        let now = Instant::now();
        self.stats.frame_times.push(now);
        if self.stats.frame_times.len() > FPS_WINDOW_SIZE {
            self.stats.frame_times.remove(0);
        }

        // Capture a video frame if enabled. frame() is a non-blocking
        // enqueue returning bool; false means the encoder task's queue was
        // full and the frame was dropped.
        if let Some(ref capture) = self.capture {
            if let Some(surface) = self
                .surfaces
                .surfaces
                .values()
                .max_by_key(|s| (s.width as u64) * (s.height as u64))
            {
                if !capture.frame(0, surface.pixels(), surface.width, surface.height) {
                    self.video_drop_count = self.video_drop_count.saturating_add(1);
                }
            }
        }
    }

    /// A channel reported an I/O or protocol error: record it and drive
    /// the critical-disconnect path.
    fn handle_channel_error(&mut self, channel: ChannelType, message: String) {
        error!("app: {} channel error: {}", channel.name(), message);
        // Surface the raw error in the bell history before the
        // state-machine path can swallow it into the modal.
        // handle_* below also pushes the resulting connection-lost
        // and modal-entry notifications.
        self.push_connection_event(
            NotifySeverity::Error,
            format!("{} channel error: {}", channel.name(), message),
        );
        // The report is written after this frame's events, from
        // the traffic leading up to the failure.
        self.note_channel_end(channel, "Error");
        self.request_disconnect_report(channel.name(), &message, "Error");
        let reason = format!("{} channel error: {}", channel.name(), message);
        self.handle_critical_disconnect(reason);
    }

    /// The USB redirection channel connected.
    fn handle_usb_channel_ready(&mut self) {
        info!("app: USB redirection channel connected");
        self.usb_channel_ready = true;
    }

    /// A USB device was connected.
    fn handle_usb_device_connected(&mut self, desc: String) {
        info!("app: USB device connected: {}", desc);
        self.usb_device_description = Some(desc);
        self.clear_usb_operation_flags();
        self.usb_connected_at = Some(Instant::now());
    }

    /// A USB device was disconnected.
    fn handle_usb_device_disconnected(&mut self) {
        info!("app: USB device disconnected");
        self.usb_device_description = None;
        self.clear_usb_operation_flags();
        self.usb_connected_at = None;
    }

    /// A USB device connection attempt failed.
    fn handle_usb_connect_failed(&mut self, err: String) {
        error!("app: USB connect failed: {}", err);
        self.clear_usb_operation_flags();
        self.usb_error_message = Some(err);
        self.usb_error_time = Some(Instant::now());
    }

    /// The WebDAV channel connected.
    fn handle_webdav_channel_ready(&mut self) {
        info!("app: WebDAV channel connected");
        self.webdav_channel_ready = true;
    }

    /// WebDAV folder sharing started.
    fn handle_webdav_sharing_started(&mut self, path: String, read_only: bool) {
        info!("app: WebDAV sharing started: {} (ro={})", path, read_only);
        self.webdav_shared_dir = Some(path);
        self.webdav_read_only = read_only;
        self.webdav_sharing_active = true;
        self.webdav_connected_at = Some(Instant::now());
    }

    /// WebDAV folder sharing stopped.
    fn handle_webdav_sharing_stopped(&mut self) {
        info!("app: WebDAV sharing stopped");
        self.webdav_shared_dir = None;
        self.webdav_sharing_active = false;
        self.webdav_connected_at = None;
    }

    /// The WebDAV channel reported an error.
    fn handle_webdav_error(&mut self, err: String) {
        error!("app: WebDAV error: {}", err);
        self.webdav_error_message = Some(err);
        self.webdav_error_time = Some(Instant::now());
    }

    /// A paste-as-keystrokes sequence finished typing.
    fn handle_paste_completed(&mut self, chars: usize, elapsed_ms: u64) {
        info!("app: paste complete: {} chars in {}ms", chars, elapsed_ms);
        self.push_notification(
            NotifySeverity::Info,
            NotificationSource::Internal,
            format!("Pasted {} chars ({}ms)", chars, elapsed_ms),
        );
    }

    /// A paste was refused because one is already being typed.
    fn handle_paste_refused(&mut self, reason: String) {
        // A second Ctrl+Alt+V while a long paste is still
        // typing. Nothing is wrong with either paste, so
        // a note rather than the blocking error window,
        // which would also hold off the next paste until
        // it was dismissed.
        info!("app: paste refused: {}", reason);
        self.push_notification(
            NotifySeverity::Info,
            NotificationSource::Internal,
            "Paste ignored: the previous paste is still being typed".to_string(),
        );
    }

    /// A paste did not type its text, or not all of it.
    fn handle_paste_failed(&mut self, reason: String) {
        error!("app: paste failed: {}", reason);
        self.paste_error_message = Some(reason);
    }

    /// A channel's connection ended without an error: clean up after
    /// it, and tear the session down if it was a critical one.
    fn handle_disconnected(&mut self, channel: ChannelType, reason: DisconnectReason) {
        info!(
            "app: channel {} disconnected ({})",
            channel.name(),
            reason.name()
        );
        self.note_channel_end(channel, reason.name());

        // Snapshot for every channel disconnect, including
        // non-critical ones. Under ticket-based deployments
        // (oVirt, Kerbside) a dropped channel is permanently
        // lost — the user silently loses audio / USB / etc.
        // Even if the session keeps running, we want
        // diagnostic data on why the channel went down.
        self.request_disconnect_report(
            channel.name(),
            &format!("channel {} disconnected", channel.name()),
            reason.name(),
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
                let reason = format!("Connection lost ({} channel disconnected)", channel.name());
                // The report was requested above, so
                // the helper only drives the state
                // machine.
                self.handle_critical_disconnect(reason);
            }
            _ => {
                debug!(
                    "app: non-critical channel {} disconnected, session continues",
                    channel.name()
                );
                // Non-critical disconnect goes to the bell as Info.
                // Surfaces e.g. usbredir / webdav drops without
                // disrupting the user.
                self.push_connection_event(
                    NotifySeverity::Info,
                    format!("{} channel disconnected", channel.name()),
                );
            }
        }
    }

    /// Finish the per-frame drain: write any disconnect snapshot the
    /// events asked for, pick up what travelled outside the queue, and
    /// refresh the shared app snapshot.
    fn finish_event_drain(&mut self) {
        if let Some(report) = self.pending_disconnect_report.take() {
            self.maybe_write_disconnect_snapshot(&report.channel, &report.message, &report.kind);
        }

        // After the drain rather than before it, so an agent change the
        // main channel published after queueing `SessionInitialized` is
        // announced after the session is, as it was when it was an event.
        self.sync_session_state();
        self.check_event_drops();

        // Act on a connection failure only once `event_rx` is empty.
        // The failure travels on its own channel, so it can be seen
        // before events the attempt sent ahead of it; a
        // `SessionInitialized` among those decides how the failure is
        // handled. Anything still queued here is drained next frame.
        if self.pending_connection_failure.is_none() {
            if let Some(rx) = self.connection_failure_rx.as_mut() {
                match rx.try_recv() {
                    Ok(message) => {
                        self.pending_connection_failure = Some(message);
                        self.connection_failure_rx = None;
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    // The attempt ended without failing.
                    Err(oneshot::error::TryRecvError::Closed) => {
                        self.connection_failure_rx = None;
                    }
                }
            }
        }
        if self.pending_connection_failure.is_some() && self.event_rx.is_empty() {
            if let Some(message) = self.pending_connection_failure.take() {
                self.handle_connection_failed(message);
            }
        }

        self.update_app_snapshot();
    }

    /// Warn once when the main channel has started dropping events.
    ///
    /// The counter is read from the shared `MainSnapshot`, not through
    /// the event queue, so it works while the queue is wedged. The
    /// snapshot is refreshed by the main channel after each read batch
    /// and each send, so it keeps moving while this thread is stalled.
    /// `try_lock` keeps the UI from ever waiting on the main channel.
    fn check_event_drops(&mut self) {
        let Ok(snap) = self.channel_snapshots.main.try_lock() else {
            return;
        };
        let current = snap.events_dropped_count;
        drop(snap);
        let notify = event_drops_need_notice(self.last_seen_events_dropped, current);
        self.last_seen_events_dropped = current;
        if notify {
            // Fixed text: the notification store folds repeats by exact
            // message match, so no count or time may appear here (#429).
            self.push_connection_event(NotifySeverity::Warn, EVENT_DROP_NOTICE);
        }
    }

    /// Pick up the mouse mode and agent state the main channel has
    /// published since the last frame.
    ///
    /// These travel on a `watch` rather than the event queue because
    /// the main channel abandons an event it cannot queue within five
    /// seconds, and a lost mouse-mode change left this app sending the
    /// pointer messages the server ignores until reconnect (#428). A
    /// `watch` only holds the latest value, so a stall can delay these
    /// but not lose them; several agent changes inside one stall are
    /// announced as one.
    fn sync_session_state(&mut self) {
        let mode = *self.session_state.mouse_mode.borrow();
        if mode != self.mouse_mode {
            info!(
                "app: mouse mode: {} ({})",
                mode,
                if mode == MOUSE_MODE_SERVER {
                    "server"
                } else {
                    "client"
                }
            );
            self.mouse_mode = mode;
        }

        let agent = {
            let seen = self.session_state.agent_connected.borrow_and_update();
            seen.has_changed().then_some(*seen)
        };
        if let Some(connected) = agent {
            info!("app: vdagent connected={}", connected);
            // Record the agent-state transition. Affects clipboard sync, paste,
            // and resolution updates — useful for the user to see when those
            // features come or go. A server with no agent at all is told
            // apart from an agent that went away (#537).
            let (state, notice) = self.agent_state.on_report(connected);
            self.agent_state = state;
            if let Some(notice) = notice {
                self.push_connection_event(NotifySeverity::Info, notice);
            }
        }
    }

    /// Clear USB operation-in-progress flags.
    fn clear_usb_operation_flags(&mut self) {
        self.usb_connecting = false;
        self.usb_disconnecting = false;
    }

    /// Sync app-level state to the shared snapshot.
    fn update_app_snapshot(&self) {
        let mut snap = self.app_snapshot.lock().expect("lock poisoned");

        // FPS from sliding-window frame_times. A single-entry window
        // yields elapsed == 0.0 and falls through to the 0.0 arm.
        snap.fps = if let (Some(oldest), Some(newest)) = (
            self.stats.frame_times.first(),
            self.stats.frame_times.last(),
        ) {
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
        snap.last_ping_interval_ms = self.stats.last_ping_interval_ms;
        snap.last_input_rtt_ms = self.stats.last_input_rtt_ms;
        snap.frames_received = self.stats.frames_received;
        snap.surfaces = self
            .surfaces
            .surfaces
            .values()
            .map(|s| SurfaceInfo {
                surface_id: s.id,
                width: s.width,
                height: s.height,
            })
            .collect();
        snap.cursor_pos = self.cursor_pos;
        snap.cursor_visible = self.cursor_visible;
        snap.mouse_mode = self.mouse_mode;
        snap.connected = self.connected;
        snap.uptime_secs = self.traffic.elapsed().as_secs_f64();
        snap.auto_reconnect_count = self.auto_reconnect_count;
        snap.video_drop_count = self.video_drop_count;
        snap.audio_volume_percent = Some(self.volume_control.volume());
        snap.audio_muted = Some(self.volume_control.muted());
        // Copied only when a key event has been logged since, as this runs
        // on every pass and the log holds up to 256 entries.
        let keys_seen = self.held_keys.events_seen();
        if snap.keyboard.as_ref().map(|k| k.events_seen) != Some(keys_seen) {
            snap.keyboard = Some(self.held_keys.snapshot());
        }
        let ui_seen = self.ui_state.updates_seen();
        if snap.ui.as_ref().map(|u| u.updates_seen) != Some(ui_seen) {
            snap.ui = Some(self.ui_state.snapshot());
        }

        // Render-side latency aggregates.
        let (img_min, img_max, img_mean) = recent_lag_stats(&self.recent_image_ready_lag_us);
        snap.image_ready_lag_recent_min_us = img_min;
        snap.image_ready_lag_recent_max_us = img_max;
        snap.image_ready_lag_recent_mean_us = img_mean;
        let (mark_min, mark_max, mark_mean) = recent_lag_stats(&self.recent_display_mark_lag_us);
        snap.display_mark_lag_recent_min_us = mark_min;
        snap.display_mark_lag_recent_max_us = mark_max;
        snap.display_mark_lag_recent_mean_us = mark_mean;
    }
}

/// Fixed text of the notification raised when the main channel drops
/// events. It must not vary, or repeats stop coalescing (#429).
const EVENT_DROP_NOTICE: &str =
    "The display stalled and missed some updates from the server; details are in the next bug report";

/// Whether the main channel's drop counter has risen since it was last
/// looked at. Pure for unit-testability.
fn event_drops_need_notice(last_seen: u64, current: u64) -> bool {
    current > last_seen
}

#[cfg(test)]
mod tests {
    use super::event_drops_need_notice;

    #[test]
    fn event_drop_notice_only_when_count_rises() {
        assert!(!event_drops_need_notice(0, 0));
        assert!(event_drops_need_notice(0, 1));
        assert!(event_drops_need_notice(3, 7));
        assert!(!event_drops_need_notice(7, 7));
        // A counter that went backwards (new session) is not a drop.
        assert!(!event_drops_need_notice(7, 0));
    }
}
