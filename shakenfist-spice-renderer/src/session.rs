//! Session orchestrator: per-connection setup and the headless
//! event loop.
//!
//! `run_connection` is what the GUI's reconnect path and the
//! headless mode both spawn on a fresh tokio runtime. It connects
//! every SPICE channel the server advertises, spawns one task per
//! channel, and waits for them to exit (cleanly, on disconnect, or
//! aborted by the per-connection `cancel` flag).
//!
//! `run_headless` is the headless-mode wrapper: it constructs a
//! null repaint `Notify`, calls `run_connection`, and drains the
//! `ChannelEvent` stream just enough to log periodic stats and to
//! shut down on main-channel disconnect or cancel.
//!
//! Both functions are renderer-substrate: they do not depend on
//! `eframe`/`egui` and have no knowledge of host-policy concerns
//! like Ctrl+C, the in-app notification store, or pedantic
//! bug-report assembly. The host (ryll's `main.rs` and `ryll/src/app/`)
//! constructs the trait objects and wraps the orchestrator.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use shakenfist_spice_protocol::constants::capabilities;
use shakenfist_spice_protocol::link::{display_channel_caps, H264Offer};
use shakenfist_spice_protocol::{
    ChannelType, ConnectionConfig, NotifySeverity, SpiceClient, CA_ALONE_WARNING,
};

use crate::byte_counter::ByteCounter;
use crate::capture_sink::CaptureSink;
use crate::channels::display::supported_h264_offer;
#[cfg(feature = "usbredir")]
use crate::channels::UsbredirChannel;
#[cfg(feature = "webdav")]
use crate::channels::WebdavChannel;
use crate::channels::{
    ChannelEvent, CursorChannel, DisplayChannel, EventSink, InputEvent, InputsChannel, MainChannel,
    PlaybackChannel, UsbCommand, VolumeControl, WebdavCommand,
};
use crate::clipboard::ClipboardBackend;
use crate::device_config::{ShareDirConfig, VirtualDiskConfig};
use crate::event_bus::EventBus;
use crate::log_config::LogConfig;
use crate::mm_clock::MmClock;
use crate::notification::{NotificationEntry, NotificationSource};
use crate::notification_sink::NotificationSink;
use crate::playback_sink::PlaybackOutput;
use crate::session_state::SessionState;
use crate::snapshots::ChannelSnapshots;
use crate::surface_mirror::SurfaceMirror;
use crate::traffic::TrafficSink;

/// Channel buffer sizes used by `run_headless`. The GUI side
/// (in `ryll/src/app/`) keeps its own copies because the egui
/// loop sizes them as part of its own setup; these defaults are
/// the headless-mode shape and are exposed for hosts that want
/// to match.
pub const EVENT_CHANNEL_SIZE: usize = 1024;
pub const INPUT_CHANNEL_SIZE: usize = 256;

/// How long the main channel will block queueing an event before giving up
/// and warning. K1 (session-001) was an abandoned-receiver deadlock where
/// main blocked forever on `send().await`; the root cause was fixed by
/// removing the intermediate temp channel, and this deadline is what makes
/// a recurrence show up in the log instead of as a silent hang. Only main
/// carries it — the secondary channels have no equivalent history.
const MAIN_EVENT_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `run_headless` waits for the connection task to wind
/// down once the session loop has stopped for some reason other
/// than the task finishing (Ctrl+C, main-channel disconnect, the
/// event stream closing). Once `run_connection` has spawned its
/// channel tasks it notices the cancel flag within 100 ms; the
/// grace only runs out when a cancel lands mid-setup, where
/// nothing polls the flag, and the task is then aborted.
const CONNECTION_JOIN_GRACE: Duration = Duration::from_secs(2);

/// Capacity of the per-headless broadcast bus that fans
/// `ChannelEvent`s out to multiple subscribers (the headless stats
/// drain, the control-socket per-client tap, and future digest /
/// MCP consumers).  A slow subscriber lags rather than back-
/// pressuring the SPICE channel producers.
pub const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// How often headless logs its `Stats:` line.
const HEADLESS_STATS_PERIOD: Duration = Duration::from_secs(10);

/// Minimal `StatusProvider` implementation for headless mode.
///
/// `spice_connected` is conservative: we report `true` as long as
/// the connection task handle is still alive (it exits only once the
/// main channel disconnects).  `agent_connected` reads the session's
/// `SessionState` watch, so it is the main channel's latest value.
/// `surfaces` is populated from the live `SurfaceMirror` so a
/// `status` reply matches what the `screenshot` verb would observe.
pub struct SessionStatus {
    /// True while the connection task is running.  The control server
    /// reads this via the `StatusProvider` trait.
    connected: Arc<AtomicBool>,
    /// Current vdagent connection state, as the main channel last
    /// published it through `SessionState`.
    agent_connected: watch::Receiver<bool>,
    /// Live pixel-store mirror.  `snapshot()` uses `try_lock` so a
    /// slow apply task never stalls the `status` reply path; on
    /// contention it falls back to an empty surface list.  This is
    /// safe because the apply task holds the lock only for a single
    /// `apply_event` call (well under a millisecond), so contention
    /// is rare in practice.
    surface_mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
}

impl SessionStatus {
    /// Build a status provider for a control socket.
    ///
    /// Nothing here is mode-specific: any mode that can supply a
    /// liveness flag, an agent-connected flag and a surface mirror
    /// can host a control socket. Headless was simply the only one
    /// that did, until web mode grew a socket of its own.
    pub fn new(
        connected: Arc<AtomicBool>,
        agent_connected: watch::Receiver<bool>,
        surface_mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
    ) -> Self {
        Self {
            connected,
            agent_connected,
            surface_mirror,
        }
    }
}

#[cfg(unix)]
impl crate::control::StatusProvider for SessionStatus {
    fn snapshot(&self) -> crate::control::protocol::StatusResult {
        // try_lock: never block the per-client task on a slow apply.
        // The apply task only holds the lock for a single
        // `apply_event` call, so contention is rare; on the rare
        // failure path we degrade gracefully to an empty list and
        // log it for debugging.
        let surfaces = match self.surface_mirror.try_lock() {
            Ok(mirror) => mirror
                .surfaces
                .iter()
                .map(|((channel_id, surface_id), surf)| {
                    let (width, height) = surf.size();
                    crate::control::protocol::SurfaceInfo {
                        channel_id: *channel_id,
                        surface_id: *surface_id,
                        width,
                        height,
                    }
                })
                .collect(),
            Err(_) => {
                tracing::debug!(
                    "headless: status snapshot could not acquire surface_mirror lock; \
                     returning empty surfaces list"
                );
                Vec::new()
            }
        };
        crate::control::protocol::StatusResult {
            spice_connected: self.connected.load(Ordering::Relaxed),
            agent_connected: *self.agent_connected.borrow(),
            surfaces,
        }
    }
}

