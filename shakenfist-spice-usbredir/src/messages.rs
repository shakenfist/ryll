//! usbredir protocol message structures.
//!
//! Each struct corresponds to a usbredir message type with `read()` for
//! deserialisation and `write()` for serialisation.  All multi-byte
//! integers are little-endian on the wire.
//!
//! Many types and methods are defined here for use in later phases
//! (device backend, transfers, virtual MSC).
#![allow(dead_code)]

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor, Read};

// ── Header ─────────────────────────────────────────────

/// 12-byte usbredir packet header (32-bit IDs).
#[derive(Debug, Clone)]
pub struct UsbredirHeader {
    pub msg_type: u32,
    pub length: u32,
    pub id: u32,
}

impl UsbredirHeader {
    pub const SIZE: usize = 12;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for usbredir header",
            ));
        }
        let mut c = Cursor::new(data);
        Ok(UsbredirHeader {
            msg_type: c.read_u32::<LittleEndian>()?,
            length: c.read_u32::<LittleEndian>()?,
            id: c.read_u32::<LittleEndian>()?,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(self.msg_type)?;
        buf.write_u32::<LittleEndian>(self.length)?;
        buf.write_u32::<LittleEndian>(self.id)?;
        Ok(())
    }
}

/// Build a complete usbredir message (header + payload).
pub fn make_usbredir_message(msg_type: u32, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(UsbredirHeader::SIZE + payload.len());
    let header = UsbredirHeader {
        msg_type,
        length: payload.len() as u32,
        id,
    };
    header.write(&mut buf).expect("write to Vec cannot fail");
    buf.extend_from_slice(payload);
    buf
}

// ── Control messages ───────────────────────────────────

/// Hello (type 0) — 68 bytes: 64-byte version string + 4-byte capabilities.
#[derive(Debug, Clone)]
pub struct Hello {
    pub version: String,
    pub capabilities: u32,
}

impl Hello {
    pub const SIZE: usize = 68;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for Hello",
            ));
        }
        let version_bytes = &data[..64];
        let end = version_bytes.iter().position(|&b| b == 0).unwrap_or(64);
        let version = String::from_utf8_lossy(&version_bytes[..end]).into_owned();
        let mut c = Cursor::new(&data[64..]);
        let capabilities = c.read_u32::<LittleEndian>()?;
        Ok(Hello {
            version,
            capabilities,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        let mut version_buf = [0u8; 64];
        let bytes = self.version.as_bytes();
        let len = bytes.len().min(63); // leave room for null terminator
        version_buf[..len].copy_from_slice(&bytes[..len]);
        buf.extend_from_slice(&version_buf);
        buf.write_u32::<LittleEndian>(self.capabilities)?;
        Ok(())
    }
}

/// DeviceConnect (type 1) — 10 bytes.
#[derive(Debug, Clone)]
pub struct DeviceConnect {
    pub speed: u8,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_version_bcd: u16,
}

impl DeviceConnect {
    pub const SIZE: usize = 10;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for DeviceConnect",
            ));
        }
        let mut c = Cursor::new(data);
        Ok(DeviceConnect {
            speed: c.read_u8()?,
            device_class: c.read_u8()?,
            device_subclass: c.read_u8()?,
            device_protocol: c.read_u8()?,
            vendor_id: c.read_u16::<LittleEndian>()?,
            product_id: c.read_u16::<LittleEndian>()?,
            device_version_bcd: c.read_u16::<LittleEndian>()?,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.speed)?;
        buf.write_u8(self.device_class)?;
        buf.write_u8(self.device_subclass)?;
        buf.write_u8(self.device_protocol)?;
        buf.write_u16::<LittleEndian>(self.vendor_id)?;
        buf.write_u16::<LittleEndian>(self.product_id)?;
        buf.write_u16::<LittleEndian>(self.device_version_bcd)?;
        Ok(())
    }
}

