mod app;
mod bugreport;
#[cfg(feature = "capture")]
mod capture;
mod metrics;
mod notifications;
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use eframe::egui;
use tracing::{info, Level};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::bugreport::PedanticConfig;
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

    if args.headless {
        run_headless(
            config,
            &args,
            virtual_disks,
            share_dir,
            capture,
            pedantic_config,
        )
    } else {
        run_gui(
            config,
            &args,
            virtual_disks,
            share_dir,
            capture,
            pedantic_config,
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
) -> Result<()> {
    info!("Running in headless mode");

    let enable_paste = args.enable_paste_as_keystrokes || args.paste_text.is_some();
    let paste_text = args.paste_text.clone();
    let paste_char_delay_ms = args.paste_char_delay_ms;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        app::run_headless(
            config,
            args.cadence,
            paste_text,
            paste_char_delay_ms,
            enable_paste,
            virtual_disks,
            share_dir,
            capture,
            args.monitors,
            pedantic_config,
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
    pedantic_config: Option<PedanticConfig>,
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
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
