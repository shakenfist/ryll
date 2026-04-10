//! Physical USB device backend using the `nusb` crate.
//!
//! Implements `UsbDeviceBackend` by forwarding all USB operations to a
//! real device on the host via the Linux kernel's usbdevfs interface.
#![allow(dead_code)]

use std::time::Duration;

use anyhow::Result;
use nusb::transfer::{
    Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient, TransferError,
};
use nusb::MaybeFuture;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{
    usb_ep_to_usbredir, usbredir_ep_to_usb, ControlSetup, DeviceSource, InterruptData,
    TransferResult, UsbDeviceBackend, UsbDeviceInfo,
};
use shakenfist_spice_usbredir::constants::Status;
use shakenfist_spice_usbredir::messages::{DeviceConnect, EpInfo, InterfaceInfo};

/// Default timeout for control and bulk transfers.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for bulk transfers (longer for large data).
const BULK_TIMEOUT: Duration = Duration::from_secs(30);

// ── Speed conversion ───────────────────────────────────

/// Map nusb Speed to usbredir speed value.
fn speed_to_usbredir(speed: Option<nusb::Speed>) -> u8 {
    match speed {
        Some(nusb::Speed::Low) => 1,
        Some(nusb::Speed::Full) => 2,
        Some(nusb::Speed::High) => 3,
        Some(nusb::Speed::Super) => 4,
        Some(nusb::Speed::SuperPlus) => 4, // treat as Super
        _ => 0,                            // Unknown
    }
}

/// Map nusb TransferType to usbredir ep_type value.
fn transfer_type_to_u8(tt: nusb::descriptors::TransferType) -> u8 {
    match tt {
        nusb::descriptors::TransferType::Control => 0,
        nusb::descriptors::TransferType::Isochronous => 1,
        nusb::descriptors::TransferType::Bulk => 2,
        nusb::descriptors::TransferType::Interrupt => 3,
    }
}

/// Map nusb TransferError to our TransferResult.
fn transfer_error_to_result(e: TransferError) -> TransferResult {
    match e {
        TransferError::Stall => TransferResult::stall(),
        TransferError::Cancelled => TransferResult::error(Status::Cancelled),
        TransferError::Disconnected => TransferResult::error(Status::Ioerror),
        TransferError::Fault => TransferResult::error(Status::Ioerror),
        _ => TransferResult::error(Status::Ioerror),
    }
}

/// Decode the request_type byte into nusb ControlType and Recipient.
fn decode_request_type(request_type: u8) -> (ControlType, Recipient) {
    let ct = match (request_type >> 5) & 3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        2 => ControlType::Vendor,
        _ => ControlType::Vendor,
    };
    let recip = match request_type & 0x1f {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        _ => Recipient::Other,
    };
    (ct, recip)
}

// ── Enumeration ────────────────────────────────────────

/// Enumerate physical USB devices on the host.
///
/// Returns an empty list if USB is inaccessible (e.g. inside a
/// container without device access).
pub fn enumerate_physical() -> Vec<UsbDeviceInfo> {
    let devices = match nusb::list_devices().wait() {
        Ok(iter) => iter,
        Err(e) => {
            warn!("usb: failed to enumerate devices: {}", e);
            return Vec::new();
        }
    };

    devices
        .map(|info| UsbDeviceInfo {
            source: DeviceSource::Physical {
                bus: info.busnum(),
                address: info.device_address(),
            },
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            name: info
                .product_string()
                .unwrap_or("Unknown USB Device")
                .to_string(),
            speed: speed_to_usbredir(info.speed()),
            device_class: info.class(),
            device_subclass: info.subclass(),
            device_protocol: info.protocol(),
        })
        .collect()
}

// ── RealDevice ─────────────────────────────────────────

/// A physical USB device opened for passthrough.
pub struct RealDevice {
    device: nusb::Device,
    interfaces: Vec<nusb::Interface>,
    device_connect: DeviceConnect,
    ep_info: EpInfo,
    iface_info: InterfaceInfo,
    configuration: u8,
    // Track alt settings ourselves since nusb doesn't provide a getter
    alt_settings: [u8; 32],
}

