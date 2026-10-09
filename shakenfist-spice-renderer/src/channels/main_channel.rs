/// Main channel handler - session management, ping/pong, channel list
use anyhow::{Context, Result};
use byteorder::{LittleEndian, WriteBytesExt};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::mm_clock::MmClock;
use crate::opcode_counters::OpcodeCounters;
use crate::session_state::SessionState;
use crate::snapshots::MainSnapshot;
use crate::{
    ByteCounter, CaptureSink, ClipboardBackend, LogConfig, NotificationEntry, NotificationSource,
    TrafficSink,
};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, take_message, AgentDisconnected, AgentTokens, ChannelsList, Disconnecting,
    MainInit, MainMouseMode, MouseModeRequest, MultiMediaTime, Notify, Ping, SetAck, WireType,
};
use shakenfist_spice_protocol::{
    main_client, main_server, ChannelType, NotifySeverity, MOUSE_MODE_CLIENT,
};

use super::agent_queue::AgentSendQueue;
use super::{ChannelEvent, EventSink, MAX_MESSAGE_BODY};

/// True when the server supports CLIENT (absolute) mouse mode but
/// is currently in a different mode. Used to decide whether to
/// send a `MOUSE_MODE_REQUEST(CLIENT)` after INIT or after a
/// MOUSE_MODE change (e.g. after the guest reboots, the server
/// often reverts to SERVER/relative mode).
pub(crate) fn should_request_client_mouse_mode(supported: u32, current: u32) -> bool {
    supported & MOUSE_MODE_CLIENT != 0 && current != MOUSE_MODE_CLIENT
}

/// Milliseconds between two consecutive server PINGs, or `None` on the
/// first PING of a session, where there is no earlier timestamp to diff
/// against. `f32` matches the `LatencyTracker` history the sample feeds;
/// sub-millisecond precision is not visible in a sparkline.
fn ping_interval_ms(last: Option<Instant>, now: Instant) -> Option<f32> {
    let last = last?;
    Some(((now - last).as_secs_f64() * 1000.0) as f32)
}

/// Normalise text so the clipboard echo dedup is invariant
/// under line-ending munging during host-clipboard round
/// trips. Windows and some Wayland compositors flip
/// `\n` ↔ `\r\n` and trim or append trailing whitespace, so
/// the raw text we sent and the raw text we read back differ
/// even though the user-visible content is identical.
fn normalize_clipboard(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end()
        .to_string()
}

/// Hash the normalised clipboard text. Storing the hash
/// instead of the text keeps clipboard contents — which can
/// include passwords — out of the long-lived per-channel
/// state.
fn hash_clipboard(text: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    normalize_clipboard(text).hash(&mut h);
    h.finish()
}

/// Decode the body of a `VD_AGENT_REPLY`.
///
/// `vd_agent.h` declares `VDAgentReply` as a packed struct of
/// two little-endian `u32`s: `{ type, error }`. `type` echoes
/// the opcode of the request being acknowledged; `error` is
/// `VD_AGENT_SUCCESS` (0) on success or a failure code.
///
/// Returns `None` if `payload` is shorter than 8 bytes —
/// caller logs and skips. Pure function so the parse logic is
/// unit-testable without standing up a `MainChannel`.
fn parse_vd_agent_reply(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 8 {
        return None;
    }
    let reply_type = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let error = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    Some((reply_type, error))
}

/// True when a `VD_AGENT_ANNOUNCE_CAPABILITIES` body (`request` then
/// a little-endian `u32` capability bitmap) sets bit `cap`.
fn agent_caps_has(payload: &[u8], cap: u32) -> bool {
    let word = 4 + (cap as usize / 32) * 4;
    payload
        .get(word..word + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & (1 << (cap % 32)) != 0)
        .unwrap_or(false)
}

/// Split the selection header off a clipboard message body.
///
/// When the guest agent has announced `VD_AGENT_CAP_CLIPBOARD_SELECTION`,
/// every `VDAgentClipboard*` message starts with a `uint8_t selection`
/// and three reserved bytes (`vd_agent.h`). spice-vdagentd adds the header
/// only when its peer announced the capability, and spice-gtk reads it only
/// when the agent did. Without it the selection is implicitly CLIPBOARD.
/// Returns `None` if the header is expected but the body is too short.
fn split_clipboard_selection(payload: &[u8], has_selection: bool) -> Option<(u8, &[u8])> {
    if !has_selection {
        return Some((VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD, payload));
    }
    if payload.len() < 4 {
        return None;
    }
    Some((payload[0], &payload[4..]))
}

/// Read the leading `uint32_t type` of a `VDAgentClipboard` or
/// `VDAgentClipboardRequest` body (selection header already removed),
/// returning it with the data that follows.
fn split_clipboard_type(body: &[u8]) -> Option<(u32, &[u8])> {
    let ty = body.get(..4)?;
    Some((u32::from_le_bytes([ty[0], ty[1], ty[2], ty[3]]), &body[4..]))
}

/// True when a `VDAgentClipboardGrab` type list (selection header
/// already removed) offers `ty` anywhere, not only first.
fn clipboard_grab_offers(types: &[u8], ty: u32) -> bool {
    types
        .as_chunks::<4>()
        .0
        .iter()
        .any(|t| u32::from_le_bytes(*t) == ty)
}

/// Build a clipboard message body: the selection header when negotiated,
/// then a `uint32_t` type, then `data`. A one-type grab, a request and a
/// `VDAgentClipboard` all share this layout. ryll does not announce
/// `VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL`, so a grab carries no serial.
fn build_clipboard_payload(has_selection: bool, selection: u8, ty: u32, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + data.len());
    if has_selection {
        payload.extend_from_slice(&[selection, 0, 0, 0]);
    }
    payload.extend_from_slice(&ty.to_le_bytes());
    payload.extend_from_slice(data);
    payload
}

const VD_AGENT_PROTOCOL: u32 = 1;

// VDAgentMessage type values — must match spice-protocol/spice/vd_agent.h
#[allow(dead_code)]
const VD_AGENT_MOUSE_STATE: u32 = 1;
const VD_AGENT_MONITORS_CONFIG: u32 = 2;
const VD_AGENT_REPLY: u32 = 3;
const VD_AGENT_CLIPBOARD: u32 = 4;
#[allow(dead_code)]
const VD_AGENT_DISPLAY_CONFIG: u32 = 5;
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;
const VD_AGENT_CLIPBOARD_GRAB: u32 = 7;
const VD_AGENT_CLIPBOARD_REQUEST: u32 = 8;
const VD_AGENT_CLIPBOARD_RELEASE: u32 = 9;

// Clipboard format types
const VD_AGENT_CLIPBOARD_NONE: u32 = 0;
const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;

// Clipboard selections. ryll only syncs CLIPBOARD; PRIMARY and
// SECONDARY are X11 concepts with no equivalent on macOS or Windows.
const VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD: u8 = 0;

const VD_AGENT_CAP_MOUSE_STATE: u32 = 0;
const VD_AGENT_CAP_MONITORS_CONFIG: u32 = 1;
const VD_AGENT_CAP_REPLY: u32 = 2;
const VD_AGENT_CAP_CLIPBOARD_BY_DEMAND: u32 = 5;
const VD_AGENT_CAP_CLIPBOARD_SELECTION: u32 = 6;
const VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS: u32 = 1;

/// Request opcodes that the guest agent acknowledges with a
/// `VD_AGENT_REPLY` message. To add another type, append its
/// constant here — that is the only change needed on the send
/// side.
const REPLY_ELIGIBLE_AGENT_REQUEST_TYPES: &[u32] = &[VD_AGENT_MONITORS_CONFIG];

/// Maximum entries retained in the recent-reply-lag ring.
const MAX_RECENT_AGENT_REPLIES: usize = 16;

/// How long the client may sit at zero agent tokens with agent messages
/// still queued before we call the agent stalled and push a Warn
/// notification.
///
/// spice-server hands tokens back as the guest consumes client data, in
/// batches of `REDS_TOKENS_TO_SEND` (5) out of a window of
/// `REDS_AGENT_WINDOW_SIZE` (10), so a client at zero tokens has at least
/// six chunks the guest has not read yet. A reading agent clears that
/// in milliseconds; five seconds of it is a guest that has stopped
/// reading. Messages the server consumes itself (MONITORS_CONFIG on QXL
/// guests) return their tokens without involving the agent, so this
/// cannot misfire the way waiting for VD_AGENT_REPLY did (#429).
const STUCK_AGENT_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(5);

/// How often a continuing stall repeats its Warn notification.
///
/// The notification text never changes, so each repeat folds into the
/// first entry and raises its `count`, as long as repeats land inside
/// the GUI's dedup window (`NOTIFICATION_DEDUP_WINDOW`, 30 s, in
/// `ryll/src/notifications.rs`). Keep this shorter than that window, or
/// a long stall becomes one panel entry per repeat again.
const STUCK_AGENT_NOTIFY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// Stable text of the stalled-agent notification; see
/// `STUCK_AGENT_NOTIFY_INTERVAL` for why it must not vary.
const STUCK_AGENT_MESSAGE: &str =
    "Guest agent is not accepting messages (clipboard and display resize are on hold)";

