mod app;
mod bugreport;
#[cfg(feature = "capture")]
mod capture;
mod metrics;
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
}
mod channels;
mod config;
mod display;
mod settings;
mod usb;
mod webdav;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use eframe::egui;
use tracing::{info, Level};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::bugreport::{AppSnapshot, ChannelSnapshots, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::config::{
    parse_share_dir, parse_virtual_disks, Args, Config, ShareDirConfig, VirtualDiskConfig,
};

/// Flag set by the Ctrl+C handler to request graceful shutdown.
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

    // Register the --pedantic gap observer before the session starts so no
    // early-boot gaps are missed (register_gap_observer replays existing keys).
    //
    // KNOWN LIMITATION: the traffic / channel-snapshot handles here are
    // fresh stubs, not the live handles that channel tasks write to inside
    // app::RyllApp::new / run_headless. Pedantic zips therefore contain:
    //   * gap_key in metadata.json and in the filename           [present]
    //   * session metadata, target host/port, runtime_metrics    [present]
    //   * traffic pcap                                           [empty]
    //   * channel-state snapshots                                [empty]
    // The gap key is usually enough to act on a report; traffic context is
    // secondary. Future work (noted in master plan's "Future work" list)
    // moves the observer registration into the app constructors where live
    // handles exist and relies on register_gap_observer's replay semantics
    // to catch any gaps fired during the construction window.
    if args.pedantic {
        std::fs::create_dir_all(&args.pedantic_dir).with_context(|| {
            format!(
                "failed to create pedantic directory {}",
                args.pedantic_dir.display()
            )
        })?;

        const PEDANTIC_REPORT_CAP: usize = 50;

        let dir = Arc::new(args.pedantic_dir.clone());
        let target_host = Arc::new(config.host.clone());
        let target_port = config.port;
        let traffic = Arc::new(TrafficBuffers::new());
        // Arc-wrap ChannelSnapshots so the closure can clone the Arc cheaply
        // rather than relying on ChannelSnapshots itself implementing Clone.
        let channel_snapshots = Arc::new(ChannelSnapshots::new());
        let app_snapshot: Arc<Mutex<AppSnapshot>> = Arc::new(Mutex::new(AppSnapshot::default()));
        let counter = Arc::new(AtomicUsize::new(0));

        shakenfist_spice_protocol::logging::register_gap_observer(Arc::new(
            move |key: &'static str| {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n >= PEDANTIC_REPORT_CAP {
                    return;
                }
                let dir = dir.clone();
                let target_host = target_host.clone();
                let traffic = traffic.clone();
                let channel_snapshots = channel_snapshots.clone();
                let app_snapshot = app_snapshot.clone();
                let key_str = key.to_string();
                tokio::spawn(async move {
                    // metrics::sample blocks for its sample window; run it on a
                    // dedicated thread so the tokio executor is not stalled.
                    let metrics = tokio::task::spawn_blocking(|| {
                        crate::metrics::sample(std::time::Duration::from_secs(1))
                    })
                    .await
                    .unwrap_or_else(|_| {
                        crate::metrics::RuntimeMetrics::unavailable(
                            "spawn_blocking panicked during metrics sample",
                        )
                    });
                    match crate::bugreport::BugReport::write_pedantic(
                        &dir,
                        &key_str,
                        &target_host,
                        target_port,
                        &traffic,
                        &channel_snapshots,
                        &app_snapshot,
                        metrics,
                    ) {
                        Ok(path) => tracing::info!("pedantic: wrote {}", path.display()),
                        Err(e) => tracing::warn!("pedantic: write failed for {}: {}", key_str, e),
                    }
                });
            },
        ));

        tracing::info!(
            "pedantic mode enabled; reports will land in {}",
            args.pedantic_dir.display()
        );
    }

    if args.headless {
        run_headless(config, &args, virtual_disks, share_dir, capture)
    } else {
        run_gui(config, &args, virtual_disks, share_dir, capture)
    }
}

fn run_headless(
    config: Config,
    args: &Args,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
) -> Result<()> {
    info!("Running in headless mode");

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        app::run_headless(
            config,
            args.cadence,
            virtual_disks,
            share_dir,
            capture,
            args.monitors,
        )
        .await
    })
}

fn run_gui(
    config: Config,
    args: &Args,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
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
    eframe::run_native(
        "Ryll - SPICE Client",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(app::RyllApp::new(
                cc,
                config,
                cadence,
                virtual_disks,
                share_dir,
                capture,
                monitors,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
