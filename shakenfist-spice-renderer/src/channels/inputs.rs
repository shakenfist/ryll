/// Inputs channel handler - keyboard and mouse input
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::snapshots::{InputEventRecord, InputsSnapshot};
use crate::{
    ByteCounter, CaptureSink, LogConfig, NotificationEntry, NotificationSource, TrafficSink,
};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, InputsKeyModifiers, KeyEvent, MessageHeader, MouseButton, MouseMotion,
    MousePosition, Notify as NotifyMessage, Ping, SetAck,
};
use shakenfist_spice_protocol::{
    inputs_client, inputs_server, keyboard_modifiers, ChannelType, NotifySeverity,
};

use super::{ChannelEvent, InputEvent};

/// spice-gtk throttles motion messages to this many pending before an ACK
const MOTION_ACK_BUNCH: u32 = 4;

/// Consecutive throttled pointer moves, with no `MOUSE_MOTION_ACK`
/// in between, that make the ack window look wedged rather than
/// briefly busy. At a browser's pointer event rate a genuine burst
/// clears in a handful of events, so this is roughly a second of a
/// pointer that is going nowhere.
const MOTION_WEDGE_THRESHOLD: u32 = 100;

/// Tracks whether the motion ack window is merely busy or wedged.
///
/// The window drains only on `MOUSE_MOTION_ACK`, and a server does
/// not acknowledge what it never consumed — so sending the pointer
/// message form the negotiated mouse mode does not use fills the
/// window once and then drops every move for the rest of the session,
/// silently. A short run of drops, by contrast, is an ordinary burst.
///
/// Separated from [`InputsChannel`] so the "warn once per wedge, and
/// again if it wedges a second time" rule can be tested without a
/// SPICE stream.
#[derive(Default)]
struct MotionWedgeDetector {
    /// Consecutive throttled moves since the last ack.
    drops_since_ack: u32,
    /// Whether the current run has already been reported.
    warned: bool,
}

impl MotionWedgeDetector {
    /// Record a throttled move. Returns `true` when the caller should
    /// emit the warning — at most once per run of drops.
    fn note_drop(&mut self) -> bool {
        self.drops_since_ack = self.drops_since_ack.saturating_add(1);
        if self.drops_since_ack >= MOTION_WEDGE_THRESHOLD && !self.warned {
            self.warned = true;
            return true;
        }
        false
    }

    /// Record an ack. The window is draining, so the run of drops was
    /// a busy burst rather than a wedge — and the warning is armed
    /// again, because a session that wedges, recovers and wedges a
    /// second time is two faults, and reporting only the first hides
    /// the one the user is looking at.
    fn note_ack(&mut self) {
        self.drops_since_ack = 0;
        self.warned = false;
    }

    /// Drops in the current run, for the warning message.
    fn drops(&self) -> u32 {
        self.drops_since_ack
    }
}

/// Maximum number of recent input events to keep in the snapshot.
const MAX_RECENT_EVENTS: usize = 50;

/// Maximum characters for a single paste-as-keystrokes sequence.
const PASTE_MAX_CHARS: usize = 4096;

/// Sub-step within a single character of a paste sequence.
#[derive(Debug, Clone, Copy)]
enum PasteSubStep {
    /// Send shift-down (if needed) + key-down, then wait half the delay.
    Press,
    /// Send key-up + shift-up (if needed), then wait the remaining half.
    Release,
}

/// State for an in-progress paste-as-keystrokes sequence.
#[derive(Debug)]
struct PasteState {
    keys: Vec<PasteKey>,
    index: usize,
    sub_step: PasteSubStep,
    half_delay: Duration,
    start: Instant,
    next_fire: Instant,
    /// Modifier state saved at paste start, restored at paste end.
    saved_ctrl: bool,
    saved_shift: bool,
    saved_alt: bool,
    /// Correlation token for control-socket-initiated pastes; `None`
    /// for `--paste-text` CLI pastes.  Echoed in `PasteCompleted` /
    /// `PasteFailed` channel events.
    request_id: Option<crate::channels::RequestId>,
    /// Optional cancellation token.  Checked before each sub-step;
    /// when cancelled the paste aborts with a `PasteFailed` event.
    cancel: Option<CancellationToken>,
}

pub struct InputsChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    repaint_notify: Arc<Notify>,
    input_rx: mpsc::Receiver<InputEvent>,
    buffer: Vec<u8>,
    last_key_time: Option<Instant>,
    button_state: u32,
    motion_count: u32,
    /// Distinguishes a busy ack window from a wedged one. See
    /// [`MotionWedgeDetector`].
    motion_wedge: MotionWedgeDetector,
    capture: Option<Arc<dyn CaptureSink>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    log_config: LogConfig,
    snapshot: Arc<Mutex<InputsSnapshot>>,
    recent_events: VecDeque<InputEventRecord>,
    bytes_in: u64,
    bytes_out: u64,
    /// Local cache of disconnect-cause diagnostic fields,
    /// flushed to `snapshot` by `update_snapshot()`.
    last_recv_ts_secs: Option<f64>,
    last_send_ts_secs: Option<f64>,
    ping_recv_count: u32,
    pong_send_count: u32,
    last_ping_recv_ts_secs: Option<f64>,
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
    /// Most recent KEY_MODIFIERS value we've sent. Restated by the
    /// idle keepalive, with the same value to keep the inputs channel
    /// non-idle without changing guest state.
    last_modifiers_sent: u16,
    /// Wall-clock time (tokio runtime) of the last inbound *or*
    /// outbound activity on the channel. The idle keepalive
    /// fires at `last_activity + KEEPALIVE_IDLE`. Updated on
    /// every recv and every send via `mark_activity()`.
    last_activity: tokio::time::Instant,
    /// Number of idle keepalive messages sent. Surfaced via the
    /// snapshot for disconnect-cause diagnostics.
    client_keepalive_send_count: u32,
    /// Session-relative seconds at the most recent keepalive
    /// send.
    last_client_keepalive_send_ts_secs: Option<f64>,
    enable_paste: bool,
    ctrl_held: bool,
    shift_held: bool,
    alt_held: bool,
    paste_state: Option<PasteState>,
    /// See `MainChannel::capture_dropped_count`.
    capture_dropped_count: u64,
}

/// Idle window before the inputs-channel keepalive fires.
/// Conservative against the empirically observed 300 s
/// server-side per-channel silence threshold (see session-001b
/// data); 10 s leaves ample headroom for jitter and clock skew.
const KEEPALIVE_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

