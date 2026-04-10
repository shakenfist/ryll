/// Cursor channel handler - cursor position, shape, and caching
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::ByteCounter;
use crate::bugreport::{CursorCacheEntry, CursorSnapshot, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::settings;
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, CursorInit, CursorSet, MessageHeader, Ping, SetAck, SpiceCursorHeader,
};
use shakenfist_spice_protocol::{cursor_client, cursor_server, ChannelType};

use super::{ChannelEvent, CursorImage};

pub struct CursorChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    buffer: Vec<u8>,
    cursor_cache: HashMap<u64, CursorImage>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    snapshot: Arc<Mutex<CursorSnapshot>>,
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,
    bytes_in: u64,
    bytes_out: u64,
}

impl CursorChannel {
    pub fn new(
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<TrafficBuffers>,
        snapshot: Arc<Mutex<CursorSnapshot>>,
    ) -> Self {
        CursorChannel {
            stream,
            event_tx,
            buffer: Vec::with_capacity(65536),
            cursor_cache: HashMap::new(),
            capture,
            byte_counter,
            traffic,
            snapshot,
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
            let mut chunk = [0u8; 65536];
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

            self.byte_counter.add(n as u64);
            if let Some(ref c) = self.capture {
                c.packet_received("cursor", &chunk[..n]);
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

            // Record to ring buffer before draining
            let raw = self.buffer[..total_size].to_vec();
            self.traffic.record_received(
                "cursor",
                header.message_type,
                message_names::cursor_server(header.message_type),
                &raw,
            );

            let payload = self.buffer[MessageHeader::SIZE..total_size].to_vec();
            self.buffer.drain(..total_size);

            self.message_count += 1;
            self.handle_message(header.message_type, &payload).await?;

            // Send ACK if needed
            if self.ack_window > 0 && self.message_count - self.last_ack >= self.ack_window {
                self.send_ack().await?;
            }
        }

        self.update_snapshot();
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

                // SpiceCursor data follows the 9-byte INIT header
                self.parse_and_emit_cursor(&payload[CursorInit::SIZE..])
                    .await;
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

                // SpiceCursor data follows the 5-byte SET header
                self.parse_and_emit_cursor(&payload[CursorSet::SIZE..])
                    .await;
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
                info!(
                    "cursor: reset, clearing cache ({} entries)",
                    self.cursor_cache.len()
                );
                self.cursor_cache.clear();
            }

            cursor_server::TRAIL => {
                debug!("cursor: trail settings received");
            }

            cursor_server::INVALIDATE_ONE => {
                if payload.len() >= 8 {
                    let id = u64::from_le_bytes([
                        payload[0], payload[1], payload[2], payload[3], payload[4], payload[5],
                        payload[6], payload[7],
                    ]);
                    info!("cursor: invalidate_one: id={}", id);
                    self.cursor_cache.remove(&id);
                }
            }

            cursor_server::INVALIDATE_ALL => {
                info!(
                    "cursor: invalidate_all, clearing cache ({} entries)",
                    self.cursor_cache.len()
                );
                self.cursor_cache.clear();
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

    /// Parse SpiceCursor data and emit a CursorShape event if successful.
    async fn parse_and_emit_cursor(&mut self, data: &[u8]) {
        if data.len() < SpiceCursorHeader::FLAGS_SIZE {
            return;
        }

        let header = match SpiceCursorHeader::read(data) {
            Ok(Some(h)) => h,
            Ok(None) => {
                debug!("cursor: FLAG_NONE set, no cursor data");
                return;
            }
            Err(e) => {
                warn!("cursor: failed to parse SpiceCursorHeader: {}", e);
                return;
            }
        };

        let from_cache = (header.flags & SpiceCursorHeader::FLAG_FROM_CACHE) != 0;
        let cache_me = (header.flags & SpiceCursorHeader::FLAG_CACHE_ME) != 0;

        info!(
            "cursor: shape: type={}, {}x{}, hot=({},{}), id={}, flags={:#x} (cache_me={}, from_cache={})",
            header.cursor_type,
            header.width,
            header.height,
            header.hot_spot_x,
            header.hot_spot_y,
            header.unique_id,
            header.flags,
            cache_me,
            from_cache,
        );

        if from_cache {
            if let Some(img) = self.cursor_cache.get(&header.unique_id) {
                info!("cursor: using cached cursor id={}", header.unique_id);
                self.event_tx
                    .send(ChannelEvent::CursorShape(img.clone()))
                    .await
                    .ok();
            } else {
                warn!(
                    "cursor: cache miss for id={} (cache has {} entries)",
                    header.unique_id,
                    self.cursor_cache.len()
                );
            }
            return;
        }

        let pixel_data = &data[SpiceCursorHeader::SIZE..];
        let image = decode_cursor_pixels(&header, pixel_data);

        if let Some(img) = image {
            if cache_me {
                info!("cursor: caching cursor id={}", header.unique_id);
                self.cursor_cache.insert(header.unique_id, img.clone());
            }
            self.event_tx
                .send(ChannelEvent::CursorShape(img))
                .await
                .ok();
        }
    }

    /// Sync local state to the shared snapshot.
    fn update_snapshot(&self) {
        let mut snap = self.snapshot.lock().unwrap();
        snap.cache_entries = self.cursor_cache.len();
        snap.cache_contents = self
            .cursor_cache
            .iter()
            .map(|(&id, img)| CursorCacheEntry {
                cursor_id: id,
                width: img.width,
                height: img.height,
                hot_spot_x: img.hot_spot_x,
                hot_spot_y: img.hot_spot_y,
            })
            .collect();
        snap.ack_generation = self.ack_generation;
        snap.ack_window = self.ack_window;
        snap.message_count = self.message_count;
        snap.last_ack = self.last_ack;
        snap.bytes_in = self.bytes_in;
        snap.bytes_out = self.bytes_out;
    }

    async fn send_ack(&mut self) -> Result<()> {
        let msg = make_message(cursor_client::ACK, &[]);
        self.send_with_log(cursor_client::ACK, &msg).await?;
        self.last_ack = self.message_count;
        Ok(())
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let msg_name = message_names::cursor_client(msg_type);
        if settings::is_verbose() {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "cursor", msg_type, msg_name, payload_size);
        }
        self.traffic.record_sent("cursor", msg_type, msg_name, data);
        let result = self.send(data).await;
        self.update_snapshot();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            c.packet_sent("cursor", data);
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}

/// Decode cursor pixel data based on cursor_type, returning RGBA pixels.
fn decode_cursor_pixels(header: &SpiceCursorHeader, pixel_data: &[u8]) -> Option<CursorImage> {
    let w = header.width as usize;
    let h = header.height as usize;
    let pixel_count = w.checked_mul(h)?;
    let rgba_size = pixel_count.checked_mul(4)?;

    if w == 0 || h == 0 {
        return None;
    }

    let mut rgba = vec![0u8; rgba_size];

    match header.cursor_type {
        // Alpha: 32-bit ARGB per pixel
        0 => {
            let needed = pixel_count * 4;
            if pixel_data.len() < needed {
                warn!(
                    "cursor: alpha data too short (have {}, need {})",
                    pixel_data.len(),
                    needed
                );
                return None;
            }
            for i in 0..pixel_count {
                let s = i * 4;
                let d = i * 4;
                rgba[d] = pixel_data[s + 2]; // R (from BGRA position)
                rgba[d + 1] = pixel_data[s + 1]; // G
                rgba[d + 2] = pixel_data[s]; // B
                rgba[d + 3] = pixel_data[s + 3]; // A
            }
        }

        // Color24: 24-bit BGR per pixel
        5 => {
            let needed = pixel_count * 3;
            if pixel_data.len() < needed {
                warn!(
                    "cursor: color24 data too short (have {}, need {})",
                    pixel_data.len(),
                    needed
                );
                return None;
            }
            for i in 0..pixel_count {
                let s = i * 3;
                let d = i * 4;
                rgba[d] = pixel_data[s + 2]; // R
                rgba[d + 1] = pixel_data[s + 1]; // G
                rgba[d + 2] = pixel_data[s]; // B
                rgba[d + 3] = 255; // A
            }
        }

        // Color32: 32-bit xRGB per pixel (x is padding, not alpha)
        6 => {
            let needed = pixel_count * 4;
            if pixel_data.len() < needed {
                warn!(
                    "cursor: color32 data too short (have {}, need {})",
                    pixel_data.len(),
                    needed
                );
                return None;
            }
            for i in 0..pixel_count {
                let s = i * 4;
                let d = i * 4;
                rgba[d] = pixel_data[s + 2]; // R (from BGRX position)
                rgba[d + 1] = pixel_data[s + 1]; // G
                rgba[d + 2] = pixel_data[s]; // B
                rgba[d + 3] = 255; // A
            }
        }

        other => {
            warn!("cursor: unsupported cursor type {} ({}x{})", other, w, h);
            return None;
        }
    }

    Some(CursorImage {
        width: header.width,
        height: header.height,
        hot_spot_x: header.hot_spot_x,
        hot_spot_y: header.hot_spot_y,
        pixels: rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_cursor_payload(
        cursor_type: u8,
        width: u16,
        height: u16,
        flags: u16,
        pixel_data: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        // SpiceCursor: flags(2) + unique_id(8) + type(1) + w(2) + h(2) + hx(2) + hy(2)
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes()); // unique_id = 1
        buf.push(cursor_type);
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // hot_spot_x
        buf.extend_from_slice(&0u16.to_le_bytes()); // hot_spot_y
        buf.extend_from_slice(pixel_data);
        buf
    }

    #[test]
    fn test_alpha_cursor_argb_to_rgba() {
        let pixels: Vec<u8> = vec![
            0x10, 0x20, 0x30, 0x80, // B=0x10, G=0x20, R=0x30, A=0x80
            0x40, 0x50, 0x60, 0xFF, // B=0x40, G=0x50, R=0x60, A=0xFF
            0x00, 0x00, 0x00, 0x00, // transparent black
            0xFF, 0xFF, 0xFF, 0xFF, // opaque white
        ];
        let data = build_cursor_payload(0, 2, 2, 0, &pixels);
        let header = SpiceCursorHeader::read(&data).unwrap().unwrap();
        let result = decode_cursor_pixels(&header, &data[SpiceCursorHeader::SIZE..]);
        assert!(result.is_some());

        let img = result.unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 16);

        // First pixel: BGRA(0x10,0x20,0x30,0x80) → RGBA(0x30,0x20,0x10,0x80)
        assert_eq!(img.pixels[0], 0x30); // R
        assert_eq!(img.pixels[1], 0x20); // G
        assert_eq!(img.pixels[2], 0x10); // B
        assert_eq!(img.pixels[3], 0x80); // A

        // Second pixel: BGRA(0x40,0x50,0x60,0xFF) → RGBA(0x60,0x50,0x40,0xFF)
        assert_eq!(img.pixels[4], 0x60); // R
        assert_eq!(img.pixels[5], 0x50); // G
        assert_eq!(img.pixels[6], 0x40); // B
        assert_eq!(img.pixels[7], 0xFF); // A

        // Third pixel: all zeros (transparent)
        assert_eq!(img.pixels[8..12], [0, 0, 0, 0]);

        // Fourth pixel: BGRA(FF,FF,FF,FF) → RGBA(FF,FF,FF,FF)
        assert_eq!(img.pixels[12..16], [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_color32_cursor_xrgb_to_rgba() {
        let pixels: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0x00]; // B=AA, G=BB, R=CC, x=00
        let data = build_cursor_payload(6, 1, 1, 0, &pixels);
        let header = SpiceCursorHeader::read(&data).unwrap().unwrap();
        let result = decode_cursor_pixels(&header, &data[SpiceCursorHeader::SIZE..]);
        assert!(result.is_some());

        let img = result.unwrap();
        assert_eq!(img.pixels, vec![0xCC, 0xBB, 0xAA, 0xFF]); // RGBA with A=255
    }

    #[test]
    fn test_from_cache_flag_no_pixel_data() {
        let data = build_cursor_payload(0, 24, 24, SpiceCursorHeader::FLAG_FROM_CACHE, &[]);
        let header = SpiceCursorHeader::read(&data).unwrap().unwrap();
        assert_eq!(
            header.flags & SpiceCursorHeader::FLAG_FROM_CACHE,
            SpiceCursorHeader::FLAG_FROM_CACHE
        );
        assert_eq!(header.width, 24);
        assert_eq!(header.height, 24);
    }

    #[test]
    fn test_flag_none_returns_none() {
        let data = build_cursor_payload(0, 24, 24, SpiceCursorHeader::FLAG_NONE, &[]);
        let result = SpiceCursorHeader::read(&data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_zero_dimension_cursor_returns_none() {
        let data = build_cursor_payload(0, 0, 0, 0, &[]);
        let header = SpiceCursorHeader::read(&data).unwrap().unwrap();
        let result = decode_cursor_pixels(&header, &data[SpiceCursorHeader::SIZE..]);
        assert!(result.is_none());
    }
}
