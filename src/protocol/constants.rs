//! SPICE protocol constants and enums

// Protocol magic and version
pub const SPICE_MAGIC: &[u8; 4] = b"REDQ";
pub const SPICE_VERSION_MAJOR: u32 = 2;
pub const SPICE_VERSION_MINOR: u32 = 2;

/// Channel types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChannelType {
    Main = 1,
    Display = 2,
    Inputs = 3,
    Cursor = 4,
    Playback = 5,
    Record = 6,
    Tunnel = 7,
    Smartcard = 8,
    Usbredir = 9,
    Port = 10,
    Webdav = 11,
}

impl ChannelType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(ChannelType::Main),
            2 => Some(ChannelType::Display),
            3 => Some(ChannelType::Inputs),
            4 => Some(ChannelType::Cursor),
            5 => Some(ChannelType::Playback),
            6 => Some(ChannelType::Record),
            7 => Some(ChannelType::Tunnel),
            8 => Some(ChannelType::Smartcard),
            9 => Some(ChannelType::Usbredir),
            10 => Some(ChannelType::Port),
            11 => Some(ChannelType::Webdav),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ChannelType::Main => "main",
            ChannelType::Display => "display",
            ChannelType::Inputs => "inputs",
            ChannelType::Cursor => "cursor",
            ChannelType::Playback => "playback",
            ChannelType::Record => "record",
            ChannelType::Tunnel => "tunnel",
            ChannelType::Smartcard => "smartcard",
            ChannelType::Usbredir => "usbredir",
            ChannelType::Port => "port",
            ChannelType::Webdav => "webdav",
        }
    }
}

/// Error codes from server
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SpiceError {
    Ok = 0,
    Error = 1,
    InvalidMagic = 2,
    InvalidData = 3,
    VersionMismatch = 4,
    NeedSecured = 5,
    NeedUnsecured = 6,
    PermissionDenied = 7,
    BadConnectionId = 8,
    ChannelUnavailable = 9,
}

impl SpiceError {
    pub fn from_u32(value: u32) -> Self {
        match value {
            0 => SpiceError::Ok,
            1 => SpiceError::Error,
            2 => SpiceError::InvalidMagic,
            3 => SpiceError::InvalidData,
            4 => SpiceError::VersionMismatch,
            5 => SpiceError::NeedSecured,
            6 => SpiceError::NeedUnsecured,
            7 => SpiceError::PermissionDenied,
            8 => SpiceError::BadConnectionId,
            9 => SpiceError::ChannelUnavailable,
            _ => SpiceError::Error,
        }
    }
}

/// Capability flags
#[allow(dead_code)]
pub mod capabilities {
    // Common capabilities
    pub const AUTH_SELECTION: u32 = 1 << 0;
    pub const AUTH_SPICE: u32 = 1 << 1;
    pub const AUTH_SASL: u32 = 1 << 2;
    pub const MINI_HEADER: u32 = 1 << 3;

    // Default common caps: AuthSelection | AuthSpice | MiniHeader
    pub const DEFAULT_COMMON: u32 = AUTH_SELECTION | AUTH_SPICE | MINI_HEADER;

    // Main channel capabilities
    pub const MAIN_SEMI_SEAMLESS_MIGRATE: u32 = 1 << 0;
    pub const MAIN_NAME_AND_UUID: u32 = 1 << 1;
    pub const MAIN_AGENT_CONNECTED_TOKENS: u32 = 1 << 2;
    pub const MAIN_SEAMLESS_MIGRATE: u32 = 1 << 3;

    pub const DEFAULT_MAIN: u32 = MAIN_SEMI_SEAMLESS_MIGRATE | MAIN_SEAMLESS_MIGRATE;

    // Display channel capabilities (SPICE_DISPLAY_CAP_*)
    pub const DISPLAY_SIZED_STREAM: u32 = 1 << 0;
    pub const DISPLAY_MONITORS_CONFIG: u32 = 1 << 1;
    pub const DISPLAY_COMPOSITE: u32 = 1 << 2;
    pub const DISPLAY_A8_SURFACE: u32 = 1 << 3;
    pub const DISPLAY_LZ4_COMPRESSION: u32 = 1 << 5;