impl InputsChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<Notify>,
        input_rx: mpsc::Receiver<InputEvent>,
        capture: Option<Arc<dyn CaptureSink>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<dyn TrafficSink>,
        snapshot: Arc<Mutex<InputsSnapshot>>,
        enable_paste: bool,
        log_config: LogConfig,
    ) -> Self {
        InputsChannel {
            stream,
            event_tx,
            repaint_notify,
            input_rx,
            buffer: Vec::with_capacity(4096),
            last_key_time: None,
            button_state: 0,
            motion_count: 0,
            motion_wedge: MotionWedgeDetector::default(),
            capture,
            byte_counter,
            traffic,
            log_config,
            snapshot,
            recent_events: VecDeque::new(),
            bytes_in: 0,
            bytes_out: 0,
            last_recv_ts_secs: None,
            last_send_ts_secs: None,
            ping_recv_count: 0,
            pong_send_count: 0,
            last_ping_recv_ts_secs: None,
            messages_recv_by_opcode: std::collections::BTreeMap::new(),
            messages_send_by_opcode: std::collections::BTreeMap::new(),
            last_unknown_opcode: None,
            unknown_opcode_count: 0,
            last_modifiers_sent: 0,
            last_activity: tokio::time::Instant::now(),
            client_keepalive_send_count: 0,
            last_client_keepalive_send_ts_secs: None,
            enable_paste,
            ctrl_held: false,
            shift_held: false,
            alt_held: false,
            paste_state: None,
            capture_dropped_count: 0,
        }
    }

    /// Run the inputs channel event loop. Wraps `run_loop`
    /// so any error propagating out of the inner select! arms
    /// is logged before the task ends — see the rationale on
    /// `MainChannel::run` (including the `Box::pin` reason).
    pub async fn run(&mut self) -> Result<()> {
        let result = Box::pin(self.run_loop()).await;
        match &result {
            Ok(()) => info!("inputs: run loop exited cleanly"),
            Err(e) => error!("inputs: run loop exited with error: {:#}", e),
        }
        result
    }

    async fn run_loop(&mut self) -> Result<()> {
        info!("inputs: channel started");

        // Send initial key modifiers (NumLock on)
        self.send_key_modifiers(keyboard_modifiers::NUM_LOCK)
            .await?;

        loop {
            // Capture before reborrows so the closure can move it
            // into the keepalive branch without a self conflict.
            let keepalive_deadline = self.last_activity + KEEPALIVE_IDLE;

            // Borrow fields separately to avoid borrow checker issues in select!
            let stream = &mut self.stream;
            let buffer = &mut self.buffer;
            let bytes_in = &mut self.bytes_in;
            let capture_dropped_count = &mut self.capture_dropped_count;
            let input_rx = &mut self.input_rx;
            let capture = &self.capture;
            let byte_counter = &self.byte_counter;

            let paste_next = self.paste_state.as_ref().map(|s| s.next_fire);

            // Create read future inline
            let read_fut = async {
                let mut chunk = [0u8; 4096];
                let n = match stream {
                    SpiceStream::Plain(s) => {
                        use tokio::io::AsyncReadExt;
                        s.read(&mut chunk).await?
                    }
                    SpiceStream::Tls(s) => {
                        use tokio::io::AsyncReadExt;
                        s.read(&mut chunk).await?
                    }
                    SpiceStream::TlsServer(s) => {
                        use tokio::io::AsyncReadExt;
                        s.read(&mut chunk).await?
                    }
                };
                if n > 0 {
                    byte_counter.add(n as u64);
                    if let Some(ref c) = capture {
                        if !c.packet_received("inputs", &chunk[..n]) {
                            *capture_dropped_count = capture_dropped_count.saturating_add(1);
                        }
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    *bytes_in += n as u64;
                }
                Ok::<_, anyhow::Error>(n)
            };

            tokio::select! {
                // Handle incoming data from server
                result = read_fut => {
                    match result {
                        Ok(0) => {
                            info!("inputs: channel disconnected");
                            self.event_tx
                                .send(ChannelEvent::Disconnected(ChannelType::Inputs))
                                .await
                                .ok();
                            self.repaint_notify.notify_one();
                            break;
                        }
                        Ok(_) => {
                            self.last_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
                            self.last_activity = tokio::time::Instant::now();
                            self.process_messages().await?;
                        }
                        Err(e) => {
                            self.event_tx
                                .send(ChannelEvent::Error {
                                    channel: ChannelType::Inputs,
                                    message: format!("read error: {}", e),
                                })
                                .await
                                .ok();
                            self.repaint_notify.notify_one();
                            break;
                        }
                    }
                }

                // Handle input events from UI.
                //
                // After waking on the first event, drain everything currently
                // queued and coalesce consecutive MouseMove events into a single
                // position update.  This keeps the channel from filling up during
                // network stalls (which would cause try_send on the producer side
                // to silently drop critical button events) and reduces the number
                // of TCP writes we need to make.
                Some(event) = input_rx.recv() => {
                    let mut batch = vec![event];
                    while let Ok(next) = input_rx.try_recv() {
                        batch.push(next);
                    }

                    let mut i = 0;
                    while i < batch.len() {
                        if matches!(batch[i], InputEvent::MouseMove { .. }) {
                            // Find the last consecutive MouseMove in this run.
                            let mut last_move = i;
                            while last_move + 1 < batch.len()
                                && matches!(batch[last_move + 1], InputEvent::MouseMove { .. })
                            {
                                last_move += 1;
                            }
                            // Send only the final position from the run.
                            self.handle_input_event(batch[last_move].clone()).await?;
                            i = last_move + 1;
                        } else if matches!(batch[i], InputEvent::MouseMotion { .. }) {
                            // Accumulate consecutive relative motions into one.
                            let mut total_dx = 0i32;
                            let mut total_dy = 0i32;
                            let mut last_motion = i;
                            while last_motion < batch.len() {
                                if let InputEvent::MouseMotion { dx, dy } = batch[last_motion] {
                                    total_dx += dx;
                                    total_dy += dy;
                                    last_motion += 1;
                                } else {
                                    break;
                                }
                            }
                            self.handle_input_event(InputEvent::MouseMotion {
                                dx: total_dx,
                                dy: total_dy,
                            })
                            .await?;
                            i = last_motion;
                        } else {
                            self.handle_input_event(batch[i].clone()).await?;
                            i += 1;
                        }
                    }
                }

                // Paste state machine: fire when the next step is due.
                _ = async {
                    match paste_next {
                        Some(t) => tokio::time::sleep_until(
                            tokio::time::Instant::from_std(t)
                        ).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.advance_paste().await?;
                }

                // Idle keepalive. When the channel has been silent in both directions for
                // KEEPALIVE_IDLE, re-send the most recent KEY_MODIFIERS value. The guest
                // sees no change (same modifier state); the server sees a client→server
                // byte and resets its per-channel idle timer. Hypothesis under test:
                // keeping inputs busy may also keep main alive, sidestepping the K1
                // main-channel rcc disconnect.
                _ = tokio::time::sleep_until(keepalive_deadline) => {
                    self.send_idle_keepalive().await?;
                }
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    async fn read_from_server(&mut self) -> Result<usize> {
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
            SpiceStream::TlsServer(s) => {
                use tokio::io::AsyncReadExt;
                s.read(&mut chunk).await?
            }
        };

        if n > 0 {
            self.buffer.extend_from_slice(&chunk[..n]);
            self.bytes_in += n as u64;
        }

        Ok(n)
    }

    async fn process_messages(&mut self) -> Result<()> {
        while self.buffer.len() >= MessageHeader::SIZE {
            let header = MessageHeader::read(&self.buffer)?;
            let total_size = MessageHeader::SIZE + header.message_size as usize;

            if self.buffer.len() < total_size {
                break;
            }

            // Record to ring buffer before draining
            let raw = self.buffer[..total_size].to_vec();
            self.traffic.record_received(
                "inputs",
                header.message_type,
                message_names::inputs_server(header.message_type),
                &raw,
            );

            let payload = self.buffer[MessageHeader::SIZE..total_size].to_vec();
            self.buffer.drain(..total_size);

            self.handle_server_message(header.message_type, &payload)
                .await?;
        }

        self.update_snapshot();
        Ok(())
    }

    async fn handle_server_message(&mut self, msg_type: u16, payload: &[u8]) -> Result<()> {
        let msg_type_str = message_names::inputs_server(msg_type);

        // Log all messages in verbose mode
        if self.log_config.verbose {
            logging::log_message(
                "received",
                "inputs",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        // Increment per-opcode recv counter before dispatch so
        // both known and unknown opcodes are counted uniformly.
        *self.messages_recv_by_opcode.entry(msg_type).or_insert(0) += 1;

        match msg_type {
            inputs_server::INIT => {
                debug!("inputs: init received");
            }

            inputs_server::KEY_MODIFIERS => {
                if payload.len() >= 2 {
                    let modifiers = u16::from_le_bytes([payload[0], payload[1]]);

                    if self.log_config.verbose {
                        logging::log_detail(&format!("modifiers={:#x}", modifiers));
                    } else {
                        debug!("inputs: key modifiers from server: {:#x}", modifiers);
                    }
                }
            }

            inputs_server::MOUSE_MOTION_ACK => {
                self.motion_count = self.motion_count.saturating_sub(MOTION_ACK_BUNCH);
                self.motion_wedge.note_ack();
                debug!("inputs: mouse motion ack (pending={})", self.motion_count);
            }

            inputs_server::SET_ACK => {
                let set_ack = SetAck::read(payload)?;

                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "generation={}, window={}",
                        set_ack.generation, set_ack.window
                    ));
                }

                // ACK_SYNC is opcode 1 (common across all channels)
                let mut ack_payload = Vec::new();
                SetAck::write_ack_sync(set_ack.generation, &mut ack_payload)?;
                let response = make_message(1, &ack_payload);
                self.send_with_log(1, &response).await?;
            }

            inputs_server::PING => {
                self.ping_recv_count = self.ping_recv_count.saturating_add(1);
                self.last_ping_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

                let ping = Ping::read(payload)?;

                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "ping_id={}, timestamp={}",
                        ping.id, ping.timestamp
                    ));
                }

                let mut pong_payload = Vec::new();
                ping.write_pong(&mut pong_payload)?;
                // Inputs channel uses same message type for pong
                let response = make_message(3, &pong_payload); // PONG
                self.send_with_log(3, &response).await?;
                self.pong_send_count = self.pong_send_count.saturating_add(1);
            }

            inputs_server::NOTIFY => {
                let notify = NotifyMessage::read(payload)?;
                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "severity={:?}, visibility={:?}, what={}, message=\"{}\"",
                        notify.severity, notify.visibility, notify.what, notify.message,
                    ));
                }
                match notify.severity {
                    NotifySeverity::Error => {
                        warn!("inputs: server notify (error): {}", notify.message)
                    }
                    NotifySeverity::Warn => {
                        warn!("inputs: server notify (warn): {}", notify.message)
                    }
                    NotifySeverity::Info => {
                        info!("inputs: server notify: {}", notify.message)
                    }
                }
                let mut entry = NotificationEntry::new(
                    notify.severity,
                    NotificationSource::Spice {
                        channel: ChannelType::Inputs,
                        what: notify.what,
                    },
                    notify.message.clone(),
                );
                if let Some(v) = notify.visibility {
                    entry = entry.with_visibility(v);
                }
                self.event_tx
                    .send(ChannelEvent::Notification(entry))
                    .await
                    .ok();
                self.repaint_notify.notify_one();
            }

            unknown => {
                // Unknown opcode — log hex once per msg_type, silent on repeat.
                logging::log_unknown_once("inputs", unknown, payload);
                self.unknown_opcode_count += 1;
                self.last_unknown_opcode = Some(unknown);
            }
        }

        Ok(())
    }

    async fn handle_input_event(&mut self, event: InputEvent) -> Result<()> {
        let ts = self.traffic.elapsed().as_secs_f64();

        match event {
            InputEvent::KeyDown(scancode) => {
                // last_key_time is recorded for the bug-report snapshot only;
                // latency is now measured from server PINGs in main_channel.rs.
                self.last_key_time = Some(Instant::now());

                self.record_event(InputEventRecord {
                    event_type: "KeyDown".to_string(),
                    scancode,
                    x: 0,
                    y: 0,
                    button_mask: 0,
                    timestamp_secs: ts,
                });

                let mut payload = Vec::new();
                KeyEvent { scancode }.write(&mut payload)?;
                let msg = make_message(inputs_client::KEY_DOWN, &payload);

                debug!("inputs: key down: scancode={:#x}", scancode);
                self.send_with_log(inputs_client::KEY_DOWN, &msg).await?;

                match scancode {
                    0x1D => self.ctrl_held = true,
                    0x2A => self.shift_held = true,
                    0x38 => self.alt_held = true,
                    _ => {}
                }
            }

            InputEvent::KeyUp(scancode) => {
                self.record_event(InputEventRecord {
                    event_type: "KeyUp".to_string(),
                    scancode,
                    x: 0,
                    y: 0,
                    button_mask: 0,
                    timestamp_secs: ts,
                });

                let mut payload = Vec::new();
                KeyEvent { scancode }.write(&mut payload)?;
                let msg = make_message(inputs_client::KEY_UP, &payload);

                debug!("inputs: key up: scancode={:#x}", scancode);
                self.send_with_log(inputs_client::KEY_UP, &msg).await?;

                match scancode {
                    0x9D => self.ctrl_held = false,
                    0xAA => self.shift_held = false,
                    0xB8 => self.alt_held = false,
                    _ => {}
                }
            }

            InputEvent::MouseMove { x, y } => {
                // Throttle: don't exceed MOTION_ACK_BUNCH * 2 pending
                if self.motion_count < MOTION_ACK_BUNCH * 2 {
                    self.record_event(InputEventRecord {
                        event_type: "MouseMove".to_string(),
                        scancode: 0,
                        x,
                        y,
                        button_mask: self.button_state,
                        timestamp_secs: ts,
                    });

                    let mut payload = Vec::new();
                    MousePosition {
                        x,
                        y,
                        buttons: self.button_state as u16,
                        display_id: 0,
                    }
                    .write(&mut payload)?;
                    let msg = make_message(inputs_client::MOUSE_POSITION, &payload);
                    self.send_with_log(inputs_client::MOUSE_POSITION, &msg)
                        .await?;
                    self.motion_count += 1;
                } else {
                    self.note_motion_throttled();
                }
            }

            InputEvent::MouseMotion { dx, dy } => {
                // Server mouse mode: send relative deltas.
                if self.motion_count < MOTION_ACK_BUNCH * 2 {
                    self.record_event(InputEventRecord {
                        event_type: "MouseMotion".to_string(),
                        scancode: 0,
                        x: dx as u32,
                        y: dy as u32,
                        button_mask: self.button_state,
                        timestamp_secs: ts,
                    });

                    let mut payload = Vec::new();
                    MouseMotion {
                        dx,
                        dy,
                        buttons: self.button_state as u16,
                    }
                    .write(&mut payload)?;
                    let msg = make_message(inputs_client::MOUSE_MOTION, &payload);
                    self.send_with_log(inputs_client::MOUSE_MOTION, &msg)
                        .await?;
                    self.motion_count += 1;
                } else {
                    self.note_motion_throttled();
                }
            }

            InputEvent::MouseDown { button, x, y } => {
                // Send position before button press. spice-gtk does the
                // same: channel-inputs.c:438-439 calls send_motion() +
                // send_position() immediately before marshalling the
                // press message. This ensures the server knows the
                // cursor location at the moment of the click, which
                // matters when the user clicks without moving first
                // (e.g. clicking a button that appeared under the
                // cursor after a dialog opened).
                let mut pos_payload = Vec::new();
                MousePosition {
                    x,
                    y,
                    buttons: self.button_state as u16,
                    display_id: 0,
                }
                .write(&mut pos_payload)?;
                let pos_msg = make_message(inputs_client::MOUSE_POSITION, &pos_payload);
                self.send_with_log(inputs_client::MOUSE_POSITION, &pos_msg)
                    .await?;

                self.button_state |= button;

                self.record_event(InputEventRecord {
                    event_type: "MouseDown".to_string(),
                    scancode: 0,
                    x,
                    y,
                    button_mask: button,
                    timestamp_secs: ts,
                });

                let mut payload = Vec::new();
                MouseButton {
                    button: button as u8,
                    buttons_state: self.button_state as u16,
                }
                .write(&mut payload)?;
                let msg = make_message(inputs_client::MOUSE_PRESS, &payload);
                info!(
                    "inputs: mouse down: button={}, pos=({},{}), state={:#x}",
                    button, x, y, self.button_state
                );
                self.send_with_log(inputs_client::MOUSE_PRESS, &msg).await?;
            }

            InputEvent::MouseUp { button, x, y } => {
                // Clear button state first, then send position with
                // cleared state before the release — matches spice-gtk
                // ordering (channel-inputs.c:509-510 calls send_motion()
                // + send_position() before marshalling the release
                // message, after clearing the button mask at line 502).
                self.button_state &= !button;

                let mut pos_payload = Vec::new();
                MousePosition {
                    x,
                    y,
                    buttons: self.button_state as u16,
                    display_id: 0,
                }
                .write(&mut pos_payload)?;
                let pos_msg = make_message(inputs_client::MOUSE_POSITION, &pos_payload);
                self.send_with_log(inputs_client::MOUSE_POSITION, &pos_msg)
                    .await?;

                self.record_event(InputEventRecord {
                    event_type: "MouseUp".to_string(),
                    scancode: 0,
                    x,
                    y,
                    button_mask: button,
                    timestamp_secs: ts,
                });

                let mut payload = Vec::new();
                MouseButton {
                    button: button as u8,
                    buttons_state: self.button_state as u16,
                }
                .write(&mut payload)?;
                let msg = make_message(inputs_client::MOUSE_RELEASE, &payload);
                debug!("inputs: mouse up: button={}, pos=({},{})", button, x, y);
                self.send_with_log(inputs_client::MOUSE_RELEASE, &msg)
                    .await?;
            }

            InputEvent::PasteText {
                text,
                char_delay_ms,
                request_id,
                cancel,
            } => {
                if !self.enable_paste {
                    warn!("inputs: paste-as-keystrokes not enabled, ignoring");
                    return Ok(());
                }
                if self.paste_state.is_some() {
                    warn!("inputs: paste already in progress, ignoring");
                    return Ok(());
                }

                // Enforce character cap
                let mut text = text;
                let char_count = text.chars().count();
                if char_count > PASTE_MAX_CHARS {
                    text = text.chars().take(PASTE_MAX_CHARS).collect();
                    warn!(
                        "inputs: paste truncated from {} to {} characters",
                        char_count, PASTE_MAX_CHARS
                    );
                }

                // Translate
                let keys = match translate_paste(&text) {
                    Ok(k) => k,
                    Err(PasteError::Unrepresentable { count, sample }) => {
                        let sample_str: String = sample
                            .iter()
                            .map(|c| format!("U+{:04X}", *c as u32))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let reason = format!(
                            "cannot paste: {} unrepresentable character(s): {}",
                            count, sample_str
                        );
                        error!("inputs: {}", reason);
                        self.event_tx
                            .send(ChannelEvent::PasteFailed { reason, request_id })
                            .await
                            .ok();
                        self.repaint_notify.notify_one();
                        return Ok(());
                    }
                };

                if keys.is_empty() {
                    self.event_tx
                        .send(ChannelEvent::PasteCompleted {
                            chars: 0,
                            elapsed_ms: 0,
                            request_id,
                        })
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                    return Ok(());
                }

                let delay = Duration::from_millis(char_delay_ms as u64);
                info!(
                    "inputs: starting paste of {} characters (delay={}ms)",
                    keys.len(),
                    char_delay_ms
                );

                // Save and release held modifiers
                let saved_ctrl = self.ctrl_held;
                let saved_shift = self.shift_held;
                let saved_alt = self.alt_held;

                if self.ctrl_held {
                    self.send_key_up(0x9D).await?;
                    self.ctrl_held = false;
                }
                if self.shift_held {
                    self.send_key_up(0xAA).await?;
                    self.shift_held = false;
                }
                if self.alt_held {
                    self.send_key_up(0xB8).await?;
                    self.alt_held = false;
                }

                let now = Instant::now();
                self.paste_state = Some(PasteState {
                    keys,
                    index: 0,
                    sub_step: PasteSubStep::Press,
                    half_delay: delay / 2,
                    start: now,
                    next_fire: now, // fire immediately
                    saved_ctrl,
                    saved_shift,
                    saved_alt,
                    request_id,
                    cancel,
                });
            }
        }

        Ok(())
    }

    /// Note a pointer move dropped by the ack-window throttle, and
    /// complain once per session if the throttle looks wedged
    /// rather than merely busy.
    ///
    /// The throttle matches spice-gtk (`channel-inputs.c`): at most
    /// `MOTION_ACK_BUNCH * 2` motions may be outstanding, and the
    /// server returns a `MOUSE_MOTION_ACK` for every
    /// `MOTION_ACK_BUNCH` it consumes. A burst of input can fill
    /// that window briefly, which is normal and not worth a word.
    ///
    /// What is not normal is the window never draining. The count
    /// only falls on an ack, so if the server acks nothing the
    /// window stays full for the rest of the session and every
    /// later pointer move is discarded — the guest pointer just
    /// stops moving. A client sending the wrong message type for
    /// the negotiated mouse mode produces exactly that, because
    /// the server does not ack what it did not consume.
    ///
    /// [`MOTION_WEDGE_THRESHOLD`] consecutive drops with no
    /// intervening ack is the line between the two. Below it, say
    /// nothing; above it, say so once. The original failure had no
    /// log line at all, which made an input problem look like a
    /// rendering or transport one.
    fn note_motion_throttled(&mut self) {
        if self.motion_wedge.note_drop() {
            warn!(
                "inputs: {} consecutive pointer moves dropped with {} \
                 motions outstanding and no MOUSE_MOTION_ACK in between; \
                 further motion will be dropped silently. If the guest \
                 pointer is not moving, check the negotiated mouse mode — \
                 the server does not acknowledge messages it ignores.",
                self.motion_wedge.drops(),
                self.motion_count
            );
        }
    }

    /// Record an input event in the local deque.
    fn record_event(&mut self, event: InputEventRecord) {
        self.recent_events.push_back(event);
        if self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
    }

    /// Sync local state to the shared snapshot.
    fn update_snapshot(&self) {
        let mut snap = self.snapshot.lock().expect("lock poisoned");
        snap.button_state = self.button_state;
        snap.motion_count = self.motion_count;
        snap.secs_since_last_key = self.last_key_time.map(|t| t.elapsed().as_secs_f64());
        snap.recent_events = self.recent_events.clone();
        snap.bytes_in = self.bytes_in;
        snap.bytes_out = self.bytes_out;
        snap.last_recv_ts_secs = self.last_recv_ts_secs;
        snap.last_send_ts_secs = self.last_send_ts_secs;
        snap.ping_recv_count = self.ping_recv_count;
        snap.pong_send_count = self.pong_send_count;
        snap.last_ping_recv_ts_secs = self.last_ping_recv_ts_secs;
        snap.client_keepalive_send_count = self.client_keepalive_send_count;
        snap.last_client_keepalive_send_ts_secs = self.last_client_keepalive_send_ts_secs;
        snap.writer_dropped_count = self.capture_dropped_count;
        snap.messages_recv_by_opcode = self.messages_recv_by_opcode.clone();
        snap.messages_send_by_opcode = self.messages_send_by_opcode.clone();
        snap.last_unknown_opcode = self.last_unknown_opcode;
        snap.unknown_opcode_count = self.unknown_opcode_count;
    }

    async fn send_key_modifiers(&mut self, modifiers: u16) -> Result<()> {
        let mut payload = Vec::new();
        InputsKeyModifiers { modifiers }.write(&mut payload)?;
        let msg = make_message(inputs_client::KEY_MODIFIERS, &payload);
        self.send_with_log(inputs_client::KEY_MODIFIERS, &msg)
            .await?;
        self.last_modifiers_sent = modifiers;
        Ok(())
    }

    /// Restate the last KEY_MODIFIERS we sent. Triggered by the
    /// idle-keepalive select branch in `run()` after
    /// `KEEPALIVE_IDLE` of channel silence. Re-sending the same
    /// modifier value is a no-op for the guest but generates a
    /// client→server byte on the inputs channel, which keeps the
    /// channel non-idle from the server's perspective.
    async fn send_idle_keepalive(&mut self) -> Result<()> {
        let modifiers = self.last_modifiers_sent;
        self.send_key_modifiers(modifiers).await?;
        self.client_keepalive_send_count = self.client_keepalive_send_count.saturating_add(1);
        // last_send_ts_secs is updated by send_with_log via
        // update_snapshot; mirror the fact here for the dedicated
        // keepalive timestamp.
        self.last_client_keepalive_send_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
        self.update_snapshot();
        Ok(())
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let msg_name = message_names::inputs_client(msg_type);
        if self.log_config.verbose || self.log_config.intimate {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "inputs", msg_type, msg_name, payload_size);
        }
        self.traffic.record_sent("inputs", msg_type, msg_name, data);
        // Increment per-opcode send counter here — single send path.
        *self.messages_send_by_opcode.entry(msg_type).or_insert(0) += 1;
        let result = self.send(data).await;
        self.update_snapshot();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            if !c.packet_sent("inputs", data) {
                self.capture_dropped_count = self.capture_dropped_count.saturating_add(1);
            }
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        self.last_send_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
        self.last_activity = tokio::time::Instant::now();
        Ok(())
    }

    /// Send a raw key-down without event recording or modifier tracking.
    async fn send_key_down(&mut self, scancode: u32) -> Result<()> {
        let mut payload = Vec::new();
        KeyEvent { scancode }.write(&mut payload)?;
        let msg = make_message(inputs_client::KEY_DOWN, &payload);
        self.send_with_log(inputs_client::KEY_DOWN, &msg).await
    }

    /// Send a raw key-up without event recording or modifier tracking.
    async fn send_key_up(&mut self, scancode: u32) -> Result<()> {
        let mut payload = Vec::new();
        KeyEvent { scancode }.write(&mut payload)?;
        let msg = make_message(inputs_client::KEY_UP, &payload);
        self.send_with_log(inputs_client::KEY_UP, &msg).await
    }

    /// Advance the paste state machine by one sub-step.
    ///
    /// Before advancing, checks whether the associated
    /// `CancellationToken` (if any) has been fired.  If it has, the
    /// paste is aborted and a `PasteFailed` event is emitted.  This
    /// lets a control-socket client disconnect abort an in-progress
    /// paste without leaving synthetic key events running.
    async fn advance_paste(&mut self) -> Result<()> {
        // Check for cancellation before doing any work.
        {
            let Some(state) = self.paste_state.as_ref() else {
                return Ok(());
            };
            if state.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                let request_id = state.request_id.clone();
                let reason = "paste cancelled (client disconnected)".to_string();
                info!("inputs: paste cancelled by cancellation token");
                self.paste_state = None;
                self.event_tx
                    .send(ChannelEvent::PasteFailed { reason, request_id })
                    .await
                    .ok();
                self.repaint_notify.notify_one();
                return Ok(());
            }
        }

        let Some(state) = self.paste_state.as_mut() else {
            return Ok(());
        };
        let key = state.keys[state.index];

        match state.sub_step {
            PasteSubStep::Press => {
                if key.shift {
                    self.send_key_down(0x2A).await?;
                }
                self.send_key_down(key.press).await?;
                let Some(state) = self.paste_state.as_mut() else {
                    return Ok(());
                };
                state.sub_step = PasteSubStep::Release;
                state.next_fire = Instant::now() + state.half_delay;
            }
            PasteSubStep::Release => {
                self.send_key_up(key.release).await?;
                if key.shift {
                    self.send_key_up(0xAA).await?;
                }

                let Some(state) = self.paste_state.as_mut() else {
                    return Ok(());
                };
                state.index += 1;

                if state.index >= state.keys.len() {
                    // Paste complete
                    let chars = state.keys.len();
                    let elapsed_ms = state.start.elapsed().as_millis() as u64;
                    let request_id = state.request_id.clone();

                    // Restore modifiers
                    let saved_ctrl = state.saved_ctrl;
                    let saved_shift = state.saved_shift;
                    let saved_alt = state.saved_alt;

                    self.paste_state = None;

                    if saved_ctrl {
                        self.send_key_down(0x1D).await?;
                        self.ctrl_held = true;
                    }
                    if saved_shift {
                        self.send_key_down(0x2A).await?;
                        self.shift_held = true;
                    }
                    if saved_alt {
                        self.send_key_down(0x38).await?;
                        self.alt_held = true;
                    }

                    info!(
                        "inputs: paste complete: {} chars in {}ms",
                        chars, elapsed_ms
                    );
                    self.event_tx
                        .send(ChannelEvent::PasteCompleted {
                            chars,
                            elapsed_ms,
                            request_id,
                        })
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                } else {
                    state.sub_step = PasteSubStep::Press;
                    state.next_fire = Instant::now() + state.half_delay;
                }
            }
        }

        Ok(())
    }
}

