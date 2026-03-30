/// Main channel handler - session management, ping/pong, channel list
use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::protocol::link::SpiceStream;
use crate::protocol::logging::{self, message_names};
use crate::protocol::messages::{
    make_message, ChannelsList, MainInit, MessageHeader, Notify, Ping, SetAck,
};
use crate::protocol::{main_client, main_server, ChannelType, NotifySeverity};
use crate::settings;

use super::ChannelEvent;

pub struct MainChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    buffer: Vec<u8>,
    session_id: Option<u32>,
    bytes_in: u64,
    bytes_out: u64,
}

impl MainChannel {
    pub fn new(stream: SpiceStream, event_tx: mpsc::Sender<ChannelEvent>) -> Self {
        MainChannel {
            stream,
            event_tx,
            buffer: Vec::with_capacity(65536),
            session_id: None,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    #[allow(dead_code)]
    pub fn session_id(&self) -> Option<u32> {
        self.session_id
    }

    /// Run the main channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("main: channel started");

        loop {
            // Read data into buffer
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
                info!("main: channel disconnected");
                self.event_tx
                    .send(ChannelEvent::Disconnected(ChannelType::Main))
                    .await
                    .ok();
                break;
            }

            self.buffer.extend_from_slice(&chunk[..n]);
            self.bytes_in += n as u64;

            // Process complete messages
            self.process_messages().await?;
        }

        Ok(())
    }

    async fn process_messages(&mut self) -> Result<()> {
        while self.buffer.len() >= MessageHeader::SIZE {
            let header = MessageHeader::read(&self.buffer)?;
            let total_size = MessageHeader::SIZE + header.message_size as usize;

            if self.buffer.len() < total_size {
                // Wait for more data
                break;
            }

            // Extract message payload
            let payload = self.buffer[MessageHeader::SIZE..total_size].to_vec();
            self.buffer.drain(..total_size);

            self.handle_message(header.message_type, &payload).await?;
        }

        Ok(())
    }

    async fn handle_message(&mut self, msg_type: u16, payload: &[u8]) -> Result<()> {
        let msg_type_str = message_names::main_server(msg_type);

        // Log all messages in verbose mode
        if settings::is_verbose() {
            logging::log_message(
                "received",
                "main",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        match msg_type {
            main_server::INIT => {
                let init = MainInit::read(payload)?;
                info!("main: session initialized: id={}", init.session_id);

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "session_id={}, display_channels_hint={}, mouse_modes={}, \
                         current_mouse_mode={}, agent_connected={}, agent_tokens={}, \
                         multimedia_time={}, ram_hint={}",
                        init.session_id,
                        init.display_channels_hint,
                        init.supported_mouse_modes,
                        init.current_mouse_mode,
                        init.agent_connected,
                        init.agent_tokens,
                        init.multi_media_time,
                        init.ram_hint
                    ));
                }

                self.session_id = Some(init.session_id);

                self.event_tx
                    .send(ChannelEvent::SessionInitialized(init.session_id))
                    .await
                    .ok();
                self.event_tx
                    .send(ChannelEvent::MouseMode(init.current_mouse_mode))
                    .await
                    .ok();
            }

            main_server::CHANNELS_LIST => {
                let list = ChannelsList::read(payload)?;
                info!(
                    "main: received channel list: {} channels",
                    list.channels.len()
                );

                let channels: Vec<(ChannelType, u8)> = list
                    .channels
                    .iter()
                    .filter_map(|c| ChannelType::from_u8(c.channel_type).map(|t| (t, c.channel_id)))
                    .collect();

                for (ch_type, ch_id) in &channels {
                    if settings::is_verbose() {
                        logging::log_detail(&format!(
                            "channel: {} (type={}, id={})",
                            ch_type.name(),
                            *ch_type as u8,
                            ch_id
                        ));
                    } else {
                        debug!("  - {} (id={})", ch_type.name(), ch_id);
                    }
                }

                self.event_tx
                    .send(ChannelEvent::ChannelsAvailable(channels))
                    .await
                    .ok();
            }

            main_server::PING => {
                let ping = Ping::read(payload)?;

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "ping_id={}, timestamp={}",
                        ping.id, ping.timestamp
                    ));
                }

                // Send pong response
                let mut pong_payload = Vec::new();
                ping.write_pong(&mut pong_payload)?;
                let response = make_message(main_client::PONG, &pong_payload);

                self.send_with_log(main_client::PONG, &response).await?;

                // Request channel list on first large ping
                if ping.id > 0 && self.session_id.is_some() {
                    self.request_channels_list().await?;
                }
            }

            main_server::SET_ACK => {
                let set_ack = SetAck::read(payload)?;

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "generation={}, window={}",
                        set_ack.generation, set_ack.window
                    ));
                }

                // Send ack_sync response
                let mut ack_payload = Vec::new();
                SetAck::write_ack_sync(set_ack.generation, &mut ack_payload)?;
                let response = make_message(main_client::ACK_SYNC, &ack_payload);

                self.send_with_log(main_client::ACK_SYNC, &response).await?;
            }

            main_server::NOTIFY => {
                let notify = Notify::read(payload)?;
                let severity = NotifySeverity::from_u32(notify.severity);

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "severity={:?}, visibility={}, what={}, message=\"{}\"",
                        severity, notify.visibility, notify.what, notify.message
                    ));
                }

                match severity {
                    NotifySeverity::Error => {
                        warn!("main: server notify (error): {}", notify.message);
                    }
                    NotifySeverity::Warn => {
                        warn!("main: server notify (warn): {}", notify.message);
                    }
                    NotifySeverity::Info => {
                        info!("main: server notify: {}", notify.message);
                    }
                }
            }

            main_server::DISCONNECTING => {
                info!("main: server sent disconnect notification");
                self.event_tx
                    .send(ChannelEvent::Disconnected(ChannelType::Main))
                    .await
                    .ok();
            }

            _ => {
                // Unknown message - log with hex dump
                logging::log_unknown("main", "received", msg_type, payload.len() as u32, payload);
            }
        }

        Ok(())
    }

    async fn request_channels_list(&mut self) -> Result<()> {
        let msg = make_message(main_client::ATTACH_CHANNELS, &[]);
        self.send_with_log(main_client::ATTACH_CHANNELS, &msg).await
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        if settings::is_verbose() {
            let msg_type_str = message_names::main_client(msg_type);
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "main", msg_type, msg_type_str, payload_size);
        }
        self.send(data).await
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}
