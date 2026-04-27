/// USB redirection channel handler — SpiceVMC transport + usbredir lifecycle
///
/// Handles the SPICE SpiceVMC transport, usbredir hello handshake,
/// device attachment/detachment lifecycle, and delegates configuration
/// and transfer messages to the active device backend.
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
#[cfg(target_os = "linux")]
use nusb::MaybeFuture;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

use crate::app::ByteCounter;
use crate::capture::CaptureSession;
use crate::config::VirtualDiskConfig;
use crate::notifications::{NotificationEntry, NotificationSource, SharedNotifications};
use crate::settings;
#[cfg(target_os = "linux")]
use crate::usb::real::RealDevice;
use crate::usb::virtual_msc::VirtualMsc;
use crate::usb::{is_ep_in, ControlSetup, DeviceBackend, InterruptData, UsbDeviceBackend};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, MessageHeader, Notify as NotifyMessage, Ping, SetAck,
};
use shakenfist_spice_protocol::{spicevmc_client, spicevmc_server, ChannelType, NotifySeverity};
use shakenfist_spice_usbredir::constants::{self, msg_type, msg_type_name, Status, RYLL_CAPS};
use shakenfist_spice_usbredir::messages::{
    make_usbredir_message, AltSettingStatus, BulkPacketHeader, ConfigurationStatus,
    ControlPacketHeader, Hello, InterruptPacketHeader, UsbredirMessage, UsbredirPayload,
};
use shakenfist_spice_usbredir::parser::UsbredirParser;

use super::{ChannelEvent, UsbCommand};

pub struct UsbredirChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    repaint_notify: Arc<Notify>,
    usb_rx: mpsc::Receiver<UsbCommand>,
    buffer: Vec<u8>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    notifications: SharedNotifications,

    // BaseChannel ACK state
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,

    // Statistics
    bytes_in: u64,
    bytes_out: u64,

    // usbredir protocol state
    parser: UsbredirParser,
    server_caps: u32,
    backend: Option<DeviceBackend>,

    // Virtual disks to auto-connect after hello exchange
    auto_connect_disks: Vec<VirtualDiskConfig>,

    // Interrupt polling
    interrupt_tx: mpsc::Sender<InterruptData>,
    interrupt_rx: mpsc::Receiver<InterruptData>,
    interrupt_handles: HashMap<u8, tokio::task::JoinHandle<()>>,
}