/// Arrow-key direction for `LogicalKey::Arrow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Navigation-cluster key for `LogicalKey::Navigation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavKey {
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
}

/// Whitespace-adjacent key for `LogicalKey::Whitespace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WSKey {
    Space,
    Tab,
    Enter,
    Backspace,
}

/// Punctuation key for `LogicalKey::Punctuation`.
///
/// Variant names match the egui naming convention used in the
/// original `key_to_scancode` map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PunctKey {
    Minus,
    Equals,
    OpenBracket,
    CloseBracket,
    Backslash,
    Semicolon,
    Quote,
    Backtick,
    Comma,
    Period,
    Slash,
}

/// A frontend-agnostic logical key identity.
///
/// The substrate uses this type as the pivot between the GUI adapter
/// (which maps egui / browser key events → `LogicalKey`) and the
/// scancode table (which maps `LogicalKey` → SPICE wire codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalKey {
    /// A letter key; the char is the uppercase form ('A'..='Z').
    Letter(char),
    /// A digit key (0..=9).
    Digit(u8),
    /// A function key (1..=12 for F1..=F12).
    Function(u8),
    /// An arrow key.
    Arrow(Direction),
    /// A navigation-cluster key.
    Navigation(NavKey),
    /// A whitespace-adjacent key.
    Whitespace(WSKey),
    /// A punctuation key.
    Punctuation(PunctKey),
    /// The Escape key.
    Escape,
}

