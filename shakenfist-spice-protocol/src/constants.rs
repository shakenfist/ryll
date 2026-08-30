//! SPICE protocol constants and enums

use serde::{Deserialize, Serialize};

// Protocol magic and version
pub const SPICE_MAGIC: &[u8; 4] = b"REDQ";
pub const SPICE_VERSION_MAJOR: u32 = 2;
pub const SPICE_VERSION_MINOR: u32 = 2;

/// Channel types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// Wire value for this error code, the inverse of
    /// [`from_u32`](Self::from_u32). Needed by the server role to
    /// serialise a `SpiceLinkReply` error field.
    pub fn to_u32(self) -> u32 {
        self as u32
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
    // Bit 4: enables server to send STREAM_ACTIVATE_REPORT; we reply
    // with STREAM_REPORT (opcode 102).  spice-gtk advertises this
    // unconditionally (channel-display.c:976).
    pub const DISPLAY_STREAM_REPORT: u32 = 1 << 4;
    pub const DISPLAY_LZ4_COMPRESSION: u32 = 1 << 5;
    /// Client requests a specific image-compression scheme via
    /// `SPICE_MSGC_DISPLAY_PREFERRED_COMPRESSION` at link-up
    /// (SPICE_DISPLAY_CAP_PREF_COMPRESSION = bit 6).
    pub const DISPLAY_PREF_COMPRESSION: u32 = 1 << 6;
    /// Client supports multiple codecs in a single session;
    /// required alongside DISPLAY_CODEC_* to enable codec
    /// negotiation (SPICE_DISPLAY_CAP_MULTI_CODEC).
    pub const DISPLAY_MULTI_CODEC: u32 = 1 << 8;
    /// Client can decode MJPEG video streams
    /// (SPICE_DISPLAY_CAP_CODEC_MJPEG).
    pub const DISPLAY_CODEC_MJPEG: u32 = 1 << 9;
    /// Client can decode H.264 video streams via openh264
    /// (SPICE_DISPLAY_CAP_CODEC_H264).
    pub const DISPLAY_CODEC_H264: u32 = 1 << 11;
    /// Client sends a preferred video-codec list via
    /// `SPICE_MSGC_DISPLAY_PREFERRED_VIDEO_CODEC_TYPE` at link-up
    /// (SPICE_DISPLAY_CAP_PREF_VIDEO_CODEC_TYPE = bit 12).
    pub const DISPLAY_PREF_VIDEO_CODEC_TYPE: u32 = 1 << 12;

    // Advertise the caps that affect how the guest QXL driver
    // renders.  Without COMPOSITE the guest falls back to a
    // software path that produces far fewer display updates.
    // LZ4_COMPRESSION allows the server to choose LZ4 over Zlib
    // for static-UI regions, improving bandwidth efficiency.
    // MULTI_CODEC + CODEC_MJPEG + CODEC_H264 tell the server it
    // may use H.264 for video streams (smaller on the wire than
    // MJPEG for sustained playback). PREF_COMPRESSION and
    // PREF_VIDEO_CODEC_TYPE let us actively steer the server's
    // choice via the matching MSGC opcodes at link-up.
    pub const DEFAULT_DISPLAY: u32 = DISPLAY_SIZED_STREAM
        | DISPLAY_MONITORS_CONFIG
        | DISPLAY_COMPOSITE
        | DISPLAY_A8_SURFACE
        | DISPLAY_STREAM_REPORT
        | DISPLAY_LZ4_COMPRESSION
        | DISPLAY_PREF_COMPRESSION
        | DISPLAY_MULTI_CODEC
        | DISPLAY_CODEC_MJPEG
        | DISPLAY_CODEC_H264
        | DISPLAY_PREF_VIDEO_CODEC_TYPE;

    /// Single source of truth mapping each known display-channel
    /// capability bit *mask* to its symbolic name, for the traffic
    /// viewer (see `logging::message_names::display_cap_name`).
    ///
    /// This table, not a second hand-written match arm elsewhere, is
    /// what `display_cap_name` consumes. Entries reference the named
    /// `DISPLAY_*` constants above rather than repeating bit numbers,
    /// so a bit position can never drift out of sync between the two.
    /// A capability added above but forgotten here is caught by
    /// `logging::tests::display_cap_name_covers_default_display`,
    /// which fails if any bit set in `DEFAULT_DISPLAY` has no entry.
    pub const DISPLAY_CAP_NAMES: &[(u32, &str)] = &[
        (DISPLAY_SIZED_STREAM, "sized_stream"),
        (DISPLAY_MONITORS_CONFIG, "monitors_config"),
        (DISPLAY_COMPOSITE, "composite"),
        (DISPLAY_A8_SURFACE, "a8_surface"),
        (DISPLAY_STREAM_REPORT, "stream_report"),
        (DISPLAY_LZ4_COMPRESSION, "lz4_compression"),
        (DISPLAY_PREF_COMPRESSION, "pref_compression"),
        (DISPLAY_MULTI_CODEC, "multi_codec"),
        (DISPLAY_CODEC_MJPEG, "codec_mjpeg"),
        (DISPLAY_CODEC_H264, "codec_h264"),
        (DISPLAY_PREF_VIDEO_CODEC_TYPE, "pref_video_codec_type"),
    ];

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
    // Channel-specific opcodes from enums.h SPICE_MSG_MAIN_* (101+).
    pub const MIGRATE_BEGIN: u16 = 101;
    pub const MIGRATE_CANCEL: u16 = 102;
    pub const INIT: u16 = 103;
    pub const CHANNELS_LIST: u16 = 104;
    pub const MOUSE_MODE: u16 = 105;
    pub const MULTI_MEDIA_TIME: u16 = 106;
    pub const AGENT_CONNECTED: u16 = 107;
    pub const AGENT_DISCONNECTED: u16 = 108;
    pub const AGENT_DATA: u16 = 109;
    pub const AGENT_TOKEN: u16 = 110;
    pub const MIGRATE_SWITCH_HOST: u16 = 111;
    pub const MIGRATE_END: u16 = 112;
    pub const NAME: u16 = 113;
    pub const UUID: u16 = 114;
    pub const AGENT_CONNECTED_TOKENS: u16 = 115;
    pub const MIGRATE_BEGIN_SEAMLESS: u16 = 116;
    pub const MIGRATE_DST_SEAMLESS_ACK: u16 = 117;
    pub const MIGRATE_DST_SEAMLESS_NACK: u16 = 118;
}