impl UsbredirChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<Notify>,
        usb_rx: mpsc::Receiver<UsbCommand>,
        auto_connect_disks: Vec<VirtualDiskConfig>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
        notifications: SharedNotifications,
    ) -> Self {
        let (interrupt_tx, interrupt_rx) = mpsc::channel(64);
        UsbredirChannel {
            stream,
            event_tx,
            repaint_notify,
            usb_rx,
            buffer: Vec::with_capacity(65536),
            capture,
            byte_counter,
            notifications,
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
            parser: UsbredirParser::new(),
            server_caps: 0,
            backend: None,
            auto_connect_disks,
            interrupt_tx,
            interrupt_rx,
            interrupt_handles: HashMap::new(),
        }
    }

    /// Run the usbredir channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("usbredir: channel started");
        self.event_tx.send(ChannelEvent::UsbChannelReady).await.ok();
        self.repaint_notify.notify_one();

        // Send our hello immediately
        self.send_hello().await?;

        loop {
            let usb_rx = &mut self.usb_rx;
            let interrupt_rx = &mut self.interrupt_rx;

            tokio::select! {
                // Network read
                result = async {
                    let mut chunk = [0u8; 65536];
                    let n = match &mut self.stream {
                        SpiceStream::Plain(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                        SpiceStream::Tls(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                    };
                    n.map(|n| (n, chunk))
                } => {
                    let (n, chunk) = result?;
                    if n == 0 {
                        info!("usbredir: channel disconnected");
                        self.stop_all_interrupt_polls();
                        self.event_tx
                            .send(ChannelEvent::Disconnected(ChannelType::Usbredir))
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
                        break;
                    }

                    self.byte_counter.add(n as u64);
                    if let Some(ref c) = self.capture {
                        c.packet_received("usbredir", &chunk[..n]);
                    }
                    self.buffer.extend_from_slice(&chunk[..n]);
                    self.bytes_in += n as u64;

                    self.process_messages().await?;
                }

                // USB commands from the app
                Some(cmd) = usb_rx.recv() => {
                    self.handle_usb_command(cmd).await?;
                }

                // Interrupt data from polling tasks
                Some(idata) = interrupt_rx.recv() => {
                    self.forward_interrupt_data(idata).await?;
                }
            }
        }

        Ok(())
    }

    // ── SPICE message processing ───────────────────────

    async fn process_messages(&mut self) -> Result<()> {
        while self.buffer.len() >= MessageHeader::SIZE {
            let header = MessageHeader::read(&self.buffer)?;
            let total_size = MessageHeader::SIZE + header.message_size as usize;

            if self.buffer.len() < total_size {
                break;
            }

            let payload = self.buffer[MessageHeader::SIZE..total_size].to_vec();
            self.buffer.drain(..total_size);

            self.message_count += 1;
            self.handle_message(header.message_type, &payload).await?;

            if self.ack_window > 0 && self.message_count - self.last_ack >= self.ack_window {
                self.send_ack().await?;
            }
        }

        Ok(())
    }

    async fn handle_message(&mut self, msg_type: u16, payload: &[u8]) -> Result<()> {
        if settings::is_verbose() {
            let msg_type_str = message_names::spicevmc_server(msg_type);
            logging::log_message(
                "received",
                "usbredir",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        match msg_type {
            spicevmc_server::DATA => {
                self.handle_vmc_data(payload).await?;
            }
            spicevmc_server::COMPRESSED_DATA => {
                self.handle_vmc_compressed_data(payload).await?;
            }
            spicevmc_server::SET_ACK => {
                let set_ack = SetAck::read(payload)?;
                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "generation={}, window={}",
                        set_ack.generation, set_ack.window
                    ));
                }
                self.ack_generation = set_ack.generation;
                self.ack_window = set_ack.window;

                let mut ack_payload = Vec::new();
                SetAck::write_ack_sync(set_ack.generation, &mut ack_payload)?;
                let response = make_message(spicevmc_client::ACK_SYNC, &ack_payload);
                self.send_with_log(spicevmc_client::ACK_SYNC, &response)
                    .await?;
            }
            spicevmc_server::PING => {
                let ping = Ping::read(payload)?;
                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "ping_id={}, timestamp={}",
                        ping.id, ping.timestamp
                    ));
                }
                let mut pong_payload = Vec::new();
                ping.write_pong(&mut pong_payload)?;
                let response = make_message(spicevmc_client::PONG, &pong_payload);
                self.send_with_log(spicevmc_client::PONG, &response).await?;
            }
            spicevmc_server::NOTIFY => {
                let notify = NotifyMessage::read(payload)?;
                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "severity={:?}, visibility={:?}, what={}, message=\"{}\"",
                        notify.severity, notify.visibility, notify.what, notify.message,
                    ));
                }
                match notify.severity {
                    NotifySeverity::Error => {
                        warn!("usbredir: server notify (error): {}", notify.message)
                    }
                    NotifySeverity::Warn => {
                        warn!("usbredir: server notify (warn): {}", notify.message)
                    }
                    NotifySeverity::Info => {
                        info!("usbredir: server notify: {}", notify.message)
                    }
                }
                let mut entry = NotificationEntry::new(
                    notify.severity,
                    NotificationSource::Spice {
                        channel: ChannelType::Usbredir,
                        what: notify.what,
                    },
                    notify.message.clone(),
                );
                if let Some(v) = notify.visibility {
                    entry = entry.with_visibility(v);
                }
                if let Ok(mut guard) = self.notifications.lock() {
                    guard.push(entry);
                }
                self.repaint_notify.notify_one();
            }
            _ => {
                logging::log_unknown_once("usbredir", msg_type, payload);
            }
        }

        Ok(())
    }

    async fn handle_vmc_data(&mut self, payload: &[u8]) -> Result<()> {
        self.parser.feed(payload);

        while let Some(msg) = self.parser.next_message()? {
            self.handle_usbredir_message(msg).await?;
        }

        Ok(())
    }

    async fn handle_vmc_compressed_data(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 5 {
            warn!(
                "usbredir: compressed_data too short: {} bytes",
                payload.len()
            );
            return Ok(());
        }

        let compression_type = payload[0];
        let uncompressed_size = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
        let compressed = &payload[5..];

        // Cap decompression size to prevent OOM from malicious server
        if uncompressed_size > 64 * 1024 * 1024 {
            warn!(
                "usbredir: decompressed size {} exceeds limit, dropping",
                uncompressed_size
            );
            return Ok(());
        }

        if settings::is_verbose() {
            logging::log_detail(&format!(
                "type={} uncompressed={} compressed={}",
                compression_type,
                uncompressed_size,
                compressed.len(),
            ));
        }

        match compression_type {
            1 => {
                let decompressed = lz4_flex::decompress(compressed, uncompressed_size)
                    .map_err(|e| anyhow::anyhow!("usbredir: LZ4 decompress failed: {}", e))?;
                if decompressed.len() != uncompressed_size {
                    warn!(
                        "usbredir: LZ4 size mismatch: expected {} got {}",
                        uncompressed_size,
                        decompressed.len(),
                    );
                }
                self.handle_vmc_data(&decompressed).await?;
            }
            0 => {
                self.handle_vmc_data(compressed).await?;
            }
            _ => {
                warn!("usbredir: unknown compression type {}", compression_type);
            }
        }

        Ok(())
    }

    // ── usbredir message handling ──────────────────────

    async fn handle_usbredir_message(&mut self, msg: UsbredirMessage) -> Result<()> {
        let type_name = payload_type_name(&msg.payload);

        match msg.payload {
            UsbredirPayload::Hello(hello) => {
                self.server_caps = hello.capabilities;
                info!(
                    "usbredir: server hello: version='{}' caps=0x{:08x}",
                    hello.version, hello.capabilities,
                );

                // Auto-connect virtual disks if configured via CLI
                let disks = std::mem::take(&mut self.auto_connect_disks);
                if let Some(disk) = disks.into_iter().next() {
                    match VirtualMsc::open(disk.path.clone(), disk.read_only).await {
                        Ok(msc) => {
                            let backend = DeviceBackend::Virtual(msc);
                            self.connect_device(backend).await?;
                        }
                        Err(e) => {
                            let msg = format!("Failed to open {}: {}", disk.path.display(), e);
                            warn!("usbredir: {}", msg);
                            self.event_tx
                                .send(ChannelEvent::UsbConnectFailed(msg))
                                .await
                                .ok();
                            self.repaint_notify.notify_one();
                        }
                    }
                }
            }

            UsbredirPayload::SetConfiguration(sc) => {
                info!(
                    "usbredir: set_configuration={} (id={})",
                    sc.configuration, msg.id
                );
                if let Some(ref mut backend) = self.backend {
                    let status = backend.set_configuration(sc.configuration).await?;
                    let resp = ConfigurationStatus {
                        status: status as u8,
                        configuration: sc.configuration,
                    };
                    let mut buf = Vec::new();
                    resp.write(&mut buf)?;
                    self.send_usbredir(msg_type::CONFIGURATION_STATUS, msg.id, &buf)
                        .await?;
                } else {
                    warn!("usbredir: set_configuration but no device connected");
                }
            }

            UsbredirPayload::GetConfiguration => {
                info!("usbredir: get_configuration (id={})", msg.id);
                if let Some(ref mut backend) = self.backend {
                    let config = backend.get_configuration().await?;
                    let resp = ConfigurationStatus {
                        status: constants::Status::Success as u8,
                        configuration: config,
                    };
                    let mut buf = Vec::new();
                    resp.write(&mut buf)?;
                    self.send_usbredir(msg_type::CONFIGURATION_STATUS, msg.id, &buf)
                        .await?;
                } else {
                    warn!("usbredir: get_configuration but no device connected");
                }
            }

            UsbredirPayload::SetAltSetting(sa) => {
                info!(
                    "usbredir: set_alt_setting iface={} alt={} (id={})",
                    sa.interface, sa.alt_setting, msg.id,
                );
                if let Some(ref mut backend) = self.backend {
                    let status = backend
                        .set_alt_setting(sa.interface, sa.alt_setting)
                        .await?;
                    let resp = AltSettingStatus {
                        status: status as u8,
                        interface: sa.interface,
                        alt_setting: sa.alt_setting,
                    };
                    let mut buf = Vec::new();
                    resp.write(&mut buf)?;
                    self.send_usbredir(msg_type::ALT_SETTING_STATUS, msg.id, &buf)
                        .await?;
                } else {
                    warn!("usbredir: set_alt_setting but no device connected");
                }
            }

            UsbredirPayload::GetAltSetting(ga) => {
                info!(
                    "usbredir: get_alt_setting iface={} (id={})",
                    ga.interface, msg.id
                );
                if let Some(ref mut backend) = self.backend {
                    let alt = backend.get_alt_setting(ga.interface).await?;
                    let resp = AltSettingStatus {
                        status: constants::Status::Success as u8,
                        interface: ga.interface,
                        alt_setting: alt,
                    };
                    let mut buf = Vec::new();
                    resp.write(&mut buf)?;
                    self.send_usbredir(msg_type::ALT_SETTING_STATUS, msg.id, &buf)
                        .await?;
                } else {
                    warn!("usbredir: get_alt_setting but no device connected");
                }
            }

            UsbredirPayload::Reset => {
                info!("usbredir: reset requested (id={})", msg.id);
                if let Some(ref mut backend) = self.backend {
                    if let Err(e) = backend.reset().await {
                        warn!("usbredir: reset failed: {}", e);
                    }
                }
            }

            UsbredirPayload::CancelDataPacket => {
                debug!(
                    "usbredir: cancel_data_packet (id={}) — cancellation not yet supported",
                    msg.id
                );
            }

            UsbredirPayload::FilterReject => {
                info!("usbredir: filter_reject");
            }

            UsbredirPayload::DeviceDisconnectAck => {
                info!("usbredir: device_disconnect_ack");
                self.backend = None;
            }

            UsbredirPayload::ControlPacket { header, data } => {
                if let Some(ref mut backend) = self.backend {
                    let is_in = header.request_type & 0x80 != 0;
                    let setup = ControlSetup {
                        endpoint: header.endpoint,
                        request_type: header.request_type,
                        request: header.request,
                        value: header.value,
                        index: header.index,
                        length: header.length,
                    };

                    let result = backend.control_transfer(&setup, &data).await?;

                    if settings::is_verbose() {
                        debug!(
                            "usbredir: control {} ep={} req=0x{:02x} rtype=0x{:02x} \
                             val=0x{:04x} idx=0x{:04x} -> status={:?} {}B",
                            if is_in { "IN" } else { "OUT" },
                            header.endpoint,
                            header.request,
                            header.request_type,
                            header.value,
                            header.index,
                            result.status,
                            result.data.len(),
                        );
                    }

                    let resp_header = ControlPacketHeader {
                        endpoint: header.endpoint,
                        request: header.request,
                        request_type: header.request_type,
                        status: result.status as u8,
                        value: header.value,
                        index: header.index,
                        length: result.data.len() as u16,
                    };
                    let mut buf = Vec::new();
                    resp_header.write(&mut buf)?;
                    buf.extend_from_slice(&result.data);
                    self.send_usbredir(msg_type::CONTROL_PACKET, msg.id, &buf)
                        .await?;
                } else {
                    warn!("usbredir: control_packet but no device connected");
                }
            }

            UsbredirPayload::BulkPacket { header, data } => {
                if let Some(ref mut backend) = self.backend {
                    let ep_in = is_ep_in(header.endpoint);
                    let actual_len = header.actual_length();

                    // Cap bulk transfer size to prevent OOM from malicious server
                    if actual_len > 16 * 1024 * 1024 {
                        warn!("usbredir: bulk transfer too large: {} bytes", actual_len);
                        let resp_header = BulkPacketHeader {
                            endpoint: header.endpoint,
                            status: Status::Inval as u8,
                            length: 0,
                            stream_id: header.stream_id,
                            length_high: 0,
                        };
                        let mut buf = Vec::new();
                        resp_header.write(&mut buf)?;
                        self.send_usbredir(msg_type::BULK_PACKET, msg.id, &buf)
                            .await?;
                        return Ok(());
                    }

                    let result = if ep_in {
                        backend
                            .bulk_in(header.endpoint, actual_len as usize)
                            .await?
                    } else {
                        backend.bulk_out(header.endpoint, &data).await?
                    };

                    if settings::is_verbose() {
                        debug!(
                            "usbredir: bulk {} ep={} -> status={:?} {}B",
                            if ep_in { "IN" } else { "OUT" },
                            header.endpoint,
                            result.status,
                            result.data.len(),
                        );
                    }

                    let data_len = result.data.len() as u32;
                    let resp_header = BulkPacketHeader {
                        endpoint: header.endpoint,
                        status: result.status as u8,
                        length: (data_len & 0xFFFF) as u16,
                        stream_id: header.stream_id,
                        length_high: ((data_len >> 16) & 0xFFFF) as u16,
                    };
                    let mut buf = Vec::new();
                    resp_header.write(&mut buf)?;
                    buf.extend_from_slice(&result.data);
                    self.send_usbredir(msg_type::BULK_PACKET, msg.id, &buf)
                        .await?;
                } else {
                    warn!("usbredir: bulk_packet but no device connected");
                }
            }

            UsbredirPayload::InterruptPacket { header, data } => {
                if let Some(ref mut backend) = self.backend {
                    if is_ep_in(header.endpoint) {
                        // Server shouldn't send IN interrupt packets — we send those
                        debug!(
                            "usbredir: ignoring interrupt IN from server (ep={})",
                            header.endpoint
                        );
                    } else {
                        // Interrupt OUT: write data to the device
                        let result = backend.interrupt_out(header.endpoint, &data).await?;
                        let resp = InterruptPacketHeader {
                            endpoint: header.endpoint,
                            status: result.status as u8,
                            length: 0,
                        };
                        let mut buf = Vec::new();
                        resp.write(&mut buf)?;
                        self.send_usbredir(msg_type::INTERRUPT_PACKET, msg.id, &buf)
                            .await?;
                    }
                } else {
                    warn!("usbredir: interrupt_packet but no device connected");
                }
            }

            UsbredirPayload::StartInterruptReceiving(sir) => {
                info!(
                    "usbredir: start_interrupt_receiving ep={} (id={})",
                    sir.endpoint, msg.id
                );
                let status = self.start_interrupt_poll(sir.endpoint).await;
                let resp = shakenfist_spice_usbredir::messages::InterruptReceivingStatus {
                    status: status as u8,
                    endpoint: sir.endpoint,
                };
                let mut buf = Vec::new();
                resp.write(&mut buf)?;
                self.send_usbredir(msg_type::INTERRUPT_RECEIVING_STATUS, msg.id, &buf)
                    .await?;
            }

            UsbredirPayload::StopInterruptReceiving(sir) => {
                info!(
                    "usbredir: stop_interrupt_receiving ep={} (id={})",
                    sir.endpoint, msg.id
                );
                self.stop_interrupt_poll(sir.endpoint);
                let resp = shakenfist_spice_usbredir::messages::InterruptReceivingStatus {
                    status: Status::Success as u8,
                    endpoint: sir.endpoint,
                };
                let mut buf = Vec::new();
                resp.write(&mut buf)?;
                self.send_usbredir(msg_type::INTERRUPT_RECEIVING_STATUS, msg.id, &buf)
                    .await?;
            }

            UsbredirPayload::Unknown { msg_type: mt, data } => {
                debug!(
                    "usbredir: unknown message type={} ({}) len={} (id={})",
                    mt,
                    type_name,
                    data.len(),
                    msg.id,
                );
            }

            _ => {
                debug!("usbredir: {} (id={})", type_name, msg.id);
            }
        }

        Ok(())
    }

    // ── USB command handling ───────────────────────────

    async fn handle_usb_command(&mut self, cmd: UsbCommand) -> Result<()> {
        match cmd {
            #[cfg(target_os = "linux")]
            UsbCommand::ConnectPhysical { bus, address } => {
                // Disconnect existing device if any
                if self.backend.is_some() {
                    self.disconnect_device().await?;
                }

                // Re-enumerate to find the device
                let devices = match nusb::list_devices().wait() {
                    Ok(iter) => iter,
                    Err(e) => {
                        let msg = format!("Failed to enumerate USB devices: {}", e);
                        warn!("usbredir: {}", msg);
                        self.event_tx
                            .send(ChannelEvent::UsbConnectFailed(msg))
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
                        return Ok(());
                    }
                };

                let nusb_info = devices
                    .into_iter()
                    .find(|d| d.busnum() == bus && d.device_address() == address);

                match nusb_info {
                    None => {
                        let msg = format!(
                            "Device not found (bus {} addr {}) — may have been unplugged",
                            bus, address,
                        );
                        warn!("usbredir: {}", msg);
                        self.event_tx
                            .send(ChannelEvent::UsbConnectFailed(msg))
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
                    }
                    Some(info) => match RealDevice::open(&info).await {
                        Ok(dev) => {
                            self.connect_device(DeviceBackend::Real(dev)).await?;
                        }
                        Err(e) => {
                            let msg = format!(
                                "Failed to open device (bus {} addr {}): {}",
                                bus, address, e,
                            );
                            warn!("usbredir: {}", msg);
                            self.event_tx
                                .send(ChannelEvent::UsbConnectFailed(msg))
                                .await
                                .ok();
                            self.repaint_notify.notify_one();
                        }
                    },
                }
            }

            UsbCommand::ConnectVirtualDisk { path, read_only } => {
                // Disconnect existing device if any
                if self.backend.is_some() {
                    self.disconnect_device().await?;
                }

                match VirtualMsc::open(path.clone(), read_only).await {
                    Ok(msc) => {
                        self.connect_device(DeviceBackend::Virtual(msc)).await?;
                    }
                    Err(e) => {
                        let msg = format!("Failed to open {}: {}", path.display(), e);
                        warn!("usbredir: {}", msg);
                        self.event_tx
                            .send(ChannelEvent::UsbConnectFailed(msg))
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
                    }
                }
            }

            UsbCommand::DisconnectDevice => {
                self.disconnect_device().await?;
            }
        }
        Ok(())
    }

    async fn connect_device(&mut self, backend: DeviceBackend) -> Result<()> {
        info!("usbredir: connecting device: {}", backend.description());

        // Send ep_info
        let ep = backend.endpoint_info();
        let mut buf = Vec::new();
        ep.write(&mut buf)?;
        self.send_usbredir(msg_type::EP_INFO, 0, &buf).await?;

        // Send interface_info
        let iface = backend.interface_info();
        buf.clear();
        iface.write(&mut buf)?;
        self.send_usbredir(msg_type::INTERFACE_INFO, 0, &buf)
            .await?;

        // Send device_connect
        let dev = backend.device_info();
        buf.clear();
        dev.write(&mut buf)?;
        self.send_usbredir(msg_type::DEVICE_CONNECT, 0, &buf)
            .await?;

        let desc = backend.description();
        self.backend = Some(backend);
        info!("usbredir: device connected: {}", desc);
        self.event_tx
            .send(ChannelEvent::UsbDeviceConnected(desc))
            .await
            .ok();
        self.repaint_notify.notify_one();
        Ok(())
    }

    async fn disconnect_device(&mut self) -> Result<()> {
        if self.backend.is_some() {
            info!("usbredir: disconnecting device");
            self.stop_all_interrupt_polls();
            self.send_usbredir(msg_type::DEVICE_DISCONNECT, 0, &[])
                .await?;
            self.backend = None;
            self.event_tx
                .send(ChannelEvent::UsbDeviceDisconnected)
                .await
                .ok();
            self.repaint_notify.notify_one();
        }
        Ok(())
    }

    // ── Interrupt polling ──────────────────────────────

    async fn start_interrupt_poll(&mut self, endpoint: u8) -> Status {
        // Limit active interrupt polls to prevent resource exhaustion
        if self.interrupt_handles.len() >= 32 {
            warn!("usbredir: too many active interrupt polls");
            return Status::Ioerror;
        }

        if self.interrupt_handles.contains_key(&endpoint) {
            debug!(
                "usbredir: interrupt poll already active for ep={}",
                endpoint
            );
            return Status::Success;
        }

        let Some(ref mut backend) = self.backend else {
            warn!("usbredir: start_interrupt_receiving but no device connected");
            return Status::Ioerror;
        };

        match backend
            .start_interrupt_in(endpoint, self.interrupt_tx.clone())
            .await
        {
            Ok(handle) => {
                self.interrupt_handles.insert(endpoint, handle);
                Status::Success
            }
            Err(e) => {
                warn!(
                    "usbredir: failed to start interrupt poll ep={}: {}",
                    endpoint, e
                );
                Status::Ioerror
            }
        }
    }

    fn stop_interrupt_poll(&mut self, endpoint: u8) {
        if let Some(handle) = self.interrupt_handles.remove(&endpoint) {
            handle.abort();
            debug!("usbredir: stopped interrupt poll ep={}", endpoint);
        }
    }

    fn stop_all_interrupt_polls(&mut self) {
        for (ep, handle) in self.interrupt_handles.drain() {
            handle.abort();
            debug!("usbredir: stopped interrupt poll ep={}", ep);
        }
    }

    async fn forward_interrupt_data(&mut self, idata: InterruptData) -> Result<()> {
        let header = InterruptPacketHeader {
            endpoint: idata.endpoint,
            status: idata.status as u8,
            length: idata.data.len() as u16,
        };
        let mut buf = Vec::new();
        header.write(&mut buf)?;
        buf.extend_from_slice(&idata.data);
        self.send_usbredir(msg_type::INTERRUPT_PACKET, 0, &buf)
            .await
    }

    // ── Hello exchange ─────────────────────────────────

    async fn send_hello(&mut self) -> Result<()> {
        let hello = Hello {
            version: "ryll usb-redir 0.1".to_string(),
            capabilities: RYLL_CAPS,
        };
        info!(
            "usbredir: sending hello: version='{}' caps=0x{:08x}",
            hello.version, hello.capabilities,
        );
        let mut buf = Vec::new();
        hello.write(&mut buf)?;
        self.send_usbredir(msg_type::HELLO, 0, &buf).await
    }

    // ── Send helpers ───────────────────────────────────

    /// Send a usbredir message wrapped in a SPICEVMC_DATA SPICE message.
    async fn send_usbredir(&mut self, usbredir_type: u32, id: u32, payload: &[u8]) -> Result<()> {
        if settings::is_verbose() {
            let name = msg_type_name(usbredir_type);
            debug!(
                "usbredir: sending {} (id={}, {} bytes)",
                name,
                id,
                payload.len()
            );
        }
        let usbredir_msg = make_usbredir_message(usbredir_type, id, payload);
        self.send_data(&usbredir_msg).await
    }

    /// Send raw bytes wrapped in a SPICEVMC_DATA SPICE message.
    async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        let msg = make_message(spicevmc_client::DATA, data);
        self.send_with_log(spicevmc_client::DATA, &msg).await
    }

    async fn send_ack(&mut self) -> Result<()> {
        let msg = make_message(spicevmc_client::ACK, &[]);
        self.send_with_log(spicevmc_client::ACK, &msg).await?;
        self.last_ack = self.message_count;
        Ok(())
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        if settings::is_verbose() {
            let msg_type_str = message_names::spicevmc_client(msg_type);
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "usbredir", msg_type, msg_type_str, payload_size);
        }
        self.send(data).await
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            c.packet_sent("usbredir", data);
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}

