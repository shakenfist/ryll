//! USB device backend abstraction.
//!
//! Defines the `UsbDeviceBackend` trait that abstracts over real (physical)
//! and virtual (emulated) USB devices.  The usbredir channel handler
//! delegates all USB operations to a backend implementation without
//! knowing whether the device is real hardware or software-emulated.
#![allow(dead_code)]

pub mod real;

use std::path::PathBuf;

use anyhow::Result;

use crate::usbredir::constants::Status;
use crate::usbredir::messages::{DeviceConnect, EpInfo, InterfaceInfo};

// ── Transfer result ────────────────────────────────────

/// Result of a USB data transfer.
///
/// Separates USB-level status (stall, timeout, etc.) from Rust errors.
/// A Rust `Err` means the backend itself failed; a `TransferResult`
/// with a non-success status means the USB transfer was rejected normally.
#[derive(Debug, Clone)]
pub struct TransferResult {
    pub status: Status,
    pub data: Vec<u8>,
}

impl TransferResult {
    pub fn success(data: Vec<u8>) -> Self {
        TransferResult {
            status: Status::Success,
            data,
        }
    }

    pub fn success_empty() -> Self {
        TransferResult {
            status: Status::Success,
            data: Vec::new(),
        }
    }

    pub fn stall() -> Self {
        TransferResult {
            status: Status::Stall,
            data: Vec::new(),
        }
    }

    pub fn error(status: Status) -> Self {
        TransferResult {
            status,
            data: Vec::new(),
        }
    }
}

// ── Control transfer setup ─────────────────────────────

/// USB control transfer setup packet fields.
///
/// Mirrors the incoming `ControlPacketHeader` from the usbredir protocol
/// minus the `status` field (which is a response-only field).
#[derive(Debug, Clone)]
pub struct ControlSetup {
    pub endpoint: u8,
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

// ── Device backend trait ───────────────────────────────

/// Abstraction over real and virtual USB devices.
///
/// The usbredir channel handler owns one `Box<dyn UsbDeviceBackend>` at
/// a time and delegates all USB operations to it.
pub trait UsbDeviceBackend: Send {
    // ── Descriptor queries ──────────────────────

    /// USB device descriptor info for usb_redir_device_connect.
    fn device_info(&self) -> DeviceConnect;

    /// Endpoint descriptor info for usb_redir_ep_info.
    fn endpoint_info(&self) -> EpInfo;

    /// Interface descriptor info for usb_redir_interface_info.
    fn interface_info(&self) -> InterfaceInfo;

    // ── Configuration management ────────────────

    /// Set USB configuration. Returns status.
    fn set_configuration(
        &mut self,
        configuration: u8,
    ) -> impl std::future::Future<Output = Result<Status>> + Send;

    /// Get current USB configuration number.
    fn get_configuration(&mut self) -> impl std::future::Future<Output = Result<u8>> + Send;

    /// Set alternate setting for an interface.
    fn set_alt_setting(
        &mut self,
        interface: u8,
        alt_setting: u8,
    ) -> impl std::future::Future<Output = Result<Status>> + Send;

    /// Get current alternate setting for an interface.
    fn get_alt_setting(
        &mut self,
        interface: u8,
    ) -> impl std::future::Future<Output = Result<u8>> + Send;

    /// Reset the USB device.
    fn reset(&mut self) -> impl std::future::Future<Output = Result<()>> + Send;

    // ── Data transfers ──────────────────────────

    /// Execute a USB control transfer.
    ///
    /// For IN transfers (device-to-host), the returned `TransferResult`
    /// contains the response data.  For OUT transfers (host-to-device),
    /// `data` carries the payload and the result data is empty.
    fn control_transfer(
        &mut self,
        setup: &ControlSetup,
        data: &[u8],
    ) -> impl std::future::Future<Output = Result<TransferResult>> + Send;

    /// Bulk IN transfer: read up to `max_len` bytes from the endpoint.
    fn bulk_in(
        &mut self,
        endpoint: u8,
        max_len: usize,
    ) -> impl std::future::Future<Output = Result<TransferResult>> + Send;

    /// Bulk OUT transfer: write `data` to the endpoint.
    fn bulk_out(
        &mut self,
        endpoint: u8,
        data: &[u8],
    ) -> impl std::future::Future<Output = Result<TransferResult>> + Send;