/// Run the SPICE connection in async context.
///
/// Connects the main channel, waits for `SessionInitialized` and
/// `ChannelsAvailable`, then spawns one task per advertised
/// channel. Returns when every channel task exits — either on
/// graceful disconnect, on error, or aborted by the `cancel`
/// flag (a fresh reconnect superseding this attempt, or a
/// host-side Ctrl+C bridge raising the same flag). If it returns
/// early with an error, or its future is dropped, the channel tasks
/// it has spawned are aborted, so a host may run one attempt after
/// another on the same runtime.
///
/// `session_state` is where the main channel publishes mouse mode and
/// agent state. The caller subscribes to it before calling this, and
/// should create a fresh one for every attempt: its senders are dropped
/// when this session ends.
///
/// `h264` is how each display channel offers H.264: by default it
/// advertises `DISPLAY_CODEC_H264` and asks for H.264 after MJPEG;
/// `--experimental-h264` asks for it first and `--no-h264` offers it
/// not at all. See [`H264Offer`]. A renderer built without the
/// `h264-decode` feature has no H.264 decoder, so it offers H.264 not
/// at all whatever `h264` asks for.
///
/// `playback` is where the guest's audio goes. `PlaybackOutput::Local`
/// plays it on this host's default output device, which needs the
/// `audio` feature: without it the playback channel is not linked.
/// `PlaybackOutput::Sink` hands it to the sink, opens no device, and
/// links the playback channel in every build.
///
/// The parameter list is the same in every feature configuration. A
/// channel whose feature is compiled out is not linked, and the
/// arguments only it would use (the usbredir receiver and virtual
/// disks, the webdav receiver and share) are accepted and ignored, as
/// is the volume control in a build without local audio output.
#[allow(clippy::too_many_arguments)]
pub async fn run_connection(
    config: ConnectionConfig,
    event_tx: mpsc::Sender<ChannelEvent>,
    session_state: SessionState,
    repaint_notify: Arc<Notify>,
    input_rx: mpsc::Receiver<InputEvent>,
    usb_rx: mpsc::Receiver<UsbCommand>,
    webdav_rx: mpsc::Receiver<WebdavCommand>,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<dyn CaptureSink>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    snapshots: ChannelSnapshots,
    monitors: u8,
    resize_rx: mpsc::Receiver<crate::channels::DisplaySize>,
    volume_control: Arc<VolumeControl>,
    enable_paste: bool,
    log_config: LogConfig,
    cancel: Arc<AtomicBool>,
    clipboard: Option<Arc<dyn ClipboardBackend>>,
    playback: PlaybackOutput,
    h264: H264Offer,
    image_cache_cap_bytes: usize,
    glz_dictionary_cap_bytes: usize,
    preferred_compression: u8,
) -> Result<()> {
    // Keep the function's public signature stable across feature
    // configurations so host callers do not have to know which
    // features the renderer was built with. When the usbredir or
    // webdav channel is compiled out, the arguments only it reads go
    // unused; suppress the resulting lints here.
    #[cfg(not(feature = "usbredir"))]
    let _ = (&usb_rx, &virtual_disks);
    #[cfg(not(feature = "webdav"))]
    let _ = (&webdav_rx, &share_dir);

    // A build without an H.264 decoder must not let the server pick
    // H.264, or every stream it sends would be dropped. This has to
    // happen before the display channel's link caps are built below.
    let requested_h264 = h264;
    let h264 = supported_h264_offer(requested_h264);
    if h264 != requested_h264 {
        info!(
            "H.264 decode not compiled in (h264-decode feature disabled): \
             not offering H.264 to the server"
        );
    }

    let client = SpiceClient::new(config)?;

    // Shared mm_time clock. The main channel writes to it from
    // `MAIN_INIT` and `MULTI_MEDIA_TIME`; the display channel reads
    // from it when building `STREAM_REPORT` payloads. Constructed
    // here so both channels share the same `Arc<MmClock>`; not
    // exposed through `run_connection`'s public signature because it
    // is purely internal plumbing — host callers never see it.
    let mm_clock = Arc::new(MmClock::new());

    // Connect main channel and run it. The main channel sends
    // ChannelEvents directly into the caller-provided `event_tx`
    // (no intermediate bounded buffer); session id and channels
    // list arrive out-of-band via oneshot channels. The earlier
    // intermediate `mpsc::channel(64)` was the root cause of K1:
    // it was drained only until session+channels were known, then
    // abandoned while main kept sending PingInterval events into it.
    // After ~65 pings (~7m45s of idle time) the buffer filled and
    // main blocked forever on send().await.
    let (session_init_tx, session_init_rx) = oneshot::channel();
    let (channels_avail_tx, channels_avail_rx) = oneshot::channel();

    let main_stream = client.connect_channel(0, ChannelType::Main, 0).await?;

    let events = EventSink::new(event_tx, repaint_notify);

    // The main channel's handshake is the session's first, so by now
    // the verifier knows whether the CA was the only identity check
    // (ryll#359). It has logged that once; show it once as well, since
    // a log line at any level is easy to miss.
    if client.trusted_by_ca_alone() {
        events
            .emit(ChannelEvent::Notification(NotificationEntry::new(
                NotifySeverity::Warn,
                NotificationSource::Connection,
                format!("TLS: {CA_ALONE_WARNING}"),
            )))
            .await;
    }

    let mut main_channel = MainChannel::new(
        main_stream,
        events.clone().with_send_timeout(MAIN_EVENT_SEND_TIMEOUT),
        session_state,
        capture.clone(),
        byte_counter.clone(),
        traffic.clone(),
        snapshots.main,
        resize_rx,
        monitors,
        log_config,
        clipboard,
        session_init_tx,
        channels_avail_tx,
        mm_clock.clone(),
    );

    // Spawn main channel task. From here on every channel task is in
    // `handles`, so an early return below aborts the ones already
    // running.
    let mut handles = ChannelTasks::default();
    handles.push((
        ChannelType::Main,
        tokio::spawn(async move { main_channel.run().await }),
    ));

    let session_id = session_init_rx
        .await
        .map_err(|_| anyhow::anyhow!("main channel dropped before SessionInitialized"))?;
    let channels = channels_avail_rx
        .await
        .map_err(|_| anyhow::anyhow!("main channel dropped before ChannelsAvailable"))?;

    info!(
        "Session {} ready with {} channels",
        session_id,
        channels.len()
    );

    // Connect other channels
    let mut input_rx = Some(input_rx);
    #[cfg(feature = "usbredir")]
    let mut usb_rx = Some(usb_rx);
    #[cfg(feature = "webdav")]
    let mut webdav_rx = Some(webdav_rx);
    let shared_glz_dictionary = DisplayChannel::new_shared_glz_dictionary(glz_dictionary_cap_bytes);

    let main_only = std::env::var("RYLL_K1_MAIN_ONLY").is_ok();
    if main_only {
        info!("RYLL_K1_MAIN_ONLY set — skipping all secondary channels (main only)");
    }

    for (channel_type, channel_id, link) in plan_channel_links(&channels, &playback) {
        if main_only {
            continue;
        }
        if let ChannelLink::Skip(reason) = link {
            info!(
                "Skipping {} channel (id={}): {}",
                channel_type.name(),
                channel_id,
                reason
            );
            continue;
        }
        match channel_type {
            ChannelType::Display => {
                // Linked with the caps for this session rather than the
                // defaults: `--no-h264` drops CODEC_H264.
                let (stream, _reply) = client
                    .connect_channel_with_caps(
                        session_id,
                        channel_type,
                        channel_id,
                        &[capabilities::DEFAULT_COMMON],
                        &[display_channel_caps(h264)],
                    )
                    .await?;
                let mut channel = DisplayChannel::new(
                    channel_id,
                    stream,
                    events.clone(),
                    capture.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.display.clone(),
                    shared_glz_dictionary.clone(),
                    log_config,
                    mm_clock.clone(),
                    h264,
                    image_cache_cap_bytes,
                    preferred_compression,
                );
                handles.push((
                    ChannelType::Display,
                    tokio::spawn(async move { channel.run().await }),
                ));
            }

            ChannelType::Cursor => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = CursorChannel::new(
                    stream,
                    events.clone(),
                    capture.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.cursor.clone(),
                    log_config,
                );
                handles.push((
                    ChannelType::Cursor,
                    tokio::spawn(async move { channel.run().await }),
                ));
            }

            // The host hands over one receiver each for inputs, usbredir
            // and webdav, so only one channel of each kind can be served.
            // plan_channel_links() already skips the later ones; the
            // `take()`s here are what moves each receiver into its channel.
            ChannelType::Inputs => {
                if let Some(input_rx) = input_rx.take() {
                    let stream = client
                        .connect_channel(session_id, channel_type, channel_id)
                        .await?;
                    let mut channel = InputsChannel::new(
                        stream,
                        events.clone(),
                        input_rx,
                        capture.clone(),
                        byte_counter.clone(),
                        traffic.clone(),
                        snapshots.inputs.clone(),
                        enable_paste,
                        log_config,
                    );
                    handles.push((
                        ChannelType::Inputs,
                        tokio::spawn(async move { channel.run().await }),
                    ));
                }
            }

            #[cfg(feature = "usbredir")]
            ChannelType::Usbredir => {
                if let Some(usb_rx) = usb_rx.take() {
                    let stream = client
                        .connect_channel(session_id, channel_type, channel_id)
                        .await?;
                    let mut channel = UsbredirChannel::new(
                        stream,
                        events.clone(),
                        usb_rx,
                        virtual_disks.clone(),
                        capture.clone(),
                        byte_counter.clone(),
                        traffic.clone(),
                        snapshots.usbredir.clone(),
                        log_config,
                    );
                    handles.push((
                        ChannelType::Usbredir,
                        tokio::spawn(async move { channel.run().await }),
                    ));
                }
            }

            #[cfg(feature = "webdav")]
            ChannelType::Webdav => {
                if let Some(webdav_rx) = webdav_rx.take() {
                    let stream = client
                        .connect_channel(session_id, channel_type, channel_id)
                        .await?;
                    let mut channel = WebdavChannel::new(
                        stream,
                        events.clone(),
                        webdav_rx,
                        share_dir.clone(),
                        capture.clone(),
                        byte_counter.clone(),
                        traffic.clone(),
                        snapshots.webdav.clone(),
                        log_config,
                    );
                    handles.push((
                        ChannelType::Webdav,
                        tokio::spawn(async move { channel.run().await }),
                    ));
                }
            }

            ChannelType::Playback => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = PlaybackChannel::new(
                    stream,
                    events.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.playback.clone(),
                    volume_control.clone(),
                    log_config,
                    cancel.clone(),
                    playback.clone(),
                );
                handles.push((
                    ChannelType::Playback,
                    tokio::spawn(async move { channel.run().await }),
                ));
            }
            // plan_channel_links() skips every other type, usbredir and
            // webdav too when their features are off, and local playback
            // without the audio feature.
            _ => {}
        }
    }

    // Cancel watcher: when the host raises the per-connection
    // cancel flag (a fresh GUI Reconnect superseding this attempt,
    // or a Ctrl+C bridge in headless mode), abort every channel
    // task so the wait loop below returns promptly. The watcher
    // polls at 100 ms; that latency is well inside human reaction
    // time and avoids adding any awaitable signalling primitive.
    let cancel_watcher = {
        let abort_handles: Vec<_> = handles
            .tasks
            .iter()
            .map(|(_, h)| h.abort_handle())
            .collect();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if cancel.load(Ordering::Relaxed) {
                    info!(
                        "session: connection cancelled, aborting {} channel tasks",
                        abort_handles.len()
                    );
                    for ah in &abort_handles {
                        ah.abort();
                    }
                    break;
                }
            }
        })
    };
    handles.helpers.push(cancel_watcher.abort_handle());

    // Wait for all channel tasks
    for (channel_type, handle) in handles.tasks.iter_mut() {
        let channel_type = *channel_type;
        match handle.await {
            Err(e) if e.is_cancelled() => {
                // Aborted by the cancel watcher; not an error.
            }
            Err(e) => {
                error!("Channel task panic on {}: {}", channel_type.name(), e);
            }
            Ok(Err(e)) => {
                let message = format!("channel error: {}", e);
                error!("session: {}: {}", channel_type.name(), message);
                events
                    .emit(ChannelEvent::Error {
                        channel: channel_type,
                        message,
                    })
                    .await;
            }
            Ok(Ok(())) => {}
        }
    }

    // Stop the watcher if the connection ended for any other
    // reason (main channel disconnect, secondary channel error).
    // Safe to call even if it already exited via the cancel path.
    cancel_watcher.abort();

    Ok(())
}

