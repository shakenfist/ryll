/// Protocol traffic logging
///
/// Provides detailed logging of SPICE protocol messages for debugging
/// and protocol coverage testing.
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Get current timestamp as float (Unix epoch seconds)
pub fn timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Format a timestamp for logging
pub fn format_timestamp() -> String {
    format!("{:.6}", timestamp())
}

/// Log a protocol message with timestamp
pub fn log_message(direction: &str, channel: &str, msg_type: u16, msg_type_str: &str, size: u32) {
    info!(
        "[{}] {} {} {} byte opcode {} {}",
        format_timestamp(),
        channel,
        direction,
        size,
        msg_type,
        msg_type_str
    );
}

/// Log message details (indented continuation)
pub fn log_detail(detail: &str) {
    info!("   ... {}", detail);
}

/// Log an unknown/undecoded message type
pub fn log_unknown(channel: &str, direction: &str, msg_type: u16, size: u32, data: &[u8]) {
    warn!(
        "[{}] {} {} {} byte UNKNOWN opcode {}",
        format_timestamp(),
        channel,
        direction,
        size,
        msg_type
    );
    hex_dump(data, 64);
}

/// Log incomplete message (waiting for more data)
#[allow(dead_code)]
pub fn log_incomplete(channel: &str, msg_type_str: &str, have: usize, want: usize) {
    debug!(
        "[{}] {} message {} incomplete: have {} bytes, want {}",
        format_timestamp(),
        channel,
        msg_type_str,
        have,
        want
    );
}

/// Hex dump of data for debugging unknown messages
pub fn hex_dump(data: &[u8], max_bytes: usize) {
    let dump_len = data.len().min(max_bytes);
    let mut offset = 0;

    while offset < dump_len {
        let chunk_end = (offset + 16).min(dump_len);
        let chunk = &data[offset..chunk_end];

        // Build printable, decimal, and hex representations
        let mut printable = String::with_capacity(16);
        let mut hex = String::with_capacity(48);

        for &byte in chunk {
            // Printable ASCII or dot
            if (0x20..0x7f).contains(&byte) {
                printable.push(byte as char);
            } else {
                printable.push('.');
            }

            // Hex representation
            hex.push_str(&format!("{:02x} ", byte));
        }

        // Pad printable to 16 chars for alignment
        while printable.len() < 16 {
            printable.push(' ');
        }

        debug!("   {:04x}: {}  {}", offset, printable, hex.trim_end());

        offset += 16;
    }

    if data.len() > max_bytes {
        debug!("   ... ({} more bytes)", data.len() - max_bytes);
    }
}

/// Message type lookup helpers for logging
pub mod message_names {
    use super::super::constants::*;

