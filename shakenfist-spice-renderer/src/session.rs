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
//! bug-report assembly. The host (ryll's `main.rs` and `app.rs`)
//! constructs the trait objects and wraps the orchestrator.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use shakenfist_spice_protocol::{ChannelType, ConnectionConfig, SpiceClient};

use crate::audio_sink::OpusPacketSink;
use crate::byte_counter::ByteCounter;
use crate::capture_sink::CaptureSink;
#[cfg(feature = "audio")]
use crate::channels::PlaybackChannel;
use crate::channels::{
    ChannelEvent, CursorChannel, DisplayChannel, InputEvent, InputsChannel, MainChannel,
    UsbCommand, UsbredirChannel, VolumeControl, WebdavChannel, WebdavCommand,
};
use crate::clipboard::ClipboardBackend;
use crate::device_config::{ShareDirConfig, VirtualDiskConfig};
use crate::log_config::LogConfig;
use crate::mm_clock::MmClock;
use crate::notification_sink::NotificationSink;
use crate::snapshots::ChannelSnapshots;
use crate::surface_mirror::SurfaceMirror;
use crate::traffic::TrafficSink;

/// Channel buffer sizes used by `run_headless`. The GUI side
/// (in `ryll/src/app.rs`) keeps its own copies because the egui
/// loop sizes them as part of its own setup; these defaults are
/// the headless-mode shape and are exposed for hosts that want
/// to match.
pub const EVENT_CHANNEL_SIZE: usize = 1024;
pub const INPUT_CHANNEL_SIZE: usize = 256;

/// Capacity of the per-headless broadcast bus that fans
/// `ChannelEvent`s out to multiple subscribers (the headless stats
/// drain, the control-socket per-client tap, and future digest /
/// MCP consumers).  A slow subscriber lags rather than back-
/// pressuring the SPICE channel producers.
pub const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Minimal stats tracker used by the headless event drain. The
/// GUI tracks much richer stats inside `RyllApp`; headless only
/// needs aggregated counters for the periodic info log.
#[derive(Default)]
struct HeadlessStats {
    frames_received: u64,
    bytes_in: u64,
    bytes_out: u64,
}