/// A session's channel tasks, aborted if this is dropped before they
/// finish.
///
/// `run_connection` returns early when a secondary channel will not
/// link, and a host can drop its future mid-session, for instance when
/// it stops waiting on Ctrl+C. Either way no channel task is left
/// running on the host's runtime, holding the session's sockets and
/// event sender. Aborting a task that has finished does nothing, so
/// dropping this once the tasks have all been awaited is a no-op.
#[derive(Default)]
struct ChannelTasks {
    tasks: Vec<ChannelTask>,
    /// Tasks that serve the channel tasks and would outlive them
    /// otherwise: the cancel watcher, which polls until it fires.
    helpers: Vec<tokio::task::AbortHandle>,
}

/// A channel task, and the channel it runs.
type ChannelTask = (
    ChannelType,
    tokio::task::JoinHandle<Result<(), anyhow::Error>>,
);

impl ChannelTasks {
    fn push(&mut self, task: ChannelTask) {
        self.tasks.push(task);
    }
}

impl Drop for ChannelTasks {
    fn drop(&mut self) {
        for (_, task) in &self.tasks {
            task.abort();
        }
        for helper in &self.helpers {
            helper.abort();
        }
    }
}

/// Spawn a control-socket server for a session.
///
/// Shared by every mode that offers `--control-socket`, so a change
/// of shape lands once rather than in each mode separately. Web mode
/// hosts one of these too; it was headless-only until a browser
/// session found four input bugs that a control-socket scenario test
/// would have caught, and could not have run.
///
/// Takes a server that is already bound (see
/// [`crate::control::Server::bind`]), so that a socket that cannot be
/// created stops the caller at startup rather than being reported by
/// this task after the session has started.
///
/// Takes a [`CancellationToken`] rather than being aborted, so the
/// server can stop accepting and wind its clients down cleanly.
///
/// `agent_connected` is the session's `SessionStateRx::agent_connected`,
/// which the `agent_connected` event is translated from. It is not on the
/// event bus because a client lagging the bus could miss a transition.
///
/// `handles` are what the verbs act on. Its clipboard must be the one
/// given to `run_connection`, and its resize sender the one whose
/// receiver was, or `clipboard_*` and `resize` reach nothing.
///
/// Unix-only: the server uses `tokio::net::UnixListener`, which has
/// no Windows equivalent in this shape.
#[cfg(unix)]
pub fn spawn_control_socket(
    server: crate::control::Server,
    status: Arc<dyn crate::control::StatusProvider>,
    event_tx: broadcast::Sender<ChannelEvent>,
    agent_connected: watch::Receiver<bool>,
    handles: crate::control::SessionHandles,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = server
            .run(status, event_tx, agent_connected, handles, cancel)
            .await
        {
            warn!("control: server exited with error: {}", e);
        }
    })
}