/// AT keyboard scancode mapping — substrate-facing, egui-agnostic.
///
/// Maps `LogicalKey` values to SPICE/PCAT scancodes.  Keys that require
/// the E0 extended prefix (arrow keys, navigation cluster) use the
/// 0x1xx convention: the low byte is the base scancode and bit 8
/// signals "extended".  `make_scancode()` encodes this for the wire.
///
/// Returns `(press_code, release_code)` ready for the wire, or `None`
/// if the key is not in the table (should not happen for well-formed
/// `LogicalKey` values, but is guarded for forward compatibility).
pub fn scancode_for_logical_key(key: LogicalKey) -> Option<(u32, u32)> {
    let base: u32 = match key {
        // Letters
        LogicalKey::Letter('A') => 0x1E,
        LogicalKey::Letter('B') => 0x30,
        LogicalKey::Letter('C') => 0x2E,
        LogicalKey::Letter('D') => 0x20,
        LogicalKey::Letter('E') => 0x12,
        LogicalKey::Letter('F') => 0x21,
        LogicalKey::Letter('G') => 0x22,
        LogicalKey::Letter('H') => 0x23,
        LogicalKey::Letter('I') => 0x17,
        LogicalKey::Letter('J') => 0x24,
        LogicalKey::Letter('K') => 0x25,
        LogicalKey::Letter('L') => 0x26,
        LogicalKey::Letter('M') => 0x32,
        LogicalKey::Letter('N') => 0x31,
        LogicalKey::Letter('O') => 0x18,
        LogicalKey::Letter('P') => 0x19,
        LogicalKey::Letter('Q') => 0x10,
        LogicalKey::Letter('R') => 0x13,
        LogicalKey::Letter('S') => 0x1F,
        LogicalKey::Letter('T') => 0x14,
        LogicalKey::Letter('U') => 0x16,
        LogicalKey::Letter('V') => 0x2F,
        LogicalKey::Letter('W') => 0x11,
        LogicalKey::Letter('X') => 0x2D,
        LogicalKey::Letter('Y') => 0x15,
        LogicalKey::Letter('Z') => 0x2C,

        // Digits
        LogicalKey::Digit(0) => 0x0B,
        LogicalKey::Digit(1) => 0x02,
        LogicalKey::Digit(2) => 0x03,
        LogicalKey::Digit(3) => 0x04,
        LogicalKey::Digit(4) => 0x05,
        LogicalKey::Digit(5) => 0x06,
        LogicalKey::Digit(6) => 0x07,
        LogicalKey::Digit(7) => 0x08,
        LogicalKey::Digit(8) => 0x09,
        LogicalKey::Digit(9) => 0x0A,

        // Function keys
        LogicalKey::Function(1) => 0x3B,
        LogicalKey::Function(2) => 0x3C,
        LogicalKey::Function(3) => 0x3D,
        LogicalKey::Function(4) => 0x3E,
        LogicalKey::Function(5) => 0x3F,
        LogicalKey::Function(6) => 0x40,
        LogicalKey::Function(7) => 0x41,
        LogicalKey::Function(8) => 0x42,
        LogicalKey::Function(9) => 0x43,
        LogicalKey::Function(10) => 0x44,
        LogicalKey::Function(11) => 0x57,
        LogicalKey::Function(12) => 0x58,

        // Whitespace-adjacent
        LogicalKey::Whitespace(WSKey::Space) => 0x39,
        LogicalKey::Whitespace(WSKey::Enter) => 0x1C,
        LogicalKey::Whitespace(WSKey::Backspace) => 0x0E,
        LogicalKey::Whitespace(WSKey::Tab) => 0x0F,

        // Escape
        LogicalKey::Escape => 0x01,

        // Navigation cluster (extended keys, 0x1xx)
        LogicalKey::Navigation(NavKey::Delete) => 0x153,
        LogicalKey::Navigation(NavKey::Insert) => 0x152,
        LogicalKey::Navigation(NavKey::Home) => 0x147,
        LogicalKey::Navigation(NavKey::End) => 0x14F,
        LogicalKey::Navigation(NavKey::PageUp) => 0x149,
        LogicalKey::Navigation(NavKey::PageDown) => 0x151,

        // Arrow keys (extended keys, 0x1xx)
        LogicalKey::Arrow(Direction::Up) => 0x148,
        LogicalKey::Arrow(Direction::Down) => 0x150,
        LogicalKey::Arrow(Direction::Left) => 0x14B,
        LogicalKey::Arrow(Direction::Right) => 0x14D,

        // Punctuation
        LogicalKey::Punctuation(PunctKey::Minus) => 0x0C,
        LogicalKey::Punctuation(PunctKey::Equals) => 0x0D,
        LogicalKey::Punctuation(PunctKey::OpenBracket) => 0x1A,
        LogicalKey::Punctuation(PunctKey::CloseBracket) => 0x1B,
        LogicalKey::Punctuation(PunctKey::Backslash) => 0x2B,
        LogicalKey::Punctuation(PunctKey::Semicolon) => 0x27,
        LogicalKey::Punctuation(PunctKey::Quote) => 0x28,
        LogicalKey::Punctuation(PunctKey::Backtick) => 0x29,
        LogicalKey::Punctuation(PunctKey::Comma) => 0x33,
        LogicalKey::Punctuation(PunctKey::Period) => 0x34,
        LogicalKey::Punctuation(PunctKey::Slash) => 0x35,

        // Catch-all for invalid/future variants
        _ => return None,
    };
    Some((make_scancode(base, false), make_scancode(base, true)))
}

