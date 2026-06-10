#[cfg(feature = "gui")]
mod app;
#[cfg(feature = "gui")]
mod auto_snapshot;
mod bugreport;
#[cfg(feature = "capture")]
mod capture;
mod notifications;
#[cfg(feature = "gui")]
mod streaming_state;
mod web;
#[cfg(not(feature = "capture"))]
mod capture {
    /// Stub CaptureSession when capture feature is disabled.
    /// Methods are never called (capture is always None), but
    /// the compiler needs to see them for type-checking.
    pub struct CaptureSession {
        #[allow(dead_code)]
        pub dir: std::path::PathBuf,
    }
    impl CaptureSession {
        /// Returns `true`: there is no queue to fill, so "accepted"
        /// is the correct contract — callers must not record drops
        /// in the no-capture build. Matches the trait's bool return.
        pub fn packet_sent(&self, _channel: &str, _data: &[u8]) -> bool {
            true
        }
        pub fn packet_received(&self, _channel: &str, _data: &[u8]) -> bool {
            true
        }
        pub fn frame(&self, _id: u32, _px: &[u8], _w: u32, _h: u32) -> bool {
            true
        }
        pub fn close(&self) {}
    }
    impl shakenfist_spice_renderer::CaptureSink for CaptureSession {
        fn packet_sent(&self, channel: &str, data: &[u8]) -> bool {
            CaptureSession::packet_sent(self, channel, data)
        }
        fn packet_received(&self, channel: &str, data: &[u8]) -> bool {
            CaptureSession::packet_received(self, channel, data)
        }
        fn frame(&self, id: u32, px: &[u8], w: u32, h: u32) -> bool {
            CaptureSession::frame(self, id, px, w, h)
        }
    }
}
#[cfg(feature = "gui")]
mod clipboard_arboard;
mod config;
#[cfg(feature = "gui")]
mod display_gui;
#[cfg(feature = "gui")]
mod input_egui;
mod settings;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
#[cfg(feature = "gui")]
use eframe::egui;
use tracing::{info, Level};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::bugreport::{AppSnapshot, BugReport, ChannelSnapshots, PedanticConfig, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::config::{
    parse_share_dir, parse_virtual_disks, Args, Config, ShareDirConfig, VirtualDiskConfig,
};
use crate::notifications::{
    register_gap_notification_observer, NotificationStore, NotificationStoreSink,
    SharedNotifications,
};

/// Process-global shutdown flag, raised by the Ctrl+C handler.
/// Each connection-bearing entry point (`run_gui`, `run_headless`)
/// bridges this into a per-attempt `Arc<AtomicBool>` cancel flag
/// that the renderer's session orchestrator polls. The renderer
/// itself has no business knowing about a process-global signal
/// flag, so this lives strictly host-side.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Convert a MiB cap (parsed as u64) into a byte count usable as
/// `usize`. Uses saturating arithmetic so that on 32-bit platforms
/// a large user input is clamped to `usize::MAX` rather than
/// silently truncated by the `as usize` cast.
fn mib_to_usize_bytes(mib: u64) -> usize {
    mib.saturating_mul(1024 * 1024)
        .try_into()
        .unwrap_or(usize::MAX)
}

fn main() -> Result<()> {
    // Baseline the runtime-metrics uptime clock at process
    // start, before tokio runtime construction or any
    // metrics::sample() call. On Linux this is a no-op; on
    // macOS it forces the LazyLock<Instant> that backs
    // `process.uptime_secs` so the field measures from
    // here rather than the first bug-report trigger. See
    // PLAN-macos-runtime-metrics-phase-03-integration.md.
    shakenfist_spice_renderer::metrics::init_at_startup();

    // Install Ctrl+C handler so graceful shutdown works on all platforms.
    ctrlc::set_handler(|| {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    })
    .expect("failed to set Ctrl+C handler");

    // Idempotent rustls CryptoProvider install. rustls 0.23 panics
    // at the no-arg `ClientConfig::builder()` call site
    // (shakenfist-spice-protocol/src/client.rs) when feature
    // unification across the workspace enables both `ring` and
    // `aws-lc-rs` and no process-level default has been installed.
    // The Linux devcontainer's resolver lands on a single provider
    // and silently auto-detects; macOS resolves with both enabled
    // and panics. Installing a default explicitly at startup
    // covers every entry path (--web already did this internally).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Optional tokio-console initialisation for the K1 hang
    // investigation. Requires `--features tokio-console` AND
    // `RUSTFLAGS=--cfg tokio_unstable` at compile time, plus
    // `RYLL_TOKIO_CONSOLE=1` at runtime. Console-subscriber
    // installs itself as the global tracing subscriber, so we
    // skip the regular tracing_subscriber init below when it
    // is active. tokio-console viewers connect over a unix
    // socket (default 127.0.0.1:6669) and show every running
    // task's state, registered Wakers, last poll time, etc.
    #[cfg(feature = "tokio-console")]
    let console_subscriber_active = std::env::var("RYLL_TOKIO_CONSOLE")
        .map(|v| v == "1")
        .unwrap_or(false);
    #[cfg(not(feature = "tokio-console"))]
    let console_subscriber_active = false;
    #[cfg(feature = "tokio-console")]
    {
        if console_subscriber_active {
            console_subscriber::init();
            // audit-allow-println — this `eprintln!` fires before
            // tracing is initialised (`set_global_default` runs a
            // few lines below), so a `tracing::info!` here would
            // silently drop. The output is a one-shot operator-
            // facing startup hint that the tokio-console
            // subscriber is live and how to connect.
            eprintln!(
                "ryll: tokio-console subscriber active. Connect with `tokio-console` \
                 from another terminal. The default endpoint is 127.0.0.1:6669."
            );
        }
    }

    // Parse command line arguments
    let args = Args::parse();

    // Set up logging
    let log_level = if args.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };

    let level_filter = tracing_subscriber::filter::LevelFilter::from_level(log_level);

    let stderr_layer = fmt::layer().with_target(false).with_filter(level_filter);

    // When verbose, also log to /tmp/ryll.log
    let _file_guard;
    if console_subscriber_active {
        // console-subscriber installed itself globally; don't
        // try to install another subscriber here. ryll's normal
        // logs go nowhere in this mode — the operator should
        // run with `RUST_LOG=info` and the console-subscriber's
        // own output, or accept the trade-off for the duration
        // of the K1 investigation.
        _file_guard = None;
    } else if args.verbose {
        let file_appender = tracing_appender::rolling::never("/tmp", "ryll.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        _file_guard = Some(guard);

        let file_layer = fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(non_blocking)
            .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);

        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .init();
        info!("Logging to /tmp/ryll.log");
    } else {
        _file_guard = None;
        tracing_subscriber::registry().with(stderr_layer).init();
    };

    // Initialize global settings for protocol logging
    settings::init(args.verbose, args.intimate);

    // Log build identity early — answers "am I running the build
    // I just made?" without needing to inspect the binary or
    // open a bug-report zip. RYLL_GIT_SHA is populated by
    // ryll/build.rs (preferred: Makefile-passed env var; fallback:
    // `git rev-parse` at compile time; last resort: "unknown").
    info!(
        "ryll v{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("RYLL_GIT_SHA"),
    );

    // Load configuration. Phase 5 step 5a removed the
    // `--web` stub: every mode now requires a real `.vv` /
    // `--url` / `--direct` because `run_web` now spawns
    // `run_connection` and actually connects to SPICE.
    let config = Config::from_args(&args)?;
    info!(
        "Connecting to {}:{} (TLS: {})",
        config.host,
        config.port,
        config.tls_port.is_some()
    );

    // Parse virtual disk configs (validates paths early)
    let virtual_disks = parse_virtual_disks(&args)?;

    // Parse shared directory config (validates path early)
    let share_dir = parse_share_dir(&args)?;

    // Create capture session if requested
    #[cfg(feature = "capture")]
    let capture = match &args.capture {
        Some(dir) => Some(Arc::new(CaptureSession::new(
            std::path::PathBuf::from(dir),
            &config.host,
            config.port,
            config.tls_port,
        )?)),
        None => None,
    };
    #[cfg(not(feature = "capture"))]
    let capture: Option<Arc<CaptureSession>> = None;

    // Resolve the pedantic output directory. Priority:
    //   1. --pedantic-dir if set
    //   2. --bug-report-dir if set
    //   3. ./ryll-pedantic-reports (historical default)
    // Eager-failure `mkdir -p` so the user hears about
    // disk/permission problems before the session starts. The
    // actual gap-observer registration happens inside the app
    // constructors (app::RyllApp::new / app::run_headless) once
    // the live traffic / channel-snapshot handles have been
    // built.
    let pedantic_config = if args.pedantic {
        let pedantic_dir = args
            .pedantic_dir
            .clone()
            .or_else(|| args.bug_report_dir.clone())
            .unwrap_or_else(|| std::path::PathBuf::from("./ryll-pedantic-reports"));
        std::fs::create_dir_all(&pedantic_dir).with_context(|| {
            format!(
                "failed to create pedantic directory {}",
                pedantic_dir.display()
            )
        })?;
        Some(PedanticConfig { dir: pedantic_dir })
    } else {
        None
    };

    let obey_guest_size = !args.no_obey_guest_size;

    if args.web {
        run_web(
            config,
            &args,
            virtual_disks,
            share_dir,
            capture,
            pedantic_config,
            obey_guest_size,
        )
    } else if args.headless {
        run_headless(
            config,
            &args,
            virtual_disks,
            share_dir,
            capture,
            pedantic_config,
            obey_guest_size,
        )
    } else {
        #[cfg(feature = "gui")]
        {
            run_gui(
                config,
                &args,
                virtual_disks,
                share_dir,
                capture,
                pedantic_config,
                obey_guest_size,
            )
        }
        #[cfg(not(feature = "gui"))]
        {
            let _ = (
                config,
                &args,
                virtual_disks,
                share_dir,
                capture,
                pedantic_config,
                obey_guest_size,
            );
            anyhow::bail!(
                "this ryll binary was built without the `gui` feature; \
                 pass --headless or --web"
            );
        }
    }
}