/// Spawn the visual-digest poller.
///
/// Watches the primary surface for a QR-encoded visual digest and
/// broadcasts a `ChannelEvent::DigestUpdated` when the frame counter
/// changes. The control server's event translator turns that into a
/// `digest_updated` wire event. See [`crate::digest`].
///
/// Shared between `run_headless` and `ryll`'s `run_web` rather than
/// living inside the former, because the two are the modes that can
/// host a control socket and the scenario tests that consume
/// `digest_updated` have to be able to drive either. Web mode got a
/// socket without this and the failure was silent: `subscribe` on
/// `digest_updated` succeeded and no event ever arrived.
///
/// Spawn this only where something will consume the events. The
/// poller decodes QR out of the framebuffer on a timer whether or not
/// anyone is listening, and web mode's ordinary path — a browser, no
/// socket — should not pay for it.
#[cfg(feature = "digest-decode")]
///
/// Takes the session's `Arc<AtomicBool>` cancel flag rather than the
/// `CancellationToken` the control socket uses. The two coexist: the
/// token stops the socket server, the flag stops everything on the
/// renderer side, and both are raised on the same shutdown path.
pub fn spawn_digest_poller(
    surface_mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
    event_tx: broadcast::Sender<ChannelEvent>,
    cancel: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        crate::digest::run_digest_poller(surface_mirror, event_tx, cancel).await;
    })
}

