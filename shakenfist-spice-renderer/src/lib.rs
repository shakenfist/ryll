//! Shared SPICE rendering substrate for ryll.
//!
//! This crate hosts the protocol-substrate types shared between
//! ryll's frontends (GUI, headless, planned `--web`). The display
//! pixel buffer, channel handlers, and session orchestrator move
//! in later phases; this phase introduces the trait/event
//! indirection that lets channel handlers stop reaching into the
//! ryll-side modules they consume today.

use std::sync::atomic::AtomicBool;

/// Global shutdown flag. Set to `true` by the Ctrl+C handler in the
/// binary; polled by long-running channel loops so they can exit
/// cleanly without cancellation tokens.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

pub mod byte_counter;
pub mod capture_sink;
pub mod channels;
pub mod clipboard;
pub mod device_config;
pub mod display;
pub mod log_config;
pub mod metrics;
pub mod notification;
pub mod snapshots;
pub mod traffic;
pub mod usb;
pub mod webdav;

pub use byte_counter::ByteCounter;
pub use capture_sink::CaptureSink;
pub use channels::{ChannelEvent, CursorImage, InputEvent, UsbCommand, WebdavCommand};
pub use clipboard::ClipboardBackend;
pub use device_config::{ShareDirConfig, VirtualDiskConfig};
pub use display::DisplaySurface;
pub use log_config::LogConfig;
pub use notification::{NotificationEntry, NotificationSource};
pub use snapshots::{
    ChannelSnapshots, CursorCacheEntry, CursorSnapshot, DecodeResult, DisplaySnapshot,
    InputEventRecord, InputsSnapshot, MainSnapshot,
};
pub use traffic::TrafficSink;