/// InterfaceInfo (type 4) — 132 bytes.
///
/// Wire format (little-endian):
///   u32  interface_count          — number of valid entries
///   u8   interface[32]            — interface numbers
///   u8   interface_class[32]
///   u8   interface_subclass[32]
///   u8   interface_protocol[32]
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub interface_count: u32,
    pub interface: [u8; 32],
    pub interface_class: [u8; 32],
    pub interface_subclass: [u8; 32],
    pub interface_protocol: [u8; 32],
}

impl InterfaceInfo {
    pub const SIZE: usize = 132;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for InterfaceInfo",
            ));
        }
        let mut rdr = io::Cursor::new(data);
        let interface_count = rdr.read_u32::<LittleEndian>()?;
        let mut interface = [0u8; 32];
        let mut interface_class = [0u8; 32];
        let mut interface_subclass = [0u8; 32];
        let mut interface_protocol = [0u8; 32];
        rdr.read_exact(&mut interface)?;
        rdr.read_exact(&mut interface_class)?;
        rdr.read_exact(&mut interface_subclass)?;
        rdr.read_exact(&mut interface_protocol)?;
        Ok(InterfaceInfo {
            interface_count,
            interface,
            interface_class,
            interface_subclass,
            interface_protocol,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(self.interface_count)?;
        buf.extend_from_slice(&self.interface);
        buf.extend_from_slice(&self.interface_class);
        buf.extend_from_slice(&self.interface_subclass);
        buf.extend_from_slice(&self.interface_protocol);
        Ok(())
    }
}

/// EpInfo (type 5) — 160 bytes.
#[derive(Debug, Clone)]
pub struct EpInfo {
    pub ep_type: [u8; 32],
    pub ep_interval: [u8; 32],
    pub ep_interface: [u8; 32],
    pub ep_max_packet_size: [u16; 32],
}

impl EpInfo {
    pub const SIZE: usize = 160;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for EpInfo",
            ));
        }
        let mut info = EpInfo {
            ep_type: [0u8; 32],
            ep_interval: [0u8; 32],
            ep_interface: [0u8; 32],
            ep_max_packet_size: [0u16; 32],
        };
        info.ep_type.copy_from_slice(&data[0..32]);
        info.ep_interval.copy_from_slice(&data[32..64]);
        info.ep_interface.copy_from_slice(&data[64..96]);
        let mut c = Cursor::new(&data[96..160]);
        for i in 0..32 {
            info.ep_max_packet_size[i] = c.read_u16::<LittleEndian>()?;
        }
        Ok(info)
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.extend_from_slice(&self.ep_type);
        buf.extend_from_slice(&self.ep_interval);
        buf.extend_from_slice(&self.ep_interface);
        for &size in &self.ep_max_packet_size {
            buf.write_u16::<LittleEndian>(size)?;
        }
        Ok(())
    }
}

/// SetConfiguration (type 6) — 1 byte.
#[derive(Debug, Clone)]
pub struct SetConfiguration {
    pub configuration: u8,
}

impl SetConfiguration {
    pub const SIZE: usize = 1;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for SetConfiguration",
            ));
        }
        Ok(SetConfiguration {
            configuration: data[0],
        })
    }
}

/// ConfigurationStatus (type 8) — 2 bytes.
#[derive(Debug, Clone)]
pub struct ConfigurationStatus {
    pub status: u8,
    pub configuration: u8,
}

impl ConfigurationStatus {
    pub const SIZE: usize = 2;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for ConfigurationStatus",
            ));
        }
        Ok(ConfigurationStatus {
            status: data[0],
            configuration: data[1],
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.status)?;
        buf.write_u8(self.configuration)?;
        Ok(())
    }
}

/// SetAltSetting (type 9) — 2 bytes.
#[derive(Debug, Clone)]
pub struct SetAltSetting {
    pub interface: u8,
    pub alt_setting: u8,
}

impl SetAltSetting {
    pub const SIZE: usize = 2;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for SetAltSetting",
            ));
        }
        Ok(SetAltSetting {
            interface: data[0],
            alt_setting: data[1],
        })
    }
}

/// GetAltSetting (type 10) — 1 byte.
#[derive(Debug, Clone)]
pub struct GetAltSetting {
    pub interface: u8,
}