/// Run a headless SPICE session.
///
/// Constructs the connection-side channel pairs internally, then
/// spawns `run_connection` and drains `ChannelEvent`s until the
/// connection task ends (main-channel disconnect or `cancel`
/// flipped by the host's Ctrl+C bridge).
///
/// The host owns:
/// - `byte_counter`, `traffic`, `snapshots`, `notifications`
///   (so the host can read them mid-session for any external
///   purpose — pedantic bug-report assembly, GUI panels, etc.)
/// - The `cancel` flag and any bridge to host-level signal state
///   (e.g. the host's process-global Ctrl+C flag flipped by its
///   `ctrlc::set_handler`).
///
/// However the event loop stops, the connection task is joined
/// before this returns, and its outcome decides the result: an
/// error from `run_connection` (a refused dial, a proxy's 401, a
/// TLS or `host_subject` rejection, a failed link) is logged as
/// `Connection task failed: ...` and returned as an `Err` carrying
/// it, so a headless process exits non-zero on a failed connect.
/// A session that ends because the host raised `cancel` or the
/// main channel disconnected returns `Ok` unless the connection
/// task itself failed, or `--paste-text` did.
///
/// A `control_socket_path` that cannot be bound is an `Err` before
/// anything connects.
#[allow(clippy::too_many_arguments)]
pub async fn run_headless(
    config: ConnectionConfig,
    cadence: bool,
    paste_text: Option<String>,
    paste_char_delay_ms: u32,
    enable_paste: bool,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<dyn CaptureSink>>,
    monitors: u8,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    snapshots: ChannelSnapshots,
    notifications: Arc<dyn NotificationSink>,
    log_config: LogConfig,
    cancel: Arc<AtomicBool>,
    h264: H264Offer,
    image_cache_cap_bytes: usize,
    glz_dictionary_cap_bytes: usize,
    preferred_compression: u8,
    control_socket_path: Option<PathBuf>,
) -> Result<()> {
    info!("Running in headless mode");

    // Bind the control socket before anything else, so a socket that
    // cannot be created fails the run before it connects. A caller
    // that asked for one and drives the session through it would
    // otherwise wait for a socket that never appears.
    #[cfg(unix)]
    let control_server = control_socket_path
        .map(|path| {
            crate::control::Server::bind(path.clone())
                .with_context(|| format!("cannot create control socket {}", path.display()))
        })
        .transpose()?;

    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);
    let (_usb_tx, usb_rx) = mpsc::channel(16);
    let (_webdav_tx, webdav_rx) = mpsc::channel(16);
    // Driven by the control socket's `resize` verb. Held for the whole
    // session even with no socket to drive it, as the GUI and web mode
    // hold theirs.
    #[cfg_attr(not(unix), allow(unused_variables))]
    let (resize_tx, resize_rx) = mpsc::channel(32);
    // The display and playback channels' counters, for the control
    // socket's `stats` verb; `snapshots` itself moves into the
    // connection task.
    #[cfg_attr(not(unix), allow(unused_variables))]
    let display_snapshot = snapshots.display.clone();
    #[cfg_attr(not(unix), allow(unused_variables))]
    let playback_snapshot = snapshots.playback.clone();
    // For the periodic `Stats:` line.
    let stats_snapshots = snapshots.clone();

    // The clipboard the main channel syncs with the guest, which the
    // control socket's `clipboard_*` verbs read and write. Made only
    // with a socket: without one nothing could read what the guest
    // copied, and the main channel would fetch every copy for nobody.
    #[cfg(unix)]
    let control_server =
        control_server.map(|server| (server, Arc::new(crate::clipboard::MemoryClipboard::new())));
    #[cfg(unix)]
    let clipboard_for_conn = control_server
        .as_ref()
        .map(|(_, clipboard)| clipboard.clone() as Arc<dyn ClipboardBackend>);
    #[cfg(not(unix))]
    let clipboard_for_conn: Option<Arc<dyn ClipboardBackend>> = None;

    // Broadcast bus that the headless event loop and the control
    // socket both subscribe to.  The fan-out task spawned below
    // drains the renderer's mpsc and republishes each event onto
    // this bus, so SPICE channel producers are never back-pressured
    // by any single consumer (e.g. a slow control-socket client).
    let event_bus = EventBus::new(EVENT_BROADCAST_CAPACITY);
    let event_broadcast_tx = event_bus.sender();

    // Headless mode does not paint anything, but the channel handlers still
    // call notify_one().  Give them a Notify whose notifications nobody
    // listens for; tokio::sync::Notify::notify_one is cheap (no allocation,
    // no waker if no waiters) so this is harmless.
    let repaint_notify = Arc::new(Notify::new());

    // Track whether the SPICE connection task is still alive. The
    // control server's `SessionStatus` impl reads this flag to answer
    // `status` queries.
    let spice_connected = Arc::new(AtomicBool::new(true));

    // Mouse mode and vdagent state, published by the main channel.
    // Subscribed here, before the session exists, so nothing the main
    // channel publishes can be missed. `status` requests and the
    // control socket's `agent_connected` event read the agent half.
    let session_state = SessionState::new();
    let session_state_rx = session_state.subscribe();

    // Live pixel store the event bus keeps up to date, for the control
    // socket's `screenshot` verb and `status` surface list and for the
    // digest poller.  Both are Unix- or feature-only, so the handle is
    // too; the bus applies events to the mirror regardless.
    #[cfg(any(unix, feature = "digest-decode"))]
    let surface_mirror = event_bus.mirror();

    // Watch the primary surface for a QR-encoded visual digest.  See
    // `spawn_digest_poller`, which `run_web` calls too — the scenario
    // tests that consume `digest_updated` have to be able to drive
    // either mode.
    #[cfg(feature = "digest-decode")]
    let _digest_handle = spawn_digest_poller(
        surface_mirror.clone(),
        event_broadcast_tx.clone(),
        cancel.clone(),
    );

    // Spawn connection task. It gets a cancel flag of its own rather
    // than the host's: whenever the event loop below stops without
    // the task having finished -- the host's `cancel`, a main-channel
    // disconnect, the event stream closing -- this flag is raised so
    // the task can be wound down and joined before this returns.
    let conn_cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_conn = conn_cancel.clone();
    let spice_connected_for_conn = spice_connected.clone();
    let mut connection_handle = tokio::spawn(async move {
        let result = run_connection(
            config,
            event_tx,
            session_state,
            repaint_notify,
            input_rx,
            usb_rx,
            webdav_rx,
            virtual_disks,
            share_dir,
            capture,
            byte_counter,
            traffic,
            snapshots,
            monitors,
            resize_rx,
            VolumeControl::new(),
            enable_paste,
            log_config,
            cancel_for_conn,
            clipboard_for_conn,
            // Headless plays the guest's audio on this host, as it always
            // has.
            PlaybackOutput::Local,
            h264,
            image_cache_cap_bytes,
            glz_dictionary_cap_bytes,
            preferred_compression,
        )
        .await;
        spice_connected_for_conn.store(false, Ordering::Relaxed);
        result
    });

    // Clone input_tx for each consumer before any `async move` closure
    // captures ownership.  Order matters: clones must precede moves.
    let paste_input_tx = input_tx.clone();
    let cadence_input_tx = input_tx.clone();
    #[cfg(unix)]
    let input_tx_for_control = input_tx.clone();

    // Cadence task if enabled
    let cadence_handle = if cadence {
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = cadence_input_tx.try_send(InputEvent::KeyDown(0x39));
                let _ = cadence_input_tx.try_send(InputEvent::KeyUp(0xB9));
            }
        }))
    } else {
        None
    };

    // Paste trigger task if --paste-text was provided
    let paste_handle = if let Some(text) = paste_text {
        let delay_ms = paste_char_delay_ms;
        Some(tokio::spawn(async move {
            // Wait for the inputs channel to be ready.
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = paste_input_tx
                .send(InputEvent::PasteText {
                    text,
                    char_delay_ms: delay_ms,
                    request_id: None, // CLI path: no correlation token
                    cancel: None,     // CLI path: no cancellation token
                })
                .await;
        }))
    } else {
        None
    };

    // Control socket task — spawned when --control-socket is set, on
    // the server bound at the top of this function. A
    // `CancellationToken` is used so the server can exit cleanly when
    // the SPICE session ends.
    //
    // The control socket is Unix-only: it uses tokio::net::UnixListener
    // which has no Windows equivalent in the same shape.  On non-Unix
    // platforms the path is logged-and-ignored.
    #[cfg(unix)]
    let control_cancel = CancellationToken::new();
    #[cfg(unix)]
    let control_handle = control_server.map(|(server, clipboard)| {
        let status: Arc<dyn crate::control::StatusProvider> = Arc::new(SessionStatus::new(
            spice_connected.clone(),
            session_state_rx.agent_connected.clone(),
            surface_mirror.clone(),
        ));
        let handles = crate::control::SessionHandles {
            input_tx: input_tx_for_control,
            surface_mirror: surface_mirror.clone(),
            clipboard,
            resize_tx: resize_tx.clone(),
            display_snapshot: display_snapshot.clone(),
            playback_snapshot: playback_snapshot.clone(),
        };
        spawn_control_socket(
            server,
            status,
            event_broadcast_tx.clone(),
            session_state_rx.agent_connected.clone(),
            handles,
            control_cancel.clone(),
        )
    });
    #[cfg(not(unix))]
    {
        if control_socket_path.is_some() {
            warn!("control: --control-socket is Unix-only; ignoring on this platform");
        }
    }

    // Event fan-out and surface-mirror apply tasks.  The fan-out is the
    // architectural pivot that lets multiple consumers (the headless
    // stats drain below, control-socket clients, future digest
    // consumers) tap the same stream without back-pressuring the
    // SPICE channel producers; see `EventBus`.  Subscribe the stats
    // drain before starting it so it cannot miss an early event.
    let stats_event_rx = event_bus.subscribe();
    let _event_bus_handles = event_bus.start(event_rx, "headless");

    // Drop our extra reference to the broadcast sender so that, once
    // the fan-out task exits (on the event mpsc closing), the remaining
    // receivers see `RecvError::Closed` and tear themselves down.
    drop(event_broadcast_tx);

    // Process events.  The headless drain subscribes to the
    // broadcast bus rather than the original mpsc — exactly the
    // same `ChannelEvent` stream, fan-out architecture.
    let mut event_rx = stats_event_rx;
    // Image draws seen on the event bus, for the `Stats:` line.
    let mut images_received: u64 = 0;
    let mut stats_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + HEADLESS_STATS_PERIOD,
        HEADLESS_STATS_PERIOD,
    );
    let mut paste_failed = false;
    // Set when the loop stops because the connection task finished.
    // Every other way out leaves it `None`, and the task is joined
    // after the loop instead.
    let mut connection_join: Option<ConnectionJoin> = None;
    // Logs each vdagent state the main channel publishes. Cleared once
    // the session's senders are gone, so a finished session does not
    // spin this branch on a `changed()` that fails at once.
    let mut agent_connected_rx = session_state_rx.agent_connected;
    let mut agent_state_open = true;

    loop {
        tokio::select! {
            changed = agent_connected_rx.changed(), if agent_state_open => {
                match changed {
                    Ok(()) => {
                        let connected = *agent_connected_rx.borrow_and_update();
                        info!("headless: vdagent connected={}", connected);
                    }
                    Err(_) => agent_state_open = false,
                }
            }
            event_result = event_rx.recv() => {
                let event = match event_result {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The headless stats drain only updates
                        // counters and emits info-log lines, so
                        // briefly missing N events here costs us
                        // some statistical accuracy but never
                        // session correctness.  Log and continue.
                        warn!(
                            "headless: event drain lagged by {} events; stats may underreport",
                            n
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("headless: event stream closed");
                        break;
                    }
                };
                match event {
                    ChannelEvent::SessionInitialized(id) => {
                        info!("Session {} initialized", id);
                    }
                    ChannelEvent::SurfaceCreated { .. } => {
                    }
                    ChannelEvent::ImageReady { .. } => {
                        images_received += 1;
                    }
                    ChannelEvent::Disconnected(ChannelType::Main, _) => {
                        info!("Main channel disconnected, exiting");
                        break;
                    }
                    ChannelEvent::UsbDeviceConnected(desc) => {
                        info!("headless: USB device connected: {}", desc);
                    }
                    ChannelEvent::UsbDeviceDisconnected => {
                        info!("headless: USB device disconnected");
                    }
                    ChannelEvent::UsbConnectFailed(err) => {
                        error!("headless: USB connect failed: {}", err);
                    }
                    ChannelEvent::WebdavChannelReady => {
                        info!("headless: WebDAV channel connected");
                    }
                    ChannelEvent::WebdavSharingStarted { path, read_only } => {
                        info!("headless: WebDAV sharing: {} (ro={})", path, read_only);
                    }
                    ChannelEvent::WebdavSharingStopped => {
                        info!("headless: WebDAV sharing stopped");
                    }
                    ChannelEvent::WebdavError(err) => {
                        error!("headless: WebDAV error: {}", err);
                    }
                    ChannelEvent::PasteCompleted { chars, elapsed_ms, .. } => {
                        info!(
                            "headless: paste complete: {} chars in {}ms",
                            chars, elapsed_ms
                        );
                    }
                    ChannelEvent::PasteFailed { reason, .. } => {
                        error!("headless: paste failed: {}", reason);
                        paste_failed = true;
                    }
                    ChannelEvent::Error { channel, message } => {
                        error!("Error on {}: {}", channel.name(), message);
                    }
                    ChannelEvent::Notification(entry) => {
                        notifications.push(entry);
                    }
                    _ => {}
                }
            }
            // Logged on a timer, not when an event arrives, so an idle
            // session still reports. Marks are the GUI's frame measure;
            // the bytes are every channel's, as each last published them.
            _ = stats_tick.tick() => {
                let marks = stats_snapshots
                    .display
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .messages_recv_by_opcode
                    .get(&shakenfist_spice_protocol::display_server::MARK)
                    .copied()
                    .unwrap_or(0);
                let (bytes_in, bytes_out) = stats_snapshots.bytes_totals();
                info!(
                    "Stats: marks={}, images={}, bytes_in={}, bytes_out={}",
                    marks, images_received, bytes_in, bytes_out
                );
            }
            join_result = &mut connection_handle => {
                connection_join = Some(join_result);
                break;
            }
            // Poll the host's cancel flag at a reasonable interval. The host
            // bridges its process-global Ctrl+C signal into this flag.
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancel.load(Ordering::Relaxed) {
                    info!("session: cancel requested");
                    break;
                }
            }
        }
    }

    // Collect the connection task's outcome however the loop ended.
    // "Event stream closed" and "connection task finished" become
    // ready together when a connect fails -- `run_connection`
    // returning drops the last event sender -- and `select!` picks
    // between ready branches at random, so the join cannot be left
    // to the loop, or an error that loses that race goes unlogged and
    // unreported.
    let connection_join = match connection_join {
        Some(join_result) => Some(join_result),
        None => {
            conn_cancel.store(true, Ordering::Relaxed);
            match tokio::time::timeout(CONNECTION_JOIN_GRACE, &mut connection_handle).await {
                Ok(join_result) => Some(join_result),
                Err(_) => {
                    warn!(
                        "session: connection task did not stop within {:?} of the session ending; aborting it",
                        CONNECTION_JOIN_GRACE
                    );
                    connection_handle.abort();
                    None
                }
            }
        }
    };
    let connection_result = connection_outcome(connection_join);

    if let Some(handle) = cadence_handle {
        handle.abort();
    }
    if let Some(handle) = paste_handle {
        handle.abort();
    }

    // Signal the control server to shut down and wait up to 2 s for
    // it to unlink the socket file cleanly.
    #[cfg(unix)]
    if let Some(handle) = control_handle {
        control_cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    connection_result?;
    if paste_failed {
        anyhow::bail!("paste-as-keystrokes failed");
    }

    info!("Headless mode finished");
    Ok(())
}

