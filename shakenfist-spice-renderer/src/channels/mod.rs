pub mod cursor;
pub mod display;
pub mod inputs;
pub mod main_channel;
#[cfg(feature = "audio")]
pub mod playback;
pub mod usbredir;
pub mod volume;
pub mod webdav;

pub use cursor::CursorChannel;
pub use display::DisplayChannel;
#[allow(unused_imports)] // PasteKey is part of translate_paste's public return type
pub use inputs::{translate_paste, InputsChannel, PasteError, PasteKey};
pub use main_channel::MainChannel;
#[cfg(feature = "audio")]
pub use playback::PlaybackChannel;
pub use usbredir::UsbredirChannel;
pub use volume::VolumeControl;
pub use webdav::WebdavChannel;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::notification::NotificationEntry;
use crate::usb::UsbDeviceInfo;
use shakenfist_spice_protocol::ChannelType;

/// A caller-chosen correlation token carried in every control-socket
/// request and echoed in the matching response.  Lives here (rather
/// than in `control::protocol`) so that channel events can carry one
/// on every platform — the control module itself is Unix-only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

/// Events sent from channels to the main application
#[derive(Debug, Clone)]
pub enum ChannelEvent {
    /// Session initialized with session ID
    SessionInitialized(u32),

    /// Channel list received
    ChannelsAvailable(Vec<(ChannelType, u8)>),

    /// Surface created
    SurfaceCreated {
        display_channel_id: u8,
        surface_id: u32,
        width: u32,
        height: u32,
    },

    /// Surface destroyed
    SurfaceDestroyed {
        display_channel_id: u8,
        surface_id: u32,
    },

    /// Image data ready to display
    ImageReady {
        display_channel_id: u8,
        surface_id: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>, // RGBA
        #[allow(dead_code)]
        image_id: u64,
        /// Session-relative seconds at the moment the emitting
        /// channel called `event_tx.send`. Used by the app to
        /// compute mpsc-queue lag for renderer-side latency
        /// diagnostics. See PLAN-video-keeping-up-phase-04.
        produced_at_secs: f64,
    },