impl GetAltSetting {
    pub const SIZE: usize = 1;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for GetAltSetting",
            ));
        }
        Ok(GetAltSetting { interface: data[0] })
    }
}

/// AltSettingStatus (type 11) — 3 bytes.
#[derive(Debug, Clone)]
pub struct AltSettingStatus {
    pub status: u8,
    pub interface: u8,
    pub alt_setting: u8,
}

impl AltSettingStatus {
    pub const SIZE: usize = 3;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for AltSettingStatus",
            ));
        }
        Ok(AltSettingStatus {
            status: data[0],
            interface: data[1],
            alt_setting: data[2],
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.status)?;
        buf.write_u8(self.interface)?;
        buf.write_u8(self.alt_setting)?;
        Ok(())
    }
}

/// StartInterruptReceiving (type 15) — 1 byte.
#[derive(Debug, Clone)]
pub struct StartInterruptReceiving {
    pub endpoint: u8,
}

impl StartInterruptReceiving {
    pub const SIZE: usize = 1;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for StartInterruptReceiving",
            ));
        }
        Ok(StartInterruptReceiving { endpoint: data[0] })
    }
}

/// StopInterruptReceiving (type 16) — 1 byte.
#[derive(Debug, Clone)]
pub struct StopInterruptReceiving {
    pub endpoint: u8,
}

impl StopInterruptReceiving {
    pub const SIZE: usize = 1;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for StopInterruptReceiving",
            ));
        }
        Ok(StopInterruptReceiving { endpoint: data[0] })
    }
}

/// InterruptReceivingStatus (type 17) — 2 bytes.
#[derive(Debug, Clone)]
pub struct InterruptReceivingStatus {
    pub status: u8,
    pub endpoint: u8,
}

impl InterruptReceivingStatus {
    pub const SIZE: usize = 2;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for InterruptReceivingStatus",
            ));
        }
        Ok(InterruptReceivingStatus {
            status: data[0],
            endpoint: data[1],
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.status)?;
        buf.write_u8(self.endpoint)?;
        Ok(())
    }
}

// ── Data transfer messages ─────────────────────────────

/// ControlPacket header (type 100) — 10 bytes, data follows.
#[derive(Debug, Clone)]
pub struct ControlPacketHeader {
    pub endpoint: u8,
    pub request: u8,
    pub request_type: u8,
    pub status: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl ControlPacketHeader {
    pub const SIZE: usize = 10;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for ControlPacketHeader",
            ));
        }
        let mut c = Cursor::new(data);
        Ok(ControlPacketHeader {
            endpoint: c.read_u8()?,
            request: c.read_u8()?,
            request_type: c.read_u8()?,
            status: c.read_u8()?,
            value: c.read_u16::<LittleEndian>()?,
            index: c.read_u16::<LittleEndian>()?,
            length: c.read_u16::<LittleEndian>()?,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.endpoint)?;
        buf.write_u8(self.request)?;
        buf.write_u8(self.request_type)?;
        buf.write_u8(self.status)?;
        buf.write_u16::<LittleEndian>(self.value)?;
        buf.write_u16::<LittleEndian>(self.index)?;
        buf.write_u16::<LittleEndian>(self.length)?;
        Ok(())
    }
}

/// BulkPacket header (type 101) — 10 bytes, data follows.
#[derive(Debug, Clone)]
pub struct BulkPacketHeader {
    pub endpoint: u8,
    pub status: u8,
    pub length: u16,
    pub stream_id: u32,
    pub length_high: u16,
}

impl BulkPacketHeader {
    pub const SIZE: usize = 10;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for BulkPacketHeader",
            ));
        }
        let mut c = Cursor::new(data);
        Ok(BulkPacketHeader {
            endpoint: c.read_u8()?,
            status: c.read_u8()?,
            length: c.read_u16::<LittleEndian>()?,
            stream_id: c.read_u32::<LittleEndian>()?,
            length_high: c.read_u16::<LittleEndian>()?,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.endpoint)?;
        buf.write_u8(self.status)?;
        buf.write_u16::<LittleEndian>(self.length)?;
        buf.write_u32::<LittleEndian>(self.stream_id)?;
        buf.write_u16::<LittleEndian>(self.length_high)?;
        Ok(())
    }

    /// Actual transfer length combining low and high 16-bit halves.
    pub fn actual_length(&self) -> u32 {
        ((self.length_high as u32) << 16) | (self.length as u32)
    }
}

