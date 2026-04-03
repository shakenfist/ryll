//! Virtual USB mass storage device backed by a RAW disk image.
//!
//! Implements `UsbDeviceBackend` by emulating a USB mass storage device
//! using Bulk-Only Transport (BOT) and a minimal SCSI command set.
//! Reads and writes map directly to the backing RAW file.
#![allow(dead_code)]

use std::io::SeekFrom;
use std::path::PathBuf;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, info, warn};

use super::{ControlSetup, TransferResult, UsbDeviceBackend};
use crate::usbredir::constants::Status;
use crate::usbredir::messages::{DeviceConnect, EpInfo, InterfaceInfo};

// ── Constants ──────────────────────────────────────────

const BLOCK_SIZE: u32 = 512;
const VENDOR_ID: u16 = 0x1d6b; // Linux Foundation
const PRODUCT_ID: u16 = 0x0104;

// USB endpoint addresses (usbredir numbering)
const EP_BULK_OUT: u8 = 2; // USB 0x02
const EP_BULK_IN: u8 = 17; // USB 0x81

// CBW/CSW
const CBW_SIGNATURE: u32 = 0x43425355; // "USBC"
const CSW_SIGNATURE: u32 = 0x53425355; // "USBS"
const CBW_SIZE: usize = 31;
const CSW_SIZE: usize = 13;

// CSW status
const CSW_STATUS_PASSED: u8 = 0;
const CSW_STATUS_FAILED: u8 = 1;

// Maximum SCSI transfer size: 2048 blocks = 1 MB. Prevents OOM from
// a malicious server sending large READ/WRITE commands.
const MAX_TRANSFER_BLOCKS: u64 = 2048;

// SCSI opcodes
const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SENSE_6: u8 = 0x1A;
const SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;
const SCSI_WRITE_10: u8 = 0x2A;

// SCSI sense keys
const SENSE_NO_SENSE: u8 = 0x00;
const SENSE_MEDIUM_ERROR: u8 = 0x03;
const SENSE_ILLEGAL_REQUEST: u8 = 0x05;
const SENSE_DATA_PROTECT: u8 = 0x07;

// USB control requests
const USB_REQ_GET_STATUS: u8 = 0x00;
const USB_REQ_SET_CONFIGURATION: u8 = 0x09;
const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
const MSC_REQ_GET_MAX_LUN: u8 = 0xFE;
const MSC_REQ_BULK_ONLY_RESET: u8 = 0xFF;

// ── USB Descriptors ────────────────────────────────────

/// 18-byte USB device descriptor.
const DEVICE_DESCRIPTOR: [u8; 18] = [
    18,   // bLength
    0x01, // bDescriptorType = Device
    0x00,
    0x02, // bcdUSB = 2.00
    0x00, // bDeviceClass (per-interface)
    0x00, // bDeviceSubClass
    0x00, // bDeviceProtocol
    64,   // bMaxPacketSize0
    (VENDOR_ID & 0xFF) as u8,
    (VENDOR_ID >> 8) as u8,
    (PRODUCT_ID & 0xFF) as u8,
    (PRODUCT_ID >> 8) as u8,
    0x00,
    0x01, // bcdDevice = 1.00
    0x01, // iManufacturer (string index 1)
    0x02, // iProduct (string index 2)
    0x03, // iSerialNumber (string index 3)
    0x01, // bNumConfigurations
];

/// Configuration descriptor (9B config + 9B interface + 7B ep OUT + 7B ep IN = 32B).
const CONFIG_DESCRIPTOR: [u8; 32] = [
    // Configuration descriptor
    9,    // bLength
    0x02, // bDescriptorType = Configuration
    32, 0,    // wTotalLength
    1,    // bNumInterfaces
    1,    // bConfigurationValue
    0,    // iConfiguration
    0xC0, // bmAttributes (self-powered)
    0,    // bMaxPower (0 mA)
    // Interface descriptor
    9,    // bLength
    0x04, // bDescriptorType = Interface
    0,    // bInterfaceNumber
    0,    // bAlternateSetting
    2,    // bNumEndpoints
    0x08, // bInterfaceClass (Mass Storage)
    0x06, // bInterfaceSubClass (SCSI)
    0x50, // bInterfaceProtocol (Bulk-Only Transport)
    0,    // iInterface
    // Endpoint descriptor: bulk OUT 0x02
    7,    // bLength
    0x05, // bDescriptorType = Endpoint
    0x02, // bEndpointAddress (OUT, ep 2)
    0x02, // bmAttributes (Bulk)
    0x00, 0x02, // wMaxPacketSize (512)
    0,    // bInterval
    // Endpoint descriptor: bulk IN 0x81
    7,    // bLength
    0x05, // bDescriptorType = Endpoint
    0x81, // bEndpointAddress (IN, ep 1)
    0x02, // bmAttributes (Bulk)
    0x00, 0x02, // wMaxPacketSize (512)
    0,    // bInterval
];

