/// WebDAV folder sharing channel handler — SpiceVMC transport
///
/// Handles the SPICE SpiceVMC transport for the WebDAV channel
/// (channel type 11). This carries multiplexed HTTP traffic between
/// the guest's spice-webdavd daemon and the client's embedded WebDAV
/// server. Each mux client gets a DuplexStream pair connecting the
/// mux layer to a per-client hyper/dav-server instance.
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, error, info, warn};

use crate::app::ByteCounter;
use crate::capture::CaptureSession;
use crate::config::ShareDirConfig;
use crate::settings;
use crate::webdav::mux::{self, MuxDemuxer, MuxFrame};
use crate::webdav::server::WebdavServer;
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{make_message, MessageHeader, Ping, SetAck};
use shakenfist_spice_protocol::{spicevmc_client, spicevmc_server, ChannelType};

use super::{ChannelEvent, WebdavCommand};

/// Response data from a per-client reader task back to the main loop.
struct MuxResponse {
    client_id: i64,
    data: Vec<u8>, // empty = client connection finished
}

/// Per-client state for a mux-multiplexed HTTP connection.
struct MuxClient {
    /// Bytes of HTTP request data received so far.
    bytes_received: u64,
    /// Write half of the client end of the DuplexStream.
    /// Request data from the guest is written here.
    write_half: WriteHalf<tokio::io::DuplexStream>,
    /// Handle for the hyper server task.
    server_handle: tokio::task::JoinHandle<()>,
    /// Handle for the response reader task.
    reader_handle: tokio::task::JoinHandle<()>,
}

pub struct WebdavChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    repaint_notify: Arc<Notify>,
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

    // Mux protocol state
    demuxer: MuxDemuxer,
    clients: HashMap<i64, MuxClient>,

    // Response channel: per-client reader tasks send
    // response data here for muxing back to the guest.
    response_tx: mpsc::Sender<MuxResponse>,
    response_rx: mpsc::Receiver<MuxResponse>,

    // WebDAV server (None until sharing is started)
    server: Option<WebdavServer>,
}