/// InterruptPacket header (type 103) — 4 bytes, data follows.
#[derive(Debug, Clone)]
pub struct InterruptPacketHeader {
    pub endpoint: u8,
    pub status: u8,
    pub length: u16,
}

impl InterruptPacketHeader {
    pub const SIZE: usize = 4;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "not enough data for InterruptPacketHeader",
            ));
        }
        let mut c = Cursor::new(data);
        Ok(InterruptPacketHeader {
            endpoint: c.read_u8()?,
            status: c.read_u8()?,
            length: c.read_u16::<LittleEndian>()?,
        })
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(self.endpoint)?;
        buf.write_u8(self.status)?;
        buf.write_u16::<LittleEndian>(self.length)?;
        Ok(())
    }
}

// ── Parsed message enum ────────────────────────────────

/// A fully parsed usbredir message.
#[derive(Debug, Clone)]
pub struct UsbredirMessage {
    pub id: u32,
    pub payload: UsbredirPayload,
}

/// Typed payload for each usbredir message type.
#[derive(Debug, Clone)]
pub enum UsbredirPayload {
    Hello(Hello),
    DeviceConnect(DeviceConnect),
    DeviceDisconnect,
    Reset,
    InterfaceInfo(InterfaceInfo),
    EpInfo(EpInfo),
    SetConfiguration(SetConfiguration),
    GetConfiguration,
    ConfigurationStatus(ConfigurationStatus),
    SetAltSetting(SetAltSetting),
    GetAltSetting(GetAltSetting),
    AltSettingStatus(AltSettingStatus),
    StartInterruptReceiving(StartInterruptReceiving),
    StopInterruptReceiving(StopInterruptReceiving),
    InterruptReceivingStatus(InterruptReceivingStatus),
    CancelDataPacket,
    FilterReject,
    DeviceDisconnectAck,
    ControlPacket {
        header: ControlPacketHeader,
        data: Vec<u8>,
    },
    BulkPacket {
        header: BulkPacketHeader,
        data: Vec<u8>,
    },
    InterruptPacket {
        header: InterruptPacketHeader,
        data: Vec<u8>,
    },
    /// Message type we don't fully parse.
    Unknown {
        msg_type: u32,
        data: Vec<u8>,
    },
}