    // Advertise the caps that affect how the guest QXL driver
    // renders.  Without COMPOSITE the guest falls back to a
    // software path that produces far fewer display updates.
    pub const DEFAULT_DISPLAY: u32 =
        DISPLAY_SIZED_STREAM | DISPLAY_MONITORS_CONFIG | DISPLAY_COMPOSITE | DISPLAY_A8_SURFACE;

    // SpiceVMC channel capabilities (SPICE_SPICEVMC_CAP_*)
    pub const SPICEVMC_LZ4: u32 = 1 << 0;

    pub const DEFAULT_SPICEVMC: u32 = SPICEVMC_LZ4;
}

/// Authentication mechanism
pub const AUTH_MECHANISM_SPICE: u32 = 1;

/// Main channel message types (server -> client)
pub mod main_server {
    pub const MIGRATE: u16 = 1;
    pub const MIGRATE_DATA: u16 = 2;
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
    pub const WAIT_FOR_CHANNELS: u16 = 5;
    pub const DISCONNECTING: u16 = 6;
    pub const NOTIFY: u16 = 7;
    pub const INIT: u16 = 103;
    pub const CHANNELS_LIST: u16 = 104;
    pub const AGENT_CONNECTED: u16 = 108;
    pub const AGENT_DISCONNECTED: u16 = 109;
    pub const AGENT_DATA: u16 = 110;
    pub const AGENT_TOKEN: u16 = 111;
}

/// Main channel message types (client -> server)
pub mod main_client {
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
    pub const MIGRATE_FLUSH_MARK: u16 = 4;
    pub const MIGRATE_DATA: u16 = 5;
    pub const DISCONNECTING: u16 = 6;
    pub const ATTACH_CHANNELS: u16 = 104;
    pub const AGENT_START: u16 = 106;
    pub const AGENT_DATA: u16 = 107;
    pub const AGENT_TOKEN: u16 = 108;
}

/// Display channel message types (server -> client)
///
/// Values from spice-protocol/spice/enums.h SPICE_MSG_DISPLAY_*
pub mod display_server {
    pub const MODE: u16 = 101;
    pub const MARK: u16 = 102;
    pub const RESET: u16 = 103;
    pub const COPY_BITS: u16 = 104;
    pub const INVALIDATE_LIST: u16 = 105;
    pub const INVAL_ALL_PIXMAPS: u16 = 108;
    pub const STREAM_CREATE: u16 = 122;
    pub const STREAM_DATA: u16 = 123;
    pub const STREAM_CLIP: u16 = 124;
    pub const STREAM_DESTROY: u16 = 125;

    // Draw operations (302+)
    pub const DRAW_FILL: u16 = 302;
    pub const DRAW_OPAQUE: u16 = 303;
    pub const DRAW_COPY: u16 = 304;
    pub const DRAW_BLEND: u16 = 305;
    pub const DRAW_BLACKNESS: u16 = 306;
    pub const DRAW_WHITENESS: u16 = 307;
    pub const DRAW_INVERS: u16 = 308;
    pub const DRAW_ROP3: u16 = 309;
    pub const DRAW_STROKE: u16 = 310;
    pub const DRAW_TEXT: u16 = 311;
    pub const DRAW_TRANSPARENT: u16 = 312;
    pub const DRAW_ALPHA_BLEND: u16 = 313;

    // Surface and extended display ops (314+)
    pub const SURFACE_CREATE: u16 = 314;
    pub const SURFACE_DESTROY: u16 = 315;
    pub const STREAM_DATA_SIZED: u16 = 316;
    pub const MONITORS_CONFIG: u16 = 317;
    pub const DRAW_COMPOSITE: u16 = 318;
    pub const STREAM_ACTIVATE_REPORT: u16 = 319;
    pub const GL_SCANOUT_UNIX: u16 = 320;
    pub const GL_DRAW: u16 = 321;

    // Common
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
}

/// Display channel message types (client -> server)
pub mod display_client {
    pub const INIT: u16 = 101;
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
}

