/// USB redirection channel handler — SpiceVMC transport + usbredir lifecycle
///
/// Handles the SPICE SpiceVMC transport, usbredir hello handshake,
/// device attachment/detachment lifecycle, and delegates configuration
/// and transfer messages to the active device backend.
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::ByteCounter;
use crate::capture::CaptureSession;
use crate::protocol::link::SpiceStream;
use crate::protocol::logging::{self, message_names};
use crate::protocol::messages::{make_message, MessageHeader, Ping, SetAck};
use crate::protocol::{spicevmc_client, spicevmc_server, ChannelType};
use crate::settings;
use crate::usb::{DeviceBackend, UsbDeviceBackend};
use crate::usbredir::constants::{self, msg_type, msg_type_name, RYLL_CAPS};
use crate::usbredir::messages::{
    make_usbredir_message, AltSettingStatus, ConfigurationStatus, Hello, UsbredirMessage,
    UsbredirPayload,
};
use crate::usbredir::parser::UsbredirParser;

use super::{ChannelEvent, UsbCommand};

pub struct UsbredirChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    usb_rx: mpsc::Receiver<UsbCommand>,
    buffer: Vec<u8>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,

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
}

impl UsbredirChannel {
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        usb_rx: mpsc::Receiver<UsbCommand>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
    ) -> Self {
        UsbredirChannel {
            stream,
            event_tx,
            usb_rx,
            buffer: Vec::with_capacity(65536),
            capture,
            byte_counter,
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
            parser: UsbredirParser::new(),
            server_caps: 0,
            backend: None,
        }
    }

    /// Run the usbredir channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("usbredir: channel started");
        self.event_tx.send(ChannelEvent::UsbChannelReady).await.ok();

        // Send our hello immediately
        self.send_hello().await?;

        loop {
            let usb_rx = &mut self.usb_rx;

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
                        self.event_tx
                            .send(ChannelEvent::Disconnected(ChannelType::Usbredir))
                            .await
                            .ok();
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
            _ => {
                logging::log_unknown(
                    "usbredir",
                    "received",
                    msg_type,
                    payload.len() as u32,
                    payload,
                );
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
                info!("usbredir: cancel_data_packet (id={})", msg.id);
                // Phase 6 will implement transfer cancellation
            }

            UsbredirPayload::FilterReject => {
                info!("usbredir: filter_reject");
            }

            UsbredirPayload::DeviceDisconnectAck => {
                info!("usbredir: device_disconnect_ack");
                self.backend = None;
            }

            UsbredirPayload::ControlPacket { .. }
            | UsbredirPayload::BulkPacket { .. }
            | UsbredirPayload::InterruptPacket { .. } => {
                // Phase 6 will handle data transfers
                debug!(
                    "usbredir: {} (id={}) — not yet implemented",
                    type_name, msg.id
                );
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
            UsbCommand::ConnectDevice(backend) => {
                self.connect_device(*backend).await?;
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

        self.backend = Some(backend);
        info!("usbredir: device connected");
        Ok(())
    }

    async fn disconnect_device(&mut self) -> Result<()> {
        if self.backend.is_some() {
            info!("usbredir: disconnecting device");
            self.send_usbredir(msg_type::DEVICE_DISCONNECT, 0, &[])
                .await?;
            self.backend = None;
        }
        Ok(())
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
        UsbredirPayload::Hello(_) => 0,
        UsbredirPayload::DeviceConnect(_) => 1,
        UsbredirPayload::DeviceDisconnect => 2,
        UsbredirPayload::Reset => 3,
        UsbredirPayload::InterfaceInfo(_) => 4,
        UsbredirPayload::EpInfo(_) => 5,
        UsbredirPayload::SetConfiguration(_) => 6,
        UsbredirPayload::GetConfiguration => 7,
        UsbredirPayload::ConfigurationStatus(_) => 8,
        UsbredirPayload::SetAltSetting(_) => 9,
        UsbredirPayload::GetAltSetting(_) => 10,
        UsbredirPayload::AltSettingStatus(_) => 11,
        UsbredirPayload::StartInterruptReceiving(_) => 15,
        UsbredirPayload::StopInterruptReceiving(_) => 16,
        UsbredirPayload::InterruptReceivingStatus(_) => 17,
        UsbredirPayload::CancelDataPacket => 21,
        UsbredirPayload::FilterReject => 22,
        UsbredirPayload::DeviceDisconnectAck => 24,
        UsbredirPayload::ControlPacket { .. } => 100,
        UsbredirPayload::BulkPacket { .. } => 101,
        UsbredirPayload::InterruptPacket { .. } => 103,
        UsbredirPayload::Unknown { msg_type, .. } => *msg_type,
    })
}