/// Get a human-readable name for a UsbredirPayload variant.
fn payload_type_name(payload: &UsbredirPayload) -> &'static str {
    msg_type_name(match payload {
        UsbredirPayload::Hello(_) => msg_type::HELLO,
        UsbredirPayload::DeviceConnect(_) => msg_type::DEVICE_CONNECT,
        UsbredirPayload::DeviceDisconnect => msg_type::DEVICE_DISCONNECT,
        UsbredirPayload::Reset => msg_type::RESET,
        UsbredirPayload::InterfaceInfo(_) => msg_type::INTERFACE_INFO,
        UsbredirPayload::EpInfo(_) => msg_type::EP_INFO,
        UsbredirPayload::SetConfiguration(_) => msg_type::SET_CONFIGURATION,
        UsbredirPayload::GetConfiguration => msg_type::GET_CONFIGURATION,
        UsbredirPayload::ConfigurationStatus(_) => msg_type::CONFIGURATION_STATUS,
        UsbredirPayload::SetAltSetting(_) => msg_type::SET_ALT_SETTING,
        UsbredirPayload::GetAltSetting(_) => msg_type::GET_ALT_SETTING,
        UsbredirPayload::AltSettingStatus(_) => msg_type::ALT_SETTING_STATUS,
        UsbredirPayload::StartInterruptReceiving(_) => msg_type::START_INTERRUPT_RECEIVING,
        UsbredirPayload::StopInterruptReceiving(_) => msg_type::STOP_INTERRUPT_RECEIVING,
        UsbredirPayload::InterruptReceivingStatus(_) => msg_type::INTERRUPT_RECEIVING_STATUS,
        UsbredirPayload::CancelDataPacket => msg_type::CANCEL_DATA_PACKET,
        UsbredirPayload::FilterReject => msg_type::FILTER_REJECT,
        UsbredirPayload::DeviceDisconnectAck => msg_type::DEVICE_DISCONNECT_ACK,
        UsbredirPayload::ControlPacket { .. } => msg_type::CONTROL_PACKET,
        UsbredirPayload::BulkPacket { .. } => msg_type::BULK_PACKET,
        UsbredirPayload::InterruptPacket { .. } => msg_type::INTERRUPT_PACKET,
        UsbredirPayload::Unknown { msg_type: mt, .. } => *mt,
    })
}
