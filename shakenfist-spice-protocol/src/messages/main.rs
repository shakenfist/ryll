//! Main channel messages.
use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{self, Cursor};

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

        // Each entry is two bytes, so the body bounds the count. Check it
        // before reserving: the count is server-supplied, and trusting it
        // would let a six-byte message reserve gigabytes.
        let room = (data.len() - 4) / 2;
        if num_channels > room {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ChannelsList declares {} channels but the body holds at most {}",
                    num_channels, room
                ),
            ));
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    // --- ChannelsList tests ---

    #[test]
    fn channels_list_reads_entries() {
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[1, 0, 2, 3]);
        let list = ChannelsList::read(&data).expect("parse");
        assert_eq!(list.channels.len(), 2);
        assert_eq!(list.channels[1].channel_type, 2);
        assert_eq!(list.channels[1].channel_id, 3);
    }

    /// shakenfist/ryll#180: the count is checked against the body before
    /// anything is reserved, so a huge count is an error, not an
    /// allocation of `count * 2` bytes.
    #[test]
    fn channels_list_count_beyond_body_is_error() {
        let mut data = u32::MAX.to_le_bytes().to_vec();
        data.extend_from_slice(&[1, 0]);
        let err = ChannelsList::read(&data).expect_err("count exceeds body");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