impl RealDevice {
    /// Open a physical USB device for passthrough.
    ///
    /// Detaches kernel drivers, claims all interfaces, and reads
    /// descriptors to populate usbredir info structs.
    pub async fn open(nusb_info: &nusb::DeviceInfo) -> Result<Self> {
        let device = nusb_info.open().await?;

        let configuration = device
            .active_configuration()
            .map(|c| c.configuration_value())
            .unwrap_or(1);

        let device_connect = DeviceConnect {
            speed: speed_to_usbredir(nusb_info.speed()),
            device_class: nusb_info.class(),
            device_subclass: nusb_info.subclass(),
            device_protocol: nusb_info.protocol(),
            vendor_id: nusb_info.vendor_id(),
            product_id: nusb_info.product_id(),
            device_version_bcd: nusb_info.device_version(),
        };

        // Claim all interfaces and read descriptors
        let mut ep_info = EpInfo {
            ep_type: [255u8; 32], // 255 = Invalid
            ep_interval: [0u8; 32],
            ep_interface: [0u8; 32],
            ep_max_packet_size: [0u16; 32],
        };
        let mut iface_info = InterfaceInfo {
            interface_count: 0,
            interface: [0u8; 32],
            interface_class: [0u8; 32],
            interface_subclass: [0u8; 32],
            interface_protocol: [0u8; 32],
        };
        let mut interfaces = Vec::new();
        let alt_settings = [0u8; 32];

        for iface_summary in nusb_info.interfaces() {
            let iface_num = iface_summary.interface_number();
            let iface = device.detach_and_claim_interface(iface_num).await?;

            // Read interface descriptor
            if let Some(desc) = iface.descriptor() {
                let idx = iface_info.interface_count as usize;
                if idx < 32 {
                    iface_info.interface[idx] = iface_num;
                    iface_info.interface_class[idx] = desc.class();
                    iface_info.interface_subclass[idx] = desc.subclass();
                    iface_info.interface_protocol[idx] = desc.protocol();
                    iface_info.interface_count += 1;

                    // Read endpoint descriptors
                    for ep_desc in desc.endpoints() {
                        let redir_ep = usb_ep_to_usbredir(ep_desc.address()) as usize;
                        if redir_ep < 32 {
                            ep_info.ep_type[redir_ep] =
                                transfer_type_to_u8(ep_desc.transfer_type());
                            ep_info.ep_interval[redir_ep] = ep_desc.interval();
                            ep_info.ep_interface[redir_ep] = iface_num;
                            ep_info.ep_max_packet_size[redir_ep] = ep_desc.max_packet_size() as u16;
                        }
                    }
                }
            }

            interfaces.push(iface);
        }

        Ok(RealDevice {
            device,
            interfaces,
            device_connect,
            ep_info,
            iface_info,
            configuration,
            alt_settings,
        })
    }

    /// Find the claimed interface that owns the given usbredir endpoint.
    fn find_interface_for_endpoint(&self, endpoint: u8) -> Option<&nusb::Interface> {
        let idx = endpoint as usize;
        if idx >= 32 {
            return None;
        }
        let iface_num = self.ep_info.ep_interface[idx];
        self.interfaces
            .iter()
            .find(|i| i.interface_number() == iface_num)
    }
}

impl UsbDeviceBackend for RealDevice {
    fn device_info(&self) -> DeviceConnect {
        self.device_connect.clone()
    }

    fn endpoint_info(&self) -> EpInfo {
        self.ep_info.clone()
    }

    fn interface_info(&self) -> InterfaceInfo {
        self.iface_info.clone()
    }

    async fn set_configuration(&mut self, configuration: u8) -> Result<Status> {
        self.device.set_configuration(configuration).await?;
        self.configuration = configuration;
        Ok(Status::Success)
    }

    async fn get_configuration(&mut self) -> Result<u8> {
        Ok(self.configuration)
    }

    async fn set_alt_setting(&mut self, interface: u8, alt_setting: u8) -> Result<Status> {
        if let Some(iface) = self
            .interfaces
            .iter()
            .find(|i| i.interface_number() == interface)
        {
            iface.set_alt_setting(alt_setting).await?;
            if (interface as usize) < 32 {
                self.alt_settings[interface as usize] = alt_setting;
            }
            Ok(Status::Success)
        } else {
            Ok(Status::Inval)
        }
    }

