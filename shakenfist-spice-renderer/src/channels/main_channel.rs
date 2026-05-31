/// Main channel handler - session management, ping/pong, channel list
use anyhow::Result;
use byteorder::{LittleEndian, WriteBytesExt};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Notify as RepaintNotify;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::mm_clock::MmClock;
use crate::snapshots::MainSnapshot;
use crate::{
    ByteCounter, CaptureSink, ClipboardBackend, LogConfig, NotificationEntry, NotificationSource,
    TrafficSink,
};
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

/// Build the body of a `SPICE_MSGC_MAIN_MOUSE_MODE_REQUEST`.
///
/// `spice.proto` declares `mouse_mode` as `flags16`, so the body
/// is a single little-endian `u16`. Writing `u32` here ships two
/// extra zero bytes, which some servers tolerate and others
/// reject as malformed — matching the read side at
/// `parse_mouse_mode_payload` which is already u16-aware.
fn build_mouse_mode_request_payload(mode: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2);
    // Vec writes never fail; unwrap is safe.
    payload
        .write_u16::<LittleEndian>(mode as u16)
        .expect("Vec write should not fail");
    payload
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
const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;

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
/// 16 entries at the 30 s probe cadence (phase 9B) covers
/// 8 minutes of agent history in a bug report.
const MAX_RECENT_AGENT_REPLIES: usize = 16;

/// Cadence for the vdagent liveness probe (phase 9B). 30 s is
/// chosen so the snapshot ring (cap 16) covers ~8 minutes of
/// agent history in a bug report — long enough to characterise
/// an intermittent stall without burning bandwidth on a working
/// agent. The probe re-sends the most recent monitors config
/// (treated by the guest agent as a no-op when unchanged), so
/// the lag of the resulting VD_AGENT_REPLY is a clean liveness
/// measurement.
const VDAGENT_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long `outstanding_agent_request_count` may stay > 0
/// before we consider the agent stuck and push a Warn
/// notification. Conservative; healthy replies arrive in
/// well under 100 ms.
const STUCK_AGENT_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(5);

/// Minimum interval between consecutive stuck-agent
/// notifications, to keep the notification panel quiet during
/// a sustained stall.
const STUCK_AGENT_NOTIFY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// Phase-02: count of pcap-capture packets rejected by the
    /// writer task's queue. Mirrored into
    /// `MainSnapshot::writer_dropped_count`.
    capture_dropped_count: u64,
    /// Per-opcode receive counts; flushed to snapshot by
    /// `update_snapshot`.
    messages_recv_by_opcode: std::collections::BTreeMap<u16, u64>,
    /// Per-opcode send counts; flushed to snapshot by
    /// `update_snapshot`.
    messages_send_by_opcode: std::collections::BTreeMap<u16, u64>,
    /// Most recent unrecognised receive opcode.
    last_unknown_opcode: Option<u16>,
    /// Count of unrecognised receive opcodes.
    unknown_opcode_count: u64,
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
    /// Most recent monitors config payload sent to the agent.
    /// Cached in `send_agent_monitors_config` so the probe
    /// can re-send without recomputing. None if we haven't sent
    /// a config yet.
    last_monitors_config: Option<Vec<u8>>,
    /// Timestamp of the most recent monitors config send (for
    /// real or probe). Updated in `send_agent_monitors_config`.
    /// Used to suppress probes if a real send happened within the
    /// probe interval.
    last_monitors_config_sent_at: Option<Instant>,
    /// Most recent stuck-agent notification time, for 60 s cool-down.
    last_stuck_agent_notification_at: Option<Instant>,
}

