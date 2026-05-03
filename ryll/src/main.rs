mod app;
mod bugreport;
#[cfg(feature = "capture")]
mod capture;
mod notifications;
// The web module is scaffolded in step 4a and wired into
// main in step 4e. Until then, the exported symbols are
// not yet called from main.
#[allow(dead_code)]
mod web;
#[cfg(not(feature = "capture"))]
mod capture {
    /// Stub CaptureSession when capture feature is disabled.
    /// Methods are never called (capture is always None), but
    /// the compiler needs to see them for type-checking.
    pub struct CaptureSession {
        pub dir: std::path::PathBuf,
    }
    impl CaptureSession {
        pub fn packet_sent(&self, _channel: &str, _data: &[u8]) {}
        pub fn packet_received(&self, _channel: &str, _data: &[u8]) {}
        pub fn frame(&self, _id: u32, _px: &[u8], _w: u32, _h: u32) {}
        pub fn close(&self) {}
    }
    impl shakenfist_spice_renderer::CaptureSink for CaptureSession {
        fn packet_sent(&self, channel: &str, data: &[u8]) {
            CaptureSession::packet_sent(self, channel, data);
        }
        fn packet_received(&self, channel: &str, data: &[u8]) {
            CaptureSession::packet_received(self, channel, data);
        }
        fn frame(&self, id: u32, px: &[u8], w: u32, h: u32) {
            CaptureSession::frame(self, id, px, w, h);
        }
    }
}
mod clipboard_arboard;
mod config;
mod display_gui;
mod input_egui;
mod settings;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
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

fn main() -> Result<()> {
    // Install Ctrl+C handler so graceful shutdown works on all platforms.
    ctrlc::set_handler(|| {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    })
    .expect("failed to set Ctrl+C handler");

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
    if args.verbose {
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

    // Load configuration
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

    // Eager-failure `mkdir -p` for the --pedantic output directory so the
    // user hears about disk/permission problems before the session starts.
    // The actual gap-observer registration happens inside the app
    // constructors (app::RyllApp::new / app::run_headless) once the live
    // traffic / channel-snapshot handles have been built.
    let pedantic_config = if args.pedantic {
        std::fs::create_dir_all(&args.pedantic_dir).with_context(|| {
            format!(
                "failed to create pedantic directory {}",
                args.pedantic_dir.display()
            )
        })?;
        Some(PedanticConfig {
            dir: args.pedantic_dir.clone(),
        })
    } else {
        None
    };

    let obey_guest_size = !args.no_obey_guest_size;

    if args.headless {
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
                obey_guest_size,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
