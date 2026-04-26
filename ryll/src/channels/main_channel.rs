/// Main channel handler - session management, ping/pong, channel list
use anyhow::Result;
use byteorder::{LittleEndian, WriteBytesExt};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::Notify as RepaintNotify;
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
use shakenfist_spice_protocol::{
    main_client, main_server, ChannelType, NotifySeverity, MOUSE_MODE_CLIENT,
};

use super::ChannelEvent;

/// Parse a SpiceMsgMainMouseMode payload. The SPICE wire format
/// is two little-endian `uint16`s — `supported_modes` followed by
/// `current_mode`. (Historical misreads of this as a single `u32`
/// produce nonsense values like 131075 when current_mode=2 and
/// supported_modes=3.) Returns `None` if the payload is shorter
/// than 4 bytes.
pub(crate) fn parse_mouse_mode_payload(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    let supported = u16::from_le_bytes([payload[0], payload[1]]);
    let current = u16::from_le_bytes([payload[2], payload[3]]);
    Some((supported, current))
}

/// True when the server supports CLIENT (absolute) mouse mode but
/// is currently in a different mode. Used to decide whether to
/// send a `MOUSE_MODE_REQUEST(CLIENT)` after INIT or after a
/// MOUSE_MODE change (e.g. after the guest reboots, the server
/// often reverts to SERVER/relative mode).
pub(crate) fn should_request_client_mouse_mode(supported: u32, current: u32) -> bool {
    supported & MOUSE_MODE_CLIENT != 0 && current != MOUSE_MODE_CLIENT
}

const VD_AGENT_PROTOCOL: u32 = 1;
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 1;
const VD_AGENT_MONITORS_CONFIG: u32 = 2;
const VD_AGENT_CLIPBOARD: u32 = 4;
const VD_AGENT_CLIPBOARD_GRAB: u32 = 7;
const VD_AGENT_CLIPBOARD_REQUEST: u32 = 8;
const VD_AGENT_CLIPBOARD_RELEASE: u32 = 9;
const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;

const VD_AGENT_CAP_MOUSE_STATE: u32 = 0;
const VD_AGENT_CAP_MONITORS_CONFIG: u32 = 1;
const VD_AGENT_CAP_REPLY: u32 = 2;
const VD_AGENT_CAP_CLIPBOARD_BY_DEMAND: u32 = 5;
const VD_AGENT_CAP_CLIPBOARD_SELECTION: u32 = 6;
const VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS: u32 = 1;

pub struct MainChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    repaint_notify: Arc<RepaintNotify>,
    buffer: Vec<u8>,
    session_id: Option<u32>,
    agent_connected: bool,
    agent_tokens: u32,
    agent_caps_announced: bool,
    guest_caps_received: bool,
    channels_requested: bool,
    monitors: u8,
    monitors_config_rx: mpsc::Receiver<(u32, u32)>,
    pending_monitors_config: Option<(u32, u32)>,
    last_sent_monitors_config: Option<(u32, u32)>,
    last_clipboard_text: Option<String>,
    cached_clipboard: Option<arboard::Clipboard>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    snapshot: Arc<Mutex<MainSnapshot>>,
    bytes_in: u64,
    bytes_out: u64,
    last_ping_at: Option<Instant>,
    /// True after `maybe_request_client_mouse_mode` sends a
    /// `MOUSE_MODE_REQUEST(CLIENT)` and until a MOUSE_MODE
    /// message confirms we're in CLIENT mode. Stops a flappy
    /// or hostile server from amplifying outbound requests
    /// 1:1 on inbound MOUSE_MODE messages.
    mouse_mode_request_pending: bool,
}

