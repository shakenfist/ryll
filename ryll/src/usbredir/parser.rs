//! usbredir protocol parser.
//!
//! Accumulates raw bytes from the SpiceVMC channel and extracts
//! complete usbredir messages.

use anyhow::Result;

use super::constants::msg_type;
use super::messages::*;

/// Maximum usbredir payload size (16 MB). Rejects messages with larger
/// payloads to prevent OOM from a malicious server.
const MAX_PAYLOAD_SIZE: u32 = 16 * 1024 * 1024;

/// Stateful parser that buffers incoming bytes and yields complete
/// usbredir messages.
pub struct UsbredirParser {
    buf: Vec<u8>,
}

impl UsbredirParser {
    pub fn new() -> Self {
        UsbredirParser {
            buf: Vec::with_capacity(65536),
        }
    }

    /// Feed raw bytes from the VMC channel.
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Try to parse and return the next complete message.
    /// Returns `Ok(None)` if not enough data is available yet.
    pub fn next_message(&mut self) -> Result<Option<UsbredirMessage>> {
        if self.buf.len() < UsbredirHeader::SIZE {
            return Ok(None);
        }

        let header = UsbredirHeader::read(&self.buf)?;

        if header.length > MAX_PAYLOAD_SIZE {
            // Drain the header and reject — don't accumulate a huge buffer
            self.buf.drain(..UsbredirHeader::SIZE);
            anyhow::bail!(
                "usbredir payload too large: {} bytes (max {})",
                header.length,
                MAX_PAYLOAD_SIZE
            );
        }

        let total = UsbredirHeader::SIZE + header.length as usize;

        if self.buf.len() < total {
            return Ok(None);
        }

        let payload_bytes = self.buf[UsbredirHeader::SIZE..total].to_vec();
        self.buf.drain(..total);

        let payload = parse_payload(header.msg_type, &payload_bytes)?;

        Ok(Some(UsbredirMessage {
            id: header.id,
            payload,
        }))
    }
}