// ── BOT state machine ──────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BotState {
    /// Waiting for a CBW on the bulk OUT endpoint.
    Idle,
    /// Expecting data from host on bulk OUT (WRITE command).
    DataOut {
        tag: u32,
        lba: u64,
        remaining: usize,
    },
    /// Have data to send to host on bulk IN, then CSW.
    DataIn,
    /// Have CSW ready to send on bulk IN.
    Status,
}

/// Parsed Command Block Wrapper.
struct Cbw {
    tag: u32,
    data_transfer_length: u32,
    direction_in: bool,
    lun: u8,
    cb_length: u8,
    cb: [u8; 16],
}

impl Cbw {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < CBW_SIZE {
            return None;
        }
        let sig = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if sig != CBW_SIGNATURE {
            return None;
        }
        let tag = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let data_transfer_length = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let direction_in = data[12] & 0x80 != 0;
        let lun = data[13] & 0x0F;
        let cb_length = data[14] & 0x1F;
        let mut cb = [0u8; 16];
        cb.copy_from_slice(&data[15..31]);
        Some(Cbw {
            tag,
            data_transfer_length,
            direction_in,
            lun,
            cb_length,
            cb,
        })
    }
}

/// Build a 13-byte Command Status Wrapper.
fn build_csw(tag: u32, data_residue: u32, status: u8) -> Vec<u8> {
    let mut csw = Vec::with_capacity(CSW_SIZE);
    csw.extend_from_slice(&CSW_SIGNATURE.to_le_bytes());
    csw.extend_from_slice(&tag.to_le_bytes());
    csw.extend_from_slice(&data_residue.to_le_bytes());
    csw.push(status);
    csw
}

// ── SCSI result ────────────────────────────────────────

struct ScsiResult {
    status: u8, // 0 = good, 2 = check condition
    data: Vec<u8>,
    sense_key: u8,
    sense_asc: u8,
    sense_ascq: u8,
}

impl ScsiResult {
    fn good(data: Vec<u8>) -> Self {
        ScsiResult {
            status: 0,
            data,
            sense_key: 0,
            sense_asc: 0,
            sense_ascq: 0,
        }
    }

    fn good_empty() -> Self {
        ScsiResult {
            status: 0,
            data: Vec::new(),
            sense_key: 0,
            sense_asc: 0,
            sense_ascq: 0,
        }
    }

    fn check_condition(sense_key: u8, asc: u8, ascq: u8) -> Self {
        ScsiResult {
            status: 2,
            data: Vec::new(),
            sense_key,
            sense_asc: asc,
            sense_ascq: ascq,
        }
    }
}

// ── VirtualMsc ─────────────────────────────────────────

/// Virtual USB mass storage device backed by a RAW disk image.
pub struct VirtualMsc {
    file: tokio::fs::File,
    file_path: PathBuf,
    read_only: bool,
    block_count: u64,

    // BOT state
    bot_state: BotState,
    pending_data: Vec<u8>,
    pending_csw: Vec<u8>,

    // Data-out accumulator for WRITE commands
    write_buf: Vec<u8>,

    // SCSI sense state (set by last failed command)
    sense_key: u8,
    sense_asc: u8,
    sense_ascq: u8,
}

impl VirtualMsc {
    /// Open a RAW disk image as a virtual USB mass storage device.
    pub async fn open(path: PathBuf, read_only: bool) -> Result<Self> {
        let file = if read_only {
            tokio::fs::File::open(&path).await?
        } else {
            tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .await?
        };

        let metadata = file.metadata().await?;
        let file_size = metadata.len();
        let block_count = file_size / BLOCK_SIZE as u64;

        if file_size % BLOCK_SIZE as u64 != 0 {
            warn!(
                "usb-disk: file size {} is not a multiple of {}, {} bytes inaccessible",
                file_size,
                BLOCK_SIZE,
                file_size % BLOCK_SIZE as u64,
            );
        }

        info!(
            "usb-disk: opened {} ({} blocks, {})",
            path.display(),
            block_count,
            if read_only { "read-only" } else { "read-write" },
        );

        Ok(VirtualMsc {
            file,
            file_path: path,
            read_only,
            block_count,
            bot_state: BotState::Idle,
            pending_data: Vec::new(),
            pending_csw: Vec::new(),
            write_buf: Vec::new(),
            sense_key: SENSE_NO_SENSE,
            sense_asc: 0,
            sense_ascq: 0,
        })
    }

    // ── Control transfer handling ──────────────────