fn run_headless(
    config: Config,
    args: &Args,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
    pedantic_config: Option<PedanticConfig>,
    // Headless mode has no window to resize, so this flag is accepted for
    // CLI symmetry but is not used.
    _obey_guest_size: bool,
) -> Result<()> {
    info!("Running in headless mode");

    let enable_paste = args.enable_paste_as_keystrokes || args.paste_text.is_some();
    let paste_text = args.paste_text.clone();
    let paste_char_delay_ms = args.paste_char_delay_ms;
    let cadence = args.cadence;
    let monitors = args.monitors;
    let image_cache_cap_bytes = mib_to_usize_bytes(args.image_cache_cap_mib);
    let glz_dictionary_cap_bytes = mib_to_usize_bytes(args.glz_dictionary_cap_mib);

    // Build the host-side scaffolding the renderer's `run_headless`
    // expects. Notifications, traffic, snapshots, and the byte
    // counter are owned by the host so it can also wire them into
    // pedantic bug-report assembly and the gap observer; the
    // renderer just records into the trait objects we hand it.
    let byte_counter = Arc::new(shakenfist_spice_renderer::ByteCounter::new());
    let traffic = Arc::new(TrafficBuffers::new());
    let notifications: SharedNotifications =
        Arc::new(std::sync::Mutex::new(NotificationStore::new()));
    let snapshots = ChannelSnapshots::new();

    // Register the --pedantic gap observer. Traffic is live in headless
    // so pedantic zips will have a real pcap. Channel-state snapshots
    // are also live (channel tasks write through the `snapshots` handle
    // passed into the renderer below). The `app_snapshot`, however,
    // is only populated by the GUI update loop; in headless it would
    // stay at its default, so we register with a fresh empty
    // AppSnapshot and warn the user.
    if let Some(pedantic) = pedantic_config {
        let app_snapshot = Arc::new(std::sync::Mutex::new(AppSnapshot::default()));
        tracing::warn!(
            "pedantic mode in headless: traffic pcap and channel-state are \
             live, but app-level snapshot (surfaces list, bandwidth, latency) \
             is not populated — that field is updated by the GUI loop only. \
             See docs/plans/PLAN-display-draw-ops-phase-09-pedantic-handles.md."
        );
        BugReport::register_pedantic_observer(
            pedantic,
            config.host.clone(),
            config.port,
            traffic.clone(),
            snapshots.clone(),
            app_snapshot,
            notifications.clone(),
        );
    }
    register_gap_notification_observer(notifications.clone());

    let connection_config: shakenfist_spice_protocol::ConnectionConfig = (&config).into();
    let traffic_dyn: Arc<dyn shakenfist_spice_renderer::TrafficSink> =
        traffic as Arc<dyn shakenfist_spice_renderer::TrafficSink>;
    let capture_dyn: Option<Arc<dyn shakenfist_spice_renderer::CaptureSink>> = capture
        .clone()
        .map(|c| c as Arc<dyn shakenfist_spice_renderer::CaptureSink>);
    let notifications_sink: Arc<dyn shakenfist_spice_renderer::NotificationSink> =
        Arc::new(NotificationStoreSink(notifications.clone()));
    let log_config = settings::log_config();

    // Per-attempt cancel flag. A small bridge task watches the
    // process-global `SHUTDOWN_REQUESTED` (raised by the Ctrl+C
    // handler) and flips this flag when it sees one — that wakes
    // the renderer's 100 ms cancel-poll branch and unwinds every
    // channel task. The bridge runs only for the lifetime of this
    // headless session.
    let cancel = Arc::new(AtomicBool::new(false));

    // --control-socket is a Unix-only flag (see config.rs).  On
    // non-Unix the field doesn't exist on Args; pass None to keep
    // run_headless's signature stable across platforms.
    #[cfg(unix)]
    let control_socket_arg = args.control_socket.clone();
    #[cfg(not(unix))]
    let control_socket_arg: Option<std::path::PathBuf> = None;

    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(async {
        let cancel_for_bridge = cancel.clone();
        let bridge = tokio::spawn(async move {
            loop {
                if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    cancel_for_bridge.store(true, Ordering::Relaxed);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });

        let res = shakenfist_spice_renderer::run_headless(
            connection_config,
            cadence,
            paste_text,
            paste_char_delay_ms,
            enable_paste,
            virtual_disks,
            share_dir,
            capture_dyn,
            monitors,
            byte_counter,
            traffic_dyn,
            snapshots,
            notifications_sink,
            log_config,
            cancel,
            image_cache_cap_bytes,
            glz_dictionary_cap_bytes,
            control_socket_arg,
        )
        .await;

        bridge.abort();
        res
    });

    // Close capture session (flushes MP4 moov atom) on the host side
    // since the renderer no longer holds the concrete `CaptureSession`.
    if let Some(ref capture) = capture {
        capture.close();
    }

    result
}

/// Bridge `flag` (typically [`SHUTDOWN_REQUESTED`]) into the
/// per-connection `cancel: Arc<AtomicBool>` that the renderer's
/// session orchestrator polls. Returns when `flag` is `true`,
/// after flipping `cancel` so the renderer's 100 ms cancel-poll
/// branch fires on its next tick.
///
/// Mirrors the inline pattern in [`run_headless`]; extracted so
/// `run_web` can reuse it and so the bridge can be unit-tested
/// without spinning up a real SPICE session.
async fn shutdown_to_cancel_bridge(flag: &'static AtomicBool, cancel: Arc<AtomicBool>) {
    use std::time::Duration;
    loop {
        if flag.load(Ordering::Relaxed) {
            cancel.store(true, Ordering::Relaxed);
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn run_web(
    config: Config,
    args: &Args,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
    pedantic_config: Option<PedanticConfig>,
    // Web mode never opens a host window, so the GUI's
    // "obey guest size" toggle is accepted for CLI symmetry
    // and ignored. The browser-driven viewport flow that
    // resolves to `VDAgentMonitorsConfig` lands in 5c.
    _obey_guest_size: bool,
) -> Result<()> {
    info!("Running in web mode");

    let web_host = args.web_host.clone();
    let web_port = args.web_port;
    let web_tls_cert = args.web_tls_cert.clone();
    let web_tls_key = args.web_tls_key.clone();
    let monitors = args.monitors;
    let image_cache_cap_bytes = mib_to_usize_bytes(args.image_cache_cap_mib);
    let glz_dictionary_cap_bytes = mib_to_usize_bytes(args.glz_dictionary_cap_mib);

    let runtime = tokio::runtime::Runtime::new()
        .with_context(|| "failed to construct tokio runtime for --web")?;

    let capture_for_renderer = capture.clone();
    let result = runtime.block_on(async move {
        // Idempotent rustls CryptoProvider install. The
        // WebrtcBridge::new path also installs this internally
        // (commit a2dc11cb), but doing it here once at startup
        // covers the case where no offer is ever received.
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Build the host-side scaffolding the renderer expects.
        // Same shape as `run_headless` so any future trait or
        // observer that grows in headless mode flows naturally
        // into web mode by mirroring the wiring.
        let byte_counter = Arc::new(shakenfist_spice_renderer::ByteCounter::new());
        let traffic = Arc::new(TrafficBuffers::new());
        let notifications: SharedNotifications =
            Arc::new(std::sync::Mutex::new(NotificationStore::new()));
        let snapshots = ChannelSnapshots::new();

        // Pedantic-mode bug-report observer. As in headless,
        // `app_snapshot` stays at its default — that field is
        // populated by the GUI loop only. The web frontend may
        // grow its own equivalent in a later phase; for now we
        // warn the user the same way headless does so the
        // pedantic zip clearly indicates the missing data.
        if let Some(pedantic) = pedantic_config {
            let app_snapshot = Arc::new(std::sync::Mutex::new(AppSnapshot::default()));
            tracing::warn!(
                "pedantic mode in web: traffic pcap and channel-state are \
                 live, but app-level snapshot (surfaces list, bandwidth, \
                 latency) is not populated — that field is updated by the \
                 GUI loop only."
            );
            BugReport::register_pedantic_observer(
                pedantic,
                config.host.clone(),
                config.port,
                traffic.clone(),
                snapshots.clone(),
                app_snapshot,
                notifications.clone(),
            );
        }
        register_gap_notification_observer(notifications.clone());

        let connection_config: shakenfist_spice_protocol::ConnectionConfig = (&config).into();
        let traffic_dyn: Arc<dyn shakenfist_spice_renderer::TrafficSink> =
            traffic.clone() as Arc<dyn shakenfist_spice_renderer::TrafficSink>;
        let capture_dyn: Option<Arc<dyn shakenfist_spice_renderer::CaptureSink>> =
            capture_for_renderer.map(|c| c as Arc<dyn shakenfist_spice_renderer::CaptureSink>);
        // Notification sink is wired even though web mode has no
        // notification UI yet — the renderer emits notifications
        // for protocol gaps that the gap observer above already
        // funnels into the SharedNotifications store; keeping
        // the sink wired means a future "web notifications" UI
        // gets the same data without any session-side changes.
        let _notifications_sink: Arc<dyn shakenfist_spice_renderer::NotificationSink> =
            Arc::new(NotificationStoreSink(notifications.clone()));
        let log_config = settings::log_config();

        // Channels the orchestrator consumes. The renderer's
        // `run_connection` takes mpsc receivers for inputs / usb
        // / webdav / resize and an mpsc sender for the channel-
        // event stream. Web mode keeps the senders for the input
        // and resize channels so 5c can drive them from browser
        // messages; usb and webdav stay closed (web mode does
        // not implement these in MVP).
        let (event_tx_mpsc, mut event_rx_mpsc) =
            tokio::sync::mpsc::channel::<shakenfist_spice_renderer::ChannelEvent>(
                shakenfist_spice_renderer::session::EVENT_CHANNEL_SIZE,
            );
        let (input_tx, input_rx) = tokio::sync::mpsc::channel::<
            shakenfist_spice_renderer::InputEvent,
        >(crate::web::server::INPUT_CHANNEL_CAPACITY);
        let (usb_tx, usb_rx) =
            tokio::sync::mpsc::channel::<shakenfist_spice_renderer::UsbCommand>(16);
        let (webdav_tx, webdav_rx) =
            tokio::sync::mpsc::channel::<shakenfist_spice_renderer::WebdavCommand>(16);
        let (resize_tx, resize_rx) =
            tokio::sync::mpsc::channel::<(u32, u32)>(crate::web::server::RESIZE_CHANNEL_CAPACITY);
        // usb / webdav senders are dropped immediately so the
        // corresponding receivers see a closed channel; the
        // matching channel tasks treat closed-channel as "no
        // commands ever, that's fine".
        drop(usb_tx);
        drop(webdav_tx);

        // Broadcaster the 5b/5d/5e relays subscribe to. The
        // forwarder task below pulls from the renderer's mpsc
        // and re-broadcasts so multiple consumers can observe
        // each event. 5b adds the surface-mirror subscriber
        // below; 5d/5e add cursor and audio subscribers.
        let (event_broadcast_tx, _) = tokio::sync::broadcast::channel::<
            shakenfist_spice_renderer::ChannelEvent,
        >(crate::web::server::EVENT_BROADCAST_CAPACITY);

        // Live pixel store that turns ChannelEvents into surface
        // mutations. The encoder reads from this via
        // `RealFrameSource` (constructed in `EncoderInfra::restart`).
        let surface_mirror = Arc::new(tokio::sync::Mutex::new(
            shakenfist_spice_renderer::SurfaceMirror::new(),
        ));

        // Phase 5e: build the Opus passthrough sink. The sink
        // is handed to `run_connection` so the playback channel
        // taps every Opus packet into it; the matching slot is
        // stashed in `WebState` so each `/offer` can plug a
        // fresh Sender in for its audio pump.
        let (opus_sink, active_opus_tx) = crate::web::audio::WebOpusSink::new();
        let opus_sink_dyn: Arc<dyn shakenfist_spice_renderer::OpusPacketSink> = opus_sink;

        // Cancel flag bridged from the process-global
        // `SHUTDOWN_REQUESTED`. Same shape as the headless path.
        let cancel = Arc::new(AtomicBool::new(false));
        let bridge_cancel = cancel.clone();
        let cancel_bridge_handle = tokio::spawn(async move {
            shutdown_to_cancel_bridge(&SHUTDOWN_REQUESTED, bridge_cancel).await;
        });

        // GUI repaint hook — unused in web mode but
        // `run_connection` requires a non-`Option` `Arc<Notify>`.
        let repaint_notify = Arc::new(tokio::sync::Notify::new());
        let volume_control = shakenfist_spice_renderer::channels::VolumeControl::new();

        // Spawn the renderer's session orchestrator. The web
        // mode has no clipboard backend (clipboard sync is
        // out of scope for the MVP) and never enables paste-as-
        // keystrokes (that's a host-side hotkey feature).
        let connection_cancel = cancel.clone();
        let connection_handle = tokio::spawn(async move {
            shakenfist_spice_renderer::run_connection(
                connection_config,
                event_tx_mpsc,
                repaint_notify,
                input_rx,
                usb_rx,
                webdav_rx,
                virtual_disks,
                share_dir,
                capture_dyn,
                byte_counter,
                traffic_dyn,
                snapshots,
                monitors,
                resize_rx,
                volume_control,
                /* enable_paste */ false,
                log_config,
                connection_cancel,
                /* clipboard */ None,
                /* opus_sink */ Some(opus_sink_dyn),
                image_cache_cap_bytes,
                glz_dictionary_cap_bytes,
            )
            .await
        });

        // Forwarder: drain the renderer's mpsc into the broadcast
        // bus so multiple subscribers (5b surface mirror, 5d
        // cursor relay, 5e audio sink) can each see every event.
        // The forwarder exits when the renderer drops its sender
        // on session shutdown.
        let event_broadcast_for_forwarder = event_broadcast_tx.clone();
        let forwarder_handle = tokio::spawn(async move {
            while let Some(event) = event_rx_mpsc.recv().await {
                let _ = event_broadcast_for_forwarder.send(event);
            }
        });

        // Surface-mirror apply-event task. Subscribes to the
        // broadcast bus and pipes every ChannelEvent through
        // `SurfaceMirror::apply_event`. `Lagged` means a slow
        // subscriber missed N events; for the mirror that's
        // bad because surface state diverges from what SPICE
        // sent — log a warning but continue rather than
        // tearing the session down. If lag becomes a real
        // operational problem it's a Phase 6 perf item
        // (larger broadcast capacity or a backpressure scheme).
        let mirror_for_task = surface_mirror.clone();
        let mut event_rx_for_mirror = event_broadcast_tx.subscribe();
        let mirror_handle = tokio::spawn(async move {
            loop {
                match event_rx_for_mirror.recv().await {
                    Ok(event) => {
                        let mut m = mirror_for_task.lock().await;
                        m.apply_event(&event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            "web: surface mirror lagged by {} events; \
                             surface state may briefly diverge",
                            n
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::info!("web: surface mirror task exiting (broadcast closed)");
                        break;
                    }
                }
            }
        });

        // Build state with the channel handles populated and
        // run the HTTP server. When the server exits (Ctrl+C
        // raised SHUTDOWN_REQUESTED, or axum::serve errored)
        // we tear the rest down before returning.
        //
        // Step 5d: clone the bus subscription + surface mirror
        // handle here (before `with_channels` moves the broadcast
        // sender) so we can spawn the cursor relay against the
        // same `bridge_slot` the signalling handler installs into.
        let cursor_event_rx = event_broadcast_tx.subscribe();
        let cursor_mirror = surface_mirror.clone();
        let state = Arc::new(crate::web::WebState::with_channels(
            input_tx,
            resize_tx,
            event_broadcast_tx,
            surface_mirror,
            active_opus_tx,
        ));
        let cursor_bridge_slot = state.bridge_slot.clone();
        let cursor_handle = tokio::spawn(crate::web::cursor::run_cursor_relay(
            cursor_event_rx,
            cursor_bridge_slot,
            cursor_mirror,
        ));

        // Phase 6b: spawn the bridge reaper. It watches the
        // active bridge's dead signal and tears down the bridge
        // + encoder + audio pump when the browser disconnects.
        // The SPICE session is left untouched. The handle is
        // retained so the reaper can be aborted in the shutdown
        // path after axum::serve returns.
        let reaper_handle = tokio::spawn(crate::web::lifecycle::run_bridge_reaper(state.clone()));

        // Phase 8a: load the optional TLS config before binding.
        // Clap's `requires =` enforces both-or-neither, so seeing
        // one flag without the other here would already have
        // been rejected at parse time.
        let tls_config = match (&web_tls_cert, &web_tls_key) {
            (Some(cert), Some(key)) => Some(crate::web::server::load_tls_config(cert, key).await?),
            _ => None,
        };

        let server_result = match tls_config {
            Some(cfg) => {
                crate::web::server::run_with_tls(state.clone(), &web_host, web_port, cfg).await
            }
            None => crate::web::run(state.clone(), &web_host, web_port).await,
        };

        // Phase 6b: explicit bridge close. After axum::serve
        // returns (Ctrl+C raised SHUTDOWN_REQUESTED, or axum
        // errored), close any active bridge so DTLS/SRTP tears
        // down cleanly before the runtime drops. Use a 2-second
        // ceiling so a wedged bridge cannot block shutdown
        // indefinitely.
        tracing::info!("web: HTTP server drained");
        {
            let bridge = {
                let mut slot = state.bridge_slot.lock().await;
                slot.take()
            };
            if let Some(b) = bridge {
                tracing::info!("web: closing active bridge before exit");
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), b.close()).await;
            }
        }
        {
            let mut enc = state.encoder.lock().await;
            tokio::time::timeout(std::time::Duration::from_secs(2), enc.stop())
                .await
                .ok();
        }
        reaper_handle.abort();

        // Server exited; ensure the cancel flag is up so the
        // renderer notices on its next 100 ms tick. The cancel
        // bridge above is doing this already if SHUTDOWN_REQUESTED
        // is set; this is the belt-and-braces case where the
        // server returned for an unrelated reason.
        cancel.store(true, Ordering::Relaxed);
        cancel_bridge_handle.abort();
        // Give the renderer a brief window to unwind cleanly.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection_handle).await;
        forwarder_handle.abort();
        mirror_handle.abort();
        cursor_handle.abort();

        server_result
    });

    // Close capture session on the host side, mirroring
    // `run_headless`.
    if let Some(ref capture) = capture {
        capture.close();
    }

    result
}

#[cfg(feature = "gui")]
fn run_gui(
    config: Config,
    args: &Args,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
    pedantic_config: Option<PedanticConfig>,
    obey_guest_size: bool,
) -> Result<()> {
    info!("Starting GUI");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_resizable(true),
        ..Default::default()
    };

    let cadence = args.cadence;
    let monitors = args.monitors;
    let enable_paste = args.enable_paste_as_keystrokes || args.paste_text.is_some();
    let paste_char_delay_ms = args.paste_char_delay_ms;
    let bug_report_dir = args.bug_report_dir.clone();
    let debug_single_thread_runtime = args.debug_single_thread_runtime;
    let auto_snapshot_interval = args.auto_snapshot_interval;
    let auto_snapshot_cap = args.auto_snapshot_cap;
    let image_cache_cap_bytes = mib_to_usize_bytes(args.image_cache_cap_mib);
    let glz_dictionary_cap_bytes = mib_to_usize_bytes(args.glz_dictionary_cap_mib);

    if let Some(interval) = auto_snapshot_interval {
        if interval < 10 {
            tracing::warn!(
                "auto-snapshot: --auto-snapshot-interval {} is below the recommended \
                 minimum of 10 s (BugReport::new samples metrics for ~2 s; \
                 shorter intervals cause overlapping samples)",
                interval
            );
        }
    }

    eframe::run_native(
        "Ryll - SPICE Client",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(app::RyllApp::new(
                cc,
                config,
                cadence,
                enable_paste,
                paste_char_delay_ms,
                virtual_disks,
                share_dir,
                capture,
                monitors,
                pedantic_config,
                bug_report_dir,
                obey_guest_size,
                debug_single_thread_runtime,
                auto_snapshot_interval,
                auto_snapshot_cap,
                image_cache_cap_bytes,
                glz_dictionary_cap_bytes,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cancel bridge installed by `run_web` mirrors the inline
    /// pattern in `run_headless`: a 100 ms poll loop that flips a
    /// per-attempt `Arc<AtomicBool>` once the process-global
    /// shutdown flag is set. Verify the bridge stays pending while
    /// the flag is false and propagates within ~500 ms once raised.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_bridge_observes_shutdown_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // Use a private static so the test never touches the
        // process-wide SHUTDOWN_REQUESTED flag.
        static TEST_FLAG: AtomicBool = AtomicBool::new(false);
        TEST_FLAG.store(false, Ordering::SeqCst);

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let handle =
            tokio::spawn(async move { shutdown_to_cancel_bridge(&TEST_FLAG, cancel_clone).await });

        // Bridge should not flip cancel while the flag is false.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "cancel bridge completed before flag was raised"
        );
        assert!(
            !cancel.load(Ordering::Relaxed),
            "cancel was flipped before the flag was raised"
        );

        // Raise the flag; bridge should flip cancel and exit.
        TEST_FLAG.store(true, Ordering::SeqCst);
        let res = tokio::time::timeout(Duration::from_millis(500), handle).await;
        assert!(
            res.is_ok(),
            "cancel bridge did not return within 500 ms after flag was raised"
        );
        assert!(
            cancel.load(Ordering::Relaxed),
            "cancel was not flipped after the flag was raised"
        );

        TEST_FLAG.store(false, Ordering::SeqCst);
    }
}