/// What `run_connection` does with one entry of the server's channel
/// list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelLink {
    /// Link the channel and run its handler.
    Open,
    /// Leave the channel unlinked, for the given reason (logged).
    Skip(&'static str),
}

/// Decide, for every entry of the server's CHANNELS_LIST, whether
/// `run_connection` links it.
///
/// The decision depends only on the list, never on where in it an entry
/// sits: a server may list its channels in any order (andris lists
/// inputs before cursor), and an entry the client can't serve is
/// skipped without stopping the walk, so every channel after it is
/// still considered. Inputs, usbredir and webdav are each served by the
/// one receiver the host hands in, so the first of each kind is linked
/// and any later one is skipped. The main channel is already linked by
/// the time the list arrives. A channel whose handler is compiled out
/// (usbredir without `usbredir`, webdav without `webdav`) is skipped
/// with a reason saying so, as is playback to a `Local` output in a
/// build without `audio`. Playback to a `Sink` is linked in every
/// build.
fn plan_channel_links(
    channels: &[(ChannelType, u8)],
    playback: &PlaybackOutput,
) -> Vec<(ChannelType, u8, ChannelLink)> {
    let mut seen_single = Vec::new();
    channels
        .iter()
        .map(|&(channel_type, channel_id)| {
            let link = match channel_type {
                ChannelType::Display | ChannelType::Cursor => ChannelLink::Open,
                ChannelType::Playback => match playback {
                    PlaybackOutput::Sink(_) => ChannelLink::Open,
                    PlaybackOutput::Local if cfg!(feature = "audio") => ChannelLink::Open,
                    PlaybackOutput::Local => {
                        ChannelLink::Skip("audio feature disabled at compile time")
                    }
                },
                ChannelType::Usbredir if !cfg!(feature = "usbredir") => {
                    ChannelLink::Skip("usbredir feature disabled at compile time")
                }
                ChannelType::Webdav if !cfg!(feature = "webdav") => {
                    ChannelLink::Skip("webdav feature disabled at compile time")
                }
                ChannelType::Inputs | ChannelType::Usbredir | ChannelType::Webdav => {
                    if seen_single.contains(&channel_type) {
                        ChannelLink::Skip("only one supported")
                    } else {
                        seen_single.push(channel_type);
                        ChannelLink::Open
                    }
                }
                ChannelType::Main => ChannelLink::Skip("already linked"),
                ChannelType::Record
                | ChannelType::Tunnel
                | ChannelType::Smartcard
                | ChannelType::Port => ChannelLink::Skip("not supported"),
            };
            (channel_type, channel_id, link)
        })
        .collect()
}