// ── Tests ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let orig = UsbredirHeader {
            msg_type: 42,
            length: 100,
            id: 7,
        };
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        assert_eq!(buf.len(), UsbredirHeader::SIZE);
        let parsed = UsbredirHeader::read(&buf).unwrap();
        assert_eq!(parsed.msg_type, 42);
        assert_eq!(parsed.length, 100);
        assert_eq!(parsed.id, 7);
    }

    #[test]
    fn hello_round_trip() {
        let orig = Hello {
            version: "ryll test 1.0".to_string(),
            capabilities: 0x17,
        };
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        assert_eq!(buf.len(), Hello::SIZE);
        let parsed = Hello::read(&buf).unwrap();
        assert_eq!(parsed.version, "ryll test 1.0");
        assert_eq!(parsed.capabilities, 0x17);
    }

    #[test]
    fn hello_empty_version() {
        let orig = Hello {
            version: String::new(),
            capabilities: 0,
        };
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        let parsed = Hello::read(&buf).unwrap();
        assert_eq!(parsed.version, "");
        assert_eq!(parsed.capabilities, 0);
    }

    #[test]
    fn hello_max_length_version() {
        let long = "A".repeat(63); // max before null terminator
        let orig = Hello {
            version: long.clone(),
            capabilities: 0xFF,
        };
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        let parsed = Hello::read(&buf).unwrap();
        assert_eq!(parsed.version, long);
    }

    #[test]
    fn device_connect_round_trip() {
        let orig = DeviceConnect {
            speed: 3,
            device_class: 0x08,
            device_subclass: 0x06,
            device_protocol: 0x50,
            vendor_id: 0x1d6b,
            product_id: 0x0104,
            device_version_bcd: 0x0200,
        };
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        assert_eq!(buf.len(), DeviceConnect::SIZE);
        let parsed = DeviceConnect::read(&buf).unwrap();
        assert_eq!(parsed.speed, 3);
        assert_eq!(parsed.vendor_id, 0x1d6b);
        assert_eq!(parsed.product_id, 0x0104);
        assert_eq!(parsed.device_version_bcd, 0x0200);
    }

    #[test]
    fn interface_info_round_trip() {
        let mut orig = InterfaceInfo {
            interface_count: 1,
            interface: [0u8; 32],
            interface_class: [0u8; 32],
            interface_subclass: [0u8; 32],
            interface_protocol: [0u8; 32],
        };
        orig.interface[0] = 0;
        orig.interface_class[0] = 0x08;
        orig.interface_subclass[0] = 0x06;
        orig.interface_protocol[0] = 0x50;
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        assert_eq!(buf.len(), InterfaceInfo::SIZE);
        let parsed = InterfaceInfo::read(&buf).unwrap();
        assert_eq!(parsed.interface_count, 1);
        assert_eq!(parsed.interface[0], 0);
        assert_eq!(parsed.interface_class[0], 0x08);
        assert_eq!(parsed.interface_protocol[0], 0x50);
    }

    #[test]
    fn ep_info_round_trip() {
        let mut orig = EpInfo {
            ep_type: [0u8; 32],
            ep_interval: [0u8; 32],
            ep_interface: [0u8; 32],
            ep_max_packet_size: [0u16; 32],
        };
        orig.ep_type[1] = 2; // bulk
        orig.ep_max_packet_size[1] = 512;
        orig.ep_max_packet_size[17] = 512; // EP 1 IN
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        assert_eq!(buf.len(), EpInfo::SIZE);
        let parsed = EpInfo::read(&buf).unwrap();
        assert_eq!(parsed.ep_type[1], 2);
        assert_eq!(parsed.ep_max_packet_size[1], 512);
        assert_eq!(parsed.ep_max_packet_size[17], 512);
    }

    #[test]
    fn control_packet_header_round_trip() {
        let orig = ControlPacketHeader {
            endpoint: 0,
            request: 0x06,
            request_type: 0x80,
            status: 0,
            value: 0x0100,
            index: 0,
            length: 18,
        };
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        assert_eq!(buf.len(), ControlPacketHeader::SIZE);
        let parsed = ControlPacketHeader::read(&buf).unwrap();
        assert_eq!(parsed.request, 0x06);
        assert_eq!(parsed.request_type, 0x80);
        assert_eq!(parsed.value, 0x0100);
        assert_eq!(parsed.length, 18);
    }

    #[test]
    fn bulk_packet_header_round_trip() {
        let orig = BulkPacketHeader {
            endpoint: 2,
            status: 0,
            length: 0x1234,
            stream_id: 0,
            length_high: 0x0056,
        };
        let mut buf = Vec::new();
        orig.write(&mut buf).unwrap();
        assert_eq!(buf.len(), BulkPacketHeader::SIZE);
        let parsed = BulkPacketHeader::read(&buf).unwrap();
        assert_eq!(parsed.endpoint, 2);
        assert_eq!(parsed.length, 0x1234);
        assert_eq!(parsed.length_high, 0x0056);
        assert_eq!(parsed.actual_length(), 0x0056_1234);
    }

    #[test]
    fn make_usbredir_message_builds_correctly() {
        let payload = [1u8, 2, 3, 4];
        let msg = make_usbredir_message(99, 42, &payload);
        assert_eq!(msg.len(), UsbredirHeader::SIZE + 4);
        let header = UsbredirHeader::read(&msg).unwrap();
        assert_eq!(header.msg_type, 99);
        assert_eq!(header.length, 4);
        assert_eq!(header.id, 42);
        assert_eq!(&msg[UsbredirHeader::SIZE..], &[1, 2, 3, 4]);
    }
}
