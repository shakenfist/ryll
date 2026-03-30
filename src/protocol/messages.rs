/// SPICE protocol message structures and serialization
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor, Read};

/// Message header (6 bytes in mini-header mode)
#[derive(Debug, Clone)]
pub struct MessageHeader {
    pub message_type: u16,
    pub message_size: u32,
}

impl MessageHeader {
    pub const SIZE: usize = 6;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for message header",
            ));
        }

        let mut cursor = Cursor::new(data);
        let message_type = cursor.read_u16::<LittleEndian>()?;
        let message_size = cursor.read_u32::<LittleEndian>()?;

        Ok(MessageHeader {
            message_type,
            message_size,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u16::<LittleEndian>(self.message_type)?;
        buf.write_u32::<LittleEndian>(self.message_size)?;
        Ok(())
    }
}

/// Main channel init message from server
#[derive(Debug, Clone)]
pub struct MainInit {
    pub session_id: u32,
    pub display_channels_hint: u32,
    pub supported_mouse_modes: u32,
    pub current_mouse_mode: u32,
    pub agent_connected: u32,
    pub agent_tokens: u32,
    pub multi_media_time: u32,
    pub ram_hint: u32,
}

impl MainInit {
    pub const SIZE: usize = 32;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for MainInit",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(MainInit {
            session_id: cursor.read_u32::<LittleEndian>()?,
            display_channels_hint: cursor.read_u32::<LittleEndian>()?,
            supported_mouse_modes: cursor.read_u32::<LittleEndian>()?,
            current_mouse_mode: cursor.read_u32::<LittleEndian>()?,
            agent_connected: cursor.read_u32::<LittleEndian>()?,
            agent_tokens: cursor.read_u32::<LittleEndian>()?,
            multi_media_time: cursor.read_u32::<LittleEndian>()?,
            ram_hint: cursor.read_u32::<LittleEndian>()?,
        })
    }
}

/// Ping message
#[derive(Debug, Clone)]
pub struct Ping {
    pub id: u32,
    pub timestamp: u64,
}

impl Ping {
    pub const SIZE: usize = 12;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for Ping",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(Ping {
            id: cursor.read_u32::<LittleEndian>()?,
            timestamp: cursor.read_u64::<LittleEndian>()?,
        })
    }

    pub fn write_pong(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(self.id)?;
        buf.write_u64::<LittleEndian>(self.timestamp)?;
        Ok(())
    }
}

/// Channel list entry
#[derive(Debug, Clone)]
pub struct ChannelEntry {
    pub channel_type: u8,
    pub channel_id: u8,
}

/// Channels list message
#[derive(Debug, Clone)]
pub struct ChannelsList {
    pub channels: Vec<ChannelEntry>,
}

impl ChannelsList {
    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for ChannelsList",
            ));
        }

        let mut cursor = Cursor::new(data);
        let num_channels = cursor.read_u32::<LittleEndian>()? as usize;

        let mut channels = Vec::with_capacity(num_channels);
        for _ in 0..num_channels {
            let channel_type = cursor.read_u8()?;
            let channel_id = cursor.read_u8()?;
            channels.push(ChannelEntry {
                channel_type,
                channel_id,
            });
        }

        Ok(ChannelsList { channels })
    }
}

/// Set ACK message
#[derive(Debug, Clone)]
pub struct SetAck {
    pub generation: u32,
    pub window: u32,
}

impl SetAck {
    pub const SIZE: usize = 8;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SetAck",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(SetAck {
            generation: cursor.read_u32::<LittleEndian>()?,
            window: cursor.read_u32::<LittleEndian>()?,
        })
    }

    pub fn write_ack_sync(generation: u32, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(generation)?;
        Ok(())
    }
}

/// Notify message
#[derive(Debug, Clone)]
pub struct Notify {
    #[allow(dead_code)]
    pub timestamp: u64,
    pub severity: u32,
    pub visibility: u32,
    pub what: u32,
    pub message: String,
}

impl Notify {
    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < 24 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for Notify",
            ));
        }

        let mut cursor = Cursor::new(data);
        let timestamp = cursor.read_u64::<LittleEndian>()?;
        let severity = cursor.read_u32::<LittleEndian>()?;
        let visibility = cursor.read_u32::<LittleEndian>()?;
        let what = cursor.read_u32::<LittleEndian>()?;
        let msg_len = cursor.read_u32::<LittleEndian>()? as usize;

        let mut msg_bytes = vec![0u8; msg_len];
        cursor.read_exact(&mut msg_bytes)?;
        let message = String::from_utf8_lossy(&msg_bytes).to_string();

        Ok(Notify {
            timestamp,
            severity,
            visibility,
            what,
            message,
        })
    }
}

/// Surface create message
#[derive(Debug, Clone)]
pub struct SurfaceCreate {
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub flags: u32,
}