/// Ceiling for `outstanding_agent_request_count`.
///
/// On QXL guests spice-server hands MONITORS_CONFIG to the display
/// device instead of the agent, so no VD_AGENT_REPLY ever comes and the
/// count grows by one per monitors-config send for the whole session.
/// Past this point the exact number carries no information, and it keeps
/// the bug-report field a readable two digits.
const MAX_OUTSTANDING_AGENT_REQUESTS: u32 = 99;

/// Whether the agent is holding the client up: connected, no tokens left,
/// and messages still waiting to go.
fn agent_starved(agent_connected: bool, agent_tokens: u32, messages_queued: bool) -> bool {
    agent_connected && agent_tokens == 0 && messages_queued
}

/// Whether to push the stalled-agent notification now.
///
/// `starved_since` is when the current starvation began, if it has;
/// `last_notified` is when this starvation episode last notified, reset
/// to `None` when the episode ends.
fn should_warn_agent_stalled(
    starved_since: Option<Instant>,
    last_notified: Option<Instant>,
    now: Instant,
) -> bool {
    let Some(since) = starved_since else {
        return false;
    };
    if now.saturating_duration_since(since) < STUCK_AGENT_THRESHOLD {
        return false;
    }
    last_notified.is_none_or(|t| now.saturating_duration_since(t) >= STUCK_AGENT_NOTIFY_INTERVAL)
}

pub struct MainChannel {
    stream: SpiceStream,
    events: EventSink,
    /// Mouse mode and agent state, published as latest values rather
    /// than as events so a stalled UI cannot lose them; see
    /// `crate::session_state`.
    state: SessionState,
    buffer: Vec<u8>,
    session_id: Option<u32>,
    agent_connected: bool,
    agent_tokens: u32,
    /// Agent messages waiting for `agent_tokens`; see `agent_queue`.
    agent_queue: AgentSendQueue,
    agent_caps_announced: bool,
    guest_caps_received: bool,
    /// Whether the guest agent announced `VD_AGENT_CAP_CLIPBOARD_SELECTION`,
    /// so that clipboard messages in both directions carry a selection
    /// header. `None` until the guest's capabilities arrive: the layout is
    /// unknown before then, so no clipboard traffic is sent or parsed.
    guest_clipboard_selection: Option<bool>,
    channels_requested: bool,
    monitors: u8,
    monitors_config_rx: mpsc::Receiver<(u32, u32)>,
    pending_monitors_config: Option<(u32, u32)>,
    last_sent_monitors_config: Option<(u32, u32)>,
    last_clipboard_hash: Option<u64>,
    clipboard: Option<Arc<dyn ClipboardBackend>>,
    capture: Option<Arc<dyn CaptureSink>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    log_config: LogConfig,
    snapshot: Arc<Mutex<MainSnapshot>>,
    bytes_in: u64,
    bytes_out: u64,
    last_ping_at: Option<Instant>,
    /// Local cache of disconnect-cause diagnostic fields,
    /// flushed to `snapshot` by `update_snapshot()`. Mirrors the
    /// matching fields on `MainSnapshot`.
    last_recv_ts_secs: Option<f64>,
    last_send_ts_secs: Option<f64>,
    ping_recv_count: u32,
    pong_send_count: u32,
    last_ping_recv_ts_secs: Option<f64>,
    /// True after `maybe_request_client_mouse_mode` sends a
    /// `MOUSE_MODE_REQUEST(CLIENT)` and until a MOUSE_MODE
    /// message confirms we're in CLIENT mode. Stops a flappy
    /// or hostile server from amplifying outbound requests
    /// 1:1 on inbound MOUSE_MODE messages.
    mouse_mode_request_pending: bool,
    /// Fired once when the INIT message arrives. The session
    /// orchestrator awaits this to learn the session id before
    /// connecting secondary channels. Wrapped in Option so the
    /// signal can be consumed exactly once via `take()`.
    session_init_signal: Option<oneshot::Sender<u32>>,
    /// Fired once when CHANNELS_LIST arrives. Carries the list of
    /// (ChannelType, channel_id) tuples the server advertised,
    /// which the orchestrator uses to spawn secondary channels.
    channels_avail_signal: Option<oneshot::Sender<Vec<(ChannelType, u8)>>>,
    /// Count of pcap-capture packets rejected by the writer task's
    /// queue. Mirrored into `MainSnapshot::writer_dropped_count`.
    capture_dropped_count: u64,
    /// The server's mouse mode as last announced by MAIN_INIT or
    /// MOUSE_MODE. Mirrored into `MainSnapshot::server_mouse_mode`.
    server_mouse_mode: Option<u32>,
    /// Bounded per-opcode message counters; flushed to the
    /// snapshot by `update_snapshot`. See `OpcodeCounters`.
    opcodes: OpcodeCounters,
    /// Shared mm_time clock — writer side. Updated from
    /// `MAIN_INIT::multi_media_time` and from
    /// `MULTI_MEDIA_TIME` messages. The display channel reads
    /// the same `Arc` to compute "now in mm_time" at
    /// `STREAM_REPORT` send time.
    mm_clock: Arc<MmClock>,
    /// Per-request-type send timestamps for REPLY-eligible
    /// agent requests. Keyed by VD_AGENT_* opcode. Populated in
    /// `send_agent_data_message` for types in
    /// `REPLY_ELIGIBLE_AGENT_REQUEST_TYPES`; consumed on REPLY
    /// receipt in `handle_agent_message`.
    ///
    /// `HashMap` (rather than `Option<(u32, Instant)>`) because
    /// `REPLY_ELIGIBLE_AGENT_REQUEST_TYPES` is sized to grow:
    /// today it has one entry (MONITORS_CONFIG), DISPLAY_CONFIG
    /// is the documented next addition for Windows agents. One
    /// allocation per channel + one lookup per send is a fine
    /// price for a fixed API surface as types are added.
    agent_request_send_ts: HashMap<u32, Instant>,
    /// Cumulative count of REPLY-eligible agent requests sent.
    agent_request_count: u32,
    /// Cumulative count of VD_AGENT_REPLY messages received.
    agent_reply_count: u32,
    /// Cumulative count of REPLY messages with non-zero `error`
    /// (anything other than VD_AGENT_SUCCESS = 0).
    agent_reply_error_count: u32,
    /// Session-relative seconds at the most recent REPLY
    /// receipt.
    last_agent_reply_ts_secs: Option<f64>,
    /// Microseconds between the most recent matched request
    /// send and its REPLY. None until the first matched REPLY.
    last_agent_reply_lag_us: Option<u32>,
    /// Bounded ring of recent reply lags (µs), oldest first.
    /// Capped at `MAX_RECENT_AGENT_REPLIES` (16).
    recent_agent_reply_lag_us: VecDeque<u32>,
    /// Count of REPLY-eligible requests sent without a matching
    /// REPLY yet. Increments on send; decrements (saturating)
    /// on every REPLY received.
    outstanding_agent_request_count: u32,
    /// When the client last ran out of agent tokens with messages still
    /// queued, if it has not had tokens back since. See `agent_starved`.
    agent_starved_since: Option<Instant>,
    /// `agent_starved_since` as session-relative seconds, for snapshots.
    agent_starved_since_ts_secs: Option<f64>,
    /// Starvation episodes that lasted past `STUCK_AGENT_THRESHOLD`.
    agent_stall_count: u32,
    /// When the current starvation episode last notified; `None` outside
    /// an episode, or before its first notification.
    last_stuck_agent_notification_at: Option<Instant>,
}