/// Main channel message types (client -> server)
pub mod main_client {
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
    pub const MIGRATE_FLUSH_MARK: u16 = 4;
    pub const MIGRATE_DATA: u16 = 5;
    pub const DISCONNECTING: u16 = 6;
    // Channel-specific opcodes from enums.h SPICE_MSGC_MAIN_* (101+).
    pub const CLIENT_INFO: u16 = 101;
    pub const MIGRATE_CONNECTED: u16 = 102;
    pub const MIGRATE_CONNECT_ERROR: u16 = 103;
    pub const ATTACH_CHANNELS: u16 = 104;
    pub const MOUSE_MODE_REQUEST: u16 = 105;
    pub const AGENT_START: u16 = 106;
    pub const AGENT_DATA: u16 = 107;
    pub const AGENT_TOKEN: u16 = 108;
    pub const MIGRATE_END: u16 = 109;
    pub const MIGRATE_DST_DO_SEAMLESS: u16 = 110;
    pub const MIGRATE_CONNECTED_SEAMLESS: u16 = 111;
    pub const QUALITY_INDICATOR: u16 = 112;
}

/// Mouse mode constants
pub const MOUSE_MODE_SERVER: u32 = 1;
pub const MOUSE_MODE_CLIENT: u32 = 2;

/// Display channel message types (server -> client)
///
/// Values from spice-protocol/spice/enums.h SPICE_MSG_DISPLAY_*
pub mod display_server {
    pub const MODE: u16 = 101;
    pub const MARK: u16 = 102;
    pub const RESET: u16 = 103;
    pub const COPY_BITS: u16 = 104;
    pub const INVALIDATE_LIST: u16 = 105;
    pub const INVAL_ALL_PIXMAPS: u16 = 106;
    pub const INVAL_PALETTE: u16 = 107;
    pub const INVAL_ALL_PALETTES: u16 = 108;
    pub const STREAM_CREATE: u16 = 122;
    pub const STREAM_DATA: u16 = 123;
    pub const STREAM_CLIP: u16 = 124;
    pub const STREAM_DESTROY: u16 = 125;
    pub const STREAM_DESTROY_ALL: u16 = 126;

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
    pub const QUALITY_INDICATOR: u16 = 322;
    pub const GL_SCANOUT2_UNIX: u16 = 323;

