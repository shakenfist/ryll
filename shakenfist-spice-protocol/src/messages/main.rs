//! Main channel messages.
//!
//! Layouts follow spice-common's `spice.proto`, `channel MainChannel`.
//! `ATTACH_CHANNELS`, `AGENT_CONNECTED` and `ACK` have empty bodies and
//! need no type. `AGENT_DATA` carries vdagent messages; see
//! [`vd_agent`](super::vd_agent).

use super::WireType;
use crate::constants::SpiceError;
use crate::reader::{BoundedReader, LinkError};

/// `SPICE_MSG_MAIN_INIT` (server to client): the session's starting state.
///
/// `supported_mouse_modes` and `current_mouse_mode` are `SPICE_MOUSE_MODE_*`
/// flags (`MOUSE_MODE_SERVER`, `MOUSE_MODE_CLIENT`).
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 32;
}

impl WireType for MainInit {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MainInit {
            session_id: r.read_u32()?,
            display_channels_hint: r.read_u32()?,
            supported_mouse_modes: r.read_u32()?,
            current_mouse_mode: r.read_u32()?,
            agent_connected: r.read_u32()?,
            agent_tokens: r.read_u32()?,
            multi_media_time: r.read_u32()?,
            ram_hint: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [
            self.session_id,
            self.display_channels_hint,
            self.supported_mouse_modes,
            self.current_mouse_mode,
            self.agent_connected,
            self.agent_tokens,
            self.multi_media_time,
            self.ram_hint,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// One entry of a [`ChannelsList`] (spice.proto `ChannelId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelEntry {
    pub channel_type: u8,
    pub channel_id: u8,
}

/// `SPICE_MSG_MAIN_CHANNELS_LIST` (server to client): the channels the
/// client may connect. Wire format: `num_of_channels` (u32), then that
/// many two-byte [`ChannelEntry`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelsList {
    pub channels: Vec<ChannelEntry>,
}

impl WireType for ChannelsList {
    /// # Errors
    ///
    /// [`LinkError::TooLarge`] if `num_of_channels` exceeds what the rest of
    /// the body can hold, and [`LinkError::Truncated`] if the body is
    /// shorter than the count.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let num_channels = r.read_u32()? as usize;

        // Each entry is two bytes, so the body bounds the count. Check it
        // before reserving: the count is server-supplied, and trusting it
        // would let a six-byte message reserve gigabytes.
        r.check_count("num_of_channels", num_channels, 2)?;

        let mut channels = Vec::with_capacity(num_channels);
        for _ in 0..num_channels {
            channels.push(ChannelEntry {
                channel_type: r.read_u8()?,
                channel_id: r.read_u8()?,
            });
        }
        Ok(ChannelsList { channels })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.channels.len() as u32).to_le_bytes());
        for c in &self.channels {
            out.push(c.channel_type);
            out.push(c.channel_id);
        }
    }
}

/// `SPICE_MSG_MAIN_MOUSE_MODE` (server to client): the mouse modes the
/// server supports and the one it is in.
///
/// spice.proto declares both as `flags16`, so each is a `u16`. Reading the
/// pair as one `u32` gives nonsense such as 131075 for supported 3 and
/// current 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainMouseMode {
    /// `SPICE_MOUSE_MODE_*` flags.
    pub supported_modes: u16,
    /// A single `SPICE_MOUSE_MODE_*` flag.
    pub current_mode: u16,
}

impl MainMouseMode {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 4;
}

impl WireType for MainMouseMode {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MainMouseMode {
            supported_modes: r.read_u16()?,
            current_mode: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.supported_modes.to_le_bytes());
        out.extend_from_slice(&self.current_mode.to_le_bytes());
    }
}

/// `SPICE_MSGC_MAIN_MOUSE_MODE_REQUEST` (client to server): the mouse mode
/// the client wants. spice.proto declares it as `flags16`, a single `u16`;
/// a `u32` would ship two extra bytes that some servers reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseModeRequest {
    /// A single `SPICE_MOUSE_MODE_*` flag.
    pub mode: u16,
}

impl MouseModeRequest {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 2;
}

impl WireType for MouseModeRequest {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MouseModeRequest {
            mode: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.mode.to_le_bytes());
    }
}

/// `SPICE_MSG_MAIN_MULTI_MEDIA_TIME` (server to client): the server's
/// multimedia clock, in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiMediaTime {
    pub time: u32,
}

impl MultiMediaTime {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 4;
}

impl WireType for MultiMediaTime {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MultiMediaTime {
            time: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.time.to_le_bytes());
    }
}

