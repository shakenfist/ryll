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
use tracing_subscriber::FmtSubscriber;

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

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

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