    fn handle_control(&self, setup: &ControlSetup, data: &[u8]) -> TransferResult {
        let is_in = setup.request_type & 0x80 != 0;
        let req_type = (setup.request_type >> 5) & 3;

        match req_type {
            0 => self.handle_standard_request(setup, data, is_in),
            1 => self.handle_class_request(setup, is_in),
            _ => TransferResult::stall(),
        }
    }

    fn handle_standard_request(
        &self,
        setup: &ControlSetup,
        _data: &[u8],
        is_in: bool,
    ) -> TransferResult {
        match setup.request {
            USB_REQ_GET_DESCRIPTOR if is_in => {
                let desc_type = (setup.value >> 8) as u8;
                let desc_index = (setup.value & 0xFF) as u8;
                self.get_descriptor(desc_type, desc_index, setup.length)
            }
            USB_REQ_SET_CONFIGURATION => TransferResult::success_empty(),
            USB_REQ_GET_STATUS if is_in => TransferResult::success(vec![0x01, 0x00]), // self-powered
            _ => TransferResult::stall(),
        }
    }

    fn get_descriptor(&self, desc_type: u8, desc_index: u8, max_len: u16) -> TransferResult {
        let desc: &[u8] = match desc_type {
            1 => &DEVICE_DESCRIPTOR, // Device
            2 => &CONFIG_DESCRIPTOR, // Configuration
            3 => return self.get_string_descriptor(desc_index, max_len),
            _ => return TransferResult::stall(),
        };
        let len = desc.len().min(max_len as usize);
        TransferResult::success(desc[..len].to_vec())
    }

    fn get_string_descriptor(&self, index: u8, max_len: u16) -> TransferResult {
        let s = match index {
            0 => {
                // Language ID descriptor
                return TransferResult::success(vec![4, 0x03, 0x09, 0x04]); // US English
            }
            1 => "ryll",
            2 => "Virtual Disk",
            3 => "00000001",
            _ => return TransferResult::stall(),
        };

        // Build USB string descriptor (UTF-16LE)
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let len = 2 + utf16.len() * 2;
        let mut desc = Vec::with_capacity(len);
        desc.push(len as u8);
        desc.push(0x03); // bDescriptorType = String
        for ch in &utf16 {
            desc.push(*ch as u8);
            desc.push((*ch >> 8) as u8);
        }
        let out_len = desc.len().min(max_len as usize);
        TransferResult::success(desc[..out_len].to_vec())
    }

    fn handle_class_request(&self, setup: &ControlSetup, is_in: bool) -> TransferResult {
        match setup.request {
            MSC_REQ_GET_MAX_LUN if is_in => TransferResult::success(vec![0x00]),
            MSC_REQ_BULK_ONLY_RESET => TransferResult::success_empty(),
            _ => TransferResult::stall(),
        }
    }

    // ── Bulk OUT handling (receives CBW + write data) ──

    async fn handle_bulk_out(&mut self, data: &[u8]) -> Result<TransferResult> {
        match self.bot_state {
            BotState::Idle => self.handle_cbw(data).await,
            BotState::DataOut {
                tag,
                lba,
                remaining,
            } => self.handle_data_out(tag, lba, remaining, data).await,
            _ => {
                warn!(
                    "usb-disk: unexpected bulk OUT in state {:?}",
                    self.bot_state
                );
                Ok(TransferResult::stall())
            }
        }
    }

    async fn handle_cbw(&mut self, data: &[u8]) -> Result<TransferResult> {
        let Some(cbw) = Cbw::parse(data) else {
            warn!("usb-disk: invalid CBW ({} bytes)", data.len());
            return Ok(TransferResult::stall());
        };

        debug!(
            "usb-disk: CBW tag={} opcode=0x{:02x} dir={} len={}",
            cbw.tag,
            cbw.cb[0],
            if cbw.direction_in { "IN" } else { "OUT" },
            cbw.data_transfer_length,
        );

        let scsi_result = self.execute_scsi(&cbw).await;

        // If the SCSI handler set up a data-out phase (e.g. WRITE(10)),
        // don't overwrite the state — handle_data_out will build the CSW.
        if matches!(self.bot_state, BotState::DataOut { .. }) {
            return Ok(TransferResult::success_empty());
        }

        // Update sense state
        if scsi_result.status != 0 {
            self.sense_key = scsi_result.sense_key;
            self.sense_asc = scsi_result.sense_asc;
            self.sense_ascq = scsi_result.sense_ascq;
        }

        // Build CSW
        let data_residue = if scsi_result.status == 0 {
            cbw.data_transfer_length
                .saturating_sub(scsi_result.data.len() as u32)
        } else {
            cbw.data_transfer_length
        };
        let csw_status = if scsi_result.status == 0 {
            CSW_STATUS_PASSED
        } else {
            CSW_STATUS_FAILED
        };
        self.pending_csw = build_csw(cbw.tag, data_residue, csw_status);

        if !scsi_result.data.is_empty() && cbw.direction_in {
            // Data-in: queue data for bulk IN, followed by CSW
            self.pending_data = scsi_result.data;
            self.bot_state = BotState::DataIn;
        } else {
            // No data phase: CSW ready
            self.bot_state = BotState::Status;
        }

        Ok(TransferResult::success_empty())
    }