impl SurfaceCreate {
    pub const SIZE: usize = 20;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for SurfaceCreate",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(SurfaceCreate {
            surface_id: cursor.read_u32::<LittleEndian>()?,
            width: cursor.read_u32::<LittleEndian>()?,
            height: cursor.read_u32::<LittleEndian>()?,
            format: cursor.read_u32::<LittleEndian>()?,
            flags: cursor.read_u32::<LittleEndian>()?,
        })
    }
}

/// Draw copy message base (SpiceMsgDisplayBase)
#[derive(Debug, Clone)]
pub struct DrawCopyBase {
    pub surface_id: u32,
    pub top: u32,
    pub left: u32,
    pub bottom: u32,
    pub right: u32,
    pub clip_type: u8,
}

impl DrawCopyBase {
    pub const SIZE: usize = 21;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for DrawCopyBase",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(DrawCopyBase {
            surface_id: cursor.read_u32::<LittleEndian>()?,
            top: cursor.read_u32::<LittleEndian>()?,
            left: cursor.read_u32::<LittleEndian>()?,
            bottom: cursor.read_u32::<LittleEndian>()?,
            right: cursor.read_u32::<LittleEndian>()?,
            clip_type: cursor.read_u8()?,
        })
    }
}

/// Image descriptor from draw message
#[derive(Debug, Clone)]
pub struct ImageDescriptor {
    pub image_id: u64,
    pub image_type: u8,
    pub flags: u8,
    pub width: u32,
    pub height: u32,
}

impl ImageDescriptor {
    pub const SIZE: usize = 18;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for ImageDescriptor",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(ImageDescriptor {
            image_id: cursor.read_u64::<LittleEndian>()?,
            image_type: cursor.read_u8()?,
            flags: cursor.read_u8()?,
            width: cursor.read_u32::<LittleEndian>()?,
            height: cursor.read_u32::<LittleEndian>()?,
        })
    }
}

/// Display init message (client -> server)
#[derive(Debug, Clone)]
pub struct DisplayInit {
    pub cache_id: u8,
    pub cache_size: u64,
    pub glz_dict_id: u8,
    pub glz_dict_window: u32,
}

impl DisplayInit {
    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.cache_id)?;
        buf.write_u64::<LittleEndian>(self.cache_size)?;
        buf.write_u8(self.glz_dict_id)?;
        buf.write_u32::<LittleEndian>(self.glz_dict_window)?;
        Ok(())
    }
}

/// Cursor init message
#[derive(Debug, Clone)]
pub struct CursorInit {
    pub x: u16,
    pub y: u16,
    #[allow(dead_code)]
    pub trail_length: u16,
    #[allow(dead_code)]
    pub trail_frequency: u16,
    pub visible: u8,
}

impl CursorInit {
    pub const SIZE: usize = 9;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for CursorInit",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(CursorInit {
            x: cursor.read_u16::<LittleEndian>()?,
            y: cursor.read_u16::<LittleEndian>()?,
            trail_length: cursor.read_u16::<LittleEndian>()?,
            trail_frequency: cursor.read_u16::<LittleEndian>()?,
            visible: cursor.read_u8()?,
        })
    }
}

/// Cursor set message
#[derive(Debug, Clone)]
pub struct CursorSet {
    pub x: u16,
    pub y: u16,
    pub visible: u8,
}

impl CursorSet {
    pub const SIZE: usize = 5;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for CursorSet",
            ));
        }

        let mut cursor = Cursor::new(data);
        Ok(CursorSet {
            x: cursor.read_u16::<LittleEndian>()?,
            y: cursor.read_u16::<LittleEndian>()?,
            visible: cursor.read_u8()?,
        })
    }
}

/// Input key modifiers message (client -> server)
pub struct InputsKeyModifiers;

impl InputsKeyModifiers {
    pub fn write(modifiers: u16, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u16::<LittleEndian>(modifiers)?;
        Ok(())
    }
}

/// Key down/up message (client -> server)
pub struct KeyEvent;

impl KeyEvent {
    pub fn write(scancode: u32, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(scancode)?;
        Ok(())
    }
}

/// Mouse position message (client -> server)
pub struct MousePosition;

impl MousePosition {
    pub fn write(
        x: u32,
        y: u32,
        buttons: u32,
        display_id: u8,
        buf: &mut Vec<u8>,
    ) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(x)?;
        buf.write_u32::<LittleEndian>(y)?;
        buf.write_u32::<LittleEndian>(buttons)?;
        buf.write_u8(display_id)?;
        Ok(())
    }
}

/// Mouse button message (client -> server)
pub struct MouseButton;

impl MouseButton {
    pub fn write(button: u32, buttons_state: u32, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(button)?;
        buf.write_u32::<LittleEndian>(buttons_state)?;
        Ok(())
    }
}

/// Helper to construct a complete message with header
pub fn make_message(message_type: u16, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(MessageHeader::SIZE + payload.len());
    let header = MessageHeader {
        message_type,
        message_size: payload.len() as u32,
    };
    header.write(&mut buf).unwrap();
    buf.extend_from_slice(payload);
    buf
}
