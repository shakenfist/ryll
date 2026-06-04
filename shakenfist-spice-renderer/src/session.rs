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
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use shakenfist_spice_protocol::{ChannelType, ConnectionConfig, SpiceClient};

use crate::audio_sink::OpusPacketSink;
use crate::byte_counter::ByteCounter;
use crate::capture_sink::CaptureSink;
use crate::channels::{
    ChannelEvent, CursorChannel, DisplayChannel, InputEvent, InputsChannel, MainChannel,
    PlaybackChannel, UsbCommand, UsbredirChannel, VolumeControl, WebdavChannel, WebdavCommand,
};
use crate::clipboard::ClipboardBackend;
use crate::device_config::{ShareDirConfig, VirtualDiskConfig};
use crate::log_config::LogConfig;
use crate::mm_clock::MmClock;
use crate::notification_sink::NotificationSink;
use crate::snapshots::ChannelSnapshots;
use crate::traffic::TrafficSink;

/// Channel buffer sizes used by `run_headless`. The GUI side
/// (in `ryll/src/app.rs`) keeps its own copies because the egui
/// loop sizes them as part of its own setup; these defaults are
/// the headless-mode shape and are exposed for hosts that want
/// to match.
pub const EVENT_CHANNEL_SIZE: usize = 1024;
pub const INPUT_CHANNEL_SIZE: usize = 256;

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
/// main channel disconnects).  `agent_connected` and `surfaces` are
/// stubs that later phase-3 steps will populate:
///
/// - `agent_connected` → wired via `ChannelEvent::AgentConnected` in
///   step 3d when the broadcast fan-out lands.
/// - `surfaces` → populated in step 3e once the `SurfaceMirror` is
///   instantiated in headless mode.
struct HeadlessStatus {
    /// True while the connection task is running.  The control server
    /// reads this via the `StatusProvider` trait.
    connected: Arc<AtomicBool>,
}

impl HeadlessStatus {
    fn new(connected: Arc<AtomicBool>) -> Self {
        Self { connected }
    }
}

impl crate::control::StatusProvider for HeadlessStatus {
    fn snapshot(&self) -> crate::control::protocol::StatusResult {
        crate::control::protocol::StatusResult {
            spice_connected: self.connected.load(Ordering::Relaxed),
            agent_connected: false, // wired in step 3d
            surfaces: Vec::new(),   // populated in step 3e
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
    let client = SpiceClient::new(config)?;

    // Shared mm_time clock. The main channel writes to it from
    // `MAIN_INIT` and `MULTI_MEDIA_TIME`; the display channel
    // reads from it when building `STREAM_REPORT` payloads
    // (phase 1F). Constructed here so both channels share the
    // same `Arc<MmClock>`; not exposed through `run_connection`'s
    // public signature because it is purely internal plumbing
    // — host callers never see it.
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

    // Headless mode does not paint anything, but the channel handlers still
    // call notify_one().  Give them a Notify whose notifications nobody
    // listens for; tokio::sync::Notify::notify_one is cheap (no allocation,
    // no waker if no waiters) so this is harmless.
    let repaint_notify = Arc::new(Notify::new());

    // Track whether the SPICE connection task is still alive. The
    // control server's `HeadlessStatus` impl reads this flag to answer
    // `status` queries.
    let spice_connected = Arc::new(AtomicBool::new(true));

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

    // Clone input_tx before cadence moves it, so the paste trigger can also use it.
    let paste_input_tx = input_tx.clone();

    // Cadence task if enabled
    let cadence_handle = if cadence {
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = input_tx.try_send(InputEvent::KeyDown(0x39));
                let _ = input_tx.try_send(InputEvent::KeyUp(0xB9));
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
    let control_cancel = CancellationToken::new();
    let control_handle = if let Some(sock_path) = control_socket_path {
        let status: Arc<dyn crate::control::StatusProvider> =
            Arc::new(HeadlessStatus::new(spice_connected.clone()));
        let server = crate::control::Server::new(sock_path);
        let token = control_cancel.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = server.run(status, token).await {
                warn!("control: server exited with error: {}", e);
            }
        }))
    } else {
        None
    };

    // Process events
    let mut stats = HeadlessStats::default();
    let mut last_stats_print = Instant::now();
    let mut paste_failed = false;

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
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
                    ChannelEvent::PasteCompleted { chars, elapsed_ms } => {
                        info!(
                            "headless: paste complete: {} chars in {}ms",
                            chars, elapsed_ms
                        );
                    }
                    ChannelEvent::PasteFailed { reason } => {
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
            _ = &mut connection_handle => {
                info!("Connection task completed");
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