    async fn handle_data_out(
        &mut self,
        tag: u32,
        lba: u64,
        remaining: usize,
        data: &[u8],
    ) -> Result<TransferResult> {
        self.write_buf.extend_from_slice(data);

        let received = self.write_buf.len();
        if received >= remaining {
            // All data received — write to file
            let result = self.do_write(lba, &self.write_buf.clone()).await;
            self.write_buf.clear();

            let (csw_status, residue) = match result {
                Ok(()) => (CSW_STATUS_PASSED, 0u32),
                Err(_) => {
                    self.sense_key = SENSE_MEDIUM_ERROR;
                    self.sense_asc = 0x03; // write fault
                    self.sense_ascq = 0x00;
                    (CSW_STATUS_FAILED, remaining as u32)
                }
            };

            self.pending_csw = build_csw(tag, residue, csw_status);
            self.bot_state = BotState::Status;
        }

        Ok(TransferResult::success_empty())
    }

    // ── Bulk IN handling (sends data + CSW) ────────

    fn handle_bulk_in(&mut self, max_len: usize) -> TransferResult {
        match self.bot_state {
            BotState::DataIn => {
                let len = self.pending_data.len().min(max_len);
                let chunk: Vec<u8> = self.pending_data.drain(..len).collect();
                if self.pending_data.is_empty() {
                    self.bot_state = BotState::Status;
                }
                TransferResult::success(chunk)
            }
            BotState::Status => {
                let csw = std::mem::take(&mut self.pending_csw);
                self.bot_state = BotState::Idle;
                TransferResult::success(csw)
            }
            _ => {
                debug!("usb-disk: bulk IN in state {:?}, no data", self.bot_state);
                TransferResult::success(Vec::new())
            }
        }
    }

    // ── SCSI command dispatch ──────────────────────

    async fn execute_scsi(&mut self, cbw: &Cbw) -> ScsiResult {
        let opcode = cbw.cb[0];
        match opcode {
            SCSI_TEST_UNIT_READY => ScsiResult::good_empty(),
            SCSI_REQUEST_SENSE => self.scsi_request_sense(cbw),
            SCSI_INQUIRY => self.scsi_inquiry(cbw),
            SCSI_MODE_SENSE_6 => self.scsi_mode_sense_6(),
            SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL => ScsiResult::good_empty(),
            SCSI_READ_CAPACITY_10 => self.scsi_read_capacity_10(),
            SCSI_READ_10 => self.scsi_read_10(cbw).await,
            SCSI_WRITE_10 => self.scsi_write_10(cbw),
            _ => {
                debug!("usb-disk: unsupported SCSI opcode 0x{:02x}", opcode);
                ScsiResult::check_condition(SENSE_ILLEGAL_REQUEST, 0x20, 0x00)
            }
        }
    }

    fn scsi_request_sense(&mut self, cbw: &Cbw) -> ScsiResult {
        let alloc_len = cbw.cb[4] as usize;
        let mut sense = vec![0u8; 18];
        sense[0] = 0x70; // current errors, fixed format
        sense[2] = self.sense_key;
        sense[7] = 0x0A; // additional sense length
        sense[12] = self.sense_asc;
        sense[13] = self.sense_ascq;

        // Clear sense after reporting
        self.sense_key = SENSE_NO_SENSE;
        self.sense_asc = 0;
        self.sense_ascq = 0;

        let len = sense.len().min(alloc_len);
        sense.truncate(len);
        ScsiResult::good(sense)
    }

    fn scsi_inquiry(&self, cbw: &Cbw) -> ScsiResult {
        let alloc_len = u16::from_be_bytes([cbw.cb[3], cbw.cb[4]]) as usize;
        let mut data = vec![0u8; 36];
        data[0] = 0x00; // peripheral device type: direct access block device
        data[1] = 0x80; // removable media
        data[2] = 0x04; // SPC-2 version
        data[3] = 0x02; // response data format
        data[4] = 31; // additional length

        // Vendor identification (8 bytes, space-padded)
        data[8..16].copy_from_slice(b"ryll    ");
        // Product identification (16 bytes, space-padded)
        data[16..32].copy_from_slice(b"Virtual Disk    ");
        // Product revision (4 bytes)
        data[32..36].copy_from_slice(b"0001");

        let len = data.len().min(alloc_len);
        data.truncate(len);
        ScsiResult::good(data)
    }

