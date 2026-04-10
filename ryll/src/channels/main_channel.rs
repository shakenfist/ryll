/// Main channel handler - session management, ping/pong, channel list
use anyhow::Result;
use byteorder::{LittleEndian, WriteBytesExt};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::ByteCounter;
use crate::bugreport::{MainSnapshot, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::settings;
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, ChannelsList, MainInit, MessageHeader, Notify, Ping, SetAck,
};
use shakenfist_spice_protocol::{main_client, main_server, ChannelType, NotifySeverity};

use super::ChannelEvent;

const VD_AGENT_PROTOCOL: u32 = 1;
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 1;
const VD_AGENT_MONITORS_CONFIG: u32 = 2;
const VD_AGENT_CAP_MOUSE_STATE: u32 = 0;
const VD_AGENT_CAP_MONITORS_CONFIG: u32 = 1;
const VD_AGENT_CAP_REPLY: u32 = 2;
const VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS: u32 = 1;

pub struct MainChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    buffer: Vec<u8>,
    session_id: Option<u32>,
    agent_connected: bool,
    agent_tokens: u32,
    agent_caps_announced: bool,
    monitors: u8,
    monitors_config_rx: mpsc::Receiver<(u32, u32)>,
    pending_monitors_config: Option<(u32, u32)>,
    last_sent_monitors_config: Option<(u32, u32)>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    snapshot: Arc<Mutex<MainSnapshot>>,
    bytes_in: u64,
    bytes_out: u64,
}