/// Dispatch on message type to parse the payload into a typed enum variant.
fn parse_payload(mt: u32, data: &[u8]) -> Result<UsbredirPayload> {
    let payload = match mt {
        msg_type::HELLO => UsbredirPayload::Hello(Hello::read(data)?),
        msg_type::DEVICE_CONNECT => UsbredirPayload::DeviceConnect(DeviceConnect::read(data)?),
        msg_type::DEVICE_DISCONNECT => UsbredirPayload::DeviceDisconnect,
        msg_type::RESET => UsbredirPayload::Reset,
        msg_type::INTERFACE_INFO => UsbredirPayload::InterfaceInfo(InterfaceInfo::read(data)?),
        msg_type::EP_INFO => UsbredirPayload::EpInfo(EpInfo::read(data)?),
        msg_type::SET_CONFIGURATION => {
            UsbredirPayload::SetConfiguration(SetConfiguration::read(data)?)
        }
        msg_type::GET_CONFIGURATION => UsbredirPayload::GetConfiguration,
        msg_type::CONFIGURATION_STATUS => {
            UsbredirPayload::ConfigurationStatus(ConfigurationStatus::read(data)?)
        }
        msg_type::SET_ALT_SETTING => UsbredirPayload::SetAltSetting(SetAltSetting::read(data)?),
        msg_type::GET_ALT_SETTING => UsbredirPayload::GetAltSetting(GetAltSetting::read(data)?),
        msg_type::ALT_SETTING_STATUS => {
            UsbredirPayload::AltSettingStatus(AltSettingStatus::read(data)?)
        }
        msg_type::START_INTERRUPT_RECEIVING => {
            UsbredirPayload::StartInterruptReceiving(StartInterruptReceiving::read(data)?)
        }
        msg_type::STOP_INTERRUPT_RECEIVING => {
            UsbredirPayload::StopInterruptReceiving(StopInterruptReceiving::read(data)?)
        }
        msg_type::INTERRUPT_RECEIVING_STATUS => {
            UsbredirPayload::InterruptReceivingStatus(InterruptReceivingStatus::read(data)?)
        }
        msg_type::CANCEL_DATA_PACKET => UsbredirPayload::CancelDataPacket,
        msg_type::FILTER_REJECT => UsbredirPayload::FilterReject,
        msg_type::DEVICE_DISCONNECT_ACK => UsbredirPayload::DeviceDisconnectAck,
        msg_type::CONTROL_PACKET => {
            let header = ControlPacketHeader::read(data)?;
            let pdata = data
                .get(ControlPacketHeader::SIZE..)
                .unwrap_or(&[])
                .to_vec();
            UsbredirPayload::ControlPacket {
                header,
                data: pdata,
            }
        }
        msg_type::BULK_PACKET => {
            let header = BulkPacketHeader::read(data)?;
            let pdata = data.get(BulkPacketHeader::SIZE..).unwrap_or(&[]).to_vec();
            UsbredirPayload::BulkPacket {
                header,
                data: pdata,
            }
        }
        msg_type::INTERRUPT_PACKET => {
            let header = InterruptPacketHeader::read(data)?;
            let pdata = data
                .get(InterruptPacketHeader::SIZE..)
                .unwrap_or(&[])
                .to_vec();
            UsbredirPayload::InterruptPacket {
                header,
                data: pdata,
            }
        }
        _ => UsbredirPayload::Unknown {
            msg_type: mt,
            data: data.to_vec(),
        },
    };
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw usbredir message from type, id, and payload bytes.
    fn raw_message(msg_type: u32, id: u32, payload: &[u8]) -> Vec<u8> {
        make_usbredir_message(msg_type, id, payload)
    }

    /// Build a hello payload (68 bytes).
    fn hello_payload(version: &str, caps: u32) -> Vec<u8> {
        let h = Hello {
            version: version.to_string(),
            capabilities: caps,
        };
        let mut buf = Vec::new();
        h.write(&mut buf).unwrap();
        buf
    }

    #[test]
    fn parser_single_hello() {
        let payload = hello_payload("test 1.0", 0x07);
        let raw = raw_message(msg_type::HELLO, 0, &payload);

        let mut parser = UsbredirParser::new();
        parser.feed(&raw);

        let msg = parser.next_message().unwrap().unwrap();
        assert_eq!(msg.id, 0);
        match msg.payload {
            UsbredirPayload::Hello(h) => {
                assert_eq!(h.version, "test 1.0");
                assert_eq!(h.capabilities, 0x07);
            }
            _ => panic!("expected Hello"),
        }

        // No more messages
        assert!(parser.next_message().unwrap().is_none());
    }

    #[test]
    fn parser_split_delivery() {
        let payload = hello_payload("split", 0x01);
        let raw = raw_message(msg_type::HELLO, 5, &payload);

        let mut parser = UsbredirParser::new();

        // Feed first half
        let mid = raw.len() / 2;
        parser.feed(&raw[..mid]);
        assert!(parser.next_message().unwrap().is_none());

        // Feed second half
        parser.feed(&raw[mid..]);
        let msg = parser.next_message().unwrap().unwrap();
        assert_eq!(msg.id, 5);
        match msg.payload {
            UsbredirPayload::Hello(h) => assert_eq!(h.version, "split"),
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn parser_multiple_messages() {
        let hello = raw_message(msg_type::HELLO, 1, &hello_payload("multi", 0));
        let disconnect = raw_message(msg_type::DEVICE_DISCONNECT, 2, &[]);
        let reset = raw_message(msg_type::RESET, 3, &[]);

        let mut parser = UsbredirParser::new();
        let mut all = Vec::new();
        all.extend_from_slice(&hello);
        all.extend_from_slice(&disconnect);
        all.extend_from_slice(&reset);
        parser.feed(&all);

        let m1 = parser.next_message().unwrap().unwrap();
        assert_eq!(m1.id, 1);
        assert!(matches!(m1.payload, UsbredirPayload::Hello(_)));

        let m2 = parser.next_message().unwrap().unwrap();
        assert_eq!(m2.id, 2);
        assert!(matches!(m2.payload, UsbredirPayload::DeviceDisconnect));

        let m3 = parser.next_message().unwrap().unwrap();
        assert_eq!(m3.id, 3);
        assert!(matches!(m3.payload, UsbredirPayload::Reset));

        assert!(parser.next_message().unwrap().is_none());
    }

    #[test]
    fn parser_unknown_type() {
        let raw = raw_message(9999, 10, &[0xAA, 0xBB]);
        let mut parser = UsbredirParser::new();
        parser.feed(&raw);

        let msg = parser.next_message().unwrap().unwrap();
        assert_eq!(msg.id, 10);
        match msg.payload {
            UsbredirPayload::Unknown { msg_type, data } => {
                assert_eq!(msg_type, 9999);
                assert_eq!(data, vec![0xAA, 0xBB]);
            }
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn parser_zero_payload_messages() {
        let disconnect = raw_message(msg_type::DEVICE_DISCONNECT, 1, &[]);
        let reset = raw_message(msg_type::RESET, 2, &[]);
        let cancel = raw_message(msg_type::CANCEL_DATA_PACKET, 3, &[]);
        let filter_reject = raw_message(msg_type::FILTER_REJECT, 4, &[]);
        let disconnect_ack = raw_message(msg_type::DEVICE_DISCONNECT_ACK, 5, &[]);

        let mut combined = Vec::new();
        combined.extend_from_slice(&disconnect);
        combined.extend_from_slice(&reset);
        combined.extend_from_slice(&cancel);
        combined.extend_from_slice(&filter_reject);
        combined.extend_from_slice(&disconnect_ack);

        let mut parser = UsbredirParser::new();
        parser.feed(&combined);

        assert!(matches!(
            parser.next_message().unwrap().unwrap().payload,
            UsbredirPayload::DeviceDisconnect
        ));
        assert!(matches!(
            parser.next_message().unwrap().unwrap().payload,
            UsbredirPayload::Reset
        ));
        assert!(matches!(
            parser.next_message().unwrap().unwrap().payload,
            UsbredirPayload::CancelDataPacket
        ));
        assert!(matches!(
            parser.next_message().unwrap().unwrap().payload,
            UsbredirPayload::FilterReject
        ));
        assert!(matches!(
            parser.next_message().unwrap().unwrap().payload,
            UsbredirPayload::DeviceDisconnectAck
        ));
        assert!(parser.next_message().unwrap().is_none());
    }

    #[test]
    fn parser_control_packet_with_data() {
        let mut payload = Vec::new();
        let hdr = ControlPacketHeader {
            endpoint: 0,
            request: 0x06,
            request_type: 0x80,
            status: 0,
            value: 0x0100,
            index: 0,
            length: 4,
        };
        hdr.write(&mut payload).unwrap();
        payload.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let raw = raw_message(msg_type::CONTROL_PACKET, 77, &payload);
        let mut parser = UsbredirParser::new();
        parser.feed(&raw);

        let msg = parser.next_message().unwrap().unwrap();
        assert_eq!(msg.id, 77);
        match msg.payload {
            UsbredirPayload::ControlPacket { header, data } => {
                assert_eq!(header.request, 0x06);
                assert_eq!(header.request_type, 0x80);
                assert_eq!(header.value, 0x0100);
                assert_eq!(data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
            }
            _ => panic!("expected ControlPacket"),
        }
    }

    #[test]
    fn parser_bulk_packet_with_data() {
        let mut payload = Vec::new();
        let hdr = BulkPacketHeader {
            endpoint: 2,
            status: 0,
            length: 3,
            stream_id: 0,
            length_high: 0,
        };
        hdr.write(&mut payload).unwrap();
        payload.extend_from_slice(&[0x01, 0x02, 0x03]);

        let raw = raw_message(msg_type::BULK_PACKET, 88, &payload);
        let mut parser = UsbredirParser::new();
        parser.feed(&raw);

        let msg = parser.next_message().unwrap().unwrap();
        match msg.payload {
            UsbredirPayload::BulkPacket { header, data } => {
                assert_eq!(header.endpoint, 2);
                assert_eq!(data, vec![0x01, 0x02, 0x03]);
            }
            _ => panic!("expected BulkPacket"),
        }
    }
}