/// A count of agent tokens. Three messages share this layout:
///
/// - `SPICE_MSG_MAIN_AGENT_CONNECTED_TOKENS` (server to client): the agent
///   connected, and this is the client's new token window;
/// - `SPICE_MSG_MAIN_AGENT_TOKEN` (server to client): tokens returned to
///   the client;
/// - `SPICE_MSGC_MAIN_AGENT_START` and `SPICE_MSGC_MAIN_AGENT_TOKEN`
///   (client to server): the tokens the client grants the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTokens {
    pub num_tokens: u32,
}

impl AgentTokens {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 4;
}

impl WireType for AgentTokens {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(AgentTokens {
            num_tokens: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.num_tokens.to_le_bytes());
    }
}

/// `SPICE_MSG_MAIN_AGENT_DISCONNECTED` (server to client): the guest agent
/// went away, with an error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDisconnected {
    /// `SPICE_LINK_ERR_*`.
    pub error_code: u32,
}

impl AgentDisconnected {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 4;

    /// The error code, with any unknown value read as
    /// [`SpiceError::Error`].
    #[must_use]
    pub fn error_code_kind(&self) -> SpiceError {
        SpiceError::from_u32(self.error_code)
    }
}

impl WireType for AgentDisconnected {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(AgentDisconnected {
            error_code: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.error_code.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::assert_round_trip;
    use crate::{MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER};

    // --- MainInit tests ---

    #[test]
    fn main_init_round_trips() {
        assert_round_trip(&MainInit {
            session_id: 1,
            display_channels_hint: 2,
            supported_mouse_modes: 3,
            current_mouse_mode: 4,
            agent_connected: 5,
            agent_tokens: 6,
            multi_media_time: 7,
            ram_hint: u32::MAX,
        });
    }

    #[test]
    fn main_init_decodes_spice_proto_layout() {
        // spice.proto MainChannel `init`: eight uint32s in this order.
        let mut body = Vec::new();
        for v in [0xdead_beefu32, 1, 3, 2, 1, 10, 0x1234, 0x0100_0000] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.push(0xff); // trailing bytes are ignored
        assert_eq!(
            MainInit::decode(&body).expect("decodes"),
            MainInit {
                session_id: 0xdead_beef,
                display_channels_hint: 1,
                supported_mouse_modes: 3,
                current_mouse_mode: 2,
                agent_connected: 1,
                agent_tokens: 10,
                multi_media_time: 0x1234,
                ram_hint: 0x0100_0000,
            }
        );
        assert!(MainInit::decode(&body[..31]).is_err());
    }

    // --- ChannelsList tests ---

    #[test]
    fn channels_list_round_trips() {
        assert_round_trip(&ChannelsList {
            channels: Vec::new(),
        });
        assert_round_trip(&ChannelsList {
            channels: vec![
                ChannelEntry {
                    channel_type: 1,
                    channel_id: 0,
                },
                ChannelEntry {
                    channel_type: 255,
                    channel_id: 7,
                },
            ],
        });
    }

    #[test]
    fn channels_list_reads_entries() {
        // spice.proto MainChannel `channels_list`: uint32 num_of_channels,
        // then ChannelId { uint8 type; uint8 id; } entries.
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[1, 0, 2, 3]);
        let list = ChannelsList::decode(&data).expect("parse");
        assert_eq!(
            list.channels,
            vec![
                ChannelEntry {
                    channel_type: 1,
                    channel_id: 0,
                },
                ChannelEntry {
                    channel_type: 2,
                    channel_id: 3,
                },
            ]
        );
    }

    /// shakenfist/ryll#180: the count is checked against the body before
    /// anything is reserved, so a huge count is an error, not an
    /// allocation of `count * 2` bytes.
    #[test]
    fn channels_list_count_beyond_body_is_error() {
        let mut data = u32::MAX.to_le_bytes().to_vec();
        data.extend_from_slice(&[1, 0]);
        assert_eq!(
            ChannelsList::decode(&data),
            Err(LinkError::TooLarge {
                what: "num_of_channels",
                value: u32::MAX as usize,
                max: 1,
            })
        );
        assert!(ChannelsList::decode(&[1, 0, 0]).is_err());
    }

    // --- MainMouseMode tests ---

    #[test]
    fn main_mouse_mode_round_trips() {
        assert_round_trip(&MainMouseMode {
            supported_modes: 3,
            current_mode: 2,
        });
    }

    // Payload bytes observed in the 2026-04-23 macbook bug-report
    // main.pcap. Parsing these as a single little-endian u32 yields
    // 131075 / 65537 / 65539 — which failed every mode check in the
    // GUI and left clicks broken after a guest reboot.
    #[test]
    fn main_mouse_mode_splits_supported_and_current() {
        let decode = |b: &[u8]| {
            let m = MainMouseMode::decode(b).expect("decodes");
            (m.supported_modes, m.current_mode)
        };
        // supported=3 (both), current=2 (CLIENT) — initial negotiation.
        assert_eq!(decode(&[0x03, 0x00, 0x02, 0x00]), (3, 2));
        // supported=1 (server only), current=1 (SERVER) — right after
        // guest reboot, agent gone.
        assert_eq!(decode(&[0x01, 0x00, 0x01, 0x00]), (1, 1));
        // supported=3 (both), current=1 (SERVER) — agent back but
        // server still in SERVER mode; this is the case that must
        // trigger a CLIENT re-request.
        assert_eq!(decode(&[0x03, 0x00, 0x01, 0x00]), (3, 1));
    }

    #[test]
    fn main_mouse_mode_rejects_short_payload() {
        assert!(MainMouseMode::decode(&[]).is_err());
        assert!(MainMouseMode::decode(&[0x03, 0x00, 0x01]).is_err());
    }

    // --- MouseModeRequest tests ---

    #[test]
    fn mouse_mode_request_round_trips() {
        assert_round_trip(&MouseModeRequest { mode: 2 });
    }

    #[test]
    fn mouse_mode_request_is_two_bytes() {
        // Regression for PR 31 blocking #3: the body is flags16 (one
        // little-endian u16), not u32. Writing u32 here shipped two
        // extra zero bytes that some servers reject as malformed.
        // Same shape regardless of which mode we ask for.
        for (mode, bytes) in [
            (MOUSE_MODE_CLIENT, vec![0x02, 0x00]),
            (MOUSE_MODE_SERVER, vec![0x01, 0x00]),
        ] {
            let mut body = Vec::new();
            MouseModeRequest { mode: mode as u16 }.write(&mut body);
            assert_eq!(body, bytes);
            assert_eq!(
                MouseModeRequest::decode(&bytes).expect("decodes").mode,
                mode as u16
            );
        }
        assert!(MouseModeRequest::decode(&[0x02]).is_err());
    }

    // --- MultiMediaTime tests ---

    #[test]
    fn multi_media_time_round_trips() {
        assert_round_trip(&MultiMediaTime { time: 0x8000_0001 });
    }

    #[test]
    fn multi_media_time_decodes_spice_proto_layout() {
        // spice.proto MainChannel `multi_media_time`: uint32 time.
        assert_eq!(
            MultiMediaTime::decode(&[0x78, 0x56, 0x34, 0x12]).expect("decodes"),
            MultiMediaTime { time: 0x1234_5678 }
        );
        assert!(MultiMediaTime::decode(&[0x78, 0x56, 0x34]).is_err());
    }

    // --- AgentTokens tests ---

    #[test]
    fn agent_tokens_round_trips() {
        assert_round_trip(&AgentTokens { num_tokens: 10 });
        assert_round_trip(&AgentTokens {
            num_tokens: u32::MAX,
        });
    }

    // SpiceMsgMainAgentConnectedTokens carries spice-server's
    // REDS_AGENT_WINDOW_SIZE (10) as a little-endian u32.
    #[test]
    fn agent_tokens_reads_window() {
        let decode = |b: &[u8]| AgentTokens::decode(b).ok().map(|t| t.num_tokens);
        assert_eq!(decode(&[0x0a, 0, 0, 0]), Some(10));
        assert_eq!(decode(&[0x0a, 0, 0, 0, 0xff]), Some(10));
        assert_eq!(decode(&[0x0a, 0, 0]), None);
        assert_eq!(decode(&[]), None);
    }

    /// ryll sends AGENT_START granting `u32::MAX` tokens; the body is the
    /// single little-endian uint32 of spice.proto's `agent_start`.
    #[test]
    fn agent_start_body_is_one_u32() {
        let mut body = Vec::new();
        AgentTokens {
            num_tokens: u32::MAX,
        }
        .write(&mut body);
        assert_eq!(body, vec![0xff, 0xff, 0xff, 0xff]);
    }

    // --- AgentDisconnected tests ---

    #[test]
    fn agent_disconnected_round_trips() {
        assert_round_trip(&AgentDisconnected { error_code: 42 });
    }

    #[test]
    fn agent_disconnected_decodes_spice_proto_layout() {
        // spice.proto MainChannel `agent_disconnected`: link_err error_code
        // (enum32).
        let msg = AgentDisconnected::decode(&[0x01, 0x00, 0x00, 0x00]).expect("decodes");
        assert_eq!(msg, AgentDisconnected { error_code: 1 });
        assert_eq!(msg.error_code_kind(), SpiceError::Error);
        assert!(AgentDisconnected::decode(&[0x01, 0x00, 0x00]).is_err());
    }
}