impl MainChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<TrafficBuffers>,
        snapshot: Arc<Mutex<MainSnapshot>>,
        monitors_config_rx: mpsc::Receiver<(u32, u32)>,
        monitors: u8,
    ) -> Self {
        MainChannel {
            stream,
            event_tx,
            buffer: Vec::with_capacity(65536),
            session_id: None,
            agent_connected: false,
            agent_tokens: 0,
            agent_caps_announced: false,
            monitors,
            monitors_config_rx,
            pending_monitors_config: None,
            last_sent_monitors_config: None,
            capture,
            byte_counter,
            traffic,
            snapshot,
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

        let mut resize_debounce: Option<tokio::time::Instant> = None;

        loop {
            let mut chunk = [0u8; 65536];
            let stream = &mut self.stream;
            let monitors_config_rx = &mut self.monitors_config_rx;

            let debounce_sleep = async {
                match resize_debounce {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                n = async {
                    match stream {
                        SpiceStream::Plain(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                        SpiceStream::Tls(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                    }
                } => {
                    let n = n?;
                    if n == 0 {
                        info!("main: channel disconnected");
                        self.event_tx
                            .send(ChannelEvent::Disconnected(ChannelType::Main))
                            .await
                            .ok();
                        break;
                    }

                    self.byte_counter.add(n as u64);
                    if let Some(ref c) = self.capture {
                        c.packet_received("main", &chunk[..n]);
                    }
                    self.buffer.extend_from_slice(&chunk[..n]);
                    self.bytes_in += n as u64;

                    self.process_messages().await?;
                }
                resize = monitors_config_rx.recv() => {
                    let Some((width, height)) = resize else {
                        continue;
                    };

                    if self.last_sent_monitors_config == Some((width, height)) {
                        continue;
                    }

                    self.pending_monitors_config = Some((width, height));
                    resize_debounce = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(200));
                }
                _ = debounce_sleep => {
                    resize_debounce = None;
                    if let Some((width, height)) = self.pending_monitors_config {
                        info!("main: resize debounced: {}x{}", width, height);
                        self.event_tx
                            .send(ChannelEvent::MonitorsConfig { width, height })
                            .await
                            .ok();
                        self.maybe_send_agent_monitors_config().await?;
                    }
                }
            }
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

            // Record to ring buffer before draining
            let raw = self.buffer[..total_size].to_vec();
            self.traffic.record_received(
                "main",
                header.message_type,
                message_names::main_server(header.message_type),
                &raw,
            );

            // Extract message payload
            let payload = self.buffer[MessageHeader::SIZE..total_size].to_vec();
            self.buffer.drain(..total_size);

            self.handle_message(header.message_type, &payload).await?;
        }

        self.update_snapshot();
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
                self.agent_connected = init.agent_connected != 0;
                self.agent_tokens = init.agent_tokens;
                self.agent_caps_announced = false;

                if self.agent_connected {
                    self.connect_agent().await?;
                }

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

            main_server::AGENT_CONNECTED => {
                info!("main: vdagent connected");
                self.agent_connected = true;
                self.connect_agent().await?;
            }

            main_server::AGENT_DISCONNECTED => {
                info!("main: vdagent disconnected");
                self.agent_connected = false;
                self.agent_caps_announced = false;
            }

            main_server::AGENT_DATA => {
                if payload.len() >= 16 {
                    let agent_type =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    let agent_size =
                        u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
                    debug!(
                        "main: agent_data from server: type={}, size={}",
                        agent_type, agent_size
                    );
                } else {
                    debug!(
                        "main: agent_data from server: {} bytes: {:02x?}",
                        payload.len(),
                        payload
                    );
                }
            }

            main_server::AGENT_TOKEN => {
                if payload.len() >= 4 {
                    let tokens =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    self.agent_tokens = self.agent_tokens.saturating_add(tokens);
                } else {
                    self.agent_tokens = self.agent_tokens.saturating_add(1);
                    warn!("main: short AGENT_TOKEN payload ({} bytes)", payload.len());
                }

                self.maybe_send_announce_capabilities().await?;
            }

            _ => {
                // Unknown message - log with hex dump
                logging::log_unknown("main", "received", msg_type, payload.len() as u32, payload);
            }
        }

        Ok(())
    }

    /// Sync local state to the shared snapshot.
    fn update_snapshot(&self) {
        let mut snap = self.snapshot.lock().unwrap();
        snap.session_id = self.session_id;
        snap.bytes_in = self.bytes_in;
        snap.bytes_out = self.bytes_out;
    }

    async fn request_channels_list(&mut self) -> Result<()> {
        let msg = make_message(main_client::ATTACH_CHANNELS, &[]);
        self.send_with_log(main_client::ATTACH_CHANNELS, &msg).await
    }

    async fn connect_agent(&mut self) -> Result<()> {
        self.send_agent_start().await?;
        self.maybe_send_announce_capabilities().await
    }

    async fn send_agent_start(&mut self) -> Result<()> {
        let mut payload = Vec::with_capacity(4);
        payload.write_u32::<LittleEndian>(u32::MAX)?;
        let msg = make_message(main_client::AGENT_START, &payload);
        self.send_with_log(main_client::AGENT_START, &msg).await
    }

    async fn maybe_send_announce_capabilities(&mut self) -> Result<()> {
        if !self.agent_connected || self.agent_caps_announced {
            return Ok(());
        }

        let caps = (1u32 << VD_AGENT_CAP_MOUSE_STATE)
            | (1u32 << VD_AGENT_CAP_MONITORS_CONFIG)
            | (1u32 << VD_AGENT_CAP_REPLY);
        let mut payload = Vec::with_capacity(8);
        payload.write_u32::<LittleEndian>(1)?;
        payload.write_u32::<LittleEndian>(caps)?;

        if self
            .send_agent_data_message(VD_AGENT_ANNOUNCE_CAPABILITIES, &payload)
            .await?
        {
            self.agent_caps_announced = true;
        }

        Ok(())
    }

    async fn maybe_send_agent_monitors_config(&mut self) -> Result<()> {
        let Some((width, height)) = self.pending_monitors_config else {
            debug!("main: monitors config: no pending config");
            return Ok(());
        };

        if !self.agent_connected {
            debug!("main: monitors config: agent not connected");
            return Ok(());
        }

        if !self.agent_caps_announced {
            debug!("main: monitors config: caps not announced yet");
            return Ok(());
        }

        if self.last_sent_monitors_config == Some((width, height)) {
            return Ok(());
        }

        info!("main: sending monitors config: {}x{}", width, height);
        if self.send_agent_monitors_config(width, height).await? {
            self.last_sent_monitors_config = Some((width, height));
            self.pending_monitors_config = None;
        } else {
            debug!("main: monitors config: no agent tokens");
        }

        Ok(())
    }

    async fn send_agent_monitors_config(&mut self, width: u32, height: u32) -> Result<bool> {
        let active = if self.monitors == 0 {
            1
        } else {
            self.monitors as u32
        };
        let flags = if active > 1 {
            VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS
        } else {
            0
        };

        let mut payload = Vec::with_capacity(8 + active as usize * 20);
        payload.write_u32::<LittleEndian>(active)?;
        payload.write_u32::<LittleEndian>(flags)?;

        for i in 0..active {
            info!(
                "main: monitors config[{}]: {}x{} pos=({},0) depth=32",
                i,
                width,
                height,
                width * i
            );
            payload.write_u32::<LittleEndian>(height)?;
            payload.write_u32::<LittleEndian>(width)?;
            payload.write_u32::<LittleEndian>(32)?;
            if active > 1 {
                payload.write_u32::<LittleEndian>(width * i)?;
            } else {
                payload.write_u32::<LittleEndian>(0)?;
            }
            payload.write_u32::<LittleEndian>(0)?;
        }

        info!(
            "main: agent monitors config: num_mon={}, flags={}",
            active, flags
        );

        debug!(
            "main: monitors config payload ({} bytes): {:02x?}",
            payload.len(),
            payload
        );

        self.send_agent_data_message(VD_AGENT_MONITORS_CONFIG, &payload)
            .await
    }

    async fn send_agent_data_message(&mut self, ty: u32, payload: &[u8]) -> Result<bool> {
        if self.agent_tokens == 0 {
            return Ok(false);
        }

        let mut agent = Vec::with_capacity(16 + payload.len());
        agent.write_u32::<LittleEndian>(VD_AGENT_PROTOCOL)?;
        agent.write_u32::<LittleEndian>(ty)?;
        agent.write_u64::<LittleEndian>(0)?;
        agent.write_u32::<LittleEndian>(payload.len() as u32)?;
        agent.extend_from_slice(payload);

        debug!(
            "main: agent_data wrapper ({} bytes): {:02x?}",
            agent.len(),
            agent
        );
        let msg = make_message(main_client::AGENT_DATA, &agent);
        self.send_with_log(main_client::AGENT_DATA, &msg).await?;
        self.agent_tokens = self.agent_tokens.saturating_sub(1);

        Ok(true)
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let msg_name = message_names::main_client(msg_type);
        if settings::is_verbose() {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "main", msg_type, msg_name, payload_size);
        }
        self.traffic.record_sent("main", msg_type, msg_name, data);
        let result = self.send(data).await;
        self.update_snapshot();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            c.packet_sent("main", data);
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}
