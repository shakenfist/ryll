pub mod cursor;
pub mod display;
pub mod inputs;
pub mod main_channel;
pub mod usbredir;
pub mod webdav;

pub use cursor::CursorChannel;
pub use display::DisplayChannel;
pub use inputs::InputsChannel;
pub use main_channel::MainChannel;
pub use usbredir::UsbredirChannel;
pub use webdav::WebdavChannel;

use std::path::PathBuf;

use crate::protocol::ChannelType;
use crate::usb::UsbDeviceInfo;

/// Events sent from channels to the main application
#[derive(Debug, Clone)]
pub enum ChannelEvent {
    /// Session initialized with session ID
    SessionInitialized(u32),

    /// Channel list received
    ChannelsAvailable(Vec<(ChannelType, u8)>),

    /// Surface created
    SurfaceCreated {
        surface_id: u32,
        width: u32,
        height: u32,
    },

    /// Surface destroyed
    SurfaceDestroyed { surface_id: u32 },

    /// Image data ready to display
    ImageReady {
        surface_id: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>, // RGBA
        #[allow(dead_code)]
        image_id: u64,
    },

    /// Display mark (frame boundary)
    DisplayMark,

    /// Cursor position updated
    CursorPosition { x: u16, y: u16, visible: bool },

    /// Cursor image shape updated
    CursorShape(CursorImage),

    /// Mouse mode from server (1=server, 2=client)
    MouseMode(u32),

    /// Statistics update (reserved for future use)
    #[allow(dead_code)]
    Statistics {
        channel: String,
        bytes_in: u64,
        bytes_out: u64,
    },

    /// Latency measurement
    Latency { key_timestamp: f64 },

    /// Connection error
    Error(String),

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
    WebdavSharingStarted { path: String, read_only: bool },

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

    /// Mouse moved
    MouseMove { x: u32, y: u32 },

    /// Mouse button pressed
    MouseDown { button: u32, x: u32, y: u32 },

    /// Mouse button released
    MouseUp { button: u32, x: u32, y: u32 },
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