    /// Image-bearing paint with chroma-keying (DRAW_TRANSPARENT).
    ///
    /// Pixels whose lower-24-bit RGB equals `chroma_rgba[0..3]`
    /// leave the destination untouched.
    ImageReadyChroma {
        display_channel_id: u8,
        surface_id: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>, // RGBA
        chroma_rgba: [u8; 4],
        #[allow(dead_code)]
        image_id: u64,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Image-bearing paint with constant-alpha blending
    /// (DRAW_ALPHA_BLEND).  Straight (non-premultiplied) alpha;
    /// per-pixel source alpha multiplies through the constant.
    ImageReadyAlpha {
        display_channel_id: u8,
        surface_id: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>, // RGBA
        alpha: u8,
        #[allow(dead_code)]
        image_id: u64,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Solid-colour fill of a destination rect.
    ///
    /// Used by DRAW_FILL (with a solid brush), DRAW_BLACKNESS,
    /// and DRAW_WHITENESS. `rect` is `(left, top, right, bottom)`
    /// in surface coordinates; `clip` is the SpiceClip rect list
    /// from the draw message (empty = no extra clipping).
    FillRect {
        display_channel_id: u8,
        surface_id: u32,
        rect: (u32, u32, u32, u32),
        colour: [u8; 4],
        clip: Vec<(u32, u32, u32, u32)>,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Intra-surface pixel copy (DRAW_COPY_BITS).
    CopyBits {
        display_channel_id: u8,
        surface_id: u32,
        src_x: u32,
        src_y: u32,
        dest_rect: (u32, u32, u32, u32),
        clip: Vec<(u32, u32, u32, u32)>,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// In-place RGB inversion of a rect (DRAW_INVERS).
    Invert {
        display_channel_id: u8,
        surface_id: u32,
        rect: (u32, u32, u32, u32),
        clip: Vec<(u32, u32, u32, u32)>,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Display mark (frame boundary)
    DisplayMark {
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Cursor position updated
    CursorPosition {
        x: u16,
        y: u16,
        visible: bool,
    },

    /// Cursor image shape updated
    CursorShape(CursorImage),

    /// Mouse mode from server (1=server, 2=client)
    MouseMode(u32),

    MonitorsConfig {
        width: u32,
        height: u32,
    },

    /// Statistics update (reserved for future use)
    #[allow(dead_code)]
    Statistics {
        channel: String,
        bytes_in: u64,
        bytes_out: u64,
    },

    /// Latency measurement (sample in milliseconds)
    Latency {
        sample_ms: f32,
    },

    /// Paste-as-keystrokes sequence completed.
    ///
    /// `request_id` is `Some` when the paste was initiated via the
    /// control socket (so subscribers can correlate the completion
    /// back to the originating `paste` request).  It is `None` for
    /// pastes initiated by the `--paste-text` CLI flag.
    PasteCompleted {
        chars: usize,
        elapsed_ms: u64,
        request_id: Option<RequestId>,
    },

    /// Paste-as-keystrokes failed (unrepresentable characters).
    ///
    /// `request_id` mirrors the semantics of `PasteCompleted`.
    PasteFailed {
        reason: String,
        request_id: Option<RequestId>,
    },

    /// vdagent connection state changed.
    AgentConnected(bool),

    /// Connection error
    Error {
        channel: ChannelType,
        message: String,
    },

    /// A channel-side notification destined for the host's
    /// notification store. Replaces the old direct
    /// `notifications.lock().push(entry)` calls inside channels
    /// — the host drains the event channel and pushes each
    /// `NotificationEntry` into its store.
    Notification(NotificationEntry),

    /// A USB redirection channel connected successfully
    UsbChannelReady,

    /// A USB device was successfully connected
    UsbDeviceConnected(String),

    /// A USB device was disconnected
    UsbDeviceDisconnected,

    /// A USB device connection attempt failed
    UsbConnectFailed(String),

    /// Available USB devices changed (enumeration result)
    #[allow(dead_code)]
    UsbDevicesChanged(Vec<UsbDeviceInfo>),

    /// A WebDAV channel connected successfully
    WebdavChannelReady,

    /// WebDAV folder sharing started
    WebdavSharingStarted {
        path: String,
        read_only: bool,
    },

    /// WebDAV folder sharing stopped
    WebdavSharingStopped,

    /// A WebDAV error occurred
    #[allow(dead_code)] // used in later phases when WebDAV serving is implemented
    WebdavError(String),

    /// Channel disconnected
    Disconnected(ChannelType),
}

/// Events sent from the application to the inputs channel
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Key pressed
    KeyDown(u32), // Scancode

    /// Key released
    KeyUp(u32), // Scancode

    /// Mouse moved (absolute position, client mode)
    MouseMove { x: u32, y: u32 },

    /// Mouse moved (relative delta, server mode)
    MouseMotion { dx: i32, dy: i32 },

    /// Mouse button pressed
    MouseDown { button: u32, x: u32, y: u32 },

    /// Mouse button released
    MouseUp { button: u32, x: u32, y: u32 },

    /// Paste a string as synthetic keystrokes (US-QWERTY).
    ///
    /// `request_id` and `cancel` are `Some` when the paste was
    /// initiated via the control socket.  The `cancel` token is
    /// polled between characters so a client disconnect can abort
    /// the in-progress paste without leaving synthetic key events
    /// running.  Both are `None` for the `--paste-text` CLI path.
    PasteText {
        text: String,
        char_delay_ms: u32,
        /// Correlation token echoed in the resulting
        /// `PasteCompleted` / `PasteFailed` channel event.
        request_id: Option<RequestId>,
        /// Optional cancellation token.  When `Some`, the paste
        /// state machine checks `cancel.is_cancelled()` before
        /// advancing each sub-step and aborts with a
        /// `PasteFailed` event if it fires.
        cancel: Option<tokio_util::sync::CancellationToken>,
    },
}

/// Commands sent from the app to the webdav channel.
#[allow(dead_code)] // variants constructed in phase 5 (UI panel)
pub enum WebdavCommand {
    /// Start sharing a local directory.
    ShareDirectory { path: PathBuf, read_only: bool },
    /// Stop sharing the current directory.
    StopSharing,
}

/// Commands sent from the app to the usbredir channel.
pub enum UsbCommand {
    /// Connect a physical USB device by bus/address (Linux only).
    #[cfg(target_os = "linux")]
    ConnectPhysical { bus: u8, address: u8 },
    /// Connect a virtual mass storage disk image.
    ConnectVirtualDisk { path: PathBuf, read_only: bool },
    /// Disconnect the currently connected device.
    DisconnectDevice,
}

/// Decoded cursor image in RGBA format
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields used in phase 2 (cursor overlay rendering)
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
    pub pixels: Vec<u8>, // RGBA
}