    /// Get main channel server message name
    pub fn main_server(msg_type: u16) -> &'static str {
        match msg_type {
            main_server::MIGRATE => "migrate",
            main_server::MIGRATE_DATA => "migrate_data",
            main_server::SET_ACK => "set_ack",
            main_server::PING => "ping",
            main_server::WAIT_FOR_CHANNELS => "wait_for_channels",
            main_server::DISCONNECTING => "disconnecting",
            main_server::NOTIFY => "notify",
            main_server::INIT => "init",
            main_server::CHANNELS_LIST => "channels_list",
            main_server::AGENT_CONNECTED => "agent_connected",
            main_server::AGENT_DISCONNECTED => "agent_disconnected",
            main_server::AGENT_DATA => "agent_data",
            main_server::AGENT_TOKEN => "agent_token",
            _ => "unknown",
        }
    }

    /// Get main channel client message name
    pub fn main_client(msg_type: u16) -> &'static str {
        match msg_type {
            main_client::ACK_SYNC => "ack_sync",
            main_client::ACK => "ack",
            main_client::PONG => "pong",
            main_client::MIGRATE_FLUSH_MARK => "migrate_flush_mark",
            main_client::MIGRATE_DATA => "migrate_data",
            main_client::DISCONNECTING => "disconnecting",
            main_client::ATTACH_CHANNELS => "attach_channels",
            main_client::AGENT_START => "agent_start",
            main_client::AGENT_DATA => "agent_data",
            main_client::AGENT_TOKEN => "agent_token",
            _ => "unknown",
        }
    }

    /// Get display channel server message name
    pub fn display_server(msg_type: u16) -> &'static str {
        match msg_type {
            display_server::MODE => "mode",
            display_server::MARK => "mark",
            display_server::RESET => "reset",
            display_server::COPY_BITS => "copy_bits",
            display_server::INVALIDATE_LIST => "invalidate_list",
            display_server::INVAL_ALL_PIXMAPS => "inval_all_pixmaps",
            display_server::STREAM_CREATE => "stream_create",
            display_server::STREAM_DATA => "stream_data",
            display_server::STREAM_CLIP => "stream_clip",
            display_server::STREAM_DESTROY => "stream_destroy",
            display_server::STREAM_DATA_SIZED => "stream_data_sized",
            display_server::DRAW_FILL => "draw_fill",
            display_server::DRAW_OPAQUE => "draw_opaque",
            display_server::DRAW_COPY => "draw_copy",
            display_server::DRAW_BLEND => "draw_blend",
            display_server::DRAW_BLACKNESS => "draw_blackness",
            display_server::DRAW_WHITENESS => "draw_whiteness",
            display_server::DRAW_INVERS => "draw_invers",
            display_server::DRAW_ROP3 => "draw_rop3",
            display_server::DRAW_STROKE => "draw_stroke",
            display_server::DRAW_TEXT => "draw_text",
            display_server::DRAW_TRANSPARENT => "draw_transparent",
            display_server::DRAW_ALPHA_BLEND => "draw_alpha_blend",
            display_server::DRAW_COMPOSITE => "draw_composite",
            display_server::SURFACE_CREATE => "surface_create",
            display_server::SURFACE_DESTROY => "surface_destroy",
            display_server::MONITORS_CONFIG => "monitors_config",
            display_server::STREAM_ACTIVATE_REPORT => "stream_activate_report",
            display_server::GL_SCANOUT_UNIX => "gl_scanout_unix",
            display_server::GL_DRAW => "gl_draw",
            display_server::SET_ACK => "set_ack",
            display_server::PING => "ping",
            _ => "unknown",
        }
    }

    /// Get display channel client message name
    pub fn display_client(msg_type: u16) -> &'static str {
        match msg_type {
            display_client::INIT => "init",
            display_client::ACK_SYNC => "ack_sync",
            display_client::ACK => "ack",
            display_client::PONG => "pong",
            _ => "unknown",
        }
    }

    /// Get inputs channel server message name
    pub fn inputs_server(msg_type: u16) -> &'static str {
        match msg_type {
            inputs_server::INIT => "init",
            inputs_server::KEY_MODIFIERS => "key_modifiers",
            inputs_server::MOUSE_MOTION_ACK => "mouse_motion_ack",
            inputs_server::SET_ACK => "set_ack",
            inputs_server::PING => "ping",
            _ => "unknown",
        }
    }

    /// Get inputs channel client message name
    pub fn inputs_client(msg_type: u16) -> &'static str {
        match msg_type {
            inputs_client::KEY_DOWN => "key_down",
            inputs_client::KEY_UP => "key_up",
            inputs_client::KEY_MODIFIERS => "key_modifiers",
            inputs_client::KEY_SCANCODE => "key_scancode",
            inputs_client::MOUSE_MOTION => "mouse_motion",
            inputs_client::MOUSE_POSITION => "mouse_position",
            inputs_client::MOUSE_PRESS => "mouse_press",
            inputs_client::MOUSE_RELEASE => "mouse_release",
            _ => "unknown",
        }
    }

    /// Get cursor channel server message name
    pub fn cursor_server(msg_type: u16) -> &'static str {
        match msg_type {
            cursor_server::INIT => "init",
            cursor_server::RESET => "reset",
            cursor_server::SET => "set",
            cursor_server::MOVE => "move",
            cursor_server::HIDE => "hide",
            cursor_server::TRAIL => "trail",
            cursor_server::INVALIDATE_ONE => "invalidate_one",
            cursor_server::INVALIDATE_ALL => "invalidate_all",
            cursor_server::SET_ACK => "set_ack",
            cursor_server::PING => "ping",
            _ => "unknown",
        }
    }

    /// Get cursor channel client message name
    pub fn cursor_client(msg_type: u16) -> &'static str {
        match msg_type {
            cursor_client::ACK_SYNC => "ack_sync",
            cursor_client::ACK => "ack",
            cursor_client::PONG => "pong",
            _ => "unknown",
        }
    }

    pub fn playback_server(msg_type: u16) -> &'static str {
        match msg_type {
            playback_server::DATA => "data",
            playback_server::MODE => "mode",
            playback_server::START => "start",
            playback_server::STOP => "stop",
            playback_server::VOLUME => "volume",
            playback_server::MUTE => "mute",
            playback_server::LATENCY => "latency",
            playback_server::SET_ACK => "set_ack",
            playback_server::PING => "ping",
            _ => "unknown",
        }
    }

    pub fn playback_client(msg_type: u16) -> &'static str {
        match msg_type {
            main_client::ACK_SYNC => "ack_sync",
            main_client::ACK => "ack",
            main_client::PONG => "pong",
            _ => "unknown",
        }
    }

    /// Get SpiceVMC/usbredir server message name
    pub fn spicevmc_server(msg_type: u16) -> &'static str {
        match msg_type {
            spicevmc_server::DATA => "vmc_data",
            spicevmc_server::COMPRESSED_DATA => "vmc_compressed_data",
            spicevmc_server::SET_ACK => "set_ack",
            spicevmc_server::PING => "ping",
            _ => "unknown",
        }
    }

    /// Get SpiceVMC/usbredir client message name
    pub fn spicevmc_client(msg_type: u16) -> &'static str {
        match msg_type {
            spicevmc_client::DATA => "vmc_data",
            spicevmc_client::COMPRESSED_DATA => "vmc_compressed_data",
            spicevmc_client::ACK_SYNC => "ack_sync",
            spicevmc_client::ACK => "ack",
            spicevmc_client::PONG => "pong",
            _ => "unknown",
        }
    }
}