    async fn get_alt_setting(&mut self, interface: u8) -> Result<u8> {
        let idx = interface as usize;
        if idx < 32 {
            Ok(self.alt_settings[idx])
        } else {
            anyhow::bail!("interface {} out of range", interface)
        }
    }

    async fn reset(&mut self) -> Result<()> {
        self.device.reset().await?;
        Ok(())
    }

    async fn control_transfer(
        &mut self,
        setup: &ControlSetup,
        data: &[u8],
    ) -> Result<TransferResult> {
        let (control_type, recipient) = decode_request_type(setup.request_type);
        let is_in = setup.request_type & 0x80 != 0;

        // Use the first claimed interface for control transfers.
        // Device::control_in/out is only available on Linux/macOS;
        // Interface::control_in/out works on all platforms including Windows.
        let Some(iface) = self.interfaces.first() else {
            return Ok(TransferResult::error(Status::Inval));
        };

        if is_in {
            match iface
                .control_in(
                    ControlIn {
                        control_type,
                        recipient,
                        request: setup.request,
                        value: setup.value,
                        index: setup.index,
                        length: setup.length,
                    },
                    TRANSFER_TIMEOUT,
                )
                .await
            {
                Ok(response) => Ok(TransferResult::success(response)),
                Err(e) => Ok(transfer_error_to_result(e)),
            }
        } else {
            match iface
                .control_out(
                    ControlOut {
                        control_type,
                        recipient,
                        request: setup.request,
                        value: setup.value,
                        index: setup.index,
                        data,
                    },
                    TRANSFER_TIMEOUT,
                )
                .await
            {
                Ok(()) => Ok(TransferResult::success_empty()),
                Err(e) => Ok(transfer_error_to_result(e)),
            }
        }
    }

    async fn bulk_in(&mut self, endpoint: u8, max_len: usize) -> Result<TransferResult> {
        let usb_addr = super::usbredir_ep_to_usb(endpoint);
        let Some(iface) = self.find_interface_for_endpoint(endpoint) else {
            return Ok(TransferResult::error(Status::Inval));
        };

        let mut ep = match iface.endpoint::<Bulk, In>(usb_addr) {
            Ok(ep) => ep,
            Err(e) => {
                warn!(
                    "usb: failed to open bulk IN endpoint 0x{:02x}: {}",
                    usb_addr, e
                );
                return Ok(TransferResult::error(Status::Inval));
            }
        };

        let buf = nusb::transfer::Buffer::new(max_len);
        ep.submit(buf);
        let completion = ep.next_complete().await;

        match completion.status {
            Ok(()) => {
                let data = completion.buffer[..completion.actual_len].to_vec();
                Ok(TransferResult::success(data))
            }
            Err(e) => Ok(transfer_error_to_result(e)),
        }
    }

    async fn bulk_out(&mut self, endpoint: u8, data: &[u8]) -> Result<TransferResult> {
        let usb_addr = super::usbredir_ep_to_usb(endpoint);
        let Some(iface) = self.find_interface_for_endpoint(endpoint) else {
            return Ok(TransferResult::error(Status::Inval));
        };

        let mut ep = match iface.endpoint::<Bulk, Out>(usb_addr) {
            Ok(ep) => ep,
            Err(e) => {
                warn!(
                    "usb: failed to open bulk OUT endpoint 0x{:02x}: {}",
                    usb_addr, e
                );
                return Ok(TransferResult::error(Status::Inval));
            }
        };

        let buf: nusb::transfer::Buffer = data.into();
        ep.submit(buf);
        let completion = ep.next_complete().await;

        match completion.status {
            Ok(()) => Ok(TransferResult::success_empty()),
            Err(e) => Ok(transfer_error_to_result(e)),
        }
    }