impl MainChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<RepaintNotify>,
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
            messages_recv_by_opcode: std::collections::BTreeMap::new(),
            messages_send_by_opcode: std::collections::BTreeMap::new(),
            last_unknown_opcode: None,
            unknown_opcode_count: 0,
            mm_clock,
            agent_request_send_ts: HashMap::new(),
            agent_request_count: 0,
            agent_reply_count: 0,
            agent_reply_error_count: 0,
            last_agent_reply_ts_secs: None,
            last_agent_reply_lag_us: None,
            recent_agent_reply_lag_us: VecDeque::new(),
            outstanding_agent_request_count: 0,
            last_monitors_config: None,
            last_monitors_config_sent_at: None,
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

        // K1 watchdog. Spawns a plain std::thread (NOT a tokio
        // task — by design: if tokio's runtime is somehow
        // wedged, this thread is unaffected) that monitors the
        // heartbeat timestamp. If main's heartbeat goes silent
        // for >5 s, the watchdog shells out to `gdb --batch -p
        // $$ -ex 'thread apply all bt'` to capture all-thread
        // backtraces at the moment of the freeze, *before* the
        // server-side rcc disconnect tears everything down.
        // Output lands in /tmp with a timestamped filename. The
        // watchdog fires once per silence period to avoid
        // multiple dumps for the same hang.
        //
        // Opt-in via RYLL_WATCHDOG_GDB=1. Requires `gdb` on
        // PATH and either a permissive `kernel.yama.ptrace_scope`
        // (=0) or `cap_sys_ptrace`.
        let last_heartbeat_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        if std::env::var("RYLL_WATCHDOG_GDB")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            let hb = last_heartbeat_ms.clone();
            let pid = std::process::id();
            info!(
                "main: K1 watchdog enabled (pid {}); will dump backtraces if heartbeat silent >5 s",
                pid
            );
            std::thread::Builder::new()
                .name("ryll-watchdog".into())
                .spawn(move || {
                    use std::sync::atomic::Ordering;
                    let mut fired = false;
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        let last = hb.load(Ordering::Relaxed);
                        if last == 0 {
                            continue;
                        }
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let gap_ms = now.saturating_sub(last);
                        if gap_ms > 5_000 {
                            if !fired {
                                fired = true;
                                let bt_path = format!("/tmp/ryll-watchdog-bt-{}-{}.txt", pid, now);
                                eprintln!(
                                    "ryll-watchdog: main heartbeat silent for {} ms, \
                                     capturing all-thread backtrace via gdb -> {}",
                                    gap_ms, bt_path
                                );
                                let bt_file = match std::fs::File::create(&bt_path) {
                                    Ok(f) => f,
                                    Err(e) => {
                                        eprintln!(
                                            "ryll-watchdog: could not create {}: {}",
                                            bt_path, e
                                        );
                                        continue;
                                    }
                                };
                                let status = std::process::Command::new("gdb")
                                    .args([
                                        "--batch",
                                        "-p",
                                        &pid.to_string(),
                                        "-ex",
                                        "set pagination off",
                                        "-ex",
                                        "thread apply all bt",
                                        "-ex",
                                        "detach",
                                        "-ex",
                                        "quit",
                                    ])
                                    .stdout(bt_file)
                                    .stderr(std::process::Stdio::null())
                                    .status();
                                match status {
                                    Ok(s) if s.success() => {
                                        eprintln!(
                                            "ryll-watchdog: backtrace captured to {}",
                                            bt_path
                                        );
                                    }
                                    Ok(s) => {
                                        eprintln!(
                                            "ryll-watchdog: gdb exited with status {:?}; \
                                             check {} for partial output",
                                            s.code(),
                                            bt_path
                                        );
                                    }
                                    Err(e) => {
                                        eprintln!("ryll-watchdog: failed to spawn gdb: {}", e);
                                    }
                                }
                            }
                        } else {
                            fired = false;
                        }
                    }
                })
                .expect("failed to spawn ryll-watchdog thread");
        }
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
        // Phase 9B: vdagent liveness probe. Fires every 30 s (VDAGENT_PROBE_INTERVAL).
        // Skip the first immediate tick so we don't probe before the agent
        // even attaches.
        let mut vdagent_probe = tokio::time::interval(VDAGENT_PROBE_INTERVAL);
        vdagent_probe.tick().await;
        // Phase 9C: stuck-agent warning. Polls every 5 s to detect
        // stalled agent requests and emit notifications with cool-down.
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
                    }
                } => {
                    last_arm = "read";
                    let n = n?;
                    if n == 0 {
                        info!("main: channel disconnected");
                        self.send_event(ChannelEvent::Disconnected(ChannelType::Main))
                            .await;
                        self.repaint_notify.notify_one();
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
                        self.send_event(ChannelEvent::MonitorsConfig { width, height })
                            .await;
                        self.repaint_notify.notify_one();
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
                    if self.agent_connected && self.agent_caps_announced {
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
                    self.send_event(ChannelEvent::Disconnected(ChannelType::Main))
                        .await;
                    self.repaint_notify.notify_one();
                    break;
                }
                _ = vdagent_probe.tick() => {
                    last_arm = "vdagent_probe";
                    // Phase 9B: Send a liveness probe if conditions are met.
                    // Skip if agent not connected or no agent caps received yet.
                    if self.guest_caps_received {
                        // Check suppression: if a real monitors-config send
                        // happened recently, skip the probe — that send is its
                        // own liveness signal.
                        let should_probe = match self.last_monitors_config_sent_at {
                            None => false,
                            Some(sent_at) => {
                                sent_at.elapsed() >= VDAGENT_PROBE_INTERVAL
                            }
                        };
                        if should_probe {
                            if let Some(payload) = self.last_monitors_config.clone() {
                                // Re-send the cached payload. The guest treats
                                // an unchanged config as a no-op, so this is safe.
                                last_arm = "vdagent_probe+send";
                                // Propagate IO errors via `?` so a socket
                                // failure during the probe surfaces
                                // immediately (matches every other send-site
                                // in this file); `Ok(false)` means we ran out
                                // of agent tokens, which is a transient and
                                // expected condition — log and try the next
                                // tick. Only refresh the send timestamp on a
                                // confirmed Ok(true) send.
                                let sent = self.send_agent_data_message(
                                    VD_AGENT_MONITORS_CONFIG,
                                    &payload,
                                ).await?;
                                if sent {
                                    self.last_monitors_config_sent_at = Some(Instant::now());
                                } else {
                                    debug!("main: vdagent probe skipped, no agent tokens");
                                }
                                last_arm = "vdagent_probe+send_done";
                            }
                        }
                    }
                }
                _ = stuck_agent_check.tick() => {
                    last_arm = "stuck_agent_check";
                    // Phase 9C: Check if agent requests are stuck and emit
                    // Warn notification if conditions are met.
                    if self.outstanding_agent_request_count == 0 {
                        // No outstanding requests; healthy state.
                        last_arm = "stuck_agent_check+no_outstanding";
                    } else if let Some(sent_at) = self.last_monitors_config_sent_at {
                        // Check if enough time has passed since the last send
                        // to consider this a stuck state.
                        if sent_at.elapsed() < STUCK_AGENT_THRESHOLD {
                            // Too soon since the last send; not yet considered stuck.
                            last_arm = "stuck_agent_check+too_soon";
                        } else {
                            // outstanding > 0 and the threshold has been exceeded.
                            // Check the notification cool-down.
                            let should_notify = self
                                .last_stuck_agent_notification_at
                                .map(|t| t.elapsed() >= STUCK_AGENT_NOTIFY_COOLDOWN)
                                .unwrap_or(true);
                            if should_notify {
                                last_arm = "stuck_agent_check+notify";
                                let elapsed_secs =
                                    sent_at.elapsed().as_secs_f64();
                                let count = self.outstanding_agent_request_count;
                                let noun = if count == 1 { "request" } else { "requests" };
                                // "last send was Xs ago" rather than "last
                                // probe sent Xs ago" — outstanding may
                                // include requests older than the most
                                // recent send (we only track the most
                                // recent send_at), so the anchor is the
                                // most recent send, not the oldest
                                // unanswered one.
                                let message = format!(
                                    "Guest agent is not replying — last send was {:.1}s ago, \
                                     {} {} outstanding",
                                    elapsed_secs, count, noun
                                );
                                let entry = NotificationEntry::new(
                                    NotifySeverity::Warn,
                                    NotificationSource::Internal,
                                    message,
                                );
                                self.send_event(ChannelEvent::Notification(entry)).await;
                                self.repaint_notify.notify_one();
                                self.last_stuck_agent_notification_at = Some(Instant::now());
                                last_arm = "stuck_agent_check+notify_done";
                            } else {
                                last_arm = "stuck_agent_check+cooldown";
                            }
                        }
                    } else {
                        // No send timestamp yet, so can't determine if stuck.
                        last_arm = "stuck_agent_check+no_timestamp";
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
        if self.log_config.verbose {
            logging::log_message(
                "received",
                "main",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        // Increment per-opcode recv counter before dispatch so
        // both known and unknown opcodes are counted uniformly.
        *self.messages_recv_by_opcode.entry(msg_type).or_insert(0) += 1;

        match msg_type {
            main_server::INIT => {
                let init = MainInit::read(payload)?;
                info!("main: session initialized: id={}", init.session_id);

                // Seed the shared mm_time clock from the server's
                // initial multi_media_time. Display channel readers
                // (phase 1F STREAM_REPORT) need this base before
                // they can compute a meaningful "now in mm_time".
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

                self.send_event(ChannelEvent::SessionInitialized(init.session_id))
                    .await;
                self.send_event(ChannelEvent::AgentConnected(self.agent_connected))
                    .await;
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
                self.send_event(ChannelEvent::MouseMode(init.current_mouse_mode))
                    .await;
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
                    self.send_event(ChannelEvent::MouseMode(current as u32))
                        .await;
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
                // Periodic multimedia-time tick. The server uses
                // this to keep our `mm_time` clock in sync with
                // its own; the display channel reads the clock at
                // STREAM_REPORT send time to compute
                // `last_frame_delay`. Updating the shared
                // `MmClock` here also makes the value visible in
                // `MainSnapshot::mm_time_*` for bug reports.
                if payload.len() >= 4 {
                    let mm_time =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
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

                self.send_event(ChannelEvent::ChannelsAvailable(channels))
                    .await;
                self.repaint_notify.notify_one();
            }

            main_server::PING => {
                let now = Instant::now();
                if let Some(last) = self.last_ping_at {
                    // f32 storage matches the LatencyTracker history
                    // Vec<f32>; loss of precision is irrelevant for a
                    // sub-millisecond sparkline.
                    let sample_ms = (now - last).as_secs_f64() * 1000.0;
                    self.send_event(ChannelEvent::Latency {
                        sample_ms: sample_ms as f32,
                    })
                    .await;
                    self.repaint_notify.notify_one();
                }
                self.last_ping_at = Some(now);
                self.ping_recv_count = self.ping_recv_count.saturating_add(1);
                self.last_ping_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

                let ping = Ping::read(payload)?;

                if self.log_config.verbose {
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
                self.pong_send_count = self.pong_send_count.saturating_add(1);

                // Request channel list on first large ping
                if ping.id > 0 && self.session_id.is_some() && !self.channels_requested {
                    self.channels_requested = true;
                    self.request_channels_list().await?;
                }
            }

            main_server::SET_ACK => {
                let set_ack = SetAck::read(payload)?;

                if self.log_config.verbose {
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
                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "severity={:?}, visibility={:?}, what={}, message=\"{}\"",
                        notify.severity, notify.visibility, notify.what, notify.message,
                    ));
                }
                match notify.severity {
                    NotifySeverity::Error => {
                        warn!("main: server notify (error): {}", notify.message)
                    }
                    NotifySeverity::Warn => {
                        warn!("main: server notify (warn): {}", notify.message)
                    }
                    NotifySeverity::Info => info!("main: server notify: {}", notify.message),
                }
                let mut entry = NotificationEntry::new(
                    notify.severity,
                    NotificationSource::Spice {
                        channel: ChannelType::Main,
                        what: notify.what,
                    },
                    notify.message.clone(),
                );
                if let Some(v) = notify.visibility {
                    entry = entry.with_visibility(v);
                }
                self.send_event(ChannelEvent::Notification(entry)).await;
                self.repaint_notify.notify_one();
            }

            main_server::DISCONNECTING => {
                info!("main: server sent disconnect notification");
                self.send_event(ChannelEvent::Disconnected(ChannelType::Main))
                    .await;
                self.repaint_notify.notify_one();
            }

            main_server::AGENT_CONNECTED => {
                info!("main: vdagent connected");
                self.agent_connected = true;
                self.send_event(ChannelEvent::AgentConnected(true)).await;
                self.connect_agent().await?;
            }

            main_server::AGENT_DISCONNECTED => {
                info!("main: vdagent disconnected");
                self.agent_connected = false;
                self.send_event(ChannelEvent::AgentConnected(false)).await;
                self.agent_caps_announced = false;
                self.guest_caps_received = false;
                // Per PR #105 review items 1 + 8: drop phase-09
                // probe bookkeeping tied to the previous agent
                // instance. Without this, after the next agent
                // reconnect:
                //   - outstanding_agent_request_count would still
                //     count requests the old agent will never reply
                //     to (spurious stuck-agent Warn notification),
                //   - a stale entry in agent_request_send_ts would
                //     match the next REPLY and yield a multi-minute
                //     lag measurement that pollutes recent_*_lag_us,
                //   - the cool-down timer would suppress a real
                //     new-agent stuck notification,
                //   - a cached monitors-config from the prior
                //     session could be re-sent by the probe to the
                //     new agent, potentially with stale geometry.
                // Clear all of them; the new session's first real
                // monitors-config send (on resize or session bring-
                // up) will re-populate the cache from current state.
                self.agent_request_send_ts.clear();
                self.outstanding_agent_request_count = 0;
                self.last_monitors_config = None;
                self.last_monitors_config_sent_at = None;
                self.last_stuck_agent_notification_at = None;
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

            unknown => {
                // Unknown opcode — log hex once per msg_type, silent on repeat.
                logging::log_unknown_once("main", unknown, payload);
                self.unknown_opcode_count += 1;
                self.last_unknown_opcode = Some(unknown);
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
        let mut snap = self.snapshot.lock().unwrap();
        snap.session_id = self.session_id;
        snap.bytes_in = self.bytes_in;
        snap.bytes_out = self.bytes_out;
        snap.last_recv_ts_secs = self.last_recv_ts_secs;
        snap.last_send_ts_secs = self.last_send_ts_secs;
        snap.ping_recv_count = self.ping_recv_count;
        snap.pong_send_count = self.pong_send_count;
        snap.last_ping_recv_ts_secs = self.last_ping_recv_ts_secs;
        snap.writer_dropped_count = self.capture_dropped_count;
        // mm_time clock state. `now()` is informational —
        // computed at snapshot time so a bug report shows the
        // server's current millisecond counter.
        snap.mm_time_now = self.mm_clock.now();
        snap.mm_time_set_count = self.mm_clock.set_count();
        snap.last_mm_time_set_ts_secs = self.mm_clock.last_set_ts_secs();
        snap.messages_recv_by_opcode = self.messages_recv_by_opcode.clone();
        snap.messages_send_by_opcode = self.messages_send_by_opcode.clone();
        snap.last_unknown_opcode = self.last_unknown_opcode;
        snap.unknown_opcode_count = self.unknown_opcode_count;
        snap.agent_request_count = self.agent_request_count;
        snap.agent_reply_count = self.agent_reply_count;
        snap.agent_reply_error_count = self.agent_reply_error_count;
        snap.last_agent_reply_ts_secs = self.last_agent_reply_ts_secs;
        snap.last_agent_reply_lag_us = self.last_agent_reply_lag_us;
        snap.recent_agent_reply_lag_us = self.recent_agent_reply_lag_us.clone();
        snap.outstanding_agent_request_count = self.outstanding_agent_request_count;
    }

    async fn request_channels_list(&mut self) -> Result<()> {
        let msg = make_message(main_client::ATTACH_CHANNELS, &[]);
        self.send_with_log(main_client::ATTACH_CHANNELS, &msg).await
    }

    /// Send a `ChannelEvent` to the renderer with a 5 s timeout
    /// and silent-drop on closed-receiver. K1 (session-001) was an
    /// abandoned-receiver deadlock where main blocked forever on
    /// `event_tx.send().await`; the root cause was fixed by
    /// removing the intermediate temp channel, but this helper is
    /// defense-in-depth so the next time something similar
    /// regresses we see a `warn!` line within 5 seconds instead of
    /// a silent multi-minute hang.
    async fn send_event(&self, ev: ChannelEvent) {
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.event_tx.send(ev)).await
        {
            Ok(Ok(())) => {}
            Ok(Err(_closed)) => {}
            Err(_elapsed) => {
                warn!(
                    "main: event_tx.send() timed out after 5 s; \
                     renderer event consumer is wedged or starved"
                );
            }
        }
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
        let mode_payload = build_mouse_mode_request_payload(MOUSE_MODE_CLIENT);
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

        // Send first; only refresh the phase-9B probe cache if the
        // send actually went on the wire (Ok(true)). Caching before
        // the send would defer the next probe by one interval after
        // an Ok(false) "no tokens" outcome, suppressing the probe
        // even though no message left the client.
        let sent = self
            .send_agent_data_message(VD_AGENT_MONITORS_CONFIG, &payload)
            .await?;
        if sent {
            self.last_monitors_config = Some(payload);
            self.last_monitors_config_sent_at = Some(Instant::now());
        }
        Ok(sent)
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

        // Track send time for REPLY-eligible request types so we
        // can compute reply lag when VD_AGENT_REPLY arrives.
        //
        // Overwriting any prior entry for `ty` is intentional —
        // VD_AGENT_REPLY has no request id, only a request type,
        // so two sends within a probe interval cannot be
        // distinguished individually. We measure lag against the
        // most recent send and accept that the in-flight earlier
        // REPLY (if any) will skip the lag-update branch when it
        // arrives (no matching map entry by then). The trade-off
        // is documented in the phase 09 plan's *Background* and
        // surfaces as a "no matching send entry" debug log in
        // handle_agent_message.
        if REPLY_ELIGIBLE_AGENT_REQUEST_TYPES.contains(&ty) {
            self.agent_request_send_ts.insert(ty, Instant::now());
            self.agent_request_count = self.agent_request_count.saturating_add(1);
            self.outstanding_agent_request_count =
                self.outstanding_agent_request_count.saturating_add(1);
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
                if payload.len() >= 8 {
                    let format =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    if format == VD_AGENT_CLIPBOARD_UTF8_TEXT {
                        debug!("main: guest clipboard grab, requesting data");
                        self.send_clipboard_request().await?;
                    }
                }
            }
            VD_AGENT_CLIPBOARD => {
                // payload: selection(u32) + format(u32) + data
                let offset = 4;
                if payload.len() > offset + 4 {
                    let data = &payload[offset + 4..];
                    if !data.is_empty() {
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
                }
            }
            VD_AGENT_CLIPBOARD_REQUEST => {
                info!("main: VD_AGENT_CLIPBOARD_REQUEST received");
                // payload: selection(u32) + format(u32)
                if payload.len() >= 8 {
                    let format =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    if format == VD_AGENT_CLIPBOARD_UTF8_TEXT {
                        debug!("main: clipboard request from guest");
                        // Same spawn_blocking + timeout shape as
                        // poll_host_clipboard: cb.get_text() can
                        // hang macOS NSPasteboard when ryll is
                        // backgrounded.
                        let text = match self.clipboard.as_ref() {
                            Some(c) => {
                                let cb = c.clone();
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
                                        warn!(
                                            "main: clipboard request timed out (1 s), \
                                             ignoring guest request"
                                        );
                                        None
                                    }
                                }
                            }
                            None => None,
                        };
                        if let Some(text) = text {
                            // Log byte count only — clipboard content may contain
                            // passwords or sensitive data.
                            info!("main: clipboard to guest ({} bytes)", text.len());
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
                    // agent disconnect) would otherwise mask a real
                    // stuck-agent symptom by dropping the outstanding
                    // count to zero. Per PR #105 review item 2.
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
        if self.log_config.verbose {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "main", msg_type, msg_name, payload_size);
        }
        self.traffic.record_sent("main", msg_type, msg_name, data);
        // Increment per-opcode send counter here — single send path.
        *self.messages_send_by_opcode.entry(msg_type).or_insert(0) += 1;
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
    use super::{
        build_mouse_mode_request_payload, hash_clipboard, parse_mouse_mode_payload,
        parse_vd_agent_reply, should_request_client_mouse_mode, VD_AGENT_ANNOUNCE_CAPABILITIES,
        VD_AGENT_CLIPBOARD, VD_AGENT_CLIPBOARD_GRAB, VD_AGENT_CLIPBOARD_RELEASE,
        VD_AGENT_CLIPBOARD_REQUEST, VD_AGENT_DISPLAY_CONFIG, VD_AGENT_MONITORS_CONFIG,
        VD_AGENT_MOUSE_STATE, VD_AGENT_REPLY,
    };
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
    fn mouse_mode_request_payload_is_two_bytes_for_client() {
        // Regression for PR 31 blocking #3: the body is flags16 (one
        // little-endian u16), not u32. Writing u32 here shipped two
        // extra zero bytes that some servers reject as malformed.
        assert_eq!(
            build_mouse_mode_request_payload(MOUSE_MODE_CLIENT),
            vec![0x02, 0x00],
        );
    }

    #[test]
    fn mouse_mode_request_payload_is_two_bytes_for_server() {
        // Same shape regardless of which mode we ask for —
        // belt-and-braces against a future "let's also encode
        // supported_modes" temptation that would re-widen the body.
        assert_eq!(
            build_mouse_mode_request_payload(MOUSE_MODE_SERVER),
            vec![0x01, 0x00],
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
}