    // ── Metadata ────────────────────────────────

    /// Whether this is a virtual (emulated) device.
    fn is_virtual(&self) -> bool;

    /// Human-readable description for logging and UI.
    fn description(&self) -> String;
}

// ── Device enumeration types ───────────────────────────

/// Where a USB device comes from.
#[derive(Debug, Clone)]
pub enum DeviceSource {
    /// A physical USB device on the host.
    Physical { bus: u8, address: u8 },
    /// A virtual mass storage device backed by a RAW disk image.
    VirtualDisk { path: PathBuf, read_only: bool },
}

/// Information about an available USB device (real or virtual)
/// for display in the UI and device selection.
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    pub source: DeviceSource,
    pub vendor_id: u16,
    pub product_id: u16,
    pub name: String,
    pub speed: u8,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
}

impl UsbDeviceInfo {
    /// Short label for UI display.
    pub fn label(&self) -> String {
        match &self.source {
            DeviceSource::Physical { bus, address } => {
                format!(
                    "{} [{:04x}:{:04x}] (bus {} addr {})",
                    self.name, self.vendor_id, self.product_id, bus, address
                )
            }
            DeviceSource::VirtualDisk { path, read_only } => {
                let ro = if *read_only { " [RO]" } else { "" };
                format!("RAW Disk: {}{}", path.display(), ro)
            }
        }
    }
}

// ── Endpoint address helpers ───────────────────────────

/// Convert a usbredir endpoint number (0-31) to a USB endpoint address.
///
/// usbredir: 0-15 = OUT, 16-31 = IN.
/// USB: 0x00-0x0F = OUT, 0x80-0x8F = IN.
pub fn usbredir_ep_to_usb(ep: u8) -> u8 {
    if ep >= 16 {
        0x80 | (ep - 16)
    } else {
        ep
    }
}

/// Convert a USB endpoint address to usbredir endpoint numbering (0-31).
pub fn usb_ep_to_usbredir(addr: u8) -> u8 {
    if addr & 0x80 != 0 {
        16 + (addr & 0x0F)
    } else {
        addr & 0x0F
    }
}

/// Check if a usbredir endpoint number is an IN (device-to-host) endpoint.
pub fn is_ep_in(ep: u8) -> bool {
    ep >= 16
}

// ── Enumeration ────────────────────────────────────────

/// Enumerate all available USB devices (physical + configured virtual).
///
/// `virtual_disks` comes from CLI flags (`--usb-disk`).
pub fn enumerate_devices(virtual_disks: &[(PathBuf, bool)]) -> Vec<UsbDeviceInfo> {
    let mut devices = real::enumerate_physical();

    // Virtual disk devices
    for (path, read_only) in virtual_disks {
        devices.push(UsbDeviceInfo {
            source: DeviceSource::VirtualDisk {
                path: path.clone(),
                read_only: *read_only,
            },
            vendor_id: 0x1d6b,  // Linux Foundation
            product_id: 0x0104, // ryll virtual disk
            name: format!(
                "Virtual Disk ({})",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
            ),
            speed: 3, // High Speed
            device_class: 0x00,
            device_subclass: 0x00,
            device_protocol: 0x00,
        });
    }

    devices
}