    fn scsi_mode_sense_6(&self) -> ScsiResult {
        let mut data = vec![0u8; 4];
        data[0] = 3; // mode data length (excluding this byte)
        data[1] = 0; // medium type
        data[2] = if self.read_only { 0x80 } else { 0x00 }; // write-protect bit
        data[3] = 0; // block descriptor length
        ScsiResult::good(data)
    }

    fn scsi_read_capacity_10(&self) -> ScsiResult {
        let last_lba = if self.block_count > 0 {
            self.block_count - 1
        } else {
            0
        };
        let mut data = vec![0u8; 8];
        // Last LBA (big-endian, 32-bit — clamp for huge images)
        let last_lba_32 = last_lba.min(u32::MAX as u64) as u32;
        data[0..4].copy_from_slice(&last_lba_32.to_be_bytes());
        data[4..8].copy_from_slice(&BLOCK_SIZE.to_be_bytes());
        ScsiResult::good(data)
    }

    async fn scsi_read_10(&mut self, cbw: &Cbw) -> ScsiResult {
        let lba = u32::from_be_bytes([cbw.cb[2], cbw.cb[3], cbw.cb[4], cbw.cb[5]]) as u64;
        let transfer_blocks = u16::from_be_bytes([cbw.cb[7], cbw.cb[8]]) as u64;

        if transfer_blocks > MAX_TRANSFER_BLOCKS {
            return ScsiResult::check_condition(SENSE_ILLEGAL_REQUEST, 0x20, 0x00);
        }

        let byte_count = (transfer_blocks * BLOCK_SIZE as u64) as usize;

        if lba + transfer_blocks > self.block_count {
            return ScsiResult::check_condition(SENSE_MEDIUM_ERROR, 0x21, 0x00);
        }

        let offset = lba * BLOCK_SIZE as u64;
        match self.do_read(offset, byte_count).await {
            Ok(data) => ScsiResult::good(data),
            Err(e) => {
                warn!("usb-disk: read error at LBA {}: {}", lba, e);
                ScsiResult::check_condition(SENSE_MEDIUM_ERROR, 0x11, 0x00)
            }
        }
    }

    fn scsi_write_10(&mut self, cbw: &Cbw) -> ScsiResult {
        if self.read_only {
            return ScsiResult::check_condition(SENSE_DATA_PROTECT, 0x27, 0x00);
        }

        let lba = u32::from_be_bytes([cbw.cb[2], cbw.cb[3], cbw.cb[4], cbw.cb[5]]) as u64;
        let transfer_blocks = u16::from_be_bytes([cbw.cb[7], cbw.cb[8]]) as u64;

        if transfer_blocks > MAX_TRANSFER_BLOCKS {
            return ScsiResult::check_condition(SENSE_ILLEGAL_REQUEST, 0x20, 0x00);
        }

        if lba + transfer_blocks > self.block_count {
            return ScsiResult::check_condition(SENSE_MEDIUM_ERROR, 0x21, 0x00);
        }

        let byte_count = (transfer_blocks * BLOCK_SIZE as u64) as usize;

        // Set up data-out phase: the host will send data on bulk OUT
        self.bot_state = BotState::DataOut {
            tag: cbw.tag,
            lba,
            remaining: byte_count,
        };
        self.write_buf.clear();

        // Return good for now — the actual write happens when data arrives.
        // The CBW handler won't set pending_csw for data-out; it's done
        // in handle_data_out() after the data is received and written.
        ScsiResult::good_empty()
    }

    // ── File I/O helpers ───────────────────────────

    async fn do_read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(offset)).await?;
        let mut buf = vec![0u8; len];
        self.file.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn do_write(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        let offset = lba * BLOCK_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset)).await?;
        self.file.write_all(data).await?;
        self.file.flush().await?;
        Ok(())
    }

    fn reset_bot(&mut self) {
        self.bot_state = BotState::Idle;
        self.pending_data.clear();
        self.pending_csw.clear();
        self.write_buf.clear();
    }
}

// ── UsbDeviceBackend implementation ────────────────────

impl UsbDeviceBackend for VirtualMsc {
    fn device_info(&self) -> DeviceConnect {
        DeviceConnect {
            speed: 3, // High Speed
            device_class: 0x00,
            device_subclass: 0x00,
            device_protocol: 0x00,
            vendor_id: VENDOR_ID,
            product_id: PRODUCT_ID,
            device_version_bcd: 0x0100,
        }
    }

