//! Messages shared by several channels: the message header and framing,
//! and the base-channel messages every channel carries (`PING`, `PONG`,
//! `SET_ACK`, `ACK_SYNC`, `NOTIFY` and `DISCONNECTING`).
//!
//! Layouts follow spice-common's `spice.proto`, `channel BaseChannel`.
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::borrow::Cow;
use std::io::{self, Cursor};

use super::WireType;
use crate::constants::{NotifySeverity, SpiceError, SpiceVisibility};
use crate::reader::{BoundedReader, LinkError};

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

/// `SPICE_MSG_PING` (server to client): an id and a timestamp, then
/// padding to the end of the message.
///
/// spice.proto declares the padding as `uint8 data[] @as_ptr(data_len)`.
/// spice-server fills it with zeros, and uses it to size the main
/// channel's network-speed test (`main-channel-client.cpp`,
/// `main_channel_marshall_ping`). Only its length matters, so that is
/// all this type keeps; the writer emits zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ping {
    pub id: u32,
    pub timestamp: u64,
    /// Bytes of padding after the fixed fields.
    pub padding_len: usize,
}

impl Ping {
    /// Size of the fixed fields; the padding follows.
    pub const SIZE: usize = 12;

    /// The `PONG` that answers this ping: its id and timestamp, echoed.
    #[must_use]
    pub fn pong(&self) -> Pong {
        Pong {
            id: self.id,
            timestamp: self.timestamp,
        }
    }
}

impl WireType for Ping {
    /// Reads the fixed fields and consumes the rest of the body as
    /// padding.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let id = r.read_u32()?;
        let timestamp = r.read_u64()?;
        let padding_len = r.remaining();
        r.read_bytes(padding_len)?;
        Ok(Ping {
            id,
            timestamp,
            padding_len,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        out.resize(out.len() + self.padding_len, 0);
    }
}

/// `SPICE_MSGC_PONG` (client to server): the id and timestamp of the
/// `PING` it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pong {
    pub id: u32,
    pub timestamp: u64,
}

impl Pong {
    pub const SIZE: usize = 12;
}

impl WireType for Pong {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(Pong {
            id: r.read_u32()?,
            timestamp: r.read_u64()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
    }
}

/// `SPICE_MSG_SET_ACK` (server to client): the ack generation and the
/// number of messages the client may receive between `ACK`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAck {
    pub generation: u32,
    pub window: u32,
}

impl SetAck {
    pub const SIZE: usize = 8;

    /// The `ACK_SYNC` that answers this message: its generation, echoed.
    #[must_use]
    pub fn ack_sync(&self) -> AckSync {
        AckSync {
            generation: self.generation,
        }
    }
}

impl WireType for SetAck {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(SetAck {
            generation: r.read_u32()?,
            window: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.window.to_le_bytes());
    }
}

/// `SPICE_MSGC_ACK_SYNC` (client to server): the generation of the
/// `SET_ACK` it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckSync {
    pub generation: u32,
}

impl AckSync {
    pub const SIZE: usize = 4;
}

impl WireType for AckSync {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(AckSync {
            generation: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.generation.to_le_bytes());
    }
}

/// Maximum NOTIFY message body length accepted by the parser.
/// libspice-server caps NOTIFY at 1 MiB; 64 KiB is well above any
/// legitimate operator-facing notify text and prevents an attacker-
/// claimed `msg_len` from triggering a multi-gigabyte allocation
/// attempt before the existing buffer bound check fails.
pub const NOTIFY_MAX_MESSAGE_LEN: u32 = 64 * 1024;

