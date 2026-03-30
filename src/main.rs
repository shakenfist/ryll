mod app;
mod channels;
mod config;
mod decompression;
mod display;
mod protocol;
mod settings;

use anyhow::Result;
use clap::Parser;
use eframe::egui;
use tracing::{info, Level};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::config::{Args, Config};

fn main() -> Result<()> {
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

    if args.headless {
        run_headless(config, &args)
    } else {
        run_gui(config, &args)
    }
}

fn run_headless(config: Config, args: &Args) -> Result<()> {
    info!("Running in headless mode");

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async { app::run_headless(config, args.cadence).await })
}

fn run_gui(config: Config, args: &Args) -> Result<()> {
    info!("Starting GUI");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_resizable(true),
        ..Default::default()
    };

    let cadence = args.cadence;
    eframe::run_native(
        "Ryll - SPICE Client",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::RyllApp::new(cc, config, cadence)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
