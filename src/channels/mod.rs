pub mod cursor;
pub mod display;
pub mod inputs;
pub mod main_channel;

pub use cursor::CursorChannel;
pub use display::DisplayChannel;
pub use inputs::InputsChannel;
pub use main_channel::MainChannel;

use crate::protocol::ChannelType;

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