/// Minimal `StatusProvider` implementation for headless mode.
///
/// `spice_connected` is conservative: we report `true` as long as
/// the connection task handle is still alive (it exits only once the
/// main channel disconnects).  `agent_connected` reflects the latest
/// `ChannelEvent::AgentConnected` observed by the broadcast fan-out.
/// `surfaces` is populated from the live `SurfaceMirror` so a
/// `status` reply matches what the `screenshot` verb would observe.
pub struct SessionStatus {
    /// True while the connection task is running.  The control server
    /// reads this via the `StatusProvider` trait.
    connected: Arc<AtomicBool>,
    /// Current vdagent connection state, updated by the broadcast
    /// fan-out task whenever a `ChannelEvent::AgentConnected` is
    /// observed.  Reads here are best-effort: a `status` request
    /// arriving between the SPICE main channel publishing the event
    /// and the fan-out task storing it will see the older value, but
    /// that race is at most one bus-tick wide.
    agent_connected: Arc<AtomicBool>,
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
        agent_connected: Arc<AtomicBool>,
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
            agent_connected: self.agent_connected.load(Ordering::Relaxed),
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
/// host-side Ctrl+C bridge raising the same flag).
#[allow(clippy::too_many_arguments)]
pub async fn run_connection(
    config: ConnectionConfig,
    event_tx: mpsc::Sender<ChannelEvent>,
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
    resize_rx: mpsc::Receiver<(u32, u32)>,
    volume_control: Arc<VolumeControl>,
    enable_paste: bool,
    log_config: LogConfig,
    cancel: Arc<AtomicBool>,
    clipboard: Option<Arc<dyn ClipboardBackend>>,
    opus_sink: Option<Arc<dyn OpusPacketSink>>,
    image_cache_cap_bytes: usize,
    glz_dictionary_cap_bytes: usize,
) -> Result<()> {
    // When the `audio` feature is off, `volume_control` and
    // `opus_sink` are never read (the playback channel arm is
    // gated out below).  Keep the function's public signature
    // stable across feature configurations so host callers do
    // not have to know which features the renderer was built
    // with; suppress the resulting unused-variable lints here.
    #[cfg(not(feature = "audio"))]
    let _ = (&volume_control, &opus_sink);

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
    // abandoned while main kept sending Latency events into it.
    // After ~65 pings (~7m45s of idle time) the buffer filled and
    // main blocked forever on send().await.
    let (session_init_tx, session_init_rx) = oneshot::channel();
    let (channels_avail_tx, channels_avail_rx) = oneshot::channel();

    let main_stream = client.connect_channel(0, ChannelType::Main, 0).await?;

    let mut main_channel = MainChannel::new(
        main_stream,
        event_tx.clone(),
        repaint_notify.clone(),
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

    // Spawn main channel task
    let main_handle = tokio::spawn(async move { main_channel.run().await });

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
    let mut handles: Vec<(
        ChannelType,
        tokio::task::JoinHandle<Result<(), anyhow::Error>>,
    )> = vec![(ChannelType::Main, main_handle)];
    let mut usb_rx = Some(usb_rx);
    let mut webdav_rx = Some(webdav_rx);
    let shared_glz_dictionary = DisplayChannel::new_shared_glz_dictionary(glz_dictionary_cap_bytes);

    let main_only = std::env::var("RYLL_K1_MAIN_ONLY").is_ok();
    if main_only {
        info!("RYLL_K1_MAIN_ONLY set — skipping all secondary channels (main only)");
    }

    for (channel_type, channel_id) in channels {
        if main_only {
            continue;
        }
        match channel_type {
            ChannelType::Display => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = DisplayChannel::new(
                    channel_id,
                    stream,
                    event_tx.clone(),
                    repaint_notify.clone(),
                    capture.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.display.clone(),
                    shared_glz_dictionary.clone(),
                    log_config,
                    mm_clock.clone(),
                    image_cache_cap_bytes,
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
                    event_tx.clone(),
                    repaint_notify.clone(),
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

            ChannelType::Inputs => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = InputsChannel::new(
                    stream,
                    event_tx.clone(),
                    repaint_notify.clone(),
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
                // input_rx is moved, can't connect more inputs channels
                break;
            }

            ChannelType::Usbredir => {
                if let Some(usb_rx) = usb_rx.take() {
                    let stream = client
                        .connect_channel(session_id, channel_type, channel_id)
                        .await?;
                    let mut channel = UsbredirChannel::new(
                        stream,
                        event_tx.clone(),
                        repaint_notify.clone(),
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
                } else {
                    info!(
                        "Skipping additional usbredir channel (id={}): only one supported",
                        channel_id
                    );
                }
            }

            ChannelType::Webdav => {
                if let Some(webdav_rx) = webdav_rx.take() {
                    let stream = client
                        .connect_channel(session_id, channel_type, channel_id)
                        .await?;
                    let mut channel = WebdavChannel::new(
                        stream,
                        event_tx.clone(),
                        repaint_notify.clone(),
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
                } else {
                    info!(
                        "Skipping additional webdav channel (id={}): only one supported",
                        channel_id
                    );
                }
            }

            #[cfg(feature = "audio")]
            ChannelType::Playback => {
                let stream = client
                    .connect_channel(session_id, channel_type, channel_id)
                    .await?;
                let mut channel = PlaybackChannel::new(
                    stream,
                    event_tx.clone(),
                    repaint_notify.clone(),
                    byte_counter.clone(),
                    traffic.clone(),
                    snapshots.playback.clone(),
                    volume_control.clone(),
                    log_config,
                    cancel.clone(),
                    opus_sink.clone(),
                );
                handles.push((
                    ChannelType::Playback,
                    tokio::spawn(async move { channel.run().await }),
                ));
            }
            #[cfg(not(feature = "audio"))]
            ChannelType::Playback => {
                info!(
                    "Skipping playback channel (id={}): audio feature disabled at compile time",
                    channel_id
                );
            }

            _ => {
                info!(
                    "Skipping channel: {} (id={})",
                    channel_type.name(),
                    channel_id
                );
            }
        }
    }

    // Cancel watcher: when the host raises the per-connection
    // cancel flag (a fresh GUI Reconnect superseding this attempt,
    // or a Ctrl+C bridge in headless mode), abort every channel
    // task so the wait loop below returns promptly. The watcher
    // polls at 100 ms; that latency is well inside human reaction
    // time and avoids adding any awaitable signalling primitive.
    let cancel_watcher = {
        let abort_handles: Vec<_> = handles.iter().map(|(_, h)| h.abort_handle()).collect();
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

    // Wait for all channel tasks
    for (channel_type, handle) in handles {
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
                event_tx
                    .send(ChannelEvent::Error {
                        channel: channel_type,
                        message,
                    })
                    .await
                    .ok();
                repaint_notify.notify_one();
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

/// Spawn a control-socket server for a session.
///
/// Shared by every mode that offers `--control-socket`, so a change
/// of shape lands once rather than in each mode separately. Web mode
/// hosts one of these too; it was headless-only until a browser
/// session found four input bugs that a control-socket scenario test
/// would have caught, and could not have run.
///
/// Takes a [`CancellationToken`] rather than being aborted, because
/// the server unlinks its socket file on the way out and an abort
/// would skip that.
///
/// Unix-only: the server uses `tokio::net::UnixListener`, which has
/// no Windows equivalent in this shape.
#[cfg(unix)]
pub fn spawn_control_socket(
    sock_path: PathBuf,
    status: Arc<dyn crate::control::StatusProvider>,
    event_tx: broadcast::Sender<ChannelEvent>,
    input_tx: mpsc::Sender<InputEvent>,
    surface_mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let server = crate::control::Server::new(sock_path);
    tokio::spawn(async move {
        if let Err(e) = server
            .run(status, event_tx, input_tx, surface_mirror, cancel)
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
    image_cache_cap_bytes: usize,
    glz_dictionary_cap_bytes: usize,
    control_socket_path: Option<PathBuf>,
) -> Result<()> {
    info!("Running in headless mode");

    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_SIZE);
    let (_usb_tx, usb_rx) = mpsc::channel(16);
    let (_webdav_tx, webdav_rx) = mpsc::channel(16);
    let (_resize_tx, resize_rx) = mpsc::channel(32);

    // Broadcast bus that the headless event loop and the control
    // socket both subscribe to.  The fan-out task spawned below
    // drains the renderer's mpsc and republishes each event onto
    // this bus, so SPICE channel producers are never back-pressured
    // by any single consumer (e.g. a slow control-socket client).
    let (event_broadcast_tx, _) = broadcast::channel::<ChannelEvent>(EVENT_BROADCAST_CAPACITY);

    // Headless mode does not paint anything, but the channel handlers still
    // call notify_one().  Give them a Notify whose notifications nobody
    // listens for; tokio::sync::Notify::notify_one is cheap (no allocation,
    // no waker if no waiters) so this is harmless.
    let repaint_notify = Arc::new(Notify::new());

    // Track whether the SPICE connection task is still alive. The
    // control server's `SessionStatus` impl reads this flag to answer
    // `status` queries.
    let spice_connected = Arc::new(AtomicBool::new(true));

    // Track the current vdagent connection state.  Updated by the
    // fan-out task whenever a `ChannelEvent::AgentConnected` arrives;
    // read by the `SessionStatus` provider so `status` requests
    // reflect reality without having to peer into the main channel.
    let agent_connected = Arc::new(AtomicBool::new(false));

    // Live pixel store rebuilt from the broadcast bus.  Constructed
    // unconditionally so the control socket's `screenshot` verb and
    // `status` surface list always have a coherent source; the apply
    // task below subscribes to the broadcast bus and pipes every
    // display-bearing `ChannelEvent` through `SurfaceMirror::apply_event`.
    // This is the same wrap pattern `run_web` uses in `ryll/src/main.rs`
    // — both code paths call the same `apply_event` helper directly on
    // the mirror, so the apply logic itself is shared without an extra
    // indirection.
    let surface_mirror = Arc::new(tokio::sync::Mutex::new(SurfaceMirror::new()));

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

    // Spawn connection task. The cancel flag is passed through so
    // a host-side Ctrl+C bridge can flip it and have every channel
    // task exit promptly.
    let cancel_for_conn = cancel.clone();
    let spice_connected_for_conn = spice_connected.clone();
    let connection_handle = tokio::spawn(async move {
        let result = run_connection(
            config,
            event_tx,
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
            None, // headless mode: no clipboard
            None, // headless mode: no opus sink (cpal output only)
            image_cache_cap_bytes,
            glz_dictionary_cap_bytes,
        )
        .await;
        spice_connected_for_conn.store(false, Ordering::Relaxed);
        result
    });
    tokio::pin!(connection_handle);

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

    // Control socket task — spawned when --control-socket is set.
    // A `CancellationToken` is used so the server can exit cleanly
    // when the SPICE session ends (rather than being aborted, which
    // would skip the socket-file unlink).
    //
    // The control socket is Unix-only: it uses tokio::net::UnixListener
    // which has no Windows equivalent in the same shape.  On non-Unix
    // platforms the path is logged-and-ignored.
    #[cfg(unix)]
    let control_cancel = CancellationToken::new();
    #[cfg(unix)]
    let control_handle = control_socket_path.map(|sock_path| {
        let status: Arc<dyn crate::control::StatusProvider> = Arc::new(SessionStatus::new(
            spice_connected.clone(),
            agent_connected.clone(),
            surface_mirror.clone(),
        ));
        spawn_control_socket(
            sock_path,
            status,
            event_broadcast_tx.clone(),
            input_tx_for_control,
            surface_mirror.clone(),
            control_cancel.clone(),
        )
    });
    #[cfg(not(unix))]
    {
        if control_socket_path.is_some() {
            warn!("control: --control-socket is Unix-only; ignoring on this platform");
        }
    }

    // Event fan-out task: drains the renderer's mpsc and republishes
    // each `ChannelEvent` onto the broadcast bus.  This is the
    // architectural pivot that lets multiple consumers (the headless
    // stats drain below, control-socket clients, future digest
    // consumers) tap the same stream without back-pressuring the
    // SPICE channel producers.  The web-mode equivalent in
    // `ryll/src/main.rs::run_web` follows the same shape; we keep
    // both paths separate rather than abstracting because each is
    // five lines and the surrounding wiring differs.
    //
    // The fan-out also caches the latest `agent_connected` state in
    // the shared `AtomicBool` so `status` requests reflect reality.
    let stats_event_rx = event_broadcast_tx.subscribe();
    let fanout_broadcast_tx = event_broadcast_tx.clone();
    let fanout_agent_connected = agent_connected.clone();
    let _fanout_handle = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            if let ChannelEvent::AgentConnected(connected) = &event {
                fanout_agent_connected.store(*connected, Ordering::Relaxed);
            }
            // `broadcast::Sender::send` is non-blocking.  An error
            // means there are no current receivers, which is fine
            // for events the headless stats drain does not care
            // about (everything proceeds normally — the broadcast
            // sender re-arms automatically when subscribers appear).
            let _ = fanout_broadcast_tx.send(event);
        }
    });
    // Surface-mirror apply task: subscribes to the broadcast bus and
    // pipes every `ChannelEvent` through `SurfaceMirror::apply_event`.
    // Mirrors `ryll/src/main.rs::run_web`'s identically-shaped task —
    // both code paths call the same `SurfaceMirror::apply_event`
    // helper directly on the shared mirror, so the apply dispatch is
    // single-sourced inside `surface_mirror.rs` itself.  A `Lagged`
    // error means a slow consumer missed N events; for the mirror
    // that's bad because surface state diverges from what SPICE sent,
    // but the same trade-off `run_web` already makes — log and continue.
    let mirror_for_apply = surface_mirror.clone();
    let mut mirror_event_rx = event_broadcast_tx.subscribe();
    let _mirror_apply_handle = tokio::spawn(async move {
        loop {
            match mirror_event_rx.recv().await {
                Ok(event) => {
                    let mut m = mirror_for_apply.lock().await;
                    m.apply_event(&event);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        "headless: surface mirror lagged by {} events; \
                         surface state may briefly diverge",
                        n
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("headless: surface mirror task exiting (broadcast closed)");
                    break;
                }
            }
        }
    });

    // Drop our extra reference to the broadcast sender so that, once
    // the fan-out task exits (on `event_rx` closing), the remaining
    // receivers see `RecvError::Closed` and tear themselves down.
    drop(event_broadcast_tx);

    // Process events.  The headless drain subscribes to the
    // broadcast bus rather than the original mpsc — exactly the
    // same `ChannelEvent` stream, fan-out architecture.
    let mut event_rx = stats_event_rx;
    let mut stats = HeadlessStats::default();
    let mut last_stats_print = Instant::now();
    let mut paste_failed = false;

    loop {
        tokio::select! {
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
                        stats.frames_received += 1;
                    }
                    ChannelEvent::Statistics { bytes_in, bytes_out, .. } => {
                        stats.bytes_in += bytes_in;
                        stats.bytes_out += bytes_out;
                    }
                    ChannelEvent::Disconnected(ChannelType::Main) => {
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
                    ChannelEvent::AgentConnected(connected) => {
                        info!("headless: vdagent connected={}", connected);
                    }
                    ChannelEvent::Error { channel, message } => {
                        error!("Error on {}: {}", channel.name(), message);
                    }
                    ChannelEvent::Notification(entry) => {
                        notifications.push(entry);
                    }
                    _ => {}
                }

                // Print stats periodically
                if last_stats_print.elapsed() >= Duration::from_secs(10) {
                    info!(
                        "Stats: frames={}, bytes_in={}, bytes_out={}",
                        stats.frames_received, stats.bytes_in, stats.bytes_out
                    );
                    last_stats_print = Instant::now();
                }
            }
            join_result = &mut connection_handle => {
                match join_result {
                    Ok(Ok(())) => info!("Connection task completed"),
                    Ok(Err(e)) => error!("Connection task failed: {:#}", e),
                    Err(e) => error!("Connection task panicked: {}", e),
                }
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

    if paste_failed {
        anyhow::bail!("paste-as-keystrokes failed");
    }

    info!("Headless mode finished");
    Ok(())
}
