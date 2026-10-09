//! Messages shared by several channels: the message header and framing,
//! and the ping, ack and notify messages.
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor, Read};

use crate::constants::{NotifySeverity, SpiceVisibility};

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

/// Maximum NOTIFY message body length accepted by the parser.
/// libspice-server caps NOTIFY at 1 MiB; 64 KiB is well above any
/// legitimate operator-facing notify text and prevents an attacker-
/// claimed `msg_len` from triggering a multi-gigabyte allocation
/// attempt before the existing buffer bound check fails.
pub const NOTIFY_MAX_MESSAGE_LEN: u32 = 64 * 1024;

/// Notify message
///
/// Wire format: timestamp(u64) + severity(u32) + visibility(u32) +
/// what(u32) + msg_len(u32) + message bytes. `severity` is parsed
/// into a [`NotifySeverity`]; `visibility` is parsed into
/// `Option<SpiceVisibility>` (`None` for any value outside 0–2).
#[derive(Debug, Clone)]
pub struct Notify {
    #[allow(dead_code)]
    pub timestamp: u64,
    pub severity: NotifySeverity,
    pub visibility: Option<SpiceVisibility>,
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
        let severity_raw = cursor.read_u32::<LittleEndian>()?;
        let visibility_raw = cursor.read_u32::<LittleEndian>()?;
        let what = cursor.read_u32::<LittleEndian>()?;
        let msg_len = cursor.read_u32::<LittleEndian>()?;
        if msg_len > NOTIFY_MAX_MESSAGE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Notify msg_len {} exceeds cap {}",
                    msg_len, NOTIFY_MAX_MESSAGE_LEN
                ),
            ));
        }
        let msg_len = msg_len as usize;

        if data.len() < 24 + msg_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Notify message body shorter than declared length",
            ));
        }

        let mut msg_bytes = vec![0u8; msg_len];
        cursor.read_exact(&mut msg_bytes)?;
        let message = String::from_utf8_lossy(&msg_bytes).to_string();

        Ok(Notify {
            timestamp,
            severity: NotifySeverity::from_u32(severity_raw),
            visibility: SpiceVisibility::from_u32(visibility_raw),
            what,
            message,
        })
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
    fn notify_parse_minimum_valid() {
        // 24-byte buffer: header with msg_len=0, no message body.
        let buf = build_notify(0, 0, 42, &[]);
        assert_eq!(buf.len(), 24);
        let msg = Notify::read(&buf).expect("minimum valid Notify failed");
        assert_eq!(msg.severity, NotifySeverity::Info);
        assert_eq!(msg.visibility, Some(SpiceVisibility::Low));
        assert_eq!(msg.what, 42);
        assert_eq!(msg.message, "");
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
            let msg = Notify::read(&buf).unwrap_or_else(|_| panic!("severity={raw} failed"));
            assert_eq!(msg.severity, expected, "severity raw={raw}");
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
            let msg = Notify::read(&buf).unwrap_or_else(|_| panic!("visibility={raw} failed"));
            assert_eq!(msg.visibility, expected, "visibility raw={raw}");
        }
    }

    #[test]
    fn notify_parse_unknown_visibility_is_none() {
        let buf = build_notify(0, 99, 0, &[]);
        let msg = Notify::read(&buf).expect("unknown visibility should not error");
        assert_eq!(msg.visibility, None);
    }

    #[test]
    fn notify_parse_unknown_severity_defaults_info() {
        let buf = build_notify(99, 0, 0, &[]);
        let msg = Notify::read(&buf).expect("unknown severity should not error");
        assert_eq!(msg.severity, NotifySeverity::Info);
    }

    #[test]
    fn notify_parse_with_500_byte_message() {
        let payload: Vec<u8> = (0u8..=127u8).cycle().take(500).collect();
        let expected_str = String::from_utf8(payload.clone()).expect("test payload is valid ASCII");
        let buf = build_notify(1, 2, 7, &payload);
        assert_eq!(buf.len(), 524);
        let msg = Notify::read(&buf).expect("500-byte message parse failed");
        assert_eq!(msg.message, expected_str);
        assert_eq!(msg.severity, NotifySeverity::Warn);
        assert_eq!(msg.visibility, Some(SpiceVisibility::High));
    }

    #[test]
    fn notify_parse_truncated_header() {
        // 23 bytes — one short of the 24-byte fixed header.
        let buf = vec![0u8; 23];
        let result = Notify::read(&buf);
        assert!(result.is_err(), "expected Err for 23-byte buffer");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn notify_parse_message_body_shorter_than_declared() {
        // Build a header claiming msg_len=100 but only append 50 bytes.
        let mut buf = Vec::with_capacity(24 + 50);
        buf.extend_from_slice(&0u64.to_le_bytes()); // timestamp
        buf.extend_from_slice(&0u32.to_le_bytes()); // severity
        buf.extend_from_slice(&0u32.to_le_bytes()); // visibility
        buf.extend_from_slice(&0u32.to_le_bytes()); // what
        buf.extend_from_slice(&100u32.to_le_bytes()); // msg_len = 100
        buf.extend_from_slice(&[0u8; 50]); // only 50 bytes follow
        let result = Notify::read(&buf);
        assert!(
            result.is_err(),
            "expected Err for body shorter than declared"
        );
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("shorter than declared"),
            "error message should mention 'shorter than declared', got: {err}",
        );
    }

    #[test]
    fn notify_parse_invalid_utf8_replaced() {
        // Non-UTF-8 bytes should be replaced lossily; the call must return Ok.
        let bad_bytes: &[u8] = &[0xFF, 0xFE, 0xFD];
        let buf = build_notify(0, 0, 0, bad_bytes);
        let msg = Notify::read(&buf).expect("invalid UTF-8 should return Ok (lossy)");
        assert!(
            msg.message.contains('\u{FFFD}'),
            "expected U+FFFD replacement char in message, got: {:?}",
            msg.message,
        );
    }

    #[test]
    fn notify_parse_oversized_msg_len_rejected() {
        // Build a header claiming msg_len just above the cap; expect Err(InvalidData).
        let bad_len: u32 = NOTIFY_MAX_MESSAGE_LEN + 1;
        let mut buf = Vec::with_capacity(24);
        buf.extend_from_slice(&0u64.to_le_bytes()); // timestamp
        buf.extend_from_slice(&0u32.to_le_bytes()); // severity
        buf.extend_from_slice(&0u32.to_le_bytes()); // visibility
        buf.extend_from_slice(&0u32.to_le_bytes()); // what
        buf.extend_from_slice(&bad_len.to_le_bytes()); // msg_len
        let err = Notify::read(&buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