/// What joining `run_headless`'s connection task produced.
type ConnectionJoin = std::result::Result<Result<()>, tokio::task::JoinError>;

/// Log the connection task's outcome and turn it into
/// `run_headless`'s result.
///
/// `None` means the task was aborted after outliving
/// [`CONNECTION_JOIN_GRACE`], which only happens once the session
/// has already been told to stop, so it is not a failure.
///
/// The `Connection task failed: {:#}` line is matched verbatim by
/// `tools/proxmox-smoke.py`; change the two together. The returned
/// error wraps rather than repeats that text, so a host that prints
/// it (ryll's `main` does, on stderr) does not produce a second
/// line containing it.
fn connection_outcome(join: Option<ConnectionJoin>) -> Result<()> {
    match join {
        // Already warned about where the task was aborted.
        None => Ok(()),
        Some(Ok(Ok(()))) => {
            info!("Connection task completed");
            Ok(())
        }
        Some(Ok(Err(e))) => {
            error!("Connection task failed: {:#}", e);
            Err(e.context("headless SPICE connection failed"))
        }
        Some(Err(e)) => {
            error!("Connection task panicked: {}", e);
            Err(anyhow::anyhow!(
                "headless SPICE connection task panicked: {}",
                e
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(plan: &[(ChannelType, u8, ChannelLink)]) -> Vec<(ChannelType, u8)> {
        plan.iter()
            .filter(|(_, _, link)| *link == ChannelLink::Open)
            .map(|&(channel_type, channel_id, _)| (channel_type, channel_id))
            .collect()
    }

    /// Whether this build has a handler for `channel_type`, for the
    /// feature-gated channel types these tests list.
    fn compiled_in(channel_type: ChannelType) -> bool {
        let features = [
            (ChannelType::Usbredir, cfg!(feature = "usbredir")),
            (ChannelType::Webdav, cfg!(feature = "webdav")),
        ];
        features
            .iter()
            .all(|&(gated, enabled)| gated != channel_type || enabled)
    }

    /// `channels` without the entries this build cannot link.
    fn linkable(channels: &[(ChannelType, u8)]) -> Vec<(ChannelType, u8)> {
        channels
            .iter()
            .copied()
            .filter(|&(channel_type, _)| compiled_in(channel_type))
            .collect()
    }

    #[test]
    fn channels_listed_after_inputs_are_still_linked() {
        // andris's order: inputs before cursor (shakenfist/ryll#509).
        let list = [
            (ChannelType::Display, 0),
            (ChannelType::Inputs, 0),
            (ChannelType::Cursor, 0),
            (ChannelType::Usbredir, 0),
            (ChannelType::Webdav, 0),
        ];
        let plan = plan_channel_links(&list, &PlaybackOutput::Local);
        assert_eq!(opened(&plan), linkable(&list));
    }

    #[test]
    fn a_second_single_receiver_channel_is_skipped_and_the_walk_goes_on() {
        let list = [
            (ChannelType::Inputs, 0),
            (ChannelType::Inputs, 1),
            (ChannelType::Usbredir, 0),
            (ChannelType::Usbredir, 1),
            (ChannelType::Webdav, 0),
            (ChannelType::Webdav, 1),
            (ChannelType::Cursor, 0),
            (ChannelType::Display, 0),
        ];
        let plan = plan_channel_links(&list, &PlaybackOutput::Local);
        assert_eq!(
            opened(&plan),
            linkable(&[
                (ChannelType::Inputs, 0),
                (ChannelType::Usbredir, 0),
                (ChannelType::Webdav, 0),
                (ChannelType::Cursor, 0),
                (ChannelType::Display, 0),
            ])
        );
        for duplicate in linkable(&[
            (ChannelType::Inputs, 1),
            (ChannelType::Usbredir, 1),
            (ChannelType::Webdav, 1),
        ]) {
            assert!(
                plan.contains(&(
                    duplicate.0,
                    duplicate.1,
                    ChannelLink::Skip("only one supported")
                )),
                "{:?} should be skipped as a duplicate",
                duplicate
            );
        }
    }

    #[test]
    fn every_display_and_cursor_channel_is_linked() {
        // A multi-head guest lists one display (and one cursor) per head.
        let list = [
            (ChannelType::Display, 0),
            (ChannelType::Display, 1),
            (ChannelType::Cursor, 0),
            (ChannelType::Cursor, 1),
        ];
        assert_eq!(
            opened(&plan_channel_links(&list, &PlaybackOutput::Local)),
            list.to_vec()
        );
    }

    #[test]
    fn unsupported_channels_are_skipped_without_stopping_the_walk() {
        let list = [
            (ChannelType::Main, 0),
            (ChannelType::Record, 0),
            (ChannelType::Smartcard, 0),
            (ChannelType::Port, 0),
            (ChannelType::Tunnel, 0),
            (ChannelType::Cursor, 0),
        ];
        let plan = plan_channel_links(&list, &PlaybackOutput::Local);
        assert_eq!(opened(&plan), vec![(ChannelType::Cursor, 0)]);
        assert_eq!(plan[0].2, ChannelLink::Skip("already linked"));
        assert!(plan[1..5]
            .iter()
            .all(|(_, _, link)| *link == ChannelLink::Skip("not supported")));
    }

    /// A sink that ignores everything, for tests that only need a
    /// `PlaybackOutput::Sink`.
    struct NullPlaybackSink;

    impl crate::playback_sink::PlaybackSink for NullPlaybackSink {
        fn on_opus_packet(&self, _packet: &[u8], _samples_in_packet: u32) {}
    }

    #[test]
    fn playback_follows_the_audio_feature() {
        let list = [(ChannelType::Playback, 0), (ChannelType::Cursor, 0)];

        // Local output needs cpal, so it links only with `audio`.
        let plan = plan_channel_links(&list, &PlaybackOutput::Local);
        let expected = if cfg!(feature = "audio") {
            ChannelLink::Open
        } else {
            ChannelLink::Skip("audio feature disabled at compile time")
        };
        assert_eq!(plan[0].2, expected);
        assert_eq!(plan[1].2, ChannelLink::Open);

        // A sink needs no device, so it links in every build.
        let plan = plan_channel_links(&list, &PlaybackOutput::Sink(Arc::new(NullPlaybackSink)));
        assert_eq!(plan[0].2, ChannelLink::Open);
        assert_eq!(plan[1].2, ChannelLink::Open);
    }

    #[test]
    fn usbredir_follows_the_usbredir_feature() {
        let plan = plan_channel_links(
            &[(ChannelType::Usbredir, 0), (ChannelType::Cursor, 0)],
            &PlaybackOutput::Local,
        );
        let expected = if cfg!(feature = "usbredir") {
            ChannelLink::Open
        } else {
            ChannelLink::Skip("usbredir feature disabled at compile time")
        };
        assert_eq!(plan[0].2, expected);
        assert_eq!(plan[1].2, ChannelLink::Open);
    }

    #[test]
    fn webdav_follows_the_webdav_feature() {
        let plan = plan_channel_links(
            &[(ChannelType::Webdav, 0), (ChannelType::Cursor, 0)],
            &PlaybackOutput::Local,
        );
        let expected = if cfg!(feature = "webdav") {
            ChannelLink::Open
        } else {
            ChannelLink::Skip("webdav feature disabled at compile time")
        };
        assert_eq!(plan[0].2, expected);
        assert_eq!(plan[1].2, ChannelLink::Open);
    }

    #[test]
    fn connection_outcome_clean_finish_is_ok() {
        assert!(connection_outcome(Some(Ok(Ok(())))).is_ok());
    }

    #[test]
    fn connection_outcome_aborted_after_stop_is_ok() {
        assert!(connection_outcome(None).is_ok());
    }

    #[test]
    fn connection_outcome_failure_is_err_carrying_the_cause() {
        let cause =
            anyhow::anyhow!("Connection refused (os error 111)").context("dialling 127.0.0.1:1");
        let err = connection_outcome(Some(Ok(Err(cause)))).unwrap_err();

        assert_eq!(err.to_string(), "headless SPICE connection failed");
        let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
        assert_eq!(
            chain,
            vec![
                "headless SPICE connection failed".to_string(),
                "dialling 127.0.0.1:1".to_string(),
                "Connection refused (os error 111)".to_string(),
            ]
        );
        // ryll's `main` prints the error with `{:?}`. That must not
        // repeat the log line tools/proxmox-smoke.py matches.
        assert!(!format!("{:?}", err).contains("Connection task failed"));
    }

    #[tokio::test]
    async fn connection_outcome_panic_is_err() {
        let deliberate = true;
        let join: ConnectionJoin = tokio::spawn(async move {
            if deliberate {
                panic!("connection_outcome_panic_is_err: deliberate");
            }
            Ok(())
        })
        .await;
        let err = connection_outcome(Some(join)).unwrap_err();
        assert!(err.to_string().contains("panicked"), "{}", err);
    }

    #[tokio::test]
    async fn dropping_channel_tasks_aborts_them_and_their_helpers() {
        // Each task holds a sender it never uses, so its receiver
        // closes only when the task is gone.
        let (task_tx, mut task_rx) = mpsc::channel::<()>(1);
        let (helper_tx, mut helper_rx) = mpsc::channel::<()>(1);
        let mut tasks = ChannelTasks::default();
        tasks.push((
            ChannelType::Main,
            tokio::spawn(async move {
                let _held = task_tx;
                std::future::pending::<Result<()>>().await
            }),
        ));
        let helper = tokio::spawn(async move {
            let _held = helper_tx;
            std::future::pending::<()>().await
        });
        tasks.helpers.push(helper.abort_handle());

        drop(tasks);
        let closed = Duration::from_secs(5);
        assert_eq!(tokio::time::timeout(closed, task_rx.recv()).await, Ok(None));
        assert_eq!(
            tokio::time::timeout(closed, helper_rx.recv()).await,
            Ok(None)
        );
    }
}
