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
#[cfg(unix)]
pub mod control;
pub mod device_config;
#[cfg(feature = "digest-decode")]
pub mod digest;
pub mod display;
pub mod encoder;
pub mod event_bus;
pub mod image_cache;
pub mod log_config;
pub mod metrics;
pub mod mm_clock;
pub mod notification;
pub mod notification_sink;
pub mod opcode_counters;
pub mod playback_sink;
pub mod pointer;
pub mod session;
pub mod session_state;
pub mod snapshots;
pub mod surface_mirror;
pub mod traffic;
#[cfg(feature = "usbredir")]
pub mod usb;
#[cfg(feature = "webdav")]
pub mod webdav;

pub use byte_counter::ByteCounter;
pub use capture_sink::CaptureSink;
pub use channels::inputs::make_scancode;
pub use channels::{ChannelEvent, CursorImage, DisplaySize, InputEvent, UsbCommand, WebdavCommand};
pub use clipboard::{ClipboardBackend, MemoryClipboard};
pub use device_config::{ShareDirConfig, VirtualDiskConfig};
pub use display::{DamageRect, DamageRegion, DisplaySurface, MAX_DAMAGE_RECTS};
pub use encoder::{
    even_dimensions, EncodedFrame, EncoderControl, FrameRef, FrameSource, RealFrameSource,
    SyntheticFrameSource,
};
#[cfg(feature = "encoder")]
pub use encoder::{EncoderTask, H264Encoder};
pub use event_bus::{EventBus, EventBusHandles};
pub use image_cache::{BoundedImageCache, InsertOutcome, RefusedReason};
pub use log_config::LogConfig;
pub use mm_clock::MmClock;
pub use notification::{NotificationEntry, NotificationSource};
pub use notification_sink::NotificationSink;
pub use opcode_counters::{OpcodeCounters, OpcodeSnapshotTarget};
pub use playback_sink::{PlaybackFormat, PlaybackOutput, PlaybackSink};
pub use pointer::PointerTranslator;
#[cfg(feature = "digest-decode")]
pub use session::spawn_digest_poller;
pub use session::{run_connection, run_headless};
#[cfg(unix)]
pub use session::{spawn_control_socket, SessionStatus};
pub use session_state::{SessionState, SessionStateRx, MOUSE_MODE_UNKNOWN};
pub use snapshots::{
    ChannelSnapshots, CursorCacheEntry, CursorSnapshot, DecodeResult, DisplaySnapshot,
    InputEventRecord, InputsSnapshot, MainSnapshot, StreamSnapshot,
};
pub use surface_mirror::{DrawOutcome, SurfaceMirror};
// Re-exported so a caller can shut a control socket down without
// taking a direct tokio-util dependency of its own.
#[cfg(unix)]
pub use tokio_util::sync::CancellationToken;
pub use traffic::TrafficSink;