/// Encode a SPICE scancode for the wire.
///
/// Normal keys use a single-byte scancode in the low byte of the u32.
/// Extended keys (E0-prefixed on the AT keyboard) are encoded as two
/// bytes: the E0 prefix in the low byte, the scancode in the next byte.
/// This matches spice-gtk's `spice_make_scancode()`.
fn make_scancode(base: u32, release: bool) -> u32 {
    let code = if release { base | 0x80 } else { base };
    if base >= 0x100 {
        // Extended key: wire bytes are [0xE0, scancode] in LE u32
        let sc = code & 0xFF;
        (sc << 8) | 0xE0
    } else {
        code
    }
}

/// A single key event in a paste-as-keystrokes sequence.
///
/// `press` and `release` are SPICE wire-format scancodes
/// (output of `make_scancode`). `shift` indicates whether
/// Left Shift must be held for this character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteKey {
    pub press: u32,
    pub release: u32,
    pub shift: bool,
}

/// Errors from paste text translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteError {
    /// The input contains characters that cannot be represented as US-QWERTY scancodes.
    Unrepresentable {
        /// Number of distinct unrepresentable codepoints.
        count: usize,
        /// Up to three sample characters for diagnostics.
        sample: Vec<char>,
    },
}

/// Map a character to its US-QWERTY AT scancode.
///
/// Returns `(base_scancode, needs_shift)` or `None` if the character has no US-QWERTY
/// representation.
fn char_to_scancode(c: char) -> Option<(u32, bool)> {
    match c {
        // Lowercase letters (unshifted)
        'a' => Some((0x1E, false)),
        'b' => Some((0x30, false)),
        'c' => Some((0x2E, false)),
        'd' => Some((0x20, false)),
        'e' => Some((0x12, false)),
        'f' => Some((0x21, false)),
        'g' => Some((0x22, false)),
        'h' => Some((0x23, false)),
        'i' => Some((0x17, false)),
        'j' => Some((0x24, false)),
        'k' => Some((0x25, false)),
        'l' => Some((0x26, false)),
        'm' => Some((0x32, false)),
        'n' => Some((0x31, false)),
        'o' => Some((0x18, false)),
        'p' => Some((0x19, false)),
        'q' => Some((0x10, false)),
        'r' => Some((0x13, false)),
        's' => Some((0x1F, false)),
        't' => Some((0x14, false)),
        'u' => Some((0x16, false)),
        'v' => Some((0x2F, false)),
        'w' => Some((0x11, false)),
        'x' => Some((0x2D, false)),
        'y' => Some((0x15, false)),
        'z' => Some((0x2C, false)),

        // Uppercase letters (shifted)
        'A' => Some((0x1E, true)),
        'B' => Some((0x30, true)),
        'C' => Some((0x2E, true)),
        'D' => Some((0x20, true)),
        'E' => Some((0x12, true)),
        'F' => Some((0x21, true)),
        'G' => Some((0x22, true)),
        'H' => Some((0x23, true)),
        'I' => Some((0x17, true)),
        'J' => Some((0x24, true)),
        'K' => Some((0x25, true)),
        'L' => Some((0x26, true)),
        'M' => Some((0x32, true)),
        'N' => Some((0x31, true)),
        'O' => Some((0x18, true)),
        'P' => Some((0x19, true)),
        'Q' => Some((0x10, true)),
        'R' => Some((0x13, true)),
        'S' => Some((0x1F, true)),
        'T' => Some((0x14, true)),
        'U' => Some((0x16, true)),
        'V' => Some((0x2F, true)),
        'W' => Some((0x11, true)),
        'X' => Some((0x2D, true)),
        'Y' => Some((0x15, true)),
        'Z' => Some((0x2C, true)),

        // Digits (unshifted)
        '0' => Some((0x0B, false)),
        '1' => Some((0x02, false)),
        '2' => Some((0x03, false)),
        '3' => Some((0x04, false)),
        '4' => Some((0x05, false)),
        '5' => Some((0x06, false)),
        '6' => Some((0x07, false)),
        '7' => Some((0x08, false)),
        '8' => Some((0x09, false)),
        '9' => Some((0x0A, false)),

        // Shifted digit-row symbols
        '!' => Some((0x02, true)),
        '@' => Some((0x03, true)),
        '#' => Some((0x04, true)),
        '$' => Some((0x05, true)),
        '%' => Some((0x06, true)),
        '^' => Some((0x07, true)),
        '&' => Some((0x08, true)),
        '*' => Some((0x09, true)),
        '(' => Some((0x0A, true)),
        ')' => Some((0x0B, true)),

        // Unshifted punctuation
        '-' => Some((0x0C, false)),
        '=' => Some((0x0D, false)),
        '[' => Some((0x1A, false)),
        ']' => Some((0x1B, false)),
        '\\' => Some((0x2B, false)),
        ';' => Some((0x27, false)),
        '\'' => Some((0x28, false)),
        '`' => Some((0x29, false)),
        ',' => Some((0x33, false)),
        '.' => Some((0x34, false)),
        '/' => Some((0x35, false)),

        // Shifted punctuation
        '_' => Some((0x0C, true)),
        '+' => Some((0x0D, true)),
        '{' => Some((0x1A, true)),
        '}' => Some((0x1B, true)),
        '|' => Some((0x2B, true)),
        ':' => Some((0x27, true)),
        '"' => Some((0x28, true)),
        '~' => Some((0x29, true)),
        '<' => Some((0x33, true)),
        '>' => Some((0x34, true)),
        '?' => Some((0x35, true)),

        // Whitespace
        ' ' => Some((0x39, false)),
        '\t' => Some((0x0F, false)),
        '\n' => Some((0x1C, false)),

        _ => None,
    }
}