    async fn start_interrupt_in(
        &mut self,
        endpoint: u8,
        tx: mpsc::Sender<InterruptData>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let usb_addr = usbredir_ep_to_usb(endpoint);
        let Some(iface) = self.find_interface_for_endpoint(endpoint) else {
            anyhow::bail!("no interface for endpoint {}", endpoint);
        };

        let max_packet_size = self.ep_info.ep_max_packet_size[endpoint as usize] as usize;
        let max_packet_size = if max_packet_size == 0 {
            8
        } else {
            max_packet_size
        };

        // Clone the interface (Arc-based) so the polling task owns it
        let iface_clone = iface.clone();
        let handle = tokio::task::spawn(async move {
            let mut ep = match iface_clone.endpoint::<Interrupt, In>(usb_addr) {
                Ok(ep) => ep,
                Err(e) => {
                    warn!(
                        "usb: failed to open interrupt IN endpoint 0x{:02x}: {}",
                        usb_addr, e
                    );
                    return;
                }
            };

            debug!(
                "usb: interrupt poll started for endpoint {} (0x{:02x})",
                endpoint, usb_addr
            );

            loop {
                let buf = nusb::transfer::Buffer::new(max_packet_size);
                ep.submit(buf);
                let completion = ep.next_complete().await;

                match completion.status {
                    Ok(()) => {
                        let data = completion.buffer[..completion.actual_len].to_vec();
                        if tx
                            .send(InterruptData {
                                endpoint,
                                data,
                                status: Status::Success,
                            })
                            .await
                            .is_err()
                        {
                            // Channel closed — handler is gone
                            break;
                        }
                    }
                    Err(TransferError::Cancelled) => {
                        debug!("usb: interrupt poll cancelled for endpoint {}", endpoint);
                        break;
                    }
                    Err(TransferError::Disconnected) => {
                        debug!(
                            "usb: device disconnected during interrupt poll ep={}",
                            endpoint
                        );
                        let _ = tx
                            .send(InterruptData {
                                endpoint,
                                data: Vec::new(),
                                status: Status::Ioerror,
                            })
                            .await;
                        break;
                    }
                    Err(e) => {
                        warn!("usb: interrupt transfer error ep={}: {:?}", endpoint, e);
                        let _ = tx
                            .send(InterruptData {
                                endpoint,
                                data: Vec::new(),
                                status: Status::Ioerror,
                            })
                            .await;
                        // Continue polling — transient errors are normal
                    }
                }
            }

            debug!("usb: interrupt poll ended for endpoint {}", endpoint);
        });

        Ok(handle)
    }

    async fn interrupt_out(&mut self, endpoint: u8, data: &[u8]) -> Result<TransferResult> {
        let usb_addr = usbredir_ep_to_usb(endpoint);
        let Some(iface) = self.find_interface_for_endpoint(endpoint) else {
            return Ok(TransferResult::error(Status::Inval));
        };

        let mut ep = match iface.endpoint::<Interrupt, Out>(usb_addr) {
            Ok(ep) => ep,
            Err(e) => {
                warn!(
                    "usb: failed to open interrupt OUT endpoint 0x{:02x}: {}",
                    usb_addr, e
                );
                return Ok(TransferResult::error(Status::Inval));
            }
        };

        let buf: nusb::transfer::Buffer = data.into();
        ep.submit(buf);
        let completion = ep.next_complete().await;

        match completion.status {
            Ok(()) => Ok(TransferResult::success_empty()),
            Err(e) => Ok(transfer_error_to_result(e)),
        }
    }

    fn is_virtual(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        format!(
            "{:04x}:{:04x}",
            self.device_connect.vendor_id, self.device_connect.product_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_conversion() {
        assert_eq!(speed_to_usbredir(Some(nusb::Speed::Low)), 1);
        assert_eq!(speed_to_usbredir(Some(nusb::Speed::Full)), 2);
        assert_eq!(speed_to_usbredir(Some(nusb::Speed::High)), 3);
        assert_eq!(speed_to_usbredir(Some(nusb::Speed::Super)), 4);
        assert_eq!(speed_to_usbredir(None), 0);
    }

    #[test]
    fn request_type_decoding() {
        // Standard Device IN
        let (ct, recip) = decode_request_type(0x80);
        assert_eq!(ct, ControlType::Standard);
        assert_eq!(recip, Recipient::Device);

        // Class Interface OUT
        let (ct, recip) = decode_request_type(0x21);
        assert_eq!(ct, ControlType::Class);
        assert_eq!(recip, Recipient::Interface);

        // Vendor Endpoint IN
        let (ct, recip) = decode_request_type(0xC2);
        assert_eq!(ct, ControlType::Vendor);
        assert_eq!(recip, Recipient::Endpoint);
    }

    #[test]
    fn enumerate_physical_does_not_panic() {
        // Should return a list (possibly empty) without panicking,
        // even in a container without USB access.
        let _devices = enumerate_physical();
    }
}
