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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{mpsc, Notify};
use tracing::{error, info};

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
) -> Result<()> {
    let client = SpiceClient::new(config)?;

    // Wait for session initialization

    // Connect main channel and run until we get session ID and channel list
    let (event_tx_clone, mut temp_rx) = mpsc::channel(64);

    let main_stream = client.connect_channel(0, ChannelType::Main, 0).await?;

    let mut main_channel = MainChannel::new(
        main_stream,
        event_tx_clone,
        repaint_notify.clone(),
        capture.clone(),
        byte_counter.clone(),
        traffic.clone(),
        snapshots.main,
        resize_rx,
        monitors,
        log_config,
        clipboard,
    );

    // Spawn main channel task
    let main_handle = tokio::spawn(async move { main_channel.run().await });

    // Wait for session init and channel list
    let mut got_session = false;
    let mut got_channels = false;
    let mut temp_session_id = 0u32;
    let mut temp_channels = Vec::new();

    loop {
        match temp_rx.recv().await {
            Some(ChannelEvent::SessionInitialized(id)) => {
                temp_session_id = id;
                got_session = true;
                event_tx
                    .send(ChannelEvent::SessionInitialized(id))
                    .await
                    .ok();
                repaint_notify.notify_one();
            }
            Some(ChannelEvent::ChannelsAvailable(chs)) => {
                temp_channels = chs;
                got_channels = true;
                event_tx
                    .send(ChannelEvent::ChannelsAvailable(temp_channels.clone()))
                    .await
                    .ok();
                repaint_notify.notify_one();
            }
            Some(other) => {
                event_tx.send(other).await.ok();
                repaint_notify.notify_one();
            }
            None => break,
        }

        if got_session && got_channels {
            break;
        }
    }

    let session_id = temp_session_id;
    let channels = temp_channels;

    info!(
        "Session {} ready with {} channels",
        session_id,
        channels.len()
    );

    // Connect other channels
    let mut handles = vec![main_handle];
    let mut usb_rx = Some(usb_rx);
    let mut webdav_rx = Some(webdav_rx);
    let shared_glz_dictionary = DisplayChannel::new_shared_glz_dictionary();

    for (channel_type, channel_id) in channels {
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
                );
                handles.push(tokio::spawn(async move { channel.run().await }));
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
                handles.push(tokio::spawn(async move { channel.run().await }));
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
                handles.push(tokio::spawn(async move { channel.run().await }));
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
                        log_config,
                    );
                    handles.push(tokio::spawn(async move { channel.run().await }));
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
                        log_config,
                    );
                    handles.push(tokio::spawn(async move { channel.run().await }));
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
                    volume_control.clone(),
                    log_config,
                    cancel.clone(),
                    opus_sink.clone(),
                );
                handles.push(tokio::spawn(async move { channel.run().await }));
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
        let abort_handles: Vec<_> = handles.iter().map(|h| h.abort_handle()).collect();
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
    for handle in handles {
        match handle.await {
            Err(e) if e.is_cancelled() => {
                // Aborted by the cancel watcher; not an error.
            }
            Err(e) => {
                error!("Channel task panic: {}", e);
            }
            Ok(Err(e)) => {
                let msg = format!("channel error: {}", e);
                error!("session: {}", msg);
                event_tx.send(ChannelEvent::Error(msg)).await.ok();
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

    // Spawn connection task. The cancel flag is passed through so
    // a host-side Ctrl+C bridge can flip it and have every channel
    // task exit promptly.
    let cancel_for_conn = cancel.clone();
    let connection_handle = tokio::spawn(async move {
        run_connection(
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
        )
        .await
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
                    ChannelEvent::Error(msg) => {
                        error!("Error: {}", msg);
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

    if paste_failed {
        anyhow::bail!("paste-as-keystrokes failed");
    }

    info!("Headless mode finished");
    Ok(())
}