    // Common
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
    pub const NOTIFY: u16 = 7;
}

/// Display channel message types (client -> server)
pub mod display_client {
    pub const INIT: u16 = 101;
    pub const STREAM_REPORT: u16 = 102;
    /// SPICE_MSGC_DISPLAY_PREFERRED_COMPRESSION (enums.h: INIT=101
    /// then STREAM_REPORT, PREFERRED_COMPRESSION, GL_DRAW_DONE,
    /// PREFERRED_VIDEO_CODEC_TYPE). Payload is a single
    /// `image_compression` u8.
    pub const PREFERRED_COMPRESSION: u16 = 103;
    /// SPICE_MSGC_DISPLAY_GL_DRAW_DONE. Not sent by ryll today
    /// (we don't advertise GL_SCANOUT); listed for opcode
    /// completeness so the gap to 105 is visible.
    pub const GL_DRAW_DONE: u16 = 104;
    /// SPICE_MSGC_DISPLAY_PREFERRED_VIDEO_CODEC_TYPE. Payload is
    /// a u8 length followed by that many `video_codec_type` u8s
    /// in preference order (spice.proto:1035-1037).
    pub const PREFERRED_VIDEO_CODEC_TYPE: u16 = 105;
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
}

/// SPICE image-compression enum values
/// (`SPICE_IMAGE_COMPRESSION_*` in enums.h). Used as the payload
/// of `SPICE_MSGC_DISPLAY_PREFERRED_COMPRESSION`.
pub mod image_compression {
    pub const INVALID: u8 = 0;
    pub const OFF: u8 = 1;
    pub const AUTO_GLZ: u8 = 2;
    pub const AUTO_LZ: u8 = 3;
    pub const QUIC: u8 = 4;
    pub const GLZ: u8 = 5;
    pub const LZ: u8 = 6;
    pub const LZ4: u8 = 7;
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
    pub const NOTIFY: u16 = 7;
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
    pub const NOTIFY: u16 = 7;
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
    pub const NOTIFY: u16 = 7;
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
    pub const NOTIFY: u16 = 7;
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

/// Raster-operation descriptors (SPICE_ROPD_* in enums.h).
///
/// Draw ops carry a u16 bitfield describing how the source,
/// brush, and destination combine. Modern QXL almost always
/// emits OP_PUT (overwrite destination).
pub mod ropd {
    pub const INVERS_SRC: u16 = 1 << 0;
    pub const INVERS_BRUSH: u16 = 1 << 1;
    pub const INVERS_DEST: u16 = 1 << 2;
    pub const OP_PUT: u16 = 1 << 3;
    pub const OP_OR: u16 = 1 << 4;
    pub const OP_AND: u16 = 1 << 5;
    pub const OP_XOR: u16 = 1 << 6;
    pub const OP_BLACKNESS: u16 = 1 << 7;
    pub const OP_WHITENESS: u16 = 1 << 8;
    pub const OP_INVERS: u16 = 1 << 9;
    pub const INVERS_RES: u16 = 1 << 10;
}

/// Brush-type tag (SPICE_BRUSH_TYPE_* in enums.h).
pub mod brush {
    pub const NONE: u8 = 0;
    pub const SOLID: u8 = 1;
    pub const PATTERN: u8 = 2;
}

/// Notify severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

/// SPICE notify-visibility levels
/// (`SPICE_NOTIFY_VISIBILITY_LOW/MEDIUM/HIGH` in protocol.h).
/// The wire format encodes this as a u32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
pub enum SpiceVisibility {
    Low = 0,
    Medium = 1,
    High = 2,
}

impl SpiceVisibility {
    /// Return the variant for a wire value, or `None` for any
    /// value outside 0–2.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(SpiceVisibility::Low),
            1 => Some(SpiceVisibility::Medium),
            2 => Some(SpiceVisibility::High),
            _ => None,
        }
    }

    /// Human-readable lowercase name used in log output and
    /// bug-report JSON.
    pub fn name(&self) -> &'static str {
        match self {
            SpiceVisibility::Low => "low",
            SpiceVisibility::Medium => "medium",
            SpiceVisibility::High => "high",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spice_error_to_u32_round_trips() {
        // Every defined code must survive to_u32 -> from_u32. The
        // unmapped-value fallback of from_u32 (-> Error) is deliberately
        // not part of this round-trip contract.
        for err in [
            SpiceError::Ok,
            SpiceError::Error,
            SpiceError::InvalidMagic,
            SpiceError::InvalidData,
            SpiceError::VersionMismatch,
            SpiceError::NeedSecured,
            SpiceError::NeedUnsecured,
            SpiceError::PermissionDenied,
            SpiceError::BadConnectionId,
            SpiceError::ChannelUnavailable,
        ] {
            assert_eq!(SpiceError::from_u32(err.to_u32()), err);
        }
    }

    #[test]
    fn spice_visibility_from_u32_round_trips() {
        assert_eq!(SpiceVisibility::from_u32(0), Some(SpiceVisibility::Low));
        assert_eq!(SpiceVisibility::from_u32(1), Some(SpiceVisibility::Medium));
        assert_eq!(SpiceVisibility::from_u32(2), Some(SpiceVisibility::High));
        assert_eq!(SpiceVisibility::from_u32(99), None);
    }

    #[test]
    fn display_stream_destroy_all_opcode_pinned() {
        // The opcode value is defined in spice-protocol/spice/enums.h:497
        // and any drift would silently break ryll's handler. The opcode
        // is also exercised end-to-end via display.rs's match arm — this
        // assertion is just the constant-value backstop.
        assert_eq!(display_server::STREAM_DESTROY_ALL, 126);
    }

    #[test]
    fn display_pref_message_opcodes_pinned() {
        // PREFERRED_COMPRESSION is the third entry in the
        // SPICE_MSGC_DISPLAY_* enum starting at INIT=101;
        // PREFERRED_VIDEO_CODEC_TYPE is the fifth. Counted from
        // spice-protocol/spice/enums.h:523-530: 101=INIT, 102=STREAM_REPORT,
        // 103=PREFERRED_COMPRESSION, 104=GL_DRAW_DONE,
        // 105=PREFERRED_VIDEO_CODEC_TYPE. Any drift here silently breaks
        // server-side preference handling — the server would reject the
        // message and we would log nothing client-side.
        assert_eq!(display_client::PREFERRED_COMPRESSION, 103);
        assert_eq!(display_client::GL_DRAW_DONE, 104);
        assert_eq!(display_client::PREFERRED_VIDEO_CODEC_TYPE, 105);
    }

    #[test]
    fn display_pref_capabilities_pinned() {
        // SPICE_DISPLAY_CAP_PREF_COMPRESSION is bit 6 and
        // SPICE_DISPLAY_CAP_PREF_VIDEO_CODEC_TYPE is bit 12 in
        // spice-protocol/spice/protocol.h:137-153 (zero-indexed list:
        // SIZED_STREAM=0 ... PREF_COMPRESSION=6 ... PREF_VIDEO_CODEC_TYPE=12).
        // Drift here would cause the server to silently ignore our preference
        // messages because the cap negotiation would have rejected them.
        assert_eq!(capabilities::DISPLAY_PREF_COMPRESSION, 1 << 6);
        assert_eq!(capabilities::DISPLAY_PREF_VIDEO_CODEC_TYPE, 1 << 12);
        assert_eq!(
            capabilities::DEFAULT_DISPLAY & capabilities::DISPLAY_PREF_COMPRESSION,
            capabilities::DISPLAY_PREF_COMPRESSION,
        );
        assert_eq!(
            capabilities::DEFAULT_DISPLAY & capabilities::DISPLAY_PREF_VIDEO_CODEC_TYPE,
            capabilities::DISPLAY_PREF_VIDEO_CODEC_TYPE,
        );
    }

    #[test]
    fn image_compression_auto_glz_value_pinned() {
        // AUTO_GLZ is the value the link-up PREFERRED_COMPRESSION message
        // sends (changed from AUTO_LZ after session 006 showed AUTO_LZ
        // disabled server-side GLZ entirely, costing 25% bandwidth on a
        // UI-heavy workload). If this drifts we'd be asking the server for
        // the wrong scheme without any wire-level error. Value from
        // spice-protocol/spice/enums.h:196-203 (INVALID=0, OFF=1,
        // AUTO_GLZ=2, AUTO_LZ=3, QUIC=4, GLZ=5, LZ=6, LZ4=7).
        assert_eq!(image_compression::AUTO_GLZ, 2);
    }
}
