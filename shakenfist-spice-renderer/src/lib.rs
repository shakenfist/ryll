//! Shared SPICE rendering substrate for ryll.
//!
//! This crate hosts the protocol-substrate types shared between
//! ryll's frontends (GUI, headless, planned `--web`). The display
//! pixel buffer, channel handlers, and session orchestrator
//! (`run_connection`, `run_headless`) all live here; the host
//! crate (`ryll`) is the egui frontend that drives them and a
//! few host-policy concerns (Ctrl+C handling, the in-app
//! notification store, pedantic bug-report registration).

pub mod byte_counter;
pub mod capture_sink;
pub mod channels;
pub mod clipboard;
pub mod device_config;
pub mod display;
pub mod encoder;
pub mod log_config;
pub mod metrics;
pub mod notification;
pub mod notification_sink;
pub mod session;
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
pub use encoder::{EncodedFrame, EncoderControl, EncoderTask, FrameRef, FrameSource, H264Encoder};
pub use log_config::LogConfig;
pub use notification::{NotificationEntry, NotificationSource};
pub use notification_sink::NotificationSink;
pub use session::{run_connection, run_headless};
pub use snapshots::{
    ChannelSnapshots, CursorCacheEntry, CursorSnapshot, DecodeResult, DisplaySnapshot,
    InputEventRecord, InputsSnapshot, MainSnapshot,
};
pub use traffic::TrafficSink;
