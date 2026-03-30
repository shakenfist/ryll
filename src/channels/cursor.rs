/// Cursor channel handler - cursor position and visibility
use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::protocol::link::SpiceStream;
use crate::protocol::logging::{self, message_names};
use crate::protocol::messages::{make_message, CursorInit, CursorSet, MessageHeader, Ping, SetAck};
use crate::protocol::{cursor_client, cursor_server, ChannelType};
use crate::settings;

use super::ChannelEvent;

pub struct CursorChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    buffer: Vec<u8>,
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,
    bytes_in: u64,
    bytes_out: u64,
}

impl CursorChannel {
    pub fn new(stream: SpiceStream, event_tx: mpsc::Sender<ChannelEvent>) -> Self {
        CursorChannel {
            stream,
            event_tx,
            buffer: Vec::with_capacity(4096),
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    /// Run the cursor channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("cursor: channel started");

        loop {
            // Read data into buffer
            let mut chunk = [0u8; 4096];
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
                info!("cursor: channel disconnected");
                self.event_tx
                    .send(ChannelEvent::Disconnected(ChannelType::Cursor))
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
        let msg_type_str = message_names::cursor_server(msg_type);

        // Log all messages in verbose mode
        if settings::is_verbose() {
            logging::log_message(
                "received",
                "cursor",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        match msg_type {
            cursor_server::INIT => {
                let init = CursorInit::read(payload)?;
                info!(
                    "cursor: init: pos=({},{}), visible={}, payload_size={}",
                    init.x,
                    init.y,
                    init.visible,
                    payload.len()
                );

                self.event_tx
                    .send(ChannelEvent::CursorPosition {
                        x: init.x,
                        y: init.y,
                        visible: init.visible != 0,
                    })
                    .await
                    .ok();
            }

            cursor_server::SET => {
                let set = CursorSet::read(payload)?;
                info!(
                    "cursor: set: pos=({},{}), visible={}, payload_size={}",
                    set.x,
                    set.y,
                    set.visible,
                    payload.len()
                );

                self.event_tx
                    .send(ChannelEvent::CursorPosition {
                        x: set.x,
                        y: set.y,
                        visible: set.visible != 0,
                    })
                    .await
                    .ok();
            }

            cursor_server::MOVE => {
                if payload.len() >= 4 {
                    let x = u16::from_le_bytes([payload[0], payload[1]]);
                    let y = u16::from_le_bytes([payload[2], payload[3]]);
                    info!("cursor: move: ({},{})", x, y);

                    self.event_tx
                        .send(ChannelEvent::CursorPosition {
                            x,
                            y,
                            visible: true,
                        })
                        .await
                        .ok();
                }
            }

            cursor_server::HIDE => {
                info!("cursor: hide");
                self.event_tx
                    .send(ChannelEvent::CursorPosition {
                        x: 0,
                        y: 0,
                        visible: false,
                    })
                    .await
                    .ok();
            }

            cursor_server::RESET => {
                debug!("cursor: reset");
            }

            cursor_server::TRAIL => {
                debug!("cursor: trail settings received");
            }

            cursor_server::SET_ACK => {
                let set_ack = SetAck::read(payload)?;

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "generation={}, window={}",
                        set_ack.generation, set_ack.window
                    ));
                }

                self.ack_generation = set_ack.generation;
                self.ack_window = set_ack.window;

                // Send ack_sync response
                let mut ack_payload = Vec::new();
                SetAck::write_ack_sync(set_ack.generation, &mut ack_payload)?;
                let response = make_message(cursor_client::ACK_SYNC, &ack_payload);
                self.send_with_log(cursor_client::ACK_SYNC, &response)
                    .await?;
            }

            cursor_server::PING => {
                let ping = Ping::read(payload)?;

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "ping_id={}, timestamp={}",
                        ping.id, ping.timestamp
                    ));
                }

                let mut pong_payload = Vec::new();
                ping.write_pong(&mut pong_payload)?;
                let response = make_message(cursor_client::PONG, &pong_payload);
                self.send_with_log(cursor_client::PONG, &response).await?;
            }

            _ => {
                // Unknown message - log with hex dump
                logging::log_unknown(
                    "cursor",
                    "received",
                    msg_type,
                    payload.len() as u32,
                    payload,
                );
            }
        }

        Ok(())
    }

    async fn send_ack(&mut self) -> Result<()> {
        let msg = make_message(cursor_client::ACK, &[]);
        self.send_with_log(cursor_client::ACK, &msg).await?;
        self.last_ack = self.message_count;
        Ok(())
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        if settings::is_verbose() {
            let msg_type_str = message_names::cursor_client(msg_type);
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "cursor", msg_type, msg_type_str, payload_size);
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
