/// Inputs channel handler - keyboard and mouse input
use anyhow::Result;
use eframe::egui;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, error, info, warn};

use crate::app::ByteCounter;
use crate::bugreport::{InputEventRecord, InputsSnapshot, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::notifications::{NotificationEntry, NotificationSource, SharedNotifications};
use crate::settings;
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
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    notifications: SharedNotifications,
    snapshot: Arc<Mutex<InputsSnapshot>>,
    recent_events: VecDeque<InputEventRecord>,
    bytes_in: u64,
    bytes_out: u64,
    enable_paste: bool,
    ctrl_held: bool,
    shift_held: bool,
    alt_held: bool,
    paste_state: Option<PasteState>,
}

impl InputsChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<Notify>,
        input_rx: mpsc::Receiver<InputEvent>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<TrafficBuffers>,
        snapshot: Arc<Mutex<InputsSnapshot>>,
        enable_paste: bool,
        notifications: SharedNotifications,
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
            capture,
            byte_counter,
            traffic,
            notifications,
            snapshot,
            recent_events: VecDeque::new(),
            bytes_in: 0,
            bytes_out: 0,
            enable_paste,
            ctrl_held: false,
            shift_held: false,
            alt_held: false,
            paste_state: None,
        }
    }

    /// Run the inputs channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("inputs: channel started");

        // Send initial key modifiers (NumLock on)
        self.send_key_modifiers(keyboard_modifiers::NUM_LOCK)
            .await?;

        loop {
            // Borrow fields separately to avoid borrow checker issues in select!
            let stream = &mut self.stream;
            let buffer = &mut self.buffer;
            let bytes_in = &mut self.bytes_in;
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
                };
                if n > 0 {
                    byte_counter.add(n as u64);
                    if let Some(ref c) = capture {
                        c.packet_received("inputs", &chunk[..n]);
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
                            self.process_messages().await?;
                        }
                        Err(e) => {
                            self.event_tx
                                .send(ChannelEvent::Error(format!("inputs: read error: {}", e)))
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
        if settings::is_verbose() {
            logging::log_message(
                "received",
                "inputs",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        match msg_type {
            inputs_server::INIT => {
                debug!("inputs: init received");
            }

            inputs_server::KEY_MODIFIERS => {
                if payload.len() >= 2 {
                    let modifiers = u16::from_le_bytes([payload[0], payload[1]]);

                    if settings::is_verbose() {
                        logging::log_detail(&format!("modifiers={:#x}", modifiers));
                    } else {
                        debug!("inputs: key modifiers from server: {:#x}", modifiers);
                    }
                }
            }

            inputs_server::MOUSE_MOTION_ACK => {
                self.motion_count = self.motion_count.saturating_sub(MOTION_ACK_BUNCH);
                debug!("inputs: mouse motion ack (pending={})", self.motion_count);
            }

            inputs_server::SET_ACK => {
                let set_ack = SetAck::read(payload)?;

                if settings::is_verbose() {
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
                let ping = Ping::read(payload)?;

                if settings::is_verbose() {
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
            }

            inputs_server::NOTIFY => {
                let notify = NotifyMessage::read(payload)?;
                if settings::is_verbose() {
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
                self.notifications
                    .lock()
                    .expect("notifications lock poisoned")
                    .push(entry);
            }

            _ => {
                // Unknown opcode — log hex once per msg_type, silent on repeat.
                logging::log_unknown_once("inputs", msg_type, payload);
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
                            .send(ChannelEvent::PasteFailed { reason })
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
                });
            }
        }

        Ok(())
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
        let mut snap = self.snapshot.lock().unwrap();
        snap.button_state = self.button_state;
        snap.motion_count = self.motion_count;
        snap.secs_since_last_key = self.last_key_time.map(|t| t.elapsed().as_secs_f64());
        snap.recent_events = self.recent_events.clone();
        snap.bytes_in = self.bytes_in;
        snap.bytes_out = self.bytes_out;
    }

    async fn send_key_modifiers(&mut self, modifiers: u16) -> Result<()> {
        let mut payload = Vec::new();
        InputsKeyModifiers { modifiers }.write(&mut payload)?;
        let msg = make_message(inputs_client::KEY_MODIFIERS, &payload);
        self.send_with_log(inputs_client::KEY_MODIFIERS, &msg).await
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let msg_name = message_names::inputs_client(msg_type);
        if settings::is_verbose() || settings::is_intimate() {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "inputs", msg_type, msg_name, payload_size);
        }
        self.traffic.record_sent("inputs", msg_type, msg_name, data);
        let result = self.send(data).await;
        self.update_snapshot();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            c.packet_sent("inputs", data);
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
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
    async fn advance_paste(&mut self) -> Result<()> {
        let state = self.paste_state.as_mut().unwrap();
        let key = state.keys[state.index];

        match state.sub_step {
            PasteSubStep::Press => {
                if key.shift {
                    self.send_key_down(0x2A).await?;
                }
                self.send_key_down(key.press).await?;
                let state = self.paste_state.as_mut().unwrap();
                state.sub_step = PasteSubStep::Release;
                state.next_fire = Instant::now() + state.half_delay;
            }
            PasteSubStep::Release => {
                self.send_key_up(key.release).await?;
                if key.shift {
                    self.send_key_up(0xAA).await?;
                }

                let state = self.paste_state.as_mut().unwrap();
                state.index += 1;

                if state.index >= state.keys.len() {
                    // Paste complete
                    let chars = state.keys.len();
                    let elapsed_ms = state.start.elapsed().as_millis() as u64;

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
                        .send(ChannelEvent::PasteCompleted { chars, elapsed_ms })
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

/// AT keyboard scancode mapping.
///
/// Maps egui key codes to SPICE/PCAT scancodes.  Keys that require
/// the E0 extended prefix (arrow keys, navigation cluster) use the
/// 0x1xx convention: the low byte is the base scancode and bit 8
/// signals "extended".  `make_scancode()` encodes this for the wire.
pub fn key_to_scancode(key: egui::Key) -> Option<(u32, u32)> {
    // Returns (press_code, release_code) ready for the wire
    static SCANCODE_MAP: std::sync::LazyLock<HashMap<egui::Key, u32>> =
        std::sync::LazyLock::new(|| {
            let mut m = HashMap::new();

            // Letters
            m.insert(egui::Key::A, 0x1E);
            m.insert(egui::Key::B, 0x30);
            m.insert(egui::Key::C, 0x2E);
            m.insert(egui::Key::D, 0x20);
            m.insert(egui::Key::E, 0x12);
            m.insert(egui::Key::F, 0x21);
            m.insert(egui::Key::G, 0x22);
            m.insert(egui::Key::H, 0x23);
            m.insert(egui::Key::I, 0x17);
            m.insert(egui::Key::J, 0x24);
            m.insert(egui::Key::K, 0x25);
            m.insert(egui::Key::L, 0x26);
            m.insert(egui::Key::M, 0x32);
            m.insert(egui::Key::N, 0x31);
            m.insert(egui::Key::O, 0x18);
            m.insert(egui::Key::P, 0x19);
            m.insert(egui::Key::Q, 0x10);
            m.insert(egui::Key::R, 0x13);
            m.insert(egui::Key::S, 0x1F);
            m.insert(egui::Key::T, 0x14);
            m.insert(egui::Key::U, 0x16);
            m.insert(egui::Key::V, 0x2F);
            m.insert(egui::Key::W, 0x11);
            m.insert(egui::Key::X, 0x2D);
            m.insert(egui::Key::Y, 0x15);
            m.insert(egui::Key::Z, 0x2C);

            // Numbers
            m.insert(egui::Key::Num0, 0x0B);
            m.insert(egui::Key::Num1, 0x02);
            m.insert(egui::Key::Num2, 0x03);
            m.insert(egui::Key::Num3, 0x04);
            m.insert(egui::Key::Num4, 0x05);
            m.insert(egui::Key::Num5, 0x06);
            m.insert(egui::Key::Num6, 0x07);
            m.insert(egui::Key::Num7, 0x08);
            m.insert(egui::Key::Num8, 0x09);
            m.insert(egui::Key::Num9, 0x0A);

            // Function keys
            m.insert(egui::Key::F1, 0x3B);
            m.insert(egui::Key::F2, 0x3C);
            m.insert(egui::Key::F3, 0x3D);
            m.insert(egui::Key::F4, 0x3E);
            m.insert(egui::Key::F5, 0x3F);
            m.insert(egui::Key::F6, 0x40);
            m.insert(egui::Key::F7, 0x41);
            m.insert(egui::Key::F8, 0x42);
            m.insert(egui::Key::F9, 0x43);
            m.insert(egui::Key::F10, 0x44);
            m.insert(egui::Key::F11, 0x57);
            m.insert(egui::Key::F12, 0x58);

            // Special keys
            m.insert(egui::Key::Space, 0x39);
            m.insert(egui::Key::Enter, 0x1C);
            m.insert(egui::Key::Escape, 0x01);
            m.insert(egui::Key::Backspace, 0x0E);
            m.insert(egui::Key::Tab, 0x0F);

            // Navigation cluster — extended keys (E0 prefix, 0x1xx)
            m.insert(egui::Key::Delete, 0x153);
            m.insert(egui::Key::Insert, 0x152);
            m.insert(egui::Key::Home, 0x147);
            m.insert(egui::Key::End, 0x14F);
            m.insert(egui::Key::PageUp, 0x149);
            m.insert(egui::Key::PageDown, 0x151);

            // Arrow keys — extended keys (E0 prefix, 0x1xx)
            m.insert(egui::Key::ArrowUp, 0x148);
            m.insert(egui::Key::ArrowDown, 0x150);
            m.insert(egui::Key::ArrowLeft, 0x14B);
            m.insert(egui::Key::ArrowRight, 0x14D);

            // Punctuation
            m.insert(egui::Key::Minus, 0x0C);
            m.insert(egui::Key::Equals, 0x0D);
            m.insert(egui::Key::OpenBracket, 0x1A);
            m.insert(egui::Key::CloseBracket, 0x1B);
            m.insert(egui::Key::Backslash, 0x2B);
            m.insert(egui::Key::Semicolon, 0x27);
            m.insert(egui::Key::Quote, 0x28);
            m.insert(egui::Key::Backtick, 0x29);
            m.insert(egui::Key::Comma, 0x33);
            m.insert(egui::Key::Period, 0x34);
            m.insert(egui::Key::Slash, 0x35);

            m
        });

    SCANCODE_MAP
        .get(&key)
        .map(|&code| (make_scancode(code, false), make_scancode(code, true)))
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

        // Safe to unwrap: pre-validation guarantees all characters are representable.
        let (base, shift) = char_to_scancode(effective).unwrap();
        keys.push(PasteKey {
            press: make_scancode(base, false),
            release: make_scancode(base, true),
            shift,
        });
    }

    Ok(keys)
}

/// Map mouse button to SPICE button flag
pub fn mouse_button_to_spice(button: egui::PointerButton) -> u32 {
    match button {
        egui::PointerButton::Primary => shakenfist_spice_protocol::mouse_buttons::LEFT,
        egui::PointerButton::Secondary => shakenfist_spice_protocol::mouse_buttons::RIGHT,
        egui::PointerButton::Middle => shakenfist_spice_protocol::mouse_buttons::MIDDLE,
        egui::PointerButton::Extra1 => shakenfist_spice_protocol::mouse_buttons::UP,
        egui::PointerButton::Extra2 => shakenfist_spice_protocol::mouse_buttons::DOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::{key_to_scancode, translate_paste, PasteError, PasteKey};
    use eframe::egui;

    // make_scancode logic for reference:
    //   normal key:   press = base,            release = base | 0x80
    //   extended key: press = (base & 0xFF) << 8 | 0xE0,
    //                 release = ((base | 0x80) & 0xFF) << 8 | 0xE0

    #[test]
    fn test_key_a_letter() {
        // Key::A → base 0x1E (normal key)
        let result = key_to_scancode(egui::Key::A);
        assert_eq!(result, Some((0x1E, 0x9E)));
    }

    #[test]
    fn test_key_num0_digit() {
        // Key::Num0 → base 0x0B (normal key)
        let result = key_to_scancode(egui::Key::Num0);
        assert_eq!(result, Some((0x0B, 0x8B)));
    }

    #[test]
    fn test_key_f1_function() {
        // Key::F1 → base 0x3B (normal key)
        let result = key_to_scancode(egui::Key::F1);
        assert_eq!(result, Some((0x3B, 0xBB)));
    }

    #[test]
    fn test_key_arrow_up_extended() {
        // Key::ArrowUp → base 0x148 (extended, E0-prefixed)
        // press:   (0x48 << 8) | 0xE0 = 0x48E0
        // release: (0xC8 << 8) | 0xE0 = 0xC8E0
        let result = key_to_scancode(egui::Key::ArrowUp);
        assert_eq!(result, Some((0x48E0, 0xC8E0)));
    }

    #[test]
    fn test_key_space() {
        // Key::Space → base 0x39 (normal key)
        let result = key_to_scancode(egui::Key::Space);
        assert_eq!(result, Some((0x39, 0xB9)));
    }

    #[test]
    fn test_key_enter() {
        // Key::Enter → base 0x1C (normal key)
        let result = key_to_scancode(egui::Key::Enter);
        assert_eq!(result, Some((0x1C, 0x9C)));
    }

    #[test]
    fn test_key_escape() {
        // Key::Escape → base 0x01 (normal key)
        let result = key_to_scancode(egui::Key::Escape);
        assert_eq!(result, Some((0x01, 0x81)));
    }

    #[test]
    fn test_unmapped_key_returns_none() {
        // Key::F13 is not in SCANCODE_MAP (only F1–F12 are mapped)
        let result = key_to_scancode(egui::Key::F13);
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
    fn paste_scancode_values_match_key_to_scancode() {
        // For representative characters, verify that translate_paste produces the same
        // press/release codes as key_to_scancode for the corresponding egui::Key.

        // 'a' -> Key::A
        let paste_result = translate_paste("a").unwrap();
        let key_result = key_to_scancode(egui::Key::A).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // '1' -> Key::Num1
        let paste_result = translate_paste("1").unwrap();
        let key_result = key_to_scancode(egui::Key::Num1).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // ' ' -> Key::Space
        let paste_result = translate_paste(" ").unwrap();
        let key_result = key_to_scancode(egui::Key::Space).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // '\t' -> Key::Tab
        let paste_result = translate_paste("\t").unwrap();
        let key_result = key_to_scancode(egui::Key::Tab).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);

        // '\n' -> Key::Enter
        let paste_result = translate_paste("\n").unwrap();
        let key_result = key_to_scancode(egui::Key::Enter).unwrap();
        assert_eq!(paste_result[0].press, key_result.0);
        assert_eq!(paste_result[0].release, key_result.1);
    }
}