/// `SPICE_MSG_NOTIFY` (server to client): a message for the user.
///
/// Wire format: `timestamp` (u64), `severity` (u32), `visibility` (u32),
/// `what` (u32), `message_len` (u32), then `message_len` bytes of text.
/// spice-server follows the text with a NUL that `message_len` does not
/// count (`main-channel-client.cpp`, `main_channel_marshall_notify`).
/// The writer does the same; the reader consumes a NUL if one follows the
/// text, but does not require it.
///
/// `severity` and `visibility` hold the wire values, so values this crate
/// does not know survive a round trip. [`severity_kind`](Self::severity_kind)
/// and [`visibility_kind`](Self::visibility_kind) interpret them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notify {
    pub timestamp: u64,
    /// `SPICE_NOTIFY_SEVERITY_*`.
    pub severity: u32,
    /// `SPICE_NOTIFY_VISIBILITY_*`.
    pub visibility: u32,
    /// An error, warning or info code, depending on `severity`.
    pub what: u32,
    /// The text, without its NUL terminator. Not necessarily UTF-8.
    pub message: Vec<u8>,
}

impl Notify {
    /// Size of the fixed fields; the text follows.
    pub const SIZE: usize = 24;

    /// The severity, with any value outside 0–2 read as
    /// [`NotifySeverity::Info`].
    #[must_use]
    pub fn severity_kind(&self) -> NotifySeverity {
        NotifySeverity::from_u32(self.severity)
    }

    /// The visibility, or `None` for any value outside 0–2.
    #[must_use]
    pub fn visibility_kind(&self) -> Option<SpiceVisibility> {
        SpiceVisibility::from_u32(self.visibility)
    }

    /// The text, with invalid UTF-8 replaced by U+FFFD.
    #[must_use]
    pub fn message_text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.message)
    }
}

impl WireType for Notify {
    /// # Errors
    ///
    /// [`LinkError::TooLarge`] if `message_len` exceeds
    /// [`NOTIFY_MAX_MESSAGE_LEN`], and [`LinkError::Truncated`] if the body
    /// is shorter than the fixed fields or the declared text.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let timestamp = r.read_u64()?;
        let severity = r.read_u32()?;
        let visibility = r.read_u32()?;
        let what = r.read_u32()?;
        let message_len = r.read_u32()?;
        if message_len > NOTIFY_MAX_MESSAGE_LEN {
            return Err(LinkError::TooLarge {
                what: "notify message_len",
                value: message_len as usize,
                max: NOTIFY_MAX_MESSAGE_LEN as usize,
            });
        }
        let message = r.read_bytes(message_len as usize)?.to_vec();
        let mut peek = r.clone();
        if peek.read_u8() == Ok(0) {
            *r = peek;
        }
        Ok(Notify {
            timestamp,
            severity,
            visibility,
            what,
            message,
        })
    }

    /// Writes the text followed by a NUL. A message longer than
    /// [`NOTIFY_MAX_MESSAGE_LEN`] is written as given, and the reader
    /// rejects it.
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        out.extend_from_slice(&self.severity.to_le_bytes());
        out.extend_from_slice(&self.visibility.to_le_bytes());
        out.extend_from_slice(&self.what.to_le_bytes());
        out.extend_from_slice(&(self.message.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.message);
        out.push(0);
    }
}

/// `SPICE_MSG_DISCONNECTING` and `SPICE_MSGC_DISCONNECTING`, which share
/// a layout: a timestamp and the reason the sender is disconnecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnecting {
    pub timestamp: u64,
    /// `SPICE_LINK_ERR_*`.
    pub reason: u32,
}

impl Disconnecting {
    pub const SIZE: usize = 12;

    /// The reason, with any unknown value read as [`SpiceError::Error`].
    #[must_use]
    pub fn reason_kind(&self) -> SpiceError {
        SpiceError::from_u32(self.reason)
    }
}