impl MainChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<RepaintNotify>,
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
            repaint_notify,
            buffer: Vec::with_capacity(65536),
            session_id: None,
            agent_connected: false,
            agent_tokens: 0,
            agent_caps_announced: false,
            monitors,
            monitors_config_rx,
            pending_monitors_config: None,
            last_sent_monitors_config: None,
            guest_caps_received: false,
            channels_requested: false,
            last_clipboard_text: None,
            cached_clipboard: None,
            capture,
            byte_counter,
            traffic,
            snapshot,
            bytes_in: 0,
            bytes_out: 0,
            last_ping_at: None,
            mouse_mode_request_pending: false,
        }
    }

    #[allow(dead_code)]
    pub fn session_id(&self) -> Option<u32> {
        self.session_id
    }

    /// Run the main channel event loop
    /// Get or create the cached clipboard instance. Returns None
    /// if the clipboard cannot be opened (e.g. no display server).
    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.cached_clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => self.cached_clipboard = Some(cb),
                Err(e) => {
                    debug!("main: failed to open clipboard: {}", e);
                    return None;
                }
            }
        }
        self.cached_clipboard.as_mut()
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("main: channel started");

        let mut resize_debounce: Option<tokio::time::Instant> = None;
        let mut clipboard_interval = tokio::time::interval(std::time::Duration::from_millis(500));
        clipboard_interval.tick().await;
        let mut last_data_received = tokio::time::Instant::now();
        let keepalive_timeout = std::time::Duration::from_secs(30);

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
                        self.repaint_notify.notify_one();
                        break;
                    }

                    last_data_received = tokio::time::Instant::now();
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
                        self.repaint_notify.notify_one();
                        self.maybe_send_agent_monitors_config().await?;
                    }
                }
                _ = clipboard_interval.tick() => {
                    if self.agent_connected && self.agent_caps_announced {
                        self.poll_host_clipboard().await?;
                    }
                }
                _ = tokio::time::sleep_until(last_data_received + keepalive_timeout) => {
                    info!("main: no data received for {}s, assuming disconnected", keepalive_timeout.as_secs());
                    self.event_tx
                        .send(ChannelEvent::Disconnected(ChannelType::Main))
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                    break;
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
                    .send(ChannelEvent::AgentConnected(self.agent_connected))
                    .await
                    .ok();
                self.repaint_notify.notify_one();
                let mode_name = match init.current_mouse_mode {
                    1 => "server (relative)",
                    2 => "client (absolute)",
                    other => {
                        warn!("main: unknown mouse mode {}", other);
                        "unknown"
                    }
                };
                info!(
                    "main: mouse mode={} ({}), supported_modes={}",
                    init.current_mouse_mode, mode_name, init.supported_mouse_modes
                );
                self.event_tx
                    .send(ChannelEvent::MouseMode(init.current_mouse_mode))
                    .await
                    .ok();
                self.repaint_notify.notify_one();

                // Request client mouse mode (absolute positioning) if
                // the server supports it. Client mode allows absolute
                // MOUSE_POSITION messages; without it the server
                // expects relative MOUSE_MOTION which ryll does not
                // yet implement.
                self.maybe_request_client_mouse_mode(
                    init.supported_mouse_modes,
                    init.current_mouse_mode,
                )
                .await?;
            }

            main_server::MOUSE_MODE => {
                // Server notifies us of a mouse mode change (may be
                // in response to our MOUSE_MODE_REQUEST, or
                // unprompted after a guest reboot).
                //
                // Wire format is two u16s: supported_modes then
                // current_mode. Parsing it as a u32 produces garbage
                // like 131075 (=0x00020003 when supported=3 and
                // current=2) which then fails every mode check.
                if let Some((supported, current)) = parse_mouse_mode_payload(payload) {
                    let mode_name = match current {
                        1 => "server (relative)",
                        2 => "client (absolute)",
                        _ => "unknown",
                    };
                    info!(
                        "main: mouse mode changed to {} ({}), supported_modes={}",
                        current, mode_name, supported
                    );
                    // Clearing happens regardless of whether the
                    // server's MOUSE_MODE was a direct response to
                    // our request — if we're now in CLIENT mode,
                    // there's nothing left to ask for.
                    if current as u32 == MOUSE_MODE_CLIENT {
                        self.mouse_mode_request_pending = false;
                    }
                    self.event_tx
                        .send(ChannelEvent::MouseMode(current as u32))
                        .await
                        .ok();
                    self.repaint_notify.notify_one();

                    // The server often reverts to SERVER mode after a
                    // guest reboot; re-request CLIENT mode so the
                    // absolute MOUSE_POSITION path keeps working.
                    self.maybe_request_client_mouse_mode(supported as u32, current as u32)
                        .await?;
                } else {
                    warn!("main: short MOUSE_MODE payload ({} bytes)", payload.len());
                }
            }

            main_server::MULTI_MEDIA_TIME => {
                // Periodic multimedia-time tick for audio/video sync.
                // Not wired into playback yet; accept the payload so
                // --pedantic doesn't flag it as an unknown opcode.
                if payload.len() >= 4 {
                    let mm_time =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    debug!("main: multi_media_time={}", mm_time);
                } else {
                    debug!(
                        "main: short MULTI_MEDIA_TIME payload ({} bytes)",
                        payload.len()
                    );
                }
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
                self.repaint_notify.notify_one();
            }

            main_server::PING => {
                let now = Instant::now();
                if let Some(last) = self.last_ping_at {
                    // f32 storage matches the LatencyTracker history
                    // Vec<f32>; loss of precision is irrelevant for a
                    // sub-millisecond sparkline.
                    let sample_ms = (now - last).as_secs_f64() * 1000.0;
                    self.event_tx
                        .send(ChannelEvent::Latency {
                            sample_ms: sample_ms as f32,
                        })
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                }
                self.last_ping_at = Some(now);

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
                if ping.id > 0 && self.session_id.is_some() && !self.channels_requested {
                    self.channels_requested = true;
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
                self.repaint_notify.notify_one();
            }

            main_server::AGENT_CONNECTED => {
                info!("main: vdagent connected");
                self.agent_connected = true;
                self.event_tx
                    .send(ChannelEvent::AgentConnected(true))
                    .await
                    .ok();
                self.connect_agent().await?;
            }

            main_server::AGENT_DISCONNECTED => {
                info!("main: vdagent disconnected");
                self.agent_connected = false;
                self.event_tx
                    .send(ChannelEvent::AgentConnected(false))
                    .await
                    .ok();
                self.agent_caps_announced = false;
                self.guest_caps_received = false;
            }

            main_server::AGENT_DATA => {
                if payload.len() >= 20 {
                    let agent_type =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    let agent_size =
                        u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]])
                            as usize;
                    let agent_payload = &payload[20..20 + agent_size.min(payload.len() - 20)];
                    debug!(
                        "main: agent_data from server: type={}, size={}",
                        agent_type, agent_size
                    );
                    self.handle_agent_message(agent_type, agent_payload).await?;
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
                // Unknown opcode — log hex once per msg_type, silent on repeat.
                logging::log_unknown_once("main", msg_type, payload);
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

    /// Send `MOUSE_MODE_REQUEST(CLIENT)` when the server supports
    /// CLIENT (absolute) mode but is currently in another mode.
    /// Called from both the INIT handler at session start and the
    /// MOUSE_MODE handler so a guest reboot — which typically
    /// reverts the server to SERVER mode — can recover absolute
    /// positioning without a reconnect.
    ///
    /// Skips sending if a prior request is already outstanding
    /// (`mouse_mode_request_pending`). This caps outbound request
    /// volume at one per round-trip, so a flappy or hostile
    /// server toggling `current_mode` can't amplify its MOUSE_MODE
    /// messages into a storm of client-side requests.
    async fn maybe_request_client_mouse_mode(
        &mut self,
        supported_modes: u32,
        current_mode: u32,
    ) -> Result<()> {
        if !should_request_client_mouse_mode(supported_modes, current_mode) {
            return Ok(());
        }
        if self.mouse_mode_request_pending {
            debug!("main: client mouse mode request already pending; skipping");
            return Ok(());
        }
        info!("main: requesting client mouse mode");
        let mut mode_payload = Vec::new();
        mode_payload.write_u32::<LittleEndian>(MOUSE_MODE_CLIENT)?;
        let msg = make_message(main_client::MOUSE_MODE_REQUEST, &mode_payload);
        self.send_with_log(main_client::MOUSE_MODE_REQUEST, &msg)
            .await?;
        self.mouse_mode_request_pending = true;
        Ok(())
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
            | (1u32 << VD_AGENT_CAP_REPLY)
            | (1u32 << VD_AGENT_CAP_CLIPBOARD_BY_DEMAND)
            | (1u32 << VD_AGENT_CAP_CLIPBOARD_SELECTION);
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

        self.send_agent_data_message(VD_AGENT_MONITORS_CONFIG, &payload)
            .await
    }

    async fn send_agent_data_message(&mut self, ty: u32, payload: &[u8]) -> Result<bool> {
        if self.agent_tokens == 0 {
            return Ok(false);
        }

        let mut agent = Vec::with_capacity(20 + payload.len());
        agent.write_u32::<LittleEndian>(VD_AGENT_PROTOCOL)?;
        agent.write_u32::<LittleEndian>(ty)?;
        agent.write_u64::<LittleEndian>(0)?;
        agent.write_u32::<LittleEndian>(payload.len() as u32)?;
        agent.extend_from_slice(payload);

        const MAX_CHUNK: usize = 2048 - 6;
        let mut offset = 0;
        while offset < agent.len() {
            if self.agent_tokens == 0 {
                return Ok(false);
            }
            let end = (offset + MAX_CHUNK).min(agent.len());
            let msg = make_message(main_client::AGENT_DATA, &agent[offset..end]);
            self.send_with_log(main_client::AGENT_DATA, &msg).await?;
            self.agent_tokens = self.agent_tokens.saturating_sub(1);
            offset = end;
        }

        Ok(true)
    }

    async fn handle_agent_message(&mut self, agent_type: u32, payload: &[u8]) -> Result<()> {
        if !self.guest_caps_received {
            self.guest_caps_received = true;
            debug!("main: guest agent active");
        }
        match agent_type {
            VD_AGENT_CLIPBOARD_GRAB => {
                // payload: selection(u32) + format(u32)
                if payload.len() >= 8 {
                    let format =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    if format == VD_AGENT_CLIPBOARD_UTF8_TEXT {
                        debug!("main: clipboard grab from guest, requesting data");
                        self.send_clipboard_request().await?;
                    }
                }
            }
            VD_AGENT_CLIPBOARD => {
                // payload: selection(u32) + format(u32) + data
                let offset = 4;
                if payload.len() > offset + 4 {
                    let _format = u32::from_le_bytes([
                        payload[offset],
                        payload[offset + 1],
                        payload[offset + 2],
                        payload[offset + 3],
                    ]);
                    let data = &payload[offset + 4..];
                    if !data.is_empty() {
                        let text = String::from_utf8_lossy(data).to_string();
                        // Log byte count only — clipboard content may
                        // contain passwords or sensitive data.
                        info!("main: clipboard from guest ({} bytes)", text.len());
                        if let Some(cb) = self.clipboard() {
                            match cb.set_text(&text) {
                                Ok(()) => debug!("main: host clipboard updated"),
                                Err(e) => {
                                    debug!("main: clipboard set failed: {}", e);
                                    self.cached_clipboard = None;
                                }
                            }
                        }
                    }
                }
            }
            VD_AGENT_CLIPBOARD_REQUEST => {
                // payload: selection(u32) + format(u32)
                if payload.len() >= 8 {
                    let format =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    if format == VD_AGENT_CLIPBOARD_UTF8_TEXT {
                        debug!("main: clipboard request from guest");
                        let text = self.clipboard().and_then(|cb| cb.get_text().ok());
                        if let Some(text) = text {
                            self.send_clipboard_data(&text).await?;
                        }
                    }
                }
            }
            VD_AGENT_CLIPBOARD_RELEASE => {
                debug!("main: clipboard release from guest");
            }
            VD_AGENT_ANNOUNCE_CAPABILITIES => {
                self.guest_caps_received = true;
                debug!("main: received agent capabilities from guest");
                if payload.len() >= 4 {
                    let request =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    if request == 1 {
                        self.agent_caps_announced = false;
                        self.maybe_send_announce_capabilities().await?;
                    }
                }
            }
            _ => {
                debug!("main: unhandled agent message type={}", agent_type);
            }
        }
        Ok(())
    }

    async fn poll_host_clipboard(&mut self) -> Result<()> {
        let text = match self.clipboard().and_then(|cb| cb.get_text().ok()) {
            Some(t) => t,
            None => return Ok(()),
        };

        if text.is_empty() {
            return Ok(());
        }

        let changed = match &self.last_clipboard_text {
            Some(prev) => prev != &text,
            None => true,
        };

        if changed {
            // Log byte count only — clipboard content may contain
            // passwords or sensitive data.
            info!("main: host clipboard changed ({} bytes)", text.len());
            self.last_clipboard_text = Some(text);
            self.send_clipboard_grab().await?;
        }

        Ok(())
    }

    async fn send_clipboard_grab(&mut self) -> Result<bool> {
        let mut payload = Vec::with_capacity(8);
        payload.write_u32::<LittleEndian>(0)?;
        payload.write_u32::<LittleEndian>(VD_AGENT_CLIPBOARD_UTF8_TEXT)?;
        self.send_agent_data_message(VD_AGENT_CLIPBOARD_GRAB, &payload)
            .await
    }

    async fn send_clipboard_request(&mut self) -> Result<bool> {
        let mut payload = Vec::with_capacity(8);
        payload.write_u32::<LittleEndian>(0)?;
        payload.write_u32::<LittleEndian>(VD_AGENT_CLIPBOARD_UTF8_TEXT)?;
        self.send_agent_data_message(VD_AGENT_CLIPBOARD_REQUEST, &payload)
            .await
    }

    async fn send_clipboard_data(&mut self, text: &str) -> Result<bool> {
        let text_bytes = text.as_bytes();
        let mut payload = Vec::with_capacity(8 + text_bytes.len());
        payload.write_u32::<LittleEndian>(0)?;
        payload.write_u32::<LittleEndian>(VD_AGENT_CLIPBOARD_UTF8_TEXT)?;
        payload.extend_from_slice(text_bytes);
        self.send_agent_data_message(VD_AGENT_CLIPBOARD, &payload)
            .await
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

#[cfg(test)]
mod tests {
    use super::{parse_mouse_mode_payload, should_request_client_mouse_mode};
    use shakenfist_spice_protocol::{MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER};

    // Payload bytes observed in the 2026-04-23 macbook bug-report
    // main.pcap. Parsing these as a single little-endian u32 yields
    // 131075 / 65537 / 65539 — which failed every mode check in the
    // GUI and left clicks broken after a guest reboot.
    #[test]
    fn parse_mouse_mode_splits_supported_and_current() {
        // supported=3 (both), current=2 (CLIENT) — initial negotiation.
        assert_eq!(
            parse_mouse_mode_payload(&[0x03, 0x00, 0x02, 0x00]),
            Some((3, 2))
        );
        // supported=1 (server only), current=1 (SERVER) — right after
        // guest reboot, agent gone.
        assert_eq!(
            parse_mouse_mode_payload(&[0x01, 0x00, 0x01, 0x00]),
            Some((1, 1))
        );
        // supported=3 (both), current=1 (SERVER) — agent back but
        // server still in SERVER mode; this is the case that must
        // trigger a CLIENT re-request.
        assert_eq!(
            parse_mouse_mode_payload(&[0x03, 0x00, 0x01, 0x00]),
            Some((3, 1))
        );
    }

    #[test]
    fn parse_mouse_mode_rejects_short_payload() {
        assert_eq!(parse_mouse_mode_payload(&[]), None);
        assert_eq!(parse_mouse_mode_payload(&[0x03, 0x00, 0x01]), None);
    }

    #[test]
    fn should_request_client_when_server_supports_it_but_is_in_server_mode() {
        // supported=3 (bitmask covering CLIENT), current=1 (SERVER):
        // this is the post-guest-reboot case the macbook report hit.
        assert!(should_request_client_mouse_mode(3, MOUSE_MODE_SERVER));
    }

    #[test]
    fn should_not_request_client_when_already_in_client_mode() {
        assert!(!should_request_client_mouse_mode(3, MOUSE_MODE_CLIENT));
    }

    #[test]
    fn should_not_request_client_when_server_does_not_support_it() {
        // supported=1 (SERVER only); no point asking for something
        // the server can't do.
        assert!(!should_request_client_mouse_mode(
            MOUSE_MODE_SERVER,
            MOUSE_MODE_SERVER
        ));
    }
}
