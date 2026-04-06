/// WebDAV folder sharing channel handler — SpiceVMC transport
///
/// Handles the SPICE SpiceVMC transport for the WebDAV channel
/// (channel type 11). This carries multiplexed HTTP traffic between
/// the guest's spice-webdavd daemon and the client's embedded WebDAV
/// server. The mux protocol and WebDAV server are stubbed in this
/// initial implementation — only the SPICE transport layer is active.
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::ByteCounter;
use crate::capture::CaptureSession;
use crate::config::ShareDirConfig;
use crate::protocol::link::SpiceStream;
use crate::protocol::logging::{self, message_names};
use crate::protocol::messages::{make_message, MessageHeader, Ping, SetAck};
use crate::protocol::{spicevmc_client, spicevmc_server, ChannelType};
use crate::settings;

use super::{ChannelEvent, WebdavCommand};

pub struct WebdavChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    webdav_rx: mpsc::Receiver<WebdavCommand>,
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

    // Sharing state
    shared_dir: Option<ShareDirConfig>,
}

impl WebdavChannel {
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        webdav_rx: mpsc::Receiver<WebdavCommand>,
        auto_share_dir: Option<ShareDirConfig>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
    ) -> Self {
        WebdavChannel {
            stream,
            event_tx,
            webdav_rx,
            buffer: Vec::with_capacity(65536),
            capture,
            byte_counter,
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
            shared_dir: auto_share_dir,
        }
    }

    /// Run the WebDAV channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("webdav: channel started");
        self.event_tx
            .send(ChannelEvent::WebdavChannelReady)
            .await
            .ok();

        // If a shared directory was configured via CLI, signal that sharing is active
        if let Some(ref dir) = self.shared_dir {
            let path_str = dir.path.display().to_string();
            info!(
                "webdav: auto-sharing directory: {} (read_only={})",
                path_str, dir.read_only,
            );
            self.event_tx
                .send(ChannelEvent::WebdavSharingStarted {
                    path: path_str,
                    read_only: dir.read_only,
                })
                .await
                .ok();
        }

        loop {
            let webdav_rx = &mut self.webdav_rx;

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
                        info!("webdav: channel disconnected");
                        self.event_tx
                            .send(ChannelEvent::Disconnected(ChannelType::Webdav))
                            .await
                            .ok();
                        break;
                    }

                    self.byte_counter.add(n as u64);
                    if let Some(ref c) = self.capture {
                        c.packet_received("webdav", &chunk[..n]);
                    }
                    self.buffer.extend_from_slice(&chunk[..n]);
                    self.bytes_in += n as u64;

                    self.process_messages().await?;
                }

                // WebDAV commands from the app
                Some(cmd) = webdav_rx.recv() => {
                    self.handle_command(cmd).await?;
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
                "webdav",
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
                    "webdav",
                    "received",
                    msg_type,
                    payload.len() as u32,
                    payload,
                );
            }
        }

        Ok(())
    }

    /// Handle raw VMC data from the server (mux frames from the guest).
    /// Stubbed for now — the mux demultiplexer will be added in phase 2.
    async fn handle_vmc_data(&mut self, payload: &[u8]) -> Result<()> {
        debug!(
            "webdav: received {} bytes of VMC data (mux stub)",
            payload.len()
        );
        Ok(())
    }

    async fn handle_vmc_compressed_data(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 5 {
            warn!("webdav: compressed_data too short: {} bytes", payload.len());
            return Ok(());
        }

        let compression_type = payload[0];
        let uncompressed_size = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
        let compressed = &payload[5..];

        // Cap decompression size to prevent OOM from malicious server
        if uncompressed_size > 64 * 1024 * 1024 {
            warn!(
                "webdav: decompressed size {} exceeds limit, dropping",
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
                    .map_err(|e| anyhow::anyhow!("webdav: LZ4 decompress failed: {}", e))?;
                if decompressed.len() != uncompressed_size {
                    warn!(
                        "webdav: LZ4 size mismatch: expected {} got {}",
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
                warn!("webdav: unknown compression type {}", compression_type);
            }
        }

        Ok(())
    }

    // ── Command handling ──────────────────────────────

    async fn handle_command(&mut self, cmd: WebdavCommand) -> Result<()> {
        match cmd {
            WebdavCommand::ShareDirectory { path, read_only } => {
                let path_str = path.display().to_string();
                info!(
                    "webdav: sharing directory: {} (read_only={})",
                    path_str, read_only
                );
                self.shared_dir = Some(ShareDirConfig { path, read_only });
                self.event_tx
                    .send(ChannelEvent::WebdavSharingStarted {
                        path: path_str,
                        read_only,
                    })
                    .await
                    .ok();
            }
            WebdavCommand::StopSharing => {
                info!("webdav: stopped sharing");
                self.shared_dir = None;
                self.event_tx
                    .send(ChannelEvent::WebdavSharingStopped)
                    .await
                    .ok();
            }
        }
        Ok(())
    }

    // ── Send helpers ───────────────────────────────────

    /// Send raw bytes wrapped in a SPICEVMC_DATA SPICE message.
    #[allow(dead_code)]
    async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        let msg = make_message(spicevmc_client::DATA, data);
        self.send_with_log(spicevmc_client::DATA, &msg).await
    }

    /// Send data with LZ4 compression if it saves space, otherwise send uncompressed.
    #[allow(dead_code)]
    async fn send_compressed_data(&mut self, data: &[u8]) -> Result<()> {
        let compressed = lz4_flex::compress_prepend_size(data);

        // Only use compression if it actually saves space (compressed + 5-byte header < original)
        if compressed.len() + 5 < data.len() {
            let mut payload = Vec::with_capacity(5 + compressed.len());
            payload.push(1u8); // compression type: LZ4
            payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
            payload.extend_from_slice(&compressed);
            let msg = make_message(spicevmc_client::COMPRESSED_DATA, &payload);
            self.send_with_log(spicevmc_client::COMPRESSED_DATA, &msg)
                .await
        } else {
            self.send_data(data).await
        }
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
            logging::log_message("sent", "webdav", msg_type, msg_type_str, payload_size);
        }
        self.send(data).await
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            c.packet_sent("webdav", data);
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}