impl WireType for Disconnecting {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(Disconnecting {
            timestamp: r.read_u64()?,
            reason: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        out.extend_from_slice(&self.reason.to_le_bytes());
    }
}

/// Helper to construct a complete message with header
pub fn make_message(message_type: u16, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(MessageHeader::SIZE + payload.len());
    let header = MessageHeader {
        message_type,
        message_size: payload.len() as u32,
    };
    header.write(&mut buf).expect("write to Vec cannot fail");
    buf.extend_from_slice(payload);
    buf
}

/// One complete message split off the front of a receive buffer by
/// [`take_message`]: the header and the raw bytes it framed.
#[derive(Debug, Clone)]
pub struct ReceivedMessage {
    pub header: MessageHeader,
    /// The whole message as received, header included.
    pub raw: Vec<u8>,
}

impl ReceivedMessage {
    /// The message body, without its header.
    pub fn payload(&self) -> &[u8] {
        &self.raw[MessageHeader::SIZE..]
    }
}

/// Split the next complete message off the front of a receive buffer.
///
/// Returns `Ok(None)` while `buffer` does not yet hold a whole message, so
/// a read loop calls this until it returns `None` and then reads more.
///
/// # Errors
///
/// Fails with [`io::ErrorKind::InvalidData`] as soon as a header declares a
/// body larger than `max_body`, without waiting for that body. The size is
/// server-supplied, so without the check a peer could make the caller
/// buffer up to 4 GiB per channel by declaring a huge message and sending
/// it slowly. The caller should drop the connection.
pub fn take_message(buffer: &mut Vec<u8>, max_body: usize) -> io::Result<Option<ReceivedMessage>> {
    if buffer.len() < MessageHeader::SIZE {
        return Ok(None);
    }
    let header = MessageHeader::read(buffer)?;
    let body = header.message_size as usize;
    if body > max_body {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "message type {} declares a {} byte body, over the {} byte limit",
                header.message_type, body, max_body
            ),
        ));
    }
    let total = MessageHeader::SIZE + body;
    if buffer.len() < total {
        return Ok(None);
    }
    let raw = buffer.drain(..total).collect();
    Ok(Some(ReceivedMessage { header, raw }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::assert_round_trip;

    // --- take_message tests ---

    #[test]
    fn take_message_splits_complete_messages() {
        let mut buffer = make_message(3, &[1, 2, 3]);
        buffer.extend_from_slice(&make_message(4, &[]));
        buffer.extend_from_slice(&[9]); // start of a third header

        let first = take_message(&mut buffer, 16)
            .expect("ok")
            .expect("complete");
        assert_eq!(first.header.message_type, 3);
        assert_eq!(first.payload(), &[1, 2, 3]);
        assert_eq!(first.raw.len(), MessageHeader::SIZE + 3);

        let second = take_message(&mut buffer, 16)
            .expect("ok")
            .expect("complete");
        assert_eq!(second.header.message_type, 4);
        assert!(second.payload().is_empty());

        assert!(take_message(&mut buffer, 16).expect("ok").is_none());
        assert_eq!(buffer, vec![9]);
    }

    #[test]
    fn take_message_waits_for_the_body() {
        let full = make_message(3, &[1, 2, 3]);
        let mut buffer = full[..full.len() - 1].to_vec();
        assert!(take_message(&mut buffer, 16).expect("ok").is_none());
        assert_eq!(
            buffer.len(),
            full.len() - 1,
            "a partial message is left in place"
        );
    }

    /// shakenfist/ryll#181: an oversized body is refused from the header
    /// alone, before any of it has arrived.
    #[test]
    fn take_message_refuses_oversized_body_from_header() {
        let mut buffer = Vec::new();
        MessageHeader {
            message_type: 3,
            message_size: u32::MAX,
        }
        .write(&mut buffer)
        .expect("write");
        let err = take_message(&mut buffer, 16).expect_err("over the limit");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let mut buffer = make_message(3, &[0; 17]);
        assert!(take_message(&mut buffer, 16).is_err());
        let mut buffer = make_message(3, &[0; 16]);
        assert!(take_message(&mut buffer, 16).expect("ok").is_some());
    }

    // --- Ping and Pong tests ---

    #[test]
    fn ping_round_trips() {
        assert_round_trip(&Ping {
            id: 7,
            timestamp: 0x0102_0304_0506_0708,
            padding_len: 0,
        });
        assert_round_trip(&Ping {
            id: u32::MAX,
            timestamp: 1,
            padding_len: 300,
        });
    }

    #[test]
    fn ping_decodes_spice_proto_layout() {
        // spice.proto BaseChannel `ping`: uint32 id, uint64 timestamp,
        // uint8 data[] to the end of the message.
        let body = [
            0x2a, 0x00, 0x00, 0x00, // id = 42
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // timestamp
            0x00, 0x00, 0x00, // three bytes of padding
        ];
        let ping = Ping::decode(&body).expect("decodes");
        assert_eq!(
            ping,
            Ping {
                id: 42,
                timestamp: 0x0102_0304_0506_0708,
                padding_len: 3,
            }
        );
        assert!(Ping::decode(&body[..11]).is_err());
    }

    /// The PONG ryll sends must stay the 12 bytes it sent before the
    /// protocol crate modelled it: the PING's id and timestamp, with no
    /// padding, whatever the PING carried.
    #[test]
    fn pong_echoes_ping_id_and_timestamp() {
        let mut body = Vec::new();
        body.extend_from_slice(&42u32.to_le_bytes());
        body.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        let mut expected = body.clone();
        body.extend_from_slice(&[0; 256]);

        let mut pong = Vec::new();
        Ping::decode(&body)
            .expect("decodes")
            .pong()
            .write(&mut pong);
        assert_eq!(pong, expected);

        // spice.proto BaseChannel client `pong`: uint32 id, uint64 timestamp.
        expected.push(0xff); // trailing bytes are ignored
        assert_eq!(
            Pong::decode(&expected).expect("decodes"),
            Pong {
                id: 42,
                timestamp: 0x0102_0304_0506_0708,
            }
        );
        assert!(Pong::decode(&expected[..11]).is_err());
    }

    #[test]
    fn pong_round_trips() {
        assert_round_trip(&Pong {
            id: 3,
            timestamp: u64::MAX,
        });
    }

    // --- SetAck and AckSync tests ---

    #[test]
    fn set_ack_round_trips() {
        assert_round_trip(&SetAck {
            generation: 1,
            window: 20,
        });
    }

    #[test]
    fn set_ack_decodes_spice_proto_layout() {
        // spice.proto BaseChannel `set_ack`: uint32 generation, uint32 window.
        let body = [0x05, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0xee];
        assert_eq!(
            SetAck::decode(&body).expect("decodes"),
            SetAck {
                generation: 5,
                window: 20,
            }
        );
        assert!(SetAck::decode(&body[..7]).is_err());
    }

    #[test]
    fn ack_sync_round_trips() {
        assert_round_trip(&AckSync { generation: 9 });
    }

    /// ryll answers SET_ACK with an ACK_SYNC holding only the generation,
    /// as spice.proto BaseChannel client `ack_sync` lays out.
    #[test]
    fn ack_sync_echoes_set_ack_generation() {
        let set_ack = SetAck {
            generation: 0x0a0b_0c0d,
            window: 20,
        };
        let mut body = Vec::new();
        set_ack.ack_sync().write(&mut body);
        assert_eq!(body, vec![0x0d, 0x0c, 0x0b, 0x0a]);
        assert_eq!(
            AckSync::decode(&body).expect("decodes"),
            AckSync {
                generation: 0x0a0b_0c0d
            }
        );
        assert!(AckSync::decode(&body[..3]).is_err());
    }

    // --- Disconnecting tests ---

    #[test]
    fn disconnecting_round_trips() {
        assert_round_trip(&Disconnecting {
            timestamp: 12345,
            reason: 99,
        });
    }

    #[test]
    fn disconnecting_decodes_spice_proto_layout() {
        // spice.proto BaseChannel `disconnecting`: uint64 time_stamp,
        // link_err reason (enum32).
        let body = [
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // time_stamp = 1
            0x07, 0x00, 0x00, 0x00, // reason = PERMISSION_DENIED
        ];
        let msg = Disconnecting::decode(&body).expect("decodes");
        assert_eq!(
            msg,
            Disconnecting {
                timestamp: 1,
                reason: 7,
            }
        );
        assert_eq!(msg.reason_kind(), SpiceError::PermissionDenied);
        assert!(Disconnecting::decode(&body[..11]).is_err());
    }

    // --- Notify tests ---

    fn build_notify(severity: u32, visibility: u32, what: u32, message: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24 + message.len());
        buf.extend_from_slice(&0u64.to_le_bytes()); // timestamp
        buf.extend_from_slice(&severity.to_le_bytes());
        buf.extend_from_slice(&visibility.to_le_bytes());
        buf.extend_from_slice(&what.to_le_bytes());
        buf.extend_from_slice(&(message.len() as u32).to_le_bytes());
        buf.extend_from_slice(message);
        buf
    }

    #[test]
    fn notify_round_trips() {
        assert_round_trip(&Notify {
            timestamp: 5,
            severity: 1,
            visibility: 2,
            what: 0,
            message: b"hello".to_vec(),
        });
        // Unknown enumerated values and non-UTF-8 text survive.
        assert_round_trip(&Notify {
            timestamp: 0,
            severity: 99,
            visibility: 77,
            what: 3,
            message: vec![0xff, 0xfe],
        });
        assert_round_trip(&Notify {
            timestamp: 0,
            severity: 0,
            visibility: 0,
            what: 0,
            message: Vec::new(),
        });
    }

    /// Laid out as spice-server writes it: spice.proto BaseChannel
    /// `notify`, then the NUL that `main_channel_marshall_notify` adds
    /// after `message_len` bytes.
    #[test]
    fn notify_decodes_spice_server_layout() {
        let body = [
            0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // time_stamp = 16
            0x01, 0x00, 0x00, 0x00, // severity = WARN
            0x02, 0x00, 0x00, 0x00, // visibility = HIGH
            0x00, 0x00, 0x00, 0x00, // what = SPICE_WARN_GENERAL
            0x02, 0x00, 0x00, 0x00, // message_len = 2
            b'h', b'i', 0x00, // message and its NUL
        ];
        let mut r = BoundedReader::new(&body);
        let msg = Notify::read(&mut r).expect("decodes");
        assert_eq!(r.remaining(), 0, "the NUL is consumed");
        assert_eq!(
            msg,
            Notify {
                timestamp: 16,
                severity: 1,
                visibility: 2,
                what: 0,
                message: b"hi".to_vec(),
            }
        );
        assert_eq!(msg.severity_kind(), NotifySeverity::Warn);
        assert_eq!(msg.visibility_kind(), Some(SpiceVisibility::High));
        assert_eq!(msg.message_text(), "hi");

        // The writer reproduces spice-server's bytes exactly.
        let mut written = Vec::new();
        msg.write(&mut written);
        assert_eq!(written, body);
    }

    #[test]
    fn notify_parse_minimum_valid() {
        // 24-byte buffer: header with msg_len=0, no message body.
        let buf = build_notify(0, 0, 42, &[]);
        assert_eq!(buf.len(), 24);
        let msg = Notify::decode(&buf).expect("minimum valid Notify failed");
        assert_eq!(msg.severity_kind(), NotifySeverity::Info);
        assert_eq!(msg.visibility_kind(), Some(SpiceVisibility::Low));
        assert_eq!(msg.what, 42);
        assert_eq!(msg.message_text(), "");
    }

    #[test]
    fn notify_parse_each_severity() {
        let cases = [
            (0u32, NotifySeverity::Info),
            (1u32, NotifySeverity::Warn),
            (2u32, NotifySeverity::Error),
        ];
        for (raw, expected) in cases {
            let buf = build_notify(raw, 0, 0, &[]);
            let msg = Notify::decode(&buf).unwrap_or_else(|_| panic!("severity={raw} failed"));
            assert_eq!(msg.severity_kind(), expected, "severity raw={raw}");
        }
    }

    #[test]
    fn notify_parse_each_visibility() {
        let cases = [
            (0u32, Some(SpiceVisibility::Low)),
            (1u32, Some(SpiceVisibility::Medium)),
            (2u32, Some(SpiceVisibility::High)),
        ];
        for (raw, expected) in cases {
            let buf = build_notify(0, raw, 0, &[]);
            let msg = Notify::decode(&buf).unwrap_or_else(|_| panic!("visibility={raw} failed"));
            assert_eq!(msg.visibility_kind(), expected, "visibility raw={raw}");
        }
    }

    #[test]
    fn notify_parse_unknown_visibility_is_none() {
        let buf = build_notify(0, 99, 0, &[]);
        let msg = Notify::decode(&buf).expect("unknown visibility should not error");
        assert_eq!(msg.visibility, 99);
        assert_eq!(msg.visibility_kind(), None);
    }

    #[test]
    fn notify_parse_unknown_severity_defaults_info() {
        let buf = build_notify(99, 0, 0, &[]);
        let msg = Notify::decode(&buf).expect("unknown severity should not error");
        assert_eq!(msg.severity, 99);
        assert_eq!(msg.severity_kind(), NotifySeverity::Info);
    }

    #[test]
    fn notify_parse_with_500_byte_message() {
        let payload: Vec<u8> = (0u8..=127u8).cycle().take(500).collect();
        let expected_str = String::from_utf8(payload.clone()).expect("test payload is valid ASCII");
        let buf = build_notify(1, 2, 7, &payload);
        assert_eq!(buf.len(), 524);
        let msg = Notify::decode(&buf).expect("500-byte message parse failed");
        assert_eq!(msg.message_text(), expected_str);
        assert_eq!(msg.severity_kind(), NotifySeverity::Warn);
        assert_eq!(msg.visibility_kind(), Some(SpiceVisibility::High));
    }

    #[test]
    fn notify_parse_truncated_header() {
        // 23 bytes — one short of the 24-byte fixed header.
        let buf = vec![0u8; 23];
        assert_eq!(
            Notify::decode(&buf),
            Err(LinkError::Truncated {
                needed: 4,
                available: 3,
            })
        );
    }

    #[test]
    fn notify_parse_message_body_shorter_than_declared() {
        // Build a header claiming msg_len=100 but only append 50 bytes.
        let mut buf = build_notify(0, 0, 0, &[]);
        buf[20..24].copy_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 50]); // only 50 bytes follow
        assert_eq!(
            Notify::decode(&buf),
            Err(LinkError::Truncated {
                needed: 100,
                available: 50,
            })
        );
    }

    #[test]
    fn notify_parse_invalid_utf8_replaced() {
        // Non-UTF-8 bytes are kept as given and replaced lossily for display.
        let bad_bytes: &[u8] = &[0xFF, 0xFE, 0xFD];
        let buf = build_notify(0, 0, 0, bad_bytes);
        let msg = Notify::decode(&buf).expect("invalid UTF-8 should return Ok");
        assert_eq!(msg.message, bad_bytes);
        assert!(
            msg.message_text().contains('\u{FFFD}'),
            "expected U+FFFD replacement char in message, got: {:?}",
            msg.message_text(),
        );
    }

    #[test]
    fn notify_parse_oversized_msg_len_rejected() {
        // A header claiming msg_len just above the cap is refused before
        // anything is allocated.
        let mut buf = build_notify(0, 0, 0, &[]);
        buf[20..24].copy_from_slice(&(NOTIFY_MAX_MESSAGE_LEN + 1).to_le_bytes());
        assert!(matches!(
            Notify::decode(&buf),
            Err(LinkError::TooLarge { .. })
        ));
    }
}
