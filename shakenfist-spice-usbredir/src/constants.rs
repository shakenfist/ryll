//! usbredir protocol constants: message types, capabilities, status codes.
#![allow(dead_code)]

/// usbredir message type IDs.
#[allow(dead_code)]
pub mod msg_type {
    pub const HELLO: u32 = 0;
    pub const DEVICE_CONNECT: u32 = 1;
    pub const DEVICE_DISCONNECT: u32 = 2;
    pub const RESET: u32 = 3;
    pub const INTERFACE_INFO: u32 = 4;
    pub const EP_INFO: u32 = 5;
    pub const SET_CONFIGURATION: u32 = 6;
    pub const GET_CONFIGURATION: u32 = 7;
    pub const CONFIGURATION_STATUS: u32 = 8;
    pub const SET_ALT_SETTING: u32 = 9;
    pub const GET_ALT_SETTING: u32 = 10;
    pub const ALT_SETTING_STATUS: u32 = 11;
    pub const START_ISO_STREAM: u32 = 12;
    pub const STOP_ISO_STREAM: u32 = 13;
    pub const ISO_STREAM_STATUS: u32 = 14;
    pub const START_INTERRUPT_RECEIVING: u32 = 15;
    pub const STOP_INTERRUPT_RECEIVING: u32 = 16;
    pub const INTERRUPT_RECEIVING_STATUS: u32 = 17;
    pub const ALLOC_BULK_STREAMS: u32 = 18;
    pub const FREE_BULK_STREAMS: u32 = 19;
    pub const BULK_STREAMS_STATUS: u32 = 20;
    pub const CANCEL_DATA_PACKET: u32 = 21;
    pub const FILTER_REJECT: u32 = 22;
    pub const FILTER_FILTER: u32 = 23;
    pub const DEVICE_DISCONNECT_ACK: u32 = 24;
    pub const START_BULK_RECEIVING: u32 = 25;
    pub const STOP_BULK_RECEIVING: u32 = 26;
    pub const BULK_RECEIVING_STATUS: u32 = 27;

    pub const CONTROL_PACKET: u32 = 100;
    pub const BULK_PACKET: u32 = 101;
    pub const ISO_PACKET: u32 = 102;
    pub const INTERRUPT_PACKET: u32 = 103;
    pub const BUFFERED_BULK_PACKET: u32 = 104;
}

/// usbredir capability bits, negotiated in the hello exchange.
#[allow(dead_code)]
pub mod cap {
    pub const BULK_STREAMS: u32 = 1 << 0;
    pub const CONNECT_DEVICE_VERSION: u32 = 1 << 1;
    pub const FILTER: u32 = 1 << 2;
    pub const DEVICE_DISCONNECT_ACK: u32 = 1 << 3;
    pub const EP_INFO_MAX_PACKET_SIZE: u32 = 1 << 4;
    pub const IDS_64BIT: u32 = 1 << 5;
    pub const BULK_LENGTH_32BIT: u32 = 1 << 6;
    pub const BULK_RECEIVING: u32 = 1 << 7;
}

/// Capabilities ryll advertises in its hello message.
pub const RYLL_CAPS: u32 =
    cap::CONNECT_DEVICE_VERSION | cap::DEVICE_DISCONNECT_ACK | cap::EP_INFO_MAX_PACKET_SIZE;

/// usbredir transfer status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Success = 0,
    Cancelled = 1,
    Inval = 2,
    Ioerror = 3,
    Stall = 4,
    Timeout = 5,
    Babble = 6,
}

impl Status {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Status::Success,
            1 => Status::Cancelled,
            2 => Status::Inval,
            3 => Status::Ioerror,
            4 => Status::Stall,
            5 => Status::Timeout,
            6 => Status::Babble,
            _ => Status::Inval,
        }
    }
}

/// USB device speed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum UsbSpeed {
    Unknown = 0,
    Low = 1,
    Full = 2,
    High = 3,
    Super = 4,
}

/// USB endpoint transfer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum EpType {
    Control = 0,
    Iso = 1,
    Bulk = 2,
    Interrupt = 3,
    Invalid = 255,
}

/// Human-readable name for a usbredir message type.
pub fn msg_type_name(t: u32) -> &'static str {
    match t {
        msg_type::HELLO => "hello",
        msg_type::DEVICE_CONNECT => "device_connect",
        msg_type::DEVICE_DISCONNECT => "device_disconnect",
        msg_type::RESET => "reset",
        msg_type::INTERFACE_INFO => "interface_info",
        msg_type::EP_INFO => "ep_info",
        msg_type::SET_CONFIGURATION => "set_configuration",
        msg_type::GET_CONFIGURATION => "get_configuration",
        msg_type::CONFIGURATION_STATUS => "configuration_status",
        msg_type::SET_ALT_SETTING => "set_alt_setting",
        msg_type::GET_ALT_SETTING => "get_alt_setting",
        msg_type::ALT_SETTING_STATUS => "alt_setting_status",
        msg_type::START_ISO_STREAM => "start_iso_stream",
        msg_type::STOP_ISO_STREAM => "stop_iso_stream",
        msg_type::ISO_STREAM_STATUS => "iso_stream_status",
        msg_type::START_INTERRUPT_RECEIVING => "start_interrupt_receiving",
        msg_type::STOP_INTERRUPT_RECEIVING => "stop_interrupt_receiving",
        msg_type::INTERRUPT_RECEIVING_STATUS => "interrupt_receiving_status",
        msg_type::ALLOC_BULK_STREAMS => "alloc_bulk_streams",
        msg_type::FREE_BULK_STREAMS => "free_bulk_streams",
        msg_type::BULK_STREAMS_STATUS => "bulk_streams_status",
        msg_type::CANCEL_DATA_PACKET => "cancel_data_packet",
        msg_type::FILTER_REJECT => "filter_reject",
        msg_type::FILTER_FILTER => "filter_filter",
        msg_type::DEVICE_DISCONNECT_ACK => "device_disconnect_ack",
        msg_type::START_BULK_RECEIVING => "start_bulk_receiving",
        msg_type::STOP_BULK_RECEIVING => "stop_bulk_receiving",
        msg_type::BULK_RECEIVING_STATUS => "bulk_receiving_status",
        msg_type::CONTROL_PACKET => "control_packet",
        msg_type::BULK_PACKET => "bulk_packet",
        msg_type::ISO_PACKET => "iso_packet",
        msg_type::INTERRUPT_PACKET => "interrupt_packet",
        msg_type::BUFFERED_BULK_PACKET => "buffered_bulk_packet",
        _ => "unknown",
    }
}