impl MainChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        events: EventSink,
        state: SessionState,
        capture: Option<Arc<dyn CaptureSink>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<dyn TrafficSink>,
        snapshot: Arc<Mutex<MainSnapshot>>,
        monitors_config_rx: mpsc::Receiver<(u32, u32)>,
        monitors: u8,
        log_config: LogConfig,
        clipboard: Option<Arc<dyn ClipboardBackend>>,
        session_init_signal: oneshot::Sender<u32>,
        channels_avail_signal: oneshot::Sender<Vec<(ChannelType, u8)>>,
        mm_clock: Arc<MmClock>,
    ) -> Self {
        MainChannel {
            stream,
            events,
            state,
            buffer: Vec::with_capacity(65536),
            session_id: None,
            agent_connected: false,
            agent_tokens: 0,
            agent_queue: AgentSendQueue::default(),
            agent_caps_announced: false,
            monitors,
            monitors_config_rx,
            pending_monitors_config: None,
            last_sent_monitors_config: None,
            guest_caps_received: false,
            guest_clipboard_selection: None,
            channels_requested: false,
            last_clipboard_hash: None,
            clipboard,
            capture,
            byte_counter,
            traffic,
            log_config,
            snapshot,
            bytes_in: 0,
            bytes_out: 0,
            last_ping_at: None,
            last_recv_ts_secs: None,
            last_send_ts_secs: None,
            ping_recv_count: 0,
            pong_send_count: 0,
            last_ping_recv_ts_secs: None,
            mouse_mode_request_pending: false,
            session_init_signal: Some(session_init_signal),
            channels_avail_signal: Some(channels_avail_signal),
            capture_dropped_count: 0,
            server_mouse_mode: None,
            opcodes: OpcodeCounters::new(message_names::main_server, message_names::main_client),
            mm_clock,
            agent_request_send_ts: HashMap::new(),
            agent_request_count: 0,
            agent_reply_count: 0,
            agent_reply_error_count: 0,
            last_agent_reply_ts_secs: None,
            last_agent_reply_lag_us: None,
            recent_agent_reply_lag_us: VecDeque::new(),
            outstanding_agent_request_count: 0,
            agent_starved_since: None,
            agent_starved_since_ts_secs: None,
            agent_stall_count: 0,
            last_stuck_agent_notification_at: None,
        }
    }

    #[allow(dead_code)]
    pub fn session_id(&self) -> Option<u32> {
        self.session_id
    }

    /// Public entry point. Wraps `run_loop` so any error
    /// propagating out of the inner select! arms is logged
    /// before the task ends. Without this, `?` propagations
    /// inside the loop end the task silently, which in
    /// session-001d hid the cause of main going dark mid-run
    /// with no log line explaining why.
    ///
    /// `Box::pin` heap-allocates the inner state machine so
    /// the wrapper does not inline `run_loop`'s entire async
    /// state into its own frame. Without this, debug builds
    /// overflowed the tokio worker stack at channel startup
    /// (verified on macOS with session-001e: stack overflow
    /// in `tokio-rt-worker` before the first PING was even
    /// processed).
    pub async fn run(&mut self) -> Result<()> {
        let result = Box::pin(self.run_loop()).await;
        match &result {
            Ok(()) => info!("main: run loop exited cleanly"),
            Err(e) => error!("main: run loop exited with error: {:#}", e),
        }
        result
    }

    // `last_arm` is observable only when the heartbeat arm fires
    // before the next iteration overwrites it; all other reads of
    // it look "dead" to clippy. The lint is correct in the strict
    // sense but uninformative for diagnostic state, so suppress
    // it for this function only. Will go away when the heartbeat
    // is removed.
    #[allow(unused_assignments)]
    async fn run_loop(&mut self) -> Result<()> {
        info!("main: channel started");

        let mut resize_debounce: Option<tokio::time::Instant> = None;
        // Diagnostic env var for the K1 hang investigation. When
        // RYLL_DISABLE_CLIPBOARD_POLL=1 is set in the environment,
        // the clipboard_interval is replaced by `None` and the
        // corresponding select! arm becomes a never-resolving
        // future (`std::future::pending`), effectively removing
        // it from main's loop. If K1 stops reproducing under this
        // flag, the clipboard arm is the trigger; if it still
        // reproduces, the bug is elsewhere. Will be removed when
        // K1 is closed.
        let disable_clipboard_poll = std::env::var("RYLL_DISABLE_CLIPBOARD_POLL")
            .map(|v| v == "1")
            .unwrap_or(false);
        if disable_clipboard_poll {
            info!("main: clipboard polling disabled via RYLL_DISABLE_CLIPBOARD_POLL");
        }
        let mut clipboard_interval = if disable_clipboard_poll {
            None
        } else {
            let mut i = tokio::time::interval(std::time::Duration::from_millis(500));
            i.tick().await;
            Some(i)
        };

        // The watchdog thread and its heartbeat.  The store side stays
        // here (see the select loop below); the thread itself lives in
        // watchdog.rs, so its deliberate raw-stderr reporting -- and the
        // wave 1 exemption marker justifying it -- scope to that module
        // rather than to all of this file.  Note that naming the marker
        // here in full would exempt this file again, which is the whole
        // thing the move was for.
        let last_heartbeat_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        super::watchdog::spawn_if_enabled(&last_heartbeat_ms);
        // Diagnostic heartbeat for the K1 hang investigation
        // (sessions 001b/c/d/f/g). main's task has been observed
        // to silently stop polling some time after T+465 across
        // every K1 reproduction — neither the read branch nor the
        // keepalive branch fires after that, but the task also
        // doesn't exit. The wrapper-level "exited cleanly" /
        // "exited with error" log lines never appear for main,
        // confirming run_loop doesn't return — it's blocked on
        // an `.await` somewhere we can't see from snapshots.
        //
        // This heartbeat fires every 1 s. Each tick logs which
        // select arm fired most recently, so when main goes
        // dark we can read backwards to "the last arm that
        // ran was X" and narrow the hang to a specific code
        // path. Removing this when K1 is closed.
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
        heartbeat.tick().await;
        // Stalled-agent warning. Polls every 5 s for agent token starvation
        // (see `STUCK_AGENT_THRESHOLD`).
        let mut stuck_agent_check = tokio::time::interval(std::time::Duration::from_secs(5));
        stuck_agent_check.tick().await;
        let mut last_arm: &'static str = "startup";
        // Iteration counter for K1 hang investigation. Incremented at
        // the top of every loop body. Logged from the heartbeat arm
        // alongside last_arm. If iter_count keeps climbing while
        // last_arm stays the same, the loop is iterating but no
        // non-heartbeat arm is firing (timer wakers/IO wakers are
        // silent). If iter_count stops climbing entirely, the loop
        // body itself is stuck somewhere.
        let mut iter_count: u64 = 0;
        let mut last_data_received = tokio::time::Instant::now();
        // Backstop for an unreachable / dead server, not a primary
        // mechanism. The SPICE server's own connectivity check is at
        // 30 s (CLIENT_CONNECTIVITY_TIMEOUT, main-channel-client.cpp:38)
        // and produces a more informative log line than our local
        // timer. Setting this above 30 s ensures the server-side
        // check fires unambiguously first when the server is still
        // alive, leaving our timer to catch the case where the
        // server disappears without any FIN/RST.
        let keepalive_timeout = std::time::Duration::from_secs(90);

        loop {
            iter_count = iter_count.wrapping_add(1);
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
                        SpiceStream::TlsServer(s) => {
                            use tokio::io::AsyncReadExt;
                            s.read(&mut chunk).await
                        }
                    }
                } => {
                    last_arm = "read";
                    let n = n?;
                    if n == 0 {
                        info!("main: channel disconnected");
                        self.emit_session_ended().await;
                        break;
                    }

                    last_data_received = tokio::time::Instant::now();
                    self.byte_counter.add(n as u64);
                    if let Some(ref c) = self.capture {
                        if !c.packet_received("main", &chunk[..n]) {
                            self.capture_dropped_count =
                                self.capture_dropped_count.saturating_add(1);
                        }
                    }
                    self.buffer.extend_from_slice(&chunk[..n]);
                    self.bytes_in += n as u64;
                    self.last_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

                    last_arm = "read+process_messages";
                    self.process_messages().await?;
                    last_arm = "read+process_messages_done";
                }
                resize = monitors_config_rx.recv() => {
                    last_arm = "monitors_config_rx";
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
                    last_arm = "debounce_sleep";
                    resize_debounce = None;
                    if let Some((width, height)) = self.pending_monitors_config {
                        info!("main: resize debounced: {}x{}", width, height);
                        self.events.emit(ChannelEvent::MonitorsConfig { width, height }).await;
                        self.maybe_send_agent_monitors_config().await?;
                    }
                }
                _ = async {
                    match &mut clipboard_interval {
                        Some(i) => { i.tick().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    last_arm = "clipboard_interval";
                    if self.agent_connected
                        && self.agent_caps_announced
                        && self.guest_clipboard_selection.is_some()
                    {
                        last_arm = "clipboard_interval+poll";
                        self.poll_host_clipboard().await?;
                        last_arm = "clipboard_interval+poll_done";
                    }
                }
                _ = tokio::time::sleep_until(last_data_received + keepalive_timeout) => {
                    last_arm = "keepalive_timeout";
                    info!("main: no data received for {}s, assuming disconnected", keepalive_timeout.as_secs());
                    // Mark the snapshot before emitting Disconnected so
                    // the disconnect-cause record can distinguish "we
                    // timed ourselves out" from a real EOF / RST.
                    if let Ok(mut snap) = self.snapshot.lock() {
                        snap.keepalive_timeout_fired = true;
                    }
                    self.emit_session_ended().await;
                    break;
                }
                _ = stuck_agent_check.tick() => {
                    last_arm = "stuck_agent_check";
                    let now = Instant::now();
                    if should_warn_agent_stalled(
                        self.agent_starved_since,
                        self.last_stuck_agent_notification_at,
                        now,
                    ) {
                        last_arm = "stuck_agent_check+notify";
                        if self.last_stuck_agent_notification_at.is_none() {
                            self.agent_stall_count = self.agent_stall_count.saturating_add(1);
                        }
                        // The varying detail goes to the log and the
                        // snapshot; the notification text stays fixed so
                        // repeats coalesce.
                        let starved_secs = self
                            .agent_starved_since
                            .map(|t| now.saturating_duration_since(t).as_secs_f64())
                            .unwrap_or_default();
                        warn!(
                            "main: guest agent stalled: no agent tokens for {:.1}s, \
                             {} message(s) queued",
                            starved_secs,
                            self.agent_queue.len()
                        );
                        let entry = NotificationEntry::new(
                            NotifySeverity::Warn,
                            NotificationSource::Internal,
                            STUCK_AGENT_MESSAGE.to_string(),
                        );
                        self.events.emit(ChannelEvent::Notification(entry)).await;
                        self.last_stuck_agent_notification_at = Some(now);
                        last_arm = "stuck_agent_check+notify_done";
                    }
                }
                _ = heartbeat.tick() => {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    last_heartbeat_ms.store(now_ms, std::sync::atomic::Ordering::Relaxed);
                    debug!(
                        "main: heartbeat T+{:.1}s iter={} last_arm={} last_recv={:?} \
                         last_send={:?} pongs={}",
                        self.traffic.elapsed().as_secs_f64(),
                        iter_count,
                        last_arm,
                        self.last_recv_ts_secs,
                        self.last_send_ts_secs,
                        self.pong_send_count,
                    );
                }
            }
        }

        Ok(())
    }

    async fn process_messages(&mut self) -> Result<()> {
        while let Some(message) = take_message(&mut self.buffer, MAX_MESSAGE_BODY)? {
            let msg_type = message.header.message_type;
            self.traffic.record_received(
                "main",
                msg_type,
                message_names::main_server(msg_type),
                &message.raw,
            );

            self.handle_message(msg_type, message.payload()).await?;
        }

        self.update_snapshot();
        Ok(())
    }

    async fn handle_message(&mut self, msg_type: u16, payload: &[u8]) -> Result<()> {
        let msg_type_str = message_names::main_server(msg_type);

        // Log all messages in verbose mode
        if self.log_config.verbose {
            logging::log_message(
                "received",
                "main",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        // Count before dispatch so both known and unknown opcodes
        // reach the counters. Opcodes with no protocol name fold into
        // the unknown-opcode fields rather than growing the map; see
        // `OpcodeCounters`.
        self.opcodes.record_recv(msg_type);

        match msg_type {
            main_server::INIT => {
                let init = MainInit::decode(payload).context("malformed main INIT")?;
                info!("main: session initialized: id={}", init.session_id);

                // Seed the shared mm_time clock from the server's initial
                // multi_media_time. Display channel readers (STREAM_REPORT) need this
                // base before they can compute a meaningful "now in mm_time".
                self.mm_clock
                    .set(init.multi_media_time, self.traffic.elapsed().as_secs_f64());

                if self.log_config.verbose {
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

                // Signal the session orchestrator before any awaits so it
                // can proceed with secondary channel setup immediately.
                if let Some(sig) = self.session_init_signal.take() {
                    let _ = sig.send(init.session_id);
                }

                if self.agent_connected {
                    self.connect_agent().await?;
                }

                self.events
                    .emit(ChannelEvent::SessionInitialized(init.session_id))
                    .await;
                // Published after the event, as the event it replaced was,
                // so the GUI announces the session before the agent.
                self.publish_agent_connected();
                let mode_name = match init.current_mouse_mode {
                    1 => "server (relative)",
                    2 => "client (absolute)",
                    other => {
                        warn!("main: unknown mouse mode {}", other);
                        "unknown"
                    }
                };
                self.server_mouse_mode = Some(init.current_mouse_mode);
                info!(
                    "main: mouse mode={} ({}), supported_modes={}",
                    init.current_mouse_mode, mode_name, init.supported_mouse_modes
                );
                self.publish_mouse_mode(init.current_mouse_mode);

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
                // current=2) which then fails every mode check. A short
                // payload is warned about and skipped.
                if let Ok(MainMouseMode {
                    supported_modes: supported,
                    current_mode: current,
                }) = MainMouseMode::decode(payload)
                {
                    let mode_name = match current {
                        1 => "server (relative)",
                        2 => "client (absolute)",
                        _ => "unknown",
                    };
                    self.server_mouse_mode = Some(current as u32);
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
                    self.publish_mouse_mode(current as u32);

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
                // Periodic multimedia-time tick. The server uses
                // this to keep our `mm_time` clock in sync with
                // its own; the display channel reads the clock at
                // STREAM_REPORT send time to compute
                // `last_frame_delay`. Updating the shared
                // `MmClock` here also makes the value visible in
                // `MainSnapshot::mm_time_*` for bug reports.
                if let Ok(MultiMediaTime { time: mm_time }) = MultiMediaTime::decode(payload) {
                    debug!("main: multi_media_time={}", mm_time);
                    self.mm_clock
                        .set(mm_time, self.traffic.elapsed().as_secs_f64());
                } else {
                    debug!(
                        "main: short MULTI_MEDIA_TIME payload ({} bytes)",
                        payload.len()
                    );
                }
            }

            main_server::CHANNELS_LIST => {
                let list = ChannelsList::decode(payload).context("malformed CHANNELS_LIST")?;
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
                    if self.log_config.verbose {
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

                // Signal the session orchestrator before any awaits so it
                // can proceed with secondary channel setup immediately.
                if let Some(sig) = self.channels_avail_signal.take() {
                    let _ = sig.send(channels.clone());
                }

                self.events
                    .emit(ChannelEvent::ChannelsAvailable(channels))
                    .await;
            }

            main_server::PING => {
                let now = Instant::now();
                let interval_ms = ping_interval_ms(self.last_ping_at, now);
                self.last_ping_at = Some(now);
                self.ping_recv_count = self.ping_recv_count.saturating_add(1);
                self.last_ping_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

                let ping = Ping::decode(payload).context("malformed PING")?;

                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "ping_id={}, timestamp={}",
                        ping.id, ping.timestamp
                    ));
                }

                // Send pong response
                let mut pong_payload = Vec::new();
                ping.pong().write(&mut pong_payload);
                let response = make_message(main_client::PONG, &pong_payload);

                self.send_with_log(main_client::PONG, &response).await?;
                self.pong_send_count = self.pong_send_count.saturating_add(1);

                // Request channel list on first large ping
                if ping.id > 0 && self.session_id.is_some() && !self.channels_requested {
                    self.channels_requested = true;
                    self.request_channels_list().await?;
                }

                // Last, so a truncated PING is torn down without leaving a
                // sample behind and a stalled renderer cannot delay the PONG.
                if let Some(sample_ms) = interval_ms {
                    self.events.emit(ChannelEvent::Latency { sample_ms }).await;
                }
            }

            main_server::SET_ACK => {
                let set_ack = SetAck::decode(payload).context("malformed SET_ACK")?;

                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "generation={}, window={}",
                        set_ack.generation, set_ack.window
                    ));
                }

                // Send ack_sync response
                let mut ack_payload = Vec::new();
                set_ack.ack_sync().write(&mut ack_payload);
                let response = make_message(main_client::ACK_SYNC, &ack_payload);

                self.send_with_log(main_client::ACK_SYNC, &response).await?;
            }

            main_server::NOTIFY => {
                let notify = Notify::decode(payload).context("malformed NOTIFY")?;
                let severity = notify.severity_kind();
                let message = notify.message_text().into_owned();
                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "severity={:?}, visibility={:?}, what={}, message=\"{}\"",
                        severity,
                        notify.visibility_kind(),
                        notify.what,
                        message,
                    ));
                }
                match severity {
                    NotifySeverity::Error => {
                        warn!("main: server notify (error): {}", message)
                    }
                    NotifySeverity::Warn => {
                        warn!("main: server notify (warn): {}", message)
                    }
                    NotifySeverity::Info => info!("main: server notify: {}", message),
                }
                let mut entry = NotificationEntry::new(
                    severity,
                    NotificationSource::Spice {
                        channel: ChannelType::Main,
                        what: notify.what,
                    },
                    message,
                );
                if let Some(v) = notify.visibility_kind() {
                    entry = entry.with_visibility(v);
                }
                self.events.emit(ChannelEvent::Notification(entry)).await;
            }

            main_server::DISCONNECTING => {
                // The reason is only logged, so a short body is no reason
                // to skip the announcement.
                match Disconnecting::decode(payload) {
                    Ok(msg) => info!(
                        "main: server sent disconnect notification (reason={})",
                        msg.reason
                    ),
                    Err(_) => info!("main: server sent disconnect notification"),
                }
                // Deliberately `emit`, not `emit_session_ended`: this is
                // only an announcement, and the read loop carries on, so
                // blocking here on a stalled UI would stop main answering
                // PINGs, which is K1. The EOF that follows is what ends
                // the loop, and it reports the end without the timeout.
                // spice-server never sends this message anyway (it only
                // handles the client's `DISCONNECTING`), and spice-gtk
                // just logs it.
                self.events
                    .emit(ChannelEvent::Disconnected(ChannelType::Main))
                    .await;
            }

            main_server::AGENT_CONNECTED => {
                info!("main: vdagent connected");
                self.agent_connected = true;
                self.publish_agent_connected();
                self.connect_agent().await?;
            }

            // Sent instead of AGENT_CONNECTED because we advertise
            // MAIN_AGENT_CONNECTED_TOKENS. spice-server reset its token
            // accounting when the previous agent detached, so adopt its new
            // window rather than carrying our old count forward (#452). As in
            // spice-gtk, tokens are not zeroed on AGENT_DISCONNECTED: the
            // server still expects the tail of a part-sent message.
            main_server::AGENT_CONNECTED_TOKENS => {
                match AgentTokens::decode(payload) {
                    Ok(AgentTokens { num_tokens: tokens }) => {
                        info!("main: vdagent connected with {} agent tokens", tokens);
                        self.agent_tokens = tokens;
                    }
                    Err(_) => warn!(
                        "main: short AGENT_CONNECTED_TOKENS payload ({} bytes), \
                         keeping {} agent tokens",
                        payload.len(),
                        self.agent_tokens
                    ),
                }
                self.agent_connected = true;
                self.publish_agent_connected();
                self.connect_agent().await?;
                self.flush_agent_queue().await?;
            }

            main_server::AGENT_DISCONNECTED => {
                // The error code is only logged; a short body still means
                // the agent has gone.
                match AgentDisconnected::decode(payload) {
                    Ok(msg) => info!("main: vdagent disconnected (error_code={})", msg.error_code),
                    Err(_) => info!("main: vdagent disconnected"),
                }
                self.agent_connected = false;
                self.publish_agent_connected();
                self.agent_caps_announced = false;
                self.guest_caps_received = false;
                self.guest_clipboard_selection = None;
                // Drop reply bookkeeping tied to the previous agent
                // instance, so a stale entry in agent_request_send_ts
                // cannot match the next agent's REPLY and record a
                // multi-minute lag.
                self.agent_request_send_ts.clear();
                self.outstanding_agent_request_count = 0;
                // Queued messages were meant for the agent that just left.
                self.agent_queue.discard_unstarted();
                // Not connected, so this ends any starvation episode.
                self.note_agent_starvation();
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
                match AgentTokens::decode(payload) {
                    Ok(AgentTokens { num_tokens: tokens }) => {
                        self.agent_tokens = self.agent_tokens.saturating_add(tokens);
                    }
                    // A short AGENT_TOKEN counts as one token. The quirk
                    // predates the protocol crate's AgentTokens, and was
                    // kept when the parse moved there so that the move
                    // changed no behaviour (andris
                    // PLAN-x11-desktop-phase-02-wire-types.md, survey
                    // finding 3).
                    Err(_) => {
                        self.agent_tokens = self.agent_tokens.saturating_add(1);
                        warn!("main: short AGENT_TOKEN payload ({} bytes)", payload.len());
                    }
                }

                self.flush_agent_queue().await?;

                self.maybe_send_announce_capabilities().await?;
            }

            unknown => {
                // Unknown opcode — log hex once per msg_type, silent on repeat.
                logging::log_unknown_once("main", unknown, payload);
                self.opcodes.note_unknown(unknown);
            }
        }

        Ok(())
    }

    /// Sync local state to the shared snapshot. Note that
    /// `keepalive_timeout_fired` is poked into the snapshot
    /// directly at the timeout site, not flushed here, since it
    /// is set once on a terminal path and then read by the
    /// disconnect-cause assembly.
    fn update_snapshot(&self) {
        let mut snap = self.snapshot.lock().expect("lock poisoned");
        snap.session_id = self.session_id;
        snap.bytes_in = self.bytes_in;
        snap.bytes_out = self.bytes_out;
        snap.last_recv_ts_secs = self.last_recv_ts_secs;
        snap.last_send_ts_secs = self.last_send_ts_secs;
        snap.ping_recv_count = self.ping_recv_count;
        snap.pong_send_count = self.pong_send_count;
        snap.last_ping_recv_ts_secs = self.last_ping_recv_ts_secs;
        snap.writer_dropped_count = self.capture_dropped_count;
        snap.server_mouse_mode = self.server_mouse_mode;
        // The sink records the drop as an Instant (it has no session
        // clock); convert to session-relative seconds against the traffic
        // clock here.
        let drops = self.events.drop_stats();
        snap.events_dropped_count = drops.total;
        snap.events_dropped_by_kind = drops
            .by_kind
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        snap.last_event_drop_ts_secs = drops
            .since_last_drop
            .map(|ago| self.traffic.elapsed().saturating_sub(ago).as_secs_f64());
        // mm_time clock state. `now()` is informational —
        // computed at snapshot time so a bug report shows the
        // server's current millisecond counter.
        snap.mm_time_now = self.mm_clock.now();
        snap.mm_time_set_count = self.mm_clock.set_count();
        snap.last_mm_time_set_ts_secs = self.mm_clock.last_set_ts_secs();
        self.opcodes.publish_into(&mut *snap);
        snap.agent_request_count = self.agent_request_count;
        snap.agent_reply_count = self.agent_reply_count;
        snap.agent_reply_error_count = self.agent_reply_error_count;
        snap.last_agent_reply_ts_secs = self.last_agent_reply_ts_secs;
        snap.last_agent_reply_lag_us = self.last_agent_reply_lag_us;
        snap.recent_agent_reply_lag_us = self.recent_agent_reply_lag_us.clone();
        snap.outstanding_agent_request_count = self.outstanding_agent_request_count;
        snap.agent_tokens = self.agent_tokens;
        snap.queued_agent_message_count = self.agent_queue.len() as u32;
        snap.agent_starved_since_ts_secs = self.agent_starved_since_ts_secs;
        snap.agent_stall_count = self.agent_stall_count;
    }

    async fn request_channels_list(&mut self) -> Result<()> {
        let msg = make_message(main_client::ATTACH_CHANNELS, &[]);
        self.send_with_log(main_client::ATTACH_CHANNELS, &msg).await
    }

    /// Publish the server's mouse mode to the frontends and wake the
    /// renderer to read it. Never blocks: unlike an event, this cannot be
    /// lost to a UI that has stopped draining the queue.
    fn publish_mouse_mode(&self, mode: u32) {
        self.state.publish_mouse_mode(mode);
        self.events.wake();
    }

    /// Publish `self.agent_connected` to the frontends, as
    /// `publish_mouse_mode` does for the mouse mode.
    fn publish_agent_connected(&self) {
        self.state.publish_agent_connected(self.agent_connected);
        self.events.wake();
    }

    /// Tell the frontends the session is over, as the read loop exits.
    ///
    /// Sent with `emit_terminal`, not `emit`, because this event must not be
    /// lost: the GUI's reconnect and disconnect snapshot run from it, and
    /// when main exits cleanly nothing else reaches the GUI (see
    /// `EventSink::emit_terminal`). A UI stall that outlasts the 5 s timeout
    /// and then recovers, as the three-minute one in test session 011 did
    /// (#430), must still deliver it.
    ///
    /// Blocking here cannot recreate K1. K1 was main stuck on a send while
    /// the session was alive, so it stopped answering PINGs and the server
    /// ended the session. Both callers have already given the session up
    /// (EOF, or no data for `keepalive_timeout`) and break straight after,
    /// so there is nothing left to answer. A dropped receiver (the GUI
    /// replacing its queue on reconnect, or exiting) ends the wait at once.
    ///
    /// The hazard left is a receiver that is alive but never drained, which
    /// would hold this task here. It is bounded by the per-connection cancel
    /// flag rather than by a deadline: `run_connection`'s cancel watcher
    /// aborts every channel task, main included, mid-wait. The GUI raises
    /// the flag in `reconnect`; headless raises it when its event loop stops
    /// and aborts the task after `CONNECTION_JOIN_GRACE`; web raises it at
    /// shutdown. Headless and web also drain this queue through a fan-out
    /// task into a broadcast bus, so it only fills if their runtime is
    /// starved. A GUI whose UI thread never drains again cannot act on this
    /// event however it is sent. A finite deadline would therefore add no
    /// safety in those cases, and would lose the event in the one that
    /// matters: a long stall that recovers.
    async fn emit_session_ended(&self) {
        self.events
            .emit_terminal(ChannelEvent::Disconnected(ChannelType::Main))
            .await;
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
        let mut mode_payload = Vec::with_capacity(MouseModeRequest::SIZE);
        MouseModeRequest {
            mode: MOUSE_MODE_CLIENT as u16,
        }
        .write(&mut mode_payload);
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
        let mut payload = Vec::with_capacity(AgentTokens::SIZE);
        AgentTokens {
            num_tokens: u32::MAX,
        }
        .write(&mut payload);
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
            debug!("main: monitors config: agent send queue full");
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

    /// Queue a guest-agent message and send as much of it as the server's
    /// tokens allow; the rest follows from `flush_agent_queue` as
    /// `AGENT_TOKEN`s arrive. Returns `Ok(false)`, sending nothing, only
    /// when the queue is full, which means the agent has stopped reading.
    async fn send_agent_data_message(&mut self, ty: u32, payload: &[u8]) -> Result<bool> {
        let mut agent = Vec::with_capacity(20 + payload.len());
        agent.write_u32::<LittleEndian>(VD_AGENT_PROTOCOL)?;
        agent.write_u32::<LittleEndian>(ty)?;
        agent.write_u64::<LittleEndian>(0)?;
        agent.write_u32::<LittleEndian>(payload.len() as u32)?;
        agent.extend_from_slice(payload);

        if !self.agent_queue.push(agent) {
            warn!(
                "main: agent message type={} dropped: {} messages already waiting for tokens",
                ty,
                self.agent_queue.len()
            );
            return Ok(false);
        }
        self.flush_agent_queue().await?;

        // Track send time for REPLY-eligible request types so we
        // can compute reply lag when VD_AGENT_REPLY arrives. This is
        // the time the message was queued; for these small messages
        // that is also when it went out, unless tokens ran short.
        //
        // Overwriting any prior entry for `ty` is intentional —
        // VD_AGENT_REPLY has no request id, only a request type,
        // so two sends in quick succession cannot be
        // distinguished individually. We measure lag against the
        // most recent send and accept that the in-flight earlier
        // REPLY (if any) will skip the lag-update branch when it
        // arrives (no matching map entry by then). The trade-off
        // surfaces as a "no matching send entry" debug log in
        // handle_agent_message.
        if REPLY_ELIGIBLE_AGENT_REQUEST_TYPES.contains(&ty) {
            self.agent_request_send_ts.insert(ty, Instant::now());
            self.agent_request_count = self.agent_request_count.saturating_add(1);
            self.outstanding_agent_request_count = self
                .outstanding_agent_request_count
                .saturating_add(1)
                .min(MAX_OUTSTANDING_AGENT_REQUESTS);
        }

        Ok(true)
    }

    /// Send queued agent chunks, one per token, until either runs out.
    async fn flush_agent_queue(&mut self) -> Result<()> {
        while self.agent_tokens > 0 {
            let Some(chunk) = self.agent_queue.next_chunk() else {
                break;
            };
            let msg = make_message(main_client::AGENT_DATA, &chunk);
            self.send_with_log(main_client::AGENT_DATA, &msg).await?;
            self.agent_tokens -= 1;
        }
        self.note_agent_starvation();
        Ok(())
    }

    /// Start or end a starvation episode to match the current token and
    /// queue state. Called wherever either can change.
    fn note_agent_starvation(&mut self) {
        let starved = agent_starved(
            self.agent_connected,
            self.agent_tokens,
            !self.agent_queue.is_empty(),
        );
        match (starved, self.agent_starved_since) {
            (true, None) => {
                self.agent_starved_since = Some(Instant::now());
                self.agent_starved_since_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
            }
            (false, Some(since)) => {
                if self.last_stuck_agent_notification_at.is_some() {
                    info!(
                        "main: guest agent accepting messages again after {:.1}s",
                        since.elapsed().as_secs_f64()
                    );
                }
                self.agent_starved_since = None;
                self.agent_starved_since_ts_secs = None;
                self.last_stuck_agent_notification_at = None;
            }
            _ => {}
        }
    }

    async fn handle_agent_message(&mut self, agent_type: u32, payload: &[u8]) -> Result<()> {
        if !self.guest_caps_received {
            self.guest_caps_received = true;
            debug!("main: guest agent active");
        }
        match agent_type {
            VD_AGENT_CLIPBOARD_GRAB
            | VD_AGENT_CLIPBOARD
            | VD_AGENT_CLIPBOARD_REQUEST
            | VD_AGENT_CLIPBOARD_RELEASE => {
                let Some(has_selection) = self.guest_clipboard_selection else {
                    debug!(
                        "main: ignoring agent clipboard message type={} before the guest's \
                         capabilities",
                        agent_type
                    );
                    return Ok(());
                };
                let Some((selection, body)) = split_clipboard_selection(payload, has_selection)
                else {
                    debug!(
                        "main: agent clipboard message type={} too short ({} bytes)",
                        agent_type,
                        payload.len()
                    );
                    return Ok(());
                };
                self.handle_guest_clipboard(agent_type, selection, body)
                    .await?;
            }
            VD_AGENT_ANNOUNCE_CAPABILITIES => {
                self.guest_caps_received = true;
                let has_selection = agent_caps_has(payload, VD_AGENT_CAP_CLIPBOARD_SELECTION);
                self.guest_clipboard_selection = Some(has_selection);
                debug!(
                    "main: received agent capabilities from guest (clipboard selection: {})",
                    has_selection
                );
                if payload.len() >= 4 {
                    let request =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    if request == 1 {
                        self.agent_caps_announced = false;
                        self.maybe_send_announce_capabilities().await?;
                    }
                }
            }
            VD_AGENT_REPLY => match parse_vd_agent_reply(payload) {
                Some((reply_type, error)) => {
                    debug!("main: VD_AGENT_REPLY type={} error={}", reply_type, error);
                    self.agent_reply_count = self.agent_reply_count.saturating_add(1);
                    if error != 0 {
                        self.agent_reply_error_count =
                            self.agent_reply_error_count.saturating_add(1);
                    }
                    self.last_agent_reply_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
                    // Correlate by request type to compute lag. Only
                    // decrement outstanding_agent_request_count when we
                    // find a matching send — a REPLY for a type we did
                    // NOT send (server bug, or our map was cleared on
                    // agent disconnect) would otherwise understate the
                    // outstanding count.
                    if let Some(sent) = self.agent_request_send_ts.remove(&reply_type) {
                        let lag_us = sent.elapsed().as_micros().try_into().unwrap_or(u32::MAX);
                        self.last_agent_reply_lag_us = Some(lag_us);
                        self.recent_agent_reply_lag_us.push_back(lag_us);
                        if self.recent_agent_reply_lag_us.len() > MAX_RECENT_AGENT_REPLIES {
                            self.recent_agent_reply_lag_us.pop_front();
                        }
                        self.outstanding_agent_request_count =
                            self.outstanding_agent_request_count.saturating_sub(1);
                    } else {
                        debug!(
                            "main: VD_AGENT_REPLY type={} has no matching send entry — \
                             skipping lag update and outstanding decrement",
                            reply_type
                        );
                    }
                }
                None => {
                    debug!(
                        "main: VD_AGENT_REPLY payload too short ({} bytes)",
                        payload.len()
                    );
                }
            },
            _ => {
                debug!("main: unhandled agent message type={}", agent_type);
            }
        }
        Ok(())
    }

    /// Handle a guest clipboard message whose selection header has been
    /// removed.
    ///
    /// Only the CLIPBOARD selection is synced. A PRIMARY grab only means
    /// text was selected in the guest: requesting CLIPBOARD in reply gets
    /// an empty answer, and copying PRIMARY to the host would clobber the
    /// host clipboard on every selection. spice-gtk likewise forwards only
    /// CLIPBOARD to its legacy single-clipboard signals (`channel-main.c`).
    async fn handle_guest_clipboard(
        &mut self,
        agent_type: u32,
        selection: u8,
        body: &[u8],
    ) -> Result<()> {
        if agent_type == VD_AGENT_CLIPBOARD_REQUEST {
            info!("main: VD_AGENT_CLIPBOARD_REQUEST received");
        }
        if selection != VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD {
            debug!(
                "main: ignoring guest clipboard message type={} for selection {}",
                agent_type, selection
            );
            if agent_type == VD_AGENT_CLIPBOARD_REQUEST {
                // We never grab other selections, so a request for one is
                // unexpected; answer NONE as spice-vdagent does for a
                // selection it does not own, so the requester stops waiting.
                self.send_clipboard_none(selection).await?;
            }
            return Ok(());
        }

        match agent_type {
            VD_AGENT_CLIPBOARD_GRAB => {
                if clipboard_grab_offers(body, VD_AGENT_CLIPBOARD_UTF8_TEXT) {
                    debug!("main: guest clipboard grab, requesting data");
                    self.send_clipboard_request().await?;
                } else {
                    debug!("main: guest clipboard grab offers no UTF-8 text");
                }
            }
            VD_AGENT_CLIPBOARD => {
                let Some((VD_AGENT_CLIPBOARD_UTF8_TEXT, data)) = split_clipboard_type(body) else {
                    debug!("main: guest clipboard data is not UTF-8 text");
                    return Ok(());
                };
                if data.is_empty() {
                    return Ok(());
                }
                let text = String::from_utf8_lossy(data).to_string();
                // Log byte count only — clipboard content may contain
                // passwords or sensitive data.
                info!("main: clipboard from guest ({} bytes)", text.len());
                if let Some(cb) = &self.clipboard {
                    match cb.set_text(&text) {
                        Ok(()) => debug!("main: host clipboard updated"),
                        Err(e) => {
                            debug!("main: clipboard set failed: {}", e);
                        }
                    }
                }
                // Record so poll_host_clipboard won't re-grab what we just set.
                // Storing the normalised hash makes the dedup
                // invariant under CRLF / LF and trailing-whitespace
                // munging during the host clipboard round trip.
                self.last_clipboard_hash = Some(hash_clipboard(&text));
            }
            VD_AGENT_CLIPBOARD_REQUEST => {
                let text = match split_clipboard_type(body) {
                    Some((VD_AGENT_CLIPBOARD_UTF8_TEXT, _)) => {
                        debug!("main: clipboard request from guest");
                        self.read_host_clipboard_for_guest().await
                    }
                    _ => {
                        debug!("main: guest requested a clipboard type other than UTF-8 text");
                        None
                    }
                };
                match text {
                    Some(text) => {
                        // Log byte count only — clipboard content may contain
                        // passwords or sensitive data.
                        info!("main: clipboard to guest ({} bytes)", text.len());
                        self.send_clipboard_data(&text).await?;
                    }
                    // Answer anyway, so the guest application asking for the
                    // paste is not left waiting on a reply that never comes.
                    None => {
                        self.send_clipboard_none(selection).await?;
                    }
                }
            }
            _ => {
                debug!("main: clipboard release from guest");
            }
        }
        Ok(())
    }

    /// Read the host clipboard to answer a guest request.
    ///
    /// Same spawn_blocking + timeout shape as poll_host_clipboard:
    /// cb.get_text() can hang macOS NSPasteboard when ryll is
    /// backgrounded.
    async fn read_host_clipboard_for_guest(&self) -> Option<String> {
        let cb = self.clipboard.as_ref()?.clone();
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::task::spawn_blocking(move || cb.get_text()),
        )
        .await
        {
            Ok(Ok(opt)) => opt,
            Ok(Err(e)) => {
                warn!("main: clipboard request task panicked: {}", e);
                None
            }
            Err(_) => {
                warn!("main: clipboard request timed out (1 s), ignoring guest request");
                None
            }
        }
    }

    async fn poll_host_clipboard(&mut self) -> Result<()> {
        // arboard::Clipboard::get_text() is synchronous and on
        // macOS reaches into NSPasteboard. When the ryll process
        // is backgrounded / on a different virtual desktop /
        // App Nap'd, that call has been observed to block the
        // calling thread for many seconds at a time. Until
        // session-001f, this lived directly on main's tokio
        // worker — a single hung clipboard poll would wedge
        // main's `select!` loop, which on backgrounded macOS
        // sessions reproducibly silenced main at the same
        // ~7-minute mark across every K1 reproduction. Other
        // channels (on different workers) kept running, so the
        // server eventually tore the session down for client
        // unresponsiveness.
        //
        // Push the call to `spawn_blocking` so it runs on
        // tokio's blocking thread pool, then wrap in a
        // `tokio::time::timeout` so a genuinely-stuck
        // pasteboard query gives up rather than starving the
        // pool indefinitely. A timed-out poll is logged at
        // warn level and treated like an empty clipboard;
        // the next 500 ms tick retries.
        let cb = match self.clipboard.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(()),
        };
        let text = match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::task::spawn_blocking(move || cb.get_text()),
        )
        .await
        {
            Ok(Ok(Some(t))) => t,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(e)) => {
                warn!("main: clipboard poll task panicked: {}", e);
                return Ok(());
            }
            Err(_) => {
                warn!("main: clipboard poll timed out (1 s), skipping");
                return Ok(());
            }
        };

        if text.is_empty() {
            return Ok(());
        }

        let new_hash = hash_clipboard(&text);
        let changed = match self.last_clipboard_hash {
            Some(prev) => prev != new_hash,
            None => true,
        };

        if changed {
            // Log byte count only — clipboard content may contain
            // passwords or sensitive data.
            info!("main: host clipboard changed ({} bytes)", text.len());
            self.last_clipboard_hash = Some(new_hash);
            self.send_clipboard_grab().await?;
        }

        Ok(())
    }

    /// A clipboard message body for the negotiated layout, or `None` (and
    /// nothing should be sent) before the guest's capabilities arrive.
    fn clipboard_payload(&self, selection: u8, ty: u32, data: &[u8]) -> Option<Vec<u8>> {
        let Some(has_selection) = self.guest_clipboard_selection else {
            debug!("main: clipboard message not sent: guest capabilities not yet received");
            return None;
        };
        Some(build_clipboard_payload(has_selection, selection, ty, data))
    }

    async fn send_clipboard_grab(&mut self) -> Result<bool> {
        let Some(payload) = self.clipboard_payload(
            VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            VD_AGENT_CLIPBOARD_UTF8_TEXT,
            &[],
        ) else {
            return Ok(false);
        };
        self.send_agent_data_message(VD_AGENT_CLIPBOARD_GRAB, &payload)
            .await
    }

    async fn send_clipboard_request(&mut self) -> Result<bool> {
        let Some(payload) = self.clipboard_payload(
            VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            VD_AGENT_CLIPBOARD_UTF8_TEXT,
            &[],
        ) else {
            return Ok(false);
        };
        self.send_agent_data_message(VD_AGENT_CLIPBOARD_REQUEST, &payload)
            .await
    }

    async fn send_clipboard_data(&mut self, text: &str) -> Result<bool> {
        let Some(payload) = self.clipboard_payload(
            VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            VD_AGENT_CLIPBOARD_UTF8_TEXT,
            text.as_bytes(),
        ) else {
            return Ok(false);
        };
        self.send_agent_data_message(VD_AGENT_CLIPBOARD, &payload)
            .await
    }

    /// Tell the guest we have no data for `selection`: a `VDAgentClipboard`
    /// of type NONE, which spice-vdagent turns into a failed selection
    /// request for the waiting application, or ignores if none is waiting.
    async fn send_clipboard_none(&mut self, selection: u8) -> Result<bool> {
        let Some(payload) = self.clipboard_payload(selection, VD_AGENT_CLIPBOARD_NONE, &[]) else {
            return Ok(false);
        };
        self.send_agent_data_message(VD_AGENT_CLIPBOARD, &payload)
            .await
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let msg_name = message_names::main_client(msg_type);
        if self.log_config.verbose {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "main", msg_type, msg_name, payload_size);
        }
        self.traffic.record_sent("main", msg_type, msg_name, data);
        // Single send path, so this is the only send-count site.
        self.opcodes.record_send(msg_type);
        let result = self.send(data).await;
        self.update_snapshot();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            if !c.packet_sent("main", data) {
                self.capture_dropped_count = self.capture_dropped_count.saturating_add(1);
            }
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        self.last_send_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{
        agent_caps_has, agent_starved, build_clipboard_payload, clipboard_grab_offers,
        hash_clipboard, parse_vd_agent_reply, ping_interval_ms, should_request_client_mouse_mode,
        should_warn_agent_stalled, split_clipboard_selection, split_clipboard_type,
        STUCK_AGENT_NOTIFY_INTERVAL, STUCK_AGENT_THRESHOLD, VD_AGENT_ANNOUNCE_CAPABILITIES,
        VD_AGENT_CAP_CLIPBOARD_SELECTION, VD_AGENT_CLIPBOARD, VD_AGENT_CLIPBOARD_GRAB,
        VD_AGENT_CLIPBOARD_NONE, VD_AGENT_CLIPBOARD_RELEASE, VD_AGENT_CLIPBOARD_REQUEST,
        VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD, VD_AGENT_CLIPBOARD_UTF8_TEXT,
        VD_AGENT_DISPLAY_CONFIG, VD_AGENT_MONITORS_CONFIG, VD_AGENT_MOUSE_STATE, VD_AGENT_REPLY,
    };
    use shakenfist_spice_protocol::{MOUSE_MODE_CLIENT, MOUSE_MODE_SERVER};

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

    #[test]
    fn vd_agent_constants_match_spice_protocol() {
        // Values from spice-protocol/spice/vd_agent.h
        // (VDAgentMessage type discriminants).
        assert_eq!(VD_AGENT_MOUSE_STATE, 1);
        assert_eq!(VD_AGENT_MONITORS_CONFIG, 2);
        assert_eq!(VD_AGENT_REPLY, 3);
        assert_eq!(VD_AGENT_CLIPBOARD, 4);
        assert_eq!(VD_AGENT_DISPLAY_CONFIG, 5);
        assert_eq!(VD_AGENT_ANNOUNCE_CAPABILITIES, 6);
        assert_eq!(VD_AGENT_CLIPBOARD_GRAB, 7);
        assert_eq!(VD_AGENT_CLIPBOARD_REQUEST, 8);
        assert_eq!(VD_AGENT_CLIPBOARD_RELEASE, 9);

        // Regression for PR 31: ANNOUNCE_CAPABILITIES used to be 1,
        // which collided with VD_AGENT_MOUSE_STATE. The server would
        // dispatch our capabilities announcement to its mouse-state
        // handler.
        assert_ne!(
            VD_AGENT_ANNOUNCE_CAPABILITIES, VD_AGENT_MOUSE_STATE,
            "ANNOUNCE_CAPABILITIES (6) must not collide with MOUSE_STATE (1)"
        );
    }

    #[test]
    fn clipboard_hash_invariant_under_crlf_lf() {
        // Round-tripping through Windows or some Wayland
        // compositors can flip LF to CRLF (or back). The dedup
        // hash must collapse those forms so the echo guard does
        // not fire on a no-op round trip.
        assert_eq!(hash_clipboard("foo\nbar"), hash_clipboard("foo\r\nbar"));
        assert_eq!(hash_clipboard("a\nb\nc"), hash_clipboard("a\r\nb\r\nc"));
        assert_eq!(hash_clipboard("only\rcr"), hash_clipboard("only\ncr"));
    }

    #[test]
    fn clipboard_hash_invariant_under_trailing_whitespace() {
        // Trailing whitespace likewise gets trimmed or appended
        // inconsistently across clipboard providers.
        assert_eq!(hash_clipboard("foo"), hash_clipboard("foo\n"));
        assert_eq!(hash_clipboard("foo"), hash_clipboard("foo  "));
        assert_eq!(hash_clipboard("foo"), hash_clipboard("foo\r\n"));
    }

    #[test]
    fn clipboard_hash_distinguishes_different_content() {
        // Sanity check: the dedup must still notice when the
        // user actually copies something different.
        assert_ne!(hash_clipboard("foo"), hash_clipboard("bar"));
        assert_ne!(hash_clipboard("foo\nbar"), hash_clipboard("foo\nbaz"));
    }

    // ── VD_AGENT_REPLY parser ───────────────────────────────

    #[test]
    fn parse_vd_agent_reply_decodes_valid_payload() {
        // VD_AGENT_MONITORS_CONFIG (type=2), VD_AGENT_SUCCESS (error=0).
        let payload = [0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_vd_agent_reply(&payload), Some((2, 0)));
    }

    #[test]
    fn parse_vd_agent_reply_decodes_error_bit() {
        // type=2 (MONITORS_CONFIG), error=42 (anything non-zero is failure).
        let payload = [0x02, 0x00, 0x00, 0x00, 0x2a, 0x00, 0x00, 0x00];
        assert_eq!(parse_vd_agent_reply(&payload), Some((2, 42)));
    }

    #[test]
    fn parse_vd_agent_reply_handles_max_values() {
        // u32::MAX in both fields — confirms little-endian decode width.
        let payload = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(parse_vd_agent_reply(&payload), Some((u32::MAX, u32::MAX)));
    }

    #[test]
    fn parse_vd_agent_reply_rejects_short_payload() {
        // 7 bytes — one short of the required 8.
        let payload = [0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_vd_agent_reply(&payload), None);
        // Empty payload.
        assert_eq!(parse_vd_agent_reply(&[]), None);
    }

    #[test]
    fn parse_vd_agent_reply_ignores_trailing_bytes() {
        // Server is permitted to send additional bytes after the
        // documented 8 — we should decode the first 8 and ignore the
        // rest rather than reject.
        let payload = [
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // valid {2, 0}
            0xff, 0xff, // trailing garbage
        ];
        assert_eq!(parse_vd_agent_reply(&payload), Some((2, 0)));
    }

    #[test]
    fn ping_interval_ms_needs_a_previous_ping() {
        assert_eq!(ping_interval_ms(None, Instant::now()), None);
    }

    #[test]
    fn ping_interval_ms_reports_the_gap_in_milliseconds() {
        let last = Instant::now();
        let now = last + std::time::Duration::from_millis(250);
        assert_eq!(ping_interval_ms(Some(last), now), Some(250.0));
    }

    #[test]
    fn ping_interval_ms_survives_a_days_long_gap() {
        // A suspended laptop can resume with an enormous gap; the f32
        // cast must still yield a finite, roughly correct number rather
        // than an infinity that would flatten the sparkline.
        let last = Instant::now();
        let now = last + std::time::Duration::from_secs(86_400);
        let sample = ping_interval_ms(Some(last), now).expect("previous ping present");
        assert!(sample.is_finite());
        assert!((sample - 86_400_000.0).abs() < 1.0, "sample was {}", sample);
    }

    // Guest ANNOUNCE_CAPABILITIES body from test sessions 013-015:
    // request=0, caps=0x00038de7 (bit 6, CLIPBOARD_SELECTION, set).
    const GUEST_CAPS: [u8; 8] = [0, 0, 0, 0, 0xe7, 0x8d, 0x03, 0x00];

    #[test]
    fn agent_caps_has_reads_the_bitmap_after_the_request_word() {
        assert!(agent_caps_has(
            &GUEST_CAPS,
            VD_AGENT_CAP_CLIPBOARD_SELECTION
        ));
        // Bit 3 (CLIPBOARD, the pre-by-demand protocol) is clear.
        assert!(!agent_caps_has(&GUEST_CAPS, 3));
        // ryll's own caps, 0x67, also carry CLIPBOARD_SELECTION.
        assert!(agent_caps_has(&[1, 0, 0, 0, 0x67, 0, 0, 0], 6));
        // A bit in a word the agent did not send is clear, not a panic.
        assert!(!agent_caps_has(&GUEST_CAPS, 40));
        assert!(!agent_caps_has(&[0, 0], 0));
    }

    #[test]
    fn guest_primary_grab_from_sessions_013_014_is_not_clipboard() {
        // Wire bytes of the guest grab ryll answered with a CLIPBOARD
        // request: selection=PRIMARY (1), types=[UTF8_TEXT].
        let grab = [0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
        let (selection, types) = split_clipboard_selection(&grab, true).unwrap();
        assert_eq!(selection, 1);
        assert_ne!(selection, VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD);
        assert!(clipboard_grab_offers(types, VD_AGENT_CLIPBOARD_UTF8_TEXT));
    }

    #[test]
    fn split_clipboard_selection_reads_a_u8_and_skips_reserved_bytes() {
        // Reserved bytes are not part of the selection, whatever they hold.
        let msg = [0x00, 0xaa, 0xbb, 0xcc, 0x01, 0x00, 0x00, 0x00];
        let (selection, body) = split_clipboard_selection(&msg, true).unwrap();
        assert_eq!(selection, VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD);
        assert_eq!(body, &msg[4..]);
        assert_eq!(split_clipboard_selection(&[0x00, 0x00, 0x00], true), None);
    }

    #[test]
    fn split_clipboard_selection_without_the_cap_is_implicitly_clipboard() {
        let msg = [0x01, 0x00, 0x00, 0x00];
        assert_eq!(
            split_clipboard_selection(&msg, false),
            Some((VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD, &msg[..]))
        );
    }

    #[test]
    fn clipboard_grab_offers_finds_text_anywhere_in_the_type_list() {
        // types = [IMAGE_PNG (2), UTF8_TEXT (1)]
        let types = [0x02, 0, 0, 0, 0x01, 0, 0, 0];
        assert!(clipboard_grab_offers(&types, VD_AGENT_CLIPBOARD_UTF8_TEXT));
        assert!(!clipboard_grab_offers(
            &types[..4],
            VD_AGENT_CLIPBOARD_UTF8_TEXT
        ));
        // A trailing partial type is ignored rather than misread.
        assert!(!clipboard_grab_offers(
            &[0x01, 0, 0],
            VD_AGENT_CLIPBOARD_UTF8_TEXT
        ));
    }

    #[test]
    fn split_clipboard_type_returns_type_and_data() {
        let body = [0x01, 0, 0, 0, b'h', b'i'];
        assert_eq!(
            split_clipboard_type(&body),
            Some((VD_AGENT_CLIPBOARD_UTF8_TEXT, &b"hi"[..]))
        );
        // The empty NONE reply the guest sends for a selection it does
        // not own (sessions 013 and 014).
        assert_eq!(
            split_clipboard_type(&[0, 0, 0, 0]),
            Some((VD_AGENT_CLIPBOARD_NONE, &[][..]))
        );
        assert_eq!(split_clipboard_type(&[0x01, 0]), None);
    }

    #[test]
    fn build_clipboard_payload_matches_the_wire_layout() {
        // The CLIPBOARD request ryll sent in sessions 013 and 014.
        assert_eq!(
            build_clipboard_payload(
                true,
                VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                VD_AGENT_CLIPBOARD_UTF8_TEXT,
                &[]
            ),
            vec![0, 0, 0, 0, 1, 0, 0, 0]
        );
        // A NONE answer for PRIMARY carries that selection back.
        assert_eq!(
            build_clipboard_payload(true, 1, VD_AGENT_CLIPBOARD_NONE, &[]),
            vec![1, 0, 0, 0, 0, 0, 0, 0]
        );
        // Without the cap there is no selection header at all.
        assert_eq!(
            build_clipboard_payload(
                false,
                VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                VD_AGENT_CLIPBOARD_UTF8_TEXT,
                b"x"
            ),
            vec![1, 0, 0, 0, b'x']
        );
    }

    #[test]
    fn agent_starved_needs_connection_zero_tokens_and_a_queue() {
        assert!(agent_starved(true, 0, true));
        // Tokens in hand: the queue drains on the next flush.
        assert!(!agent_starved(true, 1, true));
        // Nothing waiting: zero tokens holds nothing up.
        assert!(!agent_starved(true, 0, false));
        // No agent: the server discards client data, so its tokens say
        // nothing about a guest that is not there.
        assert!(!agent_starved(false, 0, true));
    }

    #[test]
    fn stall_warning_waits_for_the_threshold() {
        let since = Instant::now();
        assert!(!should_warn_agent_stalled(None, None, since));
        assert!(!should_warn_agent_stalled(Some(since), None, since));
        let almost = since + STUCK_AGENT_THRESHOLD - std::time::Duration::from_millis(1);
        assert!(!should_warn_agent_stalled(Some(since), None, almost));
        assert!(should_warn_agent_stalled(
            Some(since),
            None,
            since + STUCK_AGENT_THRESHOLD
        ));
    }

    #[test]
    fn stall_warning_repeats_at_the_notify_interval() {
        let since = Instant::now();
        let first = since + STUCK_AGENT_THRESHOLD;
        assert!(!should_warn_agent_stalled(
            Some(since),
            Some(first),
            first + STUCK_AGENT_NOTIFY_INTERVAL - std::time::Duration::from_millis(1)
        ));
        assert!(should_warn_agent_stalled(
            Some(since),
            Some(first),
            first + STUCK_AGENT_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn stall_repeats_fall_inside_the_notification_dedup_window() {
        // ryll's NotificationStore folds an identical entry only if it
        // arrives within NOTIFICATION_DEDUP_WINDOW (30 s) of the last one.
        // Repeats further apart would list a long stall once per repeat.
        assert!(STUCK_AGENT_NOTIFY_INTERVAL < std::time::Duration::from_secs(30));
    }
}