// ── Tests ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_result_success() {
        let r = TransferResult::success(vec![1, 2, 3]);
        assert_eq!(r.status, Status::Success);
        assert_eq!(r.data, vec![1, 2, 3]);
    }

    #[test]
    fn transfer_result_success_empty() {
        let r = TransferResult::success_empty();
        assert_eq!(r.status, Status::Success);
        assert!(r.data.is_empty());
    }

    #[test]
    fn transfer_result_stall() {
        let r = TransferResult::stall();
        assert_eq!(r.status, Status::Stall);
        assert!(r.data.is_empty());
    }

    #[test]
    fn transfer_result_error() {
        let r = TransferResult::error(Status::Timeout);
        assert_eq!(r.status, Status::Timeout);
        assert!(r.data.is_empty());
    }

    #[test]
    fn device_info_label_physical() {
        let info = UsbDeviceInfo {
            source: DeviceSource::Physical { bus: 1, address: 5 },
            vendor_id: 0x0781,
            product_id: 0x5583,
            name: "SanDisk Ultra".to_string(),
            speed: 3,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
        };
        let label = info.label();
        assert!(label.contains("SanDisk Ultra"));
        assert!(label.contains("0781:5583"));
        assert!(label.contains("bus 1"));
        assert!(label.contains("addr 5"));
    }

    #[test]
    fn device_info_label_virtual_rw() {
        let info = UsbDeviceInfo {
            source: DeviceSource::VirtualDisk {
                path: PathBuf::from("/tmp/test.raw"),
                read_only: false,
            },
            vendor_id: 0x1d6b,
            product_id: 0x0104,
            name: "Virtual Disk".to_string(),
            speed: 3,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
        };
        let label = info.label();
        assert!(label.contains("RAW Disk:"));
        assert!(label.contains("/tmp/test.raw"));
        assert!(!label.contains("[RO]"));
    }

    #[test]
    fn device_info_label_virtual_ro() {
        let info = UsbDeviceInfo {
            source: DeviceSource::VirtualDisk {
                path: PathBuf::from("/data/image.raw"),
                read_only: true,
            },
            vendor_id: 0x1d6b,
            product_id: 0x0104,
            name: "Virtual Disk".to_string(),
            speed: 3,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
        };
        let label = info.label();
        assert!(label.contains("[RO]"));
    }

    #[test]
    fn endpoint_out_mapping() {
        assert_eq!(usbredir_ep_to_usb(0), 0x00);
        assert_eq!(usbredir_ep_to_usb(1), 0x01);
        assert_eq!(usbredir_ep_to_usb(2), 0x02);
        assert_eq!(usbredir_ep_to_usb(15), 0x0F);
    }

    #[test]
    fn endpoint_in_mapping() {
        assert_eq!(usbredir_ep_to_usb(16), 0x80);
        assert_eq!(usbredir_ep_to_usb(17), 0x81);
        assert_eq!(usbredir_ep_to_usb(18), 0x82);
        assert_eq!(usbredir_ep_to_usb(31), 0x8F);
    }

    #[test]
    fn endpoint_reverse_mapping() {
        assert_eq!(usb_ep_to_usbredir(0x00), 0);
        assert_eq!(usb_ep_to_usbredir(0x02), 2);
        assert_eq!(usb_ep_to_usbredir(0x81), 17);
        assert_eq!(usb_ep_to_usbredir(0x82), 18);
    }

    #[test]
    fn endpoint_round_trip() {
        for ep in 0..32u8 {
            assert_eq!(usb_ep_to_usbredir(usbredir_ep_to_usb(ep)), ep);
        }
    }

    #[test]
    fn endpoint_direction() {
        assert!(!is_ep_in(0));
        assert!(!is_ep_in(2));
        assert!(!is_ep_in(15));
        assert!(is_ep_in(16));
        assert!(is_ep_in(17));
        assert!(is_ep_in(31));
    }

    #[test]
    fn enumerate_empty() {
        let devices = enumerate_devices(&[]);
        // May return physical devices if USB is accessible; at minimum no panic
        let _count = devices.len();
    }

    #[test]
    fn enumerate_includes_virtual_disks() {
        let disks = vec![
            (PathBuf::from("/tmp/test.raw"), false),
            (PathBuf::from("/data/readonly.raw"), true),
        ];
        let devices = enumerate_devices(&disks);

        // Filter to just the virtual devices (physical count varies by environment)
        let virtual_devs: Vec<_> = devices
            .iter()
            .filter(|d| matches!(d.source, DeviceSource::VirtualDisk { .. }))
            .collect();
        assert_eq!(virtual_devs.len(), 2);

        assert_eq!(virtual_devs[0].vendor_id, 0x1d6b);
        assert_eq!(virtual_devs[0].product_id, 0x0104);
        assert!(virtual_devs[0].name.contains("test.raw"));
        assert!(matches!(
            &virtual_devs[0].source,
            DeviceSource::VirtualDisk {
                read_only: false,
                ..
            }
        ));

        assert!(matches!(
            &virtual_devs[1].source,
            DeviceSource::VirtualDisk {
                read_only: true,
                ..
            }
        ));
    }
}