impl WebdavChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<Notify>,
        webdav_rx: mpsc::Receiver<WebdavCommand>,
        auto_share_dir: Option<ShareDirConfig>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
    ) -> Self {
        let (response_tx, response_rx) = mpsc::channel(256);
        WebdavChannel {
            stream,
            event_tx,
            repaint_notify,
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
            demuxer: MuxDemuxer::new(),
            clients: HashMap::new(),
            response_tx,
            response_rx,
            server: None,
        }
    }

    /// Run the WebDAV channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("webdav: channel started");
        self.event_tx
            .send(ChannelEvent::WebdavChannelReady)
            .await
            .ok();
        self.repaint_notify.notify_one();

        // If a shared directory was configured via CLI, create the server
        if let Some(ref dir) = self.shared_dir {
            let path_str = dir.path.display().to_string();
            match WebdavServer::new(dir.path.clone(), dir.read_only) {
                Ok(server) => {
                    info!(
                        "webdav: auto-sharing directory: {} (read_only={})",
                        path_str, dir.read_only,
                    );
                    self.server = Some(server);
                    self.event_tx
                        .send(ChannelEvent::WebdavSharingStarted {
                            path: path_str,
                            read_only: dir.read_only,
                        })
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                }
                Err(e) => {
                    error!("webdav: failed to create server for {}: {}", path_str, e);
                    self.event_tx
                        .send(ChannelEvent::WebdavError(format!(
                            "Failed to share {}: {}",
                            path_str, e
                        )))
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                }
            }
        }

        loop {
            let webdav_rx = &mut self.webdav_rx;
            let response_rx = &mut self.response_rx;

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
                        self.shutdown_all_clients();
                        self.event_tx
                            .send(ChannelEvent::Disconnected(ChannelType::Webdav))
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
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

                // Response data from per-client reader tasks
                Some(resp) = response_rx.recv() => {
                    self.handle_response(resp).await?;
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
    async fn handle_vmc_data(&mut self, payload: &[u8]) -> Result<()> {
        self.demuxer.feed(payload);

        while let Some(frame) = self.demuxer.next_frame() {
            self.handle_mux_frame(frame).await?;
        }

        Ok(())
    }

    /// Dispatch a single demuxed mux frame.
    async fn handle_mux_frame(&mut self, frame: MuxFrame) -> Result<()> {
        if frame.data.is_empty() {
            // Client disconnect
            if let Some(client) = self.clients.remove(&frame.client_id) {
                debug!("webdav: client {} disconnected by guest", frame.client_id);
                // Dropping write_half causes hyper to see EOF.
                // Abort tasks as a safety net.
                client.server_handle.abort();
                client.reader_handle.abort();
            } else {
                debug!(
                    "webdav: client {} disconnect for unknown client (stale close)",
                    frame.client_id
                );
            }
        } else if let Some(client) = self.clients.get_mut(&frame.client_id) {
            // Existing client — forward data to its DuplexStream
            client.bytes_received += frame.data.len() as u64;
            if let Err(e) = client.write_half.write_all(&frame.data).await {
                warn!(
                    "webdav: failed to write to client {}: {}",
                    frame.client_id, e
                );
                // Remove the broken client
                if let Some(client) = self.clients.remove(&frame.client_id) {
                    client.server_handle.abort();
                    client.reader_handle.abort();
                }
            }
        } else {
            // New client — create DuplexStream pair and spawn tasks
            let Some(ref server) = self.server else {
                warn!(
                    "webdav: received data for client {} but no directory is shared",
                    frame.client_id
                );
                return Ok(());
            };

            // Cap concurrent clients to prevent resource exhaustion
            if self.clients.len() >= 64 {
                warn!(
                    "webdav: rejecting client {} — too many concurrent clients ({})",
                    frame.client_id,
                    self.clients.len()
                );
                return Ok(());
            }

            info!(
                "webdav: new client {} ({} bytes initial data)",
                frame.client_id,
                frame.data.len(),
            );

            let (client_end, server_end) = tokio::io::duplex(65536);
            let (read_half, mut write_half) = tokio::io::split(client_end);

            // Spawn server task: hyper + dav-server on the server end
            let server_clone = server.clone();
            let cid = frame.client_id;
            let server_handle = tokio::spawn(async move {
                if let Err(e) = server_clone.serve_client(server_end).await {
                    debug!("webdav: server task for client {} ended: {}", cid, e);
                }
            });

            // Spawn reader task: reads response bytes from client end,
            // sends them back to the main loop via response_tx
            let tx = self.response_tx.clone();
            let reader_cid = frame.client_id;
            let reader_handle = tokio::spawn(async move {
                let mut read_half = read_half;
                let mut buf = [0u8; 65536];
                loop {
                    match read_half.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            // Connection closed or error — signal completion
                            tx.send(MuxResponse {
                                client_id: reader_cid,
                                data: vec![],
                            })
                            .await
                            .ok();
                            break;
                        }
                        Ok(n) => {
                            if tx
                                .send(MuxResponse {
                                    client_id: reader_cid,
                                    data: buf[..n].to_vec(),
                                })
                                .await
                                .is_err()
                            {
                                break; // Channel closed, main loop is gone
                            }
                        }
                    }
                }
            });

            // Write initial request data
            if let Err(e) = write_half.write_all(&frame.data).await {
                warn!(
                    "webdav: failed to write initial data for client {}: {}",
                    frame.client_id, e
                );
                server_handle.abort();
                reader_handle.abort();
                return Ok(());
            }

            self.clients.insert(
                frame.client_id,
                MuxClient {
                    bytes_received: frame.data.len() as u64,
                    write_half,
                    server_handle,
                    reader_handle,
                },
            );
        }

        Ok(())
    }

    /// Handle response data from a per-client reader task.
    async fn handle_response(&mut self, resp: MuxResponse) -> Result<()> {
        if resp.data.is_empty() {
            // Client connection finished — send close frame to guest
            debug!("webdav: client {} finished, sending close", resp.client_id);
            self.send_mux_close(resp.client_id).await?;
            if let Some(client) = self.clients.remove(&resp.client_id) {
                client.server_handle.abort();
                client.reader_handle.abort();
            }
        } else {
            self.send_mux_frame(resp.client_id, &resp.data).await?;
        }
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
                match WebdavServer::new(path.clone(), read_only) {
                    Ok(server) => {
                        info!(
                            "webdav: sharing directory: {} (read_only={})",
                            path_str, read_only
                        );
                        self.server = Some(server);
                        self.shared_dir = Some(ShareDirConfig { path, read_only });
                        self.event_tx
                            .send(ChannelEvent::WebdavSharingStarted {
                                path: path_str,
                                read_only,
                            })
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
                    }
                    Err(e) => {
                        error!("webdav: failed to create server for {}: {}", path_str, e);
                        self.event_tx
                            .send(ChannelEvent::WebdavError(format!(
                                "Failed to share {}: {}",
                                path_str, e
                            )))
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
                    }
                }
            }
            WebdavCommand::StopSharing => {
                info!("webdav: stopped sharing");
                self.shutdown_all_clients();
                self.server = None;
                self.shared_dir = None;
                self.event_tx
                    .send(ChannelEvent::WebdavSharingStopped)
                    .await
                    .ok();
                self.repaint_notify.notify_one();
            }
        }
        Ok(())
    }

    // ── Client lifecycle ──────────────────────────────

    /// Abort all client tasks and clear the client map.
    fn shutdown_all_clients(&mut self) {
        for (cid, client) in self.clients.drain() {
            debug!("webdav: shutting down client {}", cid);
            client.server_handle.abort();
            client.reader_handle.abort();
        }
    }

    // ── Send helpers ───────────────────────────────────

    /// Send raw bytes wrapped in a SPICEVMC_DATA SPICE message.
    async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        let msg = make_message(spicevmc_client::DATA, data);
        self.send_with_log(spicevmc_client::DATA, &msg).await
    }

    /// Send a mux-framed response to the guest.
    async fn send_mux_frame(&mut self, client_id: i64, data: &[u8]) -> Result<()> {
        let frame = mux::encode_mux_frame(client_id, data);
        self.send_data(&frame).await
    }

    /// Signal to the guest that we are closing a client connection.
    async fn send_mux_close(&mut self, client_id: i64) -> Result<()> {
        self.send_mux_frame(client_id, &[]).await
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