/// Input channel message types (client -> server)
pub mod inputs_client {
    pub const KEY_DOWN: u16 = 101;
    pub const KEY_UP: u16 = 102;
    pub const KEY_MODIFIERS: u16 = 103;
    pub const KEY_SCANCODE: u16 = 104;
    pub const MOUSE_MOTION: u16 = 111;
    pub const MOUSE_POSITION: u16 = 112;
    pub const MOUSE_PRESS: u16 = 113;
    pub const MOUSE_RELEASE: u16 = 114;
}

/// Input channel message types (server -> client)
pub mod inputs_server {
    pub const INIT: u16 = 101;
    pub const KEY_MODIFIERS: u16 = 102;
    pub const MOUSE_MOTION_ACK: u16 = 111;
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
}

/// Cursor channel message types (server -> client)
pub mod cursor_server {
    pub const INIT: u16 = 101;
    pub const RESET: u16 = 102;
    pub const SET: u16 = 103;
    pub const MOVE: u16 = 104;
    pub const HIDE: u16 = 105;
    pub const TRAIL: u16 = 106;
    pub const INVALIDATE_ONE: u16 = 107;
    pub const INVALIDATE_ALL: u16 = 108;
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
}

/// Cursor channel message types (client -> server)
pub mod cursor_client {
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
}

pub mod playback_server {
    pub const DATA: u16 = 101;
    pub const MODE: u16 = 102;
    pub const START: u16 = 103;
    pub const STOP: u16 = 104;
    pub const VOLUME: u16 = 105;
    pub const MUTE: u16 = 106;
    pub const LATENCY: u16 = 107;
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
}

/// SpiceVMC channel message types (server -> client)
///
/// Used by usbredir (type 9), port (type 10), and webdav (type 11) channels.
/// Values from spice-protocol/spice/enums.h SPICE_MSG_SPICEVMC_*
pub mod spicevmc_server {
    pub const DATA: u16 = 101;
    pub const COMPRESSED_DATA: u16 = 102;
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
}

/// SpiceVMC channel message types (client -> server)
pub mod spicevmc_client {
    pub const DATA: u16 = 101;
    pub const COMPRESSED_DATA: u16 = 102;
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
}

/// Image compression types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ImageType {
    Pixmap = 0,
    Quic = 1,
    LzPalette = 100,
    LzRgb = 101,
    GlzRgb = 102,
    FromCache = 103,
    Surface = 104,
    Jpeg = 105,
    FromCacheLossless = 106,
    ZlibGlzRgb = 107,
    JpegAlpha = 108,
    Lz4 = 109,
}

#[allow(dead_code)]
pub const IMAGE_FLAGS_CACHE_ME: u8 = 1 << 0;

impl ImageType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(ImageType::Pixmap),
            1 => Some(ImageType::Quic),
            100 => Some(ImageType::LzPalette),
            101 => Some(ImageType::LzRgb),
            102 => Some(ImageType::GlzRgb),
            103 => Some(ImageType::FromCache),
            104 => Some(ImageType::Surface),
            105 => Some(ImageType::Jpeg),
            106 => Some(ImageType::FromCacheLossless),
            107 => Some(ImageType::ZlibGlzRgb),
            108 => Some(ImageType::JpegAlpha),
            109 => Some(ImageType::Lz4),
            _ => None,
        }
    }
}

/// Keyboard modifier flags
#[allow(dead_code)]
pub mod keyboard_modifiers {
    pub const SCROLL_LOCK: u16 = 1 << 0;
    pub const NUM_LOCK: u16 = 1 << 1;
    pub const CAPS_LOCK: u16 = 1 << 2;
}

/// Mouse button flags
pub mod mouse_buttons {
    pub const LEFT: u32 = 1 << 0;
    pub const MIDDLE: u32 = 1 << 1;
    pub const RIGHT: u32 = 1 << 2;
    pub const UP: u32 = 1 << 3;
    pub const DOWN: u32 = 1 << 4;
}

/// Notify severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NotifySeverity {
    Info = 0,
    Warn = 1,
    Error = 2,
}

impl NotifySeverity {
    pub fn from_u32(value: u32) -> Self {
        match value {
            0 => NotifySeverity::Info,
            1 => NotifySeverity::Warn,
            2 => NotifySeverity::Error,
            _ => NotifySeverity::Info,
        }
    }
}
