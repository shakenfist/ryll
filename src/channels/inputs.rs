/// Inputs channel handler - keyboard and mouse input
use anyhow::Result;
use eframe::egui;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::app::ByteCounter;
use crate::bugreport::{InputEventRecord, InputsSnapshot, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::protocol::link::SpiceStream;
use crate::protocol::logging::{self, message_names};
use crate::protocol::messages::{
    make_message, InputsKeyModifiers, KeyEvent, MessageHeader, MouseButton, MousePosition, Ping,
    SetAck,
};
use crate::protocol::{inputs_client, inputs_server, keyboard_modifiers, ChannelType};
use crate::settings;

use super::{ChannelEvent, InputEvent};

/// spice-gtk throttles motion messages to this many pending before an ACK
const MOTION_ACK_BUNCH: u32 = 4;

/// Maximum number of recent input events to keep in the snapshot.
const MAX_RECENT_EVENTS: usize = 50;

pub struct InputsChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    input_rx: mpsc::Receiver<InputEvent>,
    buffer: Vec<u8>,
    last_key_time: Option<Instant>,
    button_state: u32,
    motion_count: u32,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    snapshot: Arc<Mutex<InputsSnapshot>>,
    recent_events: VecDeque<InputEventRecord>,
    bytes_in: u64,
    bytes_out: u64,
}

impl InputsChannel {
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        input_rx: mpsc::Receiver<InputEvent>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<TrafficBuffers>,
        snapshot: Arc<Mutex<InputsSnapshot>>,
    ) -> Self {
        InputsChannel {
            stream,
            event_tx,
            input_rx,
            buffer: Vec::with_capacity(4096),
            last_key_time: None,
            button_state: 0,
            motion_count: 0,
            capture,
            byte_counter,
            traffic,
            snapshot,
            recent_events: VecDeque::new(),
            bytes_in: 0,
            bytes_out: 0,
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
                        } else {
                            self.handle_input_event(batch[i].clone()).await?;
                            i += 1;
                        }
                    }
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

            _ => {
                // Unknown message - log with hex dump
                logging::log_unknown(
                    "inputs",
                    "received",
                    msg_type,
                    payload.len() as u32,
                    payload,
                );
            }
        }

        Ok(())
    }

    async fn handle_input_event(&mut self, event: InputEvent) -> Result<()> {
        let ts = self.traffic.elapsed().as_secs_f64();

        match event {
            InputEvent::KeyDown(scancode) => {
                self.last_key_time = Some(Instant::now());

                // Send latency timestamp
                self.event_tx
                    .send(ChannelEvent::Latency {
                        key_timestamp: self.last_key_time.unwrap().elapsed().as_secs_f64(),
                    })
                    .await
                    .ok();

                self.record_event(InputEventRecord {
                    event_type: "KeyDown".to_string(),
                    scancode,
                    x: 0,
                    y: 0,
                    button_mask: 0,
                    timestamp_secs: ts,
                });

                let mut payload = Vec::new();
                KeyEvent::write(scancode, &mut payload)?;
                let msg = make_message(inputs_client::KEY_DOWN, &payload);

                info!("inputs: key down: scancode={:#x}", scancode);
                self.send_with_log(inputs_client::KEY_DOWN, &msg).await?;
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
                KeyEvent::write(scancode, &mut payload)?;
                let msg = make_message(inputs_client::KEY_UP, &payload);

                info!("inputs: key up: scancode={:#x}", scancode);
                self.send_with_log(inputs_client::KEY_UP, &msg).await?;
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
                    MousePosition::write(x, y, self.button_state, 0, &mut payload)?;
                    let msg = make_message(inputs_client::MOUSE_POSITION, &payload);
                    self.send_with_log(inputs_client::MOUSE_POSITION, &msg)
                        .await?;
                    self.motion_count += 1;
                }
            }

            InputEvent::MouseDown { button, x, y } => {
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
                MouseButton::write(button, self.button_state, &mut payload)?;
                let msg = make_message(inputs_client::MOUSE_PRESS, &payload);
                debug!("inputs: mouse down: button={}, pos=({},{})", button, x, y);
                self.send_with_log(inputs_client::MOUSE_PRESS, &msg).await?;
            }

            InputEvent::MouseUp { button, x, y } => {
                self.button_state &= !button;

                self.record_event(InputEventRecord {
                    event_type: "MouseUp".to_string(),
                    scancode: 0,
                    x,
                    y,
                    button_mask: button,
                    timestamp_secs: ts,
                });

                let mut payload = Vec::new();
                MouseButton::write(button, self.button_state, &mut payload)?;
                let msg = make_message(inputs_client::MOUSE_RELEASE, &payload);
                debug!("inputs: mouse up: button={}, pos=({},{})", button, x, y);
                self.send_with_log(inputs_client::MOUSE_RELEASE, &msg)
                    .await?;
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
        InputsKeyModifiers::write(modifiers, &mut payload)?;
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

/// Map mouse button to SPICE button flag
pub fn mouse_button_to_spice(button: egui::PointerButton) -> u32 {
    match button {
        egui::PointerButton::Primary => crate::protocol::mouse_buttons::LEFT,
        egui::PointerButton::Secondary => crate::protocol::mouse_buttons::RIGHT,
        egui::PointerButton::Middle => crate::protocol::mouse_buttons::MIDDLE,
        egui::PointerButton::Extra1 => crate::protocol::mouse_buttons::UP,
        egui::PointerButton::Extra2 => crate::protocol::mouse_buttons::DOWN,
    }
}