/// Translate a string into a sequence of scancode triples for typing on a US-QWERTY keyboard.
///
/// Returns one `PasteKey` per typeable character. CRLF sequences are collapsed to a single Enter.
/// Bare `\r` is also treated as Enter. The input is pre-validated: if any character cannot be
/// mapped, nothing is returned and the error reports which characters failed.
///
/// This function does not enforce the 4096-character cap (that is the caller's responsibility).
pub fn translate_paste(text: &str) -> Result<Vec<PasteKey>, PasteError> {
    // Pre-validation pass: collect unrepresentable characters.
    let mut bad_chars: Vec<char> = Vec::new();
    for c in text.chars() {
        if c == '\r' {
            continue; // handled by CRLF collapsing
        }
        if char_to_scancode(c).is_none() && !bad_chars.contains(&c) {
            bad_chars.push(c);
        }
    }
    if !bad_chars.is_empty() {
        let count = bad_chars.len();
        let sample = bad_chars.into_iter().take(3).collect();
        return Err(PasteError::Unrepresentable { count, sample });
    }

    // Translation pass: convert characters to PasteKey triples.
    let mut keys = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let effective = if c == '\r' {
            // Collapse \r\n to single \n; bare \r becomes \n.
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            '\n'
        } else {
            c
        };

        let (base, shift) = char_to_scancode(effective)
            .expect("pre-validation guarantees all characters are representable");
        keys.push(PasteKey {
            press: make_scancode(base, false),
            release: make_scancode(base, true),
            shift,
        });
    }

    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::{
        scancode_for_logical_key, translate_paste, Direction, LogicalKey, MotionWedgeDetector,
        PasteError, PasteKey, WSKey, MOTION_WEDGE_THRESHOLD,
    };

    /// A burst that clears is not a wedge, and must say nothing.
    #[test]
    fn a_short_run_of_drops_does_not_warn() {
        let mut d = MotionWedgeDetector::default();
        for _ in 0..(MOTION_WEDGE_THRESHOLD - 1) {
            assert!(!d.note_drop(), "warned before the threshold");
        }
        d.note_ack();
        assert_eq!(d.drops(), 0, "an ack should clear the run");
    }

    /// Past the threshold, warn — but only once, or a wedged session
    /// emits a line per pointer event for as long as it lasts.
    #[test]
    fn a_wedged_window_warns_exactly_once() {
        let mut d = MotionWedgeDetector::default();
        let warnings = (0..(MOTION_WEDGE_THRESHOLD * 3))
            .filter(|_| d.note_drop())
            .count();
        assert_eq!(warnings, 1, "expected one warning for one wedge");
    }

    /// A session that wedges, recovers and wedges again is two
    /// faults. Reporting only the first hides the one the user is
    /// looking at, so an ack re-arms the warning.
    #[test]
    fn a_second_wedge_after_recovery_warns_again() {
        let mut d = MotionWedgeDetector::default();
        let first = (0..MOTION_WEDGE_THRESHOLD)
            .filter(|_| d.note_drop())
            .count();
        assert_eq!(first, 1);

        d.note_ack();

        let second = (0..MOTION_WEDGE_THRESHOLD)
            .filter(|_| d.note_drop())
            .count();
        assert_eq!(second, 1, "the second wedge went unreported");
    }

    // make_scancode logic for reference:
    //   normal key:   press = base,            release = base | 0x80
    //   extended key: press = (base & 0xFF) << 8 | 0xE0,
    //                 release = ((base | 0x80) & 0xFF) << 8 | 0xE0

    #[test]
    fn test_key_a_letter() {
        // Letter 'A' → base 0x1E (normal key)
        let result = scancode_for_logical_key(LogicalKey::Letter('A'));
        assert_eq!(result, Some((0x1E, 0x9E)));
    }

    #[test]
    fn test_key_num0_digit() {
        // Digit 0 → base 0x0B (normal key)
        let result = scancode_for_logical_key(LogicalKey::Digit(0));
        assert_eq!(result, Some((0x0B, 0x8B)));
    }

    #[test]
    fn test_key_f1_function() {
        // Function 1 (F1) → base 0x3B (normal key)
        let result = scancode_for_logical_key(LogicalKey::Function(1));
        assert_eq!(result, Some((0x3B, 0xBB)));
    }

    #[test]
    fn test_key_arrow_up_extended() {
        // Arrow::Up → base 0x148 (extended, E0-prefixed)
        // press:   (0x48 << 8) | 0xE0 = 0x48E0
        // release: (0xC8 << 8) | 0xE0 = 0xC8E0
        let result = scancode_for_logical_key(LogicalKey::Arrow(Direction::Up));
        assert_eq!(result, Some((0x48E0, 0xC8E0)));
    }

    #[test]
    fn test_key_space() {
        // Whitespace::Space → base 0x39 (normal key)
        let result = scancode_for_logical_key(LogicalKey::Whitespace(WSKey::Space));
        assert_eq!(result, Some((0x39, 0xB9)));
    }

    #[test]
    fn test_key_enter() {
        // Whitespace::Enter → base 0x1C (normal key)
        let result = scancode_for_logical_key(LogicalKey::Whitespace(WSKey::Enter));
        assert_eq!(result, Some((0x1C, 0x9C)));
    }

    #[test]
    fn test_key_escape() {
        // Escape → base 0x01 (normal key)
        let result = scancode_for_logical_key(LogicalKey::Escape);
        assert_eq!(result, Some((0x01, 0x81)));
    }

    #[test]
    fn test_unmapped_key_returns_none() {
        // Function(13) is not in the table (only F1–F12 are mapped)
        let result = scancode_for_logical_key(LogicalKey::Function(13));
        assert_eq!(result, None);
    }

    #[test]
    fn paste_empty_string() {
        let result = translate_paste("");
        assert_eq!(result, Ok(vec![]));
    }

    #[test]
    fn paste_lowercase_letters() {
        let result = translate_paste("abc").unwrap();
        assert_eq!(result.len(), 3);
        // a: base 0x1E
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x1E,
                release: 0x9E,
                shift: false
            }
        );
        // b: base 0x30
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x30,
                release: 0xB0,
                shift: false
            }
        );
        // c: base 0x2E
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x2E,
                release: 0xAE,
                shift: false
            }
        );
    }

    #[test]
    fn paste_uppercase_letters() {
        let result = translate_paste("ABC").unwrap();
        assert_eq!(result.len(), 3);
        // A: base 0x1E (same as 'a'), shifted
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x1E,
                release: 0x9E,
                shift: true
            }
        );
        // B: base 0x30 (same as 'b'), shifted
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x30,
                release: 0xB0,
                shift: true
            }
        );
        // C: base 0x2E (same as 'c'), shifted
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x2E,
                release: 0xAE,
                shift: true
            }
        );
    }

    #[test]
    fn paste_mixed_case() {
        let result = translate_paste("Hello").unwrap();
        assert_eq!(result.len(), 5);
        // H: base 0x23, shifted
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x23,
                release: 0xA3,
                shift: true
            }
        );
        // e: base 0x12, unshifted
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x12,
                release: 0x92,
                shift: false
            }
        );
        // l: base 0x26, unshifted
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x26,
                release: 0xA6,
                shift: false
            }
        );
        assert_eq!(
            result[3],
            PasteKey {
                press: 0x26,
                release: 0xA6,
                shift: false
            }
        );
        // o: base 0x18, unshifted
        assert_eq!(
            result[4],
            PasteKey {
                press: 0x18,
                release: 0x98,
                shift: false
            }
        );
    }

    #[test]
    fn paste_digits() {
        let result = translate_paste("0123456789").unwrap();
        assert_eq!(result.len(), 10);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x0B,
                release: 0x8B,
                shift: false
            }
        ); // 0
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x02,
                release: 0x82,
                shift: false
            }
        ); // 1
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x03,
                release: 0x83,
                shift: false
            }
        ); // 2
        assert_eq!(
            result[3],
            PasteKey {
                press: 0x04,
                release: 0x84,
                shift: false
            }
        ); // 3
        assert_eq!(
            result[4],
            PasteKey {
                press: 0x05,
                release: 0x85,
                shift: false
            }
        ); // 4
        assert_eq!(
            result[5],
            PasteKey {
                press: 0x06,
                release: 0x86,
                shift: false
            }
        ); // 5
        assert_eq!(
            result[6],
            PasteKey {
                press: 0x07,
                release: 0x87,
                shift: false
            }
        ); // 6
        assert_eq!(
            result[7],
            PasteKey {
                press: 0x08,
                release: 0x88,
                shift: false
            }
        ); // 7
        assert_eq!(
            result[8],
            PasteKey {
                press: 0x09,
                release: 0x89,
                shift: false
            }
        ); // 8
        assert_eq!(
            result[9],
            PasteKey {
                press: 0x0A,
                release: 0x8A,
                shift: false
            }
        ); // 9
    }

    #[test]
    fn paste_shifted_digit_symbols() {
        let result = translate_paste("!@#$%^&*()").unwrap();
        assert_eq!(result.len(), 10);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x02,
                release: 0x82,
                shift: true
            }
        ); // ! (1)
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x03,
                release: 0x83,
                shift: true
            }
        ); // @ (2)
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x04,
                release: 0x84,
                shift: true
            }
        ); // # (3)
        assert_eq!(
            result[3],
            PasteKey {
                press: 0x05,
                release: 0x85,
                shift: true
            }
        ); // $ (4)
        assert_eq!(
            result[4],
            PasteKey {
                press: 0x06,
                release: 0x86,
                shift: true
            }
        ); // % (5)
        assert_eq!(
            result[5],
            PasteKey {
                press: 0x07,
                release: 0x87,
                shift: true
            }
        ); // ^ (6)
        assert_eq!(
            result[6],
            PasteKey {
                press: 0x08,
                release: 0x88,
                shift: true
            }
        ); // & (7)
        assert_eq!(
            result[7],
            PasteKey {
                press: 0x09,
                release: 0x89,
                shift: true
            }
        ); // * (8)
        assert_eq!(
            result[8],
            PasteKey {
                press: 0x0A,
                release: 0x8A,
                shift: true
            }
        ); // ( (9)
        assert_eq!(
            result[9],
            PasteKey {
                press: 0x0B,
                release: 0x8B,
                shift: true
            }
        ); // ) (0)
    }

    #[test]
    fn paste_unshifted_punctuation() {
        let result = translate_paste("-=[]\\;',./ ").unwrap();
        assert_eq!(result.len(), 11);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x0C,
                release: 0x8C,
                shift: false
            }
        ); // -
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x0D,
                release: 0x8D,
                shift: false
            }
        ); // =
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x1A,
                release: 0x9A,
                shift: false
            }
        ); // [
        assert_eq!(
            result[3],
            PasteKey {
                press: 0x1B,
                release: 0x9B,
                shift: false
            }
        ); // ]
        assert_eq!(
            result[4],
            PasteKey {
                press: 0x2B,
                release: 0xAB,
                shift: false
            }
        ); // \
        assert_eq!(
            result[5],
            PasteKey {
                press: 0x27,
                release: 0xA7,
                shift: false
            }
        ); // ;
        assert_eq!(
            result[6],
            PasteKey {
                press: 0x28,
                release: 0xA8,
                shift: false
            }
        ); // '
        assert_eq!(
            result[7],
            PasteKey {
                press: 0x33,
                release: 0xB3,
                shift: false
            }
        ); // ,
        assert_eq!(
            result[8],
            PasteKey {
                press: 0x34,
                release: 0xB4,
                shift: false
            }
        ); // .
        assert_eq!(
            result[9],
            PasteKey {
                press: 0x35,
                release: 0xB5,
                shift: false
            }
        ); // /
        assert_eq!(
            result[10],
            PasteKey {
                press: 0x39,
                release: 0xB9,
                shift: false
            }
        ); // space (added for test)
    }

    #[test]
    fn paste_shifted_punctuation() {
        let result = translate_paste("_+{}|:\"<>?").unwrap();
        assert_eq!(result.len(), 10);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x0C,
                release: 0x8C,
                shift: true
            }
        ); // _
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x0D,
                release: 0x8D,
                shift: true
            }
        ); // +
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x1A,
                release: 0x9A,
                shift: true
            }
        ); // {
        assert_eq!(
            result[3],
            PasteKey {
                press: 0x1B,
                release: 0x9B,
                shift: true
            }
        ); // }
        assert_eq!(
            result[4],
            PasteKey {
                press: 0x2B,
                release: 0xAB,
                shift: true
            }
        ); // |
        assert_eq!(
            result[5],
            PasteKey {
                press: 0x27,
                release: 0xA7,
                shift: true
            }
        ); // :
        assert_eq!(
            result[6],
            PasteKey {
                press: 0x28,
                release: 0xA8,
                shift: true
            }
        ); // "
        assert_eq!(
            result[7],
            PasteKey {
                press: 0x33,
                release: 0xB3,
                shift: true
            }
        ); // <
        assert_eq!(
            result[8],
            PasteKey {
                press: 0x34,
                release: 0xB4,
                shift: true
            }
        ); // >
        assert_eq!(
            result[9],
            PasteKey {
                press: 0x35,
                release: 0xB5,
                shift: true
            }
        ); // ?
    }

    #[test]
    fn paste_backtick_and_tilde() {
        // Backtick unshifted
        let result = translate_paste("`").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x29,
                release: 0xA9,
                shift: false
            }
        );

        // Tilde shifted
        let result = translate_paste("~").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x29,
                release: 0xA9,
                shift: true
            }
        );
    }

    #[test]
    fn paste_whitespace() {
        let result = translate_paste("a b").unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x1E,
                release: 0x9E,
                shift: false
            }
        ); // a
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x39,
                release: 0xB9,
                shift: false
            }
        ); // space
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x30,
                release: 0xB0,
                shift: false
            }
        ); // b
    }

    #[test]
    fn paste_tab() {
        let result = translate_paste("a\tb").unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x1E,
                release: 0x9E,
                shift: false
            }
        ); // a
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x0F,
                release: 0x8F,
                shift: false
            }
        ); // tab
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x30,
                release: 0xB0,
                shift: false
            }
        ); // b
    }

    #[test]
    fn paste_newline() {
        let result = translate_paste("a\nb").unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x1E,
                release: 0x9E,
                shift: false
            }
        ); // a
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x1C,
                release: 0x9C,
                shift: false
            }
        ); // enter
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x30,
                release: 0xB0,
                shift: false
            }
        ); // b
    }

    #[test]
    fn paste_crlf_collapsed() {
        let result = translate_paste("a\r\nb").unwrap();
        assert_eq!(result.len(), 3); // Not 4 — CRLF collapsed to single Enter
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x1E,
                release: 0x9E,
                shift: false
            }
        ); // a
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x1C,
                release: 0x9C,
                shift: false
            }
        ); // enter
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x30,
                release: 0xB0,
                shift: false
            }
        ); // b
    }

    #[test]
    fn paste_bare_cr() {
        let result = translate_paste("a\rb").unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(
            result[0],
            PasteKey {
                press: 0x1E,
                release: 0x9E,
                shift: false
            }
        ); // a
        assert_eq!(
            result[1],
            PasteKey {
                press: 0x1C,
                release: 0x9C,
                shift: false
            }
        ); // enter
        assert_eq!(
            result[2],
            PasteKey {
                press: 0x30,
                release: 0xB0,
                shift: false
            }
        ); // b
    }

    #[test]
    fn paste_non_ascii_rejected() {
        let result = translate_paste("café");
        assert!(result.is_err());
        match result {
            Err(PasteError::Unrepresentable { count, sample }) => {
                assert_eq!(count, 1);
                assert_eq!(sample, vec!['é']);
            }
            _ => panic!("Expected Unrepresentable error"),
        }
    }

    #[test]
    fn paste_multiple_non_ascii() {
        let result = translate_paste("αβγδ hello");
        assert!(result.is_err());
        match result {
            Err(PasteError::Unrepresentable { count, sample }) => {
                assert_eq!(count, 4);
                assert_eq!(sample, vec!['α', 'β', 'γ']);
            }
            _ => panic!("Expected Unrepresentable error"),
        }
    }

    #[test]
    fn paste_all_printable_ascii() {
        for code in 0x20u8..=0x7Eu8 {
            let c = code as char;
            let s = c.to_string();
            let result = translate_paste(&s);
            assert!(
                result.is_ok(),
                "Character '{}' (0x{:02X}) should be translatable",
                c,
                code
            );
            assert_eq!(
                result.unwrap().len(),
                1,
                "Character '{}' should produce exactly one PasteKey",
                c
            );
        }
    }

    #[test]
    fn paste_scancode_values_match_scancode_for_logical_key() {
        // For representative characters, verify that translate_paste produces the same
        // press/release codes as scancode_for_logical_key for the corresponding LogicalKey.

        // 'a' -> LogicalKey::Letter('A')
        let paste_result = translate_paste("a").unwrap();
        let key_result = scancode_for_logical_key(LogicalKey::Letter('A')).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // '1' -> LogicalKey::Digit(1)
        let paste_result = translate_paste("1").unwrap();
        let key_result = scancode_for_logical_key(LogicalKey::Digit(1)).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // ' ' -> LogicalKey::Whitespace(WSKey::Space)
        let paste_result = translate_paste(" ").unwrap();
        let key_result = scancode_for_logical_key(LogicalKey::Whitespace(WSKey::Space)).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // '\t' -> LogicalKey::Whitespace(WSKey::Tab)
        let paste_result = translate_paste("\t").unwrap();
        let key_result = scancode_for_logical_key(LogicalKey::Whitespace(WSKey::Tab)).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // '\n' -> LogicalKey::Whitespace(WSKey::Enter)
        let paste_result = translate_paste("\n").unwrap();
        let key_result = scancode_for_logical_key(LogicalKey::Whitespace(WSKey::Enter)).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);
    }
}