    fn endpoint_info(&self) -> EpInfo {
        let mut info = EpInfo {
            ep_type: [255u8; 32], // Invalid
            ep_interval: [0u8; 32],
            ep_interface: [0u8; 32],
            ep_max_packet_size: [0u16; 32],
        };
        // Bulk OUT (usbredir ep 2)
        info.ep_type[EP_BULK_OUT as usize] = 2; // Bulk
        info.ep_interface[EP_BULK_OUT as usize] = 0;
        info.ep_max_packet_size[EP_BULK_OUT as usize] = 512;
        // Bulk IN (usbredir ep 17)
        info.ep_type[EP_BULK_IN as usize] = 2; // Bulk
        info.ep_interface[EP_BULK_IN as usize] = 0;
        info.ep_max_packet_size[EP_BULK_IN as usize] = 512;
        info
    }

    fn interface_info(&self) -> InterfaceInfo {
        let mut info = InterfaceInfo {
            interface_count: [0u8; 32],
            interface_class: [0u8; 32],
            interface_subclass: [0u8; 32],
            interface_protocol: [0u8; 32],
        };
        info.interface_count[0] = 1;
        info.interface_class[0] = 0x08; // Mass Storage
        info.interface_subclass[0] = 0x06; // SCSI
        info.interface_protocol[0] = 0x50; // Bulk-Only Transport
        info
    }

    async fn set_configuration(&mut self, _configuration: u8) -> Result<Status> {
        Ok(Status::Success)
    }

    async fn get_configuration(&mut self) -> Result<u8> {
        Ok(1)
    }

    async fn set_alt_setting(&mut self, _interface: u8, _alt_setting: u8) -> Result<Status> {
        Ok(Status::Success)
    }

    async fn get_alt_setting(&mut self, _interface: u8) -> Result<u8> {
        Ok(0)
    }

    async fn reset(&mut self) -> Result<()> {
        self.reset_bot();
        self.sense_key = SENSE_NO_SENSE;
        self.sense_asc = 0;
        self.sense_ascq = 0;
        Ok(())
    }

    async fn control_transfer(
        &mut self,
        setup: &ControlSetup,
        data: &[u8],
    ) -> Result<TransferResult> {
        Ok(self.handle_control(setup, data))
    }

    async fn bulk_in(&mut self, endpoint: u8, max_len: usize) -> Result<TransferResult> {
        if endpoint != EP_BULK_IN {
            return Ok(TransferResult::stall());
        }
        Ok(self.handle_bulk_in(max_len))
    }

    async fn bulk_out(&mut self, endpoint: u8, data: &[u8]) -> Result<TransferResult> {
        if endpoint != EP_BULK_OUT {
            return Ok(TransferResult::stall());
        }
        self.handle_bulk_out(data).await
    }

    fn is_virtual(&self) -> bool {
        true
    }

    fn description(&self) -> String {
        let ro = if self.read_only { " [RO]" } else { "" };
        format!("RAW Disk: {}{}", self.file_path.display(), ro)
    }
}

