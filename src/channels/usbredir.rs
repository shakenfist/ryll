/// USB redirection channel handler — SpiceVMC transport layer
///
/// Receives SPICEVMC_DATA and SPICEVMC_COMPRESSED_DATA messages from
/// the server, decompresses LZ4 payloads, and parses the usbredir
/// protocol messages within.
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
use crate::usbredir::constants::msg_type_name;
use crate::usbredir::messages::UsbredirPayload;
use crate::usbredir::parser::UsbredirParser;

use super::ChannelEvent;

pub struct UsbredirChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
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

    // usbredir protocol parser
    parser: UsbredirParser,
}

impl UsbredirChannel {
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
    ) -> Self {
        UsbredirChannel {
            stream,
            event_tx,
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
        }
    }

    /// Run the usbredir channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("usbredir: channel started");
        self.event_tx.send(ChannelEvent::UsbChannelReady).await.ok();

        loop {
            let mut chunk = [0u8; 65536];
            let n = match &mut self.stream {
                SpiceStream::Plain(s) => {
                    use tokio::io::AsyncReadExt;
                    s.read(&mut chunk).await?
                }
                SpiceStream::Tls(s) => {
                    use tokio::io::AsyncReadExt;
                    s.read(&mut chunk).await?
                }
            };

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

        Ok(())
    }

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

            // Send ACK if needed
            if self.ack_window > 0 && self.message_count - self.last_ack >= self.ack_window {
                self.send_ack().await?;
            }
        }

        Ok(())
    }

    async fn handle_message(&mut self, msg_type: u16, payload: &[u8]) -> Result<()> {
        // Log all messages in verbose mode
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

    async fn handle_usbredir_message(
        &mut self,
        msg: crate::usbredir::messages::UsbredirMessage,
    ) -> Result<()> {
        let type_name = msg_type_name(match &msg.payload {
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
        });

        match &msg.payload {
            UsbredirPayload::Hello(hello) => {
                info!(
                    "usbredir: server hello: version='{}' caps=0x{:08x}",
                    hello.version, hello.capabilities,
                );
            }
            UsbredirPayload::Reset => {
                info!("usbredir: reset requested (id={})", msg.id);
            }
            UsbredirPayload::SetConfiguration(sc) => {
                info!(
                    "usbredir: set_configuration={} (id={})",
                    sc.configuration, msg.id
                );
            }
            UsbredirPayload::GetConfiguration => {
                info!("usbredir: get_configuration (id={})", msg.id);
            }
            UsbredirPayload::SetAltSetting(sa) => {
                info!(
                    "usbredir: set_alt_setting iface={} alt={} (id={})",
                    sa.interface, sa.alt_setting, msg.id,
                );
            }
            UsbredirPayload::GetAltSetting(ga) => {
                info!(
                    "usbredir: get_alt_setting iface={} (id={})",
                    ga.interface, msg.id
                );
            }
            UsbredirPayload::CancelDataPacket => {
                info!("usbredir: cancel_data_packet (id={})", msg.id);
            }
            UsbredirPayload::FilterReject => {
                info!("usbredir: filter_reject");
            }
            UsbredirPayload::DeviceDisconnectAck => {
                info!("usbredir: device_disconnect_ack");
            }
            UsbredirPayload::Unknown { msg_type, data } => {
                debug!(
                    "usbredir: unknown message type={} ({}) len={} (id={})",
                    msg_type,
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
                // LZ4
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
                // No compression
                self.handle_vmc_data(compressed).await?;
            }
            _ => {
                warn!("usbredir: unknown compression type {}", compression_type);
            }
        }

        Ok(())
    }

    /// Send raw usbredir data to the server wrapped in a SPICEVMC_DATA message.
    #[allow(dead_code)]
    pub async fn send_data(&mut self, data: &[u8]) -> Result<()> {
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