// ── Tests ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Create a temp file of given size for testing.
    fn create_test_image(size: usize) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&vec![0u8; size]).unwrap();
        f.flush().unwrap();
        f
    }

    /// Build a CBW with the given SCSI command and parameters.
    fn make_cbw(tag: u32, transfer_len: u32, direction_in: bool, cdb: &[u8]) -> Vec<u8> {
        let mut cbw = vec![0u8; CBW_SIZE];
        cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        cbw[4..8].copy_from_slice(&tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&transfer_len.to_le_bytes());
        cbw[12] = if direction_in { 0x80 } else { 0x00 };
        cbw[13] = 0; // LUN
        cbw[14] = cdb.len().min(16) as u8;
        cbw[15..15 + cdb.len().min(16)].copy_from_slice(&cdb[..cdb.len().min(16)]);
        cbw
    }

    #[test]
    fn cbw_parse_valid() {
        let cbw_data = make_cbw(42, 512, true, &[SCSI_READ_10, 0, 0, 0, 0, 1, 0, 0, 1, 0]);
        let cbw = Cbw::parse(&cbw_data).unwrap();
        assert_eq!(cbw.tag, 42);
        assert_eq!(cbw.data_transfer_length, 512);
        assert!(cbw.direction_in);
        assert_eq!(cbw.cb[0], SCSI_READ_10);
    }

    #[test]
    fn cbw_parse_invalid_signature() {
        let mut data = vec![0u8; CBW_SIZE];
        data[0..4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(Cbw::parse(&data).is_none());
    }

    #[test]
    fn cbw_parse_too_short() {
        assert!(Cbw::parse(&[0u8; 10]).is_none());
    }

    #[test]
    fn csw_build() {
        let csw = build_csw(42, 0, CSW_STATUS_PASSED);
        assert_eq!(csw.len(), CSW_SIZE);
        assert_eq!(&csw[0..4], &CSW_SIGNATURE.to_le_bytes());
        assert_eq!(u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]), 42);
        assert_eq!(csw[12], CSW_STATUS_PASSED);
    }

    #[tokio::test]
    async fn inquiry_response() {
        let img = create_test_image(512 * 1024);
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();

        let cbw = make_cbw(1, 36, true, &[SCSI_INQUIRY, 0, 0, 0, 36, 0]);
        dev.handle_bulk_out(&cbw).await.unwrap();

        let result = dev.handle_bulk_in(36);
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.data.len(), 36);
        assert_eq!(&result.data[8..16], b"ryll    ");
        assert_eq!(&result.data[16..32], b"Virtual Disk    ");
    }

    #[tokio::test]
    async fn read_capacity() {
        let img = create_test_image(1024 * 1024); // 2048 blocks
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();

        let cbw = make_cbw(
            2,
            8,
            true,
            &[SCSI_READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        dev.handle_bulk_out(&cbw).await.unwrap();

        let result = dev.handle_bulk_in(8);
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.data.len(), 8);

        let last_lba = u32::from_be_bytes([
            result.data[0],
            result.data[1],
            result.data[2],
            result.data[3],
        ]);
        let block_size = u32::from_be_bytes([
            result.data[4],
            result.data[5],
            result.data[6],
            result.data[7],
        ]);
        assert_eq!(last_lba, 2047);
        assert_eq!(block_size, 512);
    }

    #[tokio::test]
    async fn test_unit_ready() {
        let img = create_test_image(512);
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();

        let cbw = make_cbw(3, 0, false, &[SCSI_TEST_UNIT_READY, 0, 0, 0, 0, 0]);
        dev.handle_bulk_out(&cbw).await.unwrap();

        // Should go directly to Status
        let csw = dev.handle_bulk_in(CSW_SIZE);
        assert_eq!(csw.status, Status::Success);
        assert_eq!(csw.data.len(), CSW_SIZE);
        assert_eq!(csw.data[12], CSW_STATUS_PASSED);
    }

    #[tokio::test]
    async fn read_write_round_trip() {
        let img = create_test_image(512 * 10);
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();

        // WRITE(10): 1 block at LBA 3
        let write_cdb = [SCSI_WRITE_10, 0, 0, 0, 0, 3, 0, 0, 1, 0];
        let cbw = make_cbw(10, 512, false, &write_cdb);
        dev.handle_bulk_out(&cbw).await.unwrap();

        // Send the data (512 bytes of 0xAA)
        let write_data = vec![0xAAu8; 512];
        dev.handle_bulk_out(&write_data).await.unwrap();

        // Get CSW
        let csw = dev.handle_bulk_in(CSW_SIZE);
        assert_eq!(csw.data[12], CSW_STATUS_PASSED);

        // READ(10): 1 block at LBA 3
        let read_cdb = [SCSI_READ_10, 0, 0, 0, 0, 3, 0, 0, 1, 0];
        let cbw = make_cbw(11, 512, true, &read_cdb);
        dev.handle_bulk_out(&cbw).await.unwrap();

        // Get data
        let read_result = dev.handle_bulk_in(512);
        assert_eq!(read_result.status, Status::Success);
        assert_eq!(read_result.data, vec![0xAAu8; 512]);

        // Get CSW
        let csw = dev.handle_bulk_in(CSW_SIZE);
        assert_eq!(csw.data[12], CSW_STATUS_PASSED);
    }

    #[tokio::test]
    async fn write_read_only_rejected() {
        let img = create_test_image(512 * 4);
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), true)
            .await
            .unwrap();

        let write_cdb = [SCSI_WRITE_10, 0, 0, 0, 0, 0, 0, 0, 1, 0];
        let cbw = make_cbw(20, 512, false, &write_cdb);
        dev.handle_bulk_out(&cbw).await.unwrap();

        // CSW should indicate failure
        let csw = dev.handle_bulk_in(CSW_SIZE);
        assert_eq!(csw.data[12], CSW_STATUS_FAILED);

        // REQUEST SENSE should give write-protected
        let sense_cbw = make_cbw(21, 18, true, &[SCSI_REQUEST_SENSE, 0, 0, 0, 18, 0]);
        dev.handle_bulk_out(&sense_cbw).await.unwrap();
        let sense = dev.handle_bulk_in(18);
        assert_eq!(sense.data[2], SENSE_DATA_PROTECT);
    }

    #[tokio::test]
    async fn unknown_scsi_opcode() {
        let img = create_test_image(512);
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();

        let cbw = make_cbw(30, 0, false, &[0xFF, 0, 0, 0, 0, 0]);
        dev.handle_bulk_out(&cbw).await.unwrap();

        let csw = dev.handle_bulk_in(CSW_SIZE);
        assert_eq!(csw.data[12], CSW_STATUS_FAILED);

        // Verify sense is ILLEGAL REQUEST
        let sense_cbw = make_cbw(31, 18, true, &[SCSI_REQUEST_SENSE, 0, 0, 0, 18, 0]);
        dev.handle_bulk_out(&sense_cbw).await.unwrap();
        let sense = dev.handle_bulk_in(18);
        assert_eq!(sense.data[2], SENSE_ILLEGAL_REQUEST);
    }

    #[tokio::test]
    async fn read_past_end() {
        let img = create_test_image(512 * 2); // 2 blocks
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();

        // Try to read LBA 5 (past end)
        let read_cdb = [SCSI_READ_10, 0, 0, 0, 0, 5, 0, 0, 1, 0];
        let cbw = make_cbw(40, 512, true, &read_cdb);
        dev.handle_bulk_out(&cbw).await.unwrap();

        let csw = dev.handle_bulk_in(CSW_SIZE);
        assert_eq!(csw.data[12], CSW_STATUS_FAILED);
    }

    #[tokio::test]
    async fn mode_sense_write_protect() {
        let img = create_test_image(512);

        // Read-write
        let mut dev_rw = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();
        let cbw = make_cbw(50, 4, true, &[SCSI_MODE_SENSE_6, 0, 0, 0, 4, 0]);
        dev_rw.handle_bulk_out(&cbw).await.unwrap();
        let result = dev_rw.handle_bulk_in(4);
        assert_eq!(result.data[2] & 0x80, 0x00); // not write-protected

        // Read-only
        let mut dev_ro = VirtualMsc::open(img.path().to_path_buf(), true)
            .await
            .unwrap();
        let cbw = make_cbw(51, 4, true, &[SCSI_MODE_SENSE_6, 0, 0, 0, 4, 0]);
        dev_ro.handle_bulk_out(&cbw).await.unwrap();
        let result = dev_ro.handle_bulk_in(4);
        assert_eq!(result.data[2] & 0x80, 0x80); // write-protected
    }

    #[tokio::test]
    async fn request_sense_clears_after_read() {
        let img = create_test_image(512);
        let mut dev = VirtualMsc::open(img.path().to_path_buf(), false)
            .await
            .unwrap();

        // Trigger an error (unknown opcode)
        let cbw = make_cbw(60, 0, false, &[0xFF, 0, 0, 0, 0, 0]);
        dev.handle_bulk_out(&cbw).await.unwrap();
        dev.handle_bulk_in(CSW_SIZE); // drain CSW

        // First REQUEST SENSE should have error
        let sense_cbw = make_cbw(61, 18, true, &[SCSI_REQUEST_SENSE, 0, 0, 0, 18, 0]);
        dev.handle_bulk_out(&sense_cbw).await.unwrap();
        let sense1 = dev.handle_bulk_in(18);
        dev.handle_bulk_in(CSW_SIZE); // drain CSW
        assert_eq!(sense1.data[2], SENSE_ILLEGAL_REQUEST);

        // Second REQUEST SENSE should be clear
        let sense_cbw2 = make_cbw(62, 18, true, &[SCSI_REQUEST_SENSE, 0, 0, 0, 18, 0]);
        dev.handle_bulk_out(&sense_cbw2).await.unwrap();
        let sense2 = dev.handle_bulk_in(18);
        assert_eq!(sense2.data[2], SENSE_NO_SENSE);
    }

    #[test]
    fn control_get_max_lun() {
        let setup = ControlSetup {
            endpoint: 0,
            request_type: 0xA1, // Class, Interface, IN
            request: MSC_REQ_GET_MAX_LUN,
            value: 0,
            index: 0,
            length: 1,
        };
        let img = create_test_image(512);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dev = rt
            .block_on(VirtualMsc::open(img.path().to_path_buf(), false))
            .unwrap();
        let result = dev.handle_control(&setup, &[]);
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.data, vec![0x00]);
    }

    #[test]
    fn control_get_device_descriptor() {
        let setup = ControlSetup {
            endpoint: 0,
            request_type: 0x80, // Standard, Device, IN
            request: USB_REQ_GET_DESCRIPTOR,
            value: 0x0100, // Device descriptor
            index: 0,
            length: 18,
        };
        let img = create_test_image(512);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dev = rt
            .block_on(VirtualMsc::open(img.path().to_path_buf(), false))
            .unwrap();
        let result = dev.handle_control(&setup, &[]);
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.data.len(), 18);
        assert_eq!(result.data[0], 18); // bLength
        assert_eq!(result.data[1], 0x01); // bDescriptorType
    }
}
