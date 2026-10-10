/// Cursor channel handler - cursor position, shape, and caching
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

use crate::opcode_counters::OpcodeCounters;
use crate::snapshots::{CursorCacheEntry, CursorSnapshot};
use crate::{
    ByteCounter, CaptureSink, LogConfig, NotificationEntry, NotificationSource, TrafficSink,
};
use shakenfist_spice_compression::limits;
use shakenfist_spice_protocol::constants::{cursor_flags, cursor_type};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, take_message, CursorHeader, CursorInitHead, CursorInvalOne, CursorMove,
    CursorSetHead, Notify as NotifyMessage, Ping, SetAck, SpiceCursor, SpiceCursorRef, WireType,
};
use shakenfist_spice_protocol::reader::BoundedReader;
use shakenfist_spice_protocol::{cursor_client, cursor_server, ChannelType, NotifySeverity};

use super::{ChannelEvent, CursorImage, EventSink, MAX_MESSAGE_BODY};

pub struct CursorChannel {
    stream: SpiceStream,
    events: EventSink,
    buffer: Vec<u8>,
    cursor_cache: HashMap<u64, CursorImage>,
    capture: Option<Arc<dyn CaptureSink>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    log_config: LogConfig,
    snapshot: Arc<Mutex<CursorSnapshot>>,
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,
    bytes_in: u64,
    bytes_out: u64,
    /// Local cache of disconnect-cause diagnostic fields,
    /// flushed to `snapshot` by `update_snapshot()`.
    last_recv_ts_secs: Option<f64>,
    last_send_ts_secs: Option<f64>,
    ping_recv_count: u32,
    pong_send_count: u32,
    last_ping_recv_ts_secs: Option<f64>,
    /// See `MainChannel::capture_dropped_count`.
    capture_dropped_count: u64,
    /// Bounded per-opcode message counters; flushed to the
    /// snapshot by `update_snapshot`. See `OpcodeCounters`.
    opcodes: OpcodeCounters,
}

impl CursorChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: SpiceStream,
        events: EventSink,
        capture: Option<Arc<dyn CaptureSink>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<dyn TrafficSink>,
        snapshot: Arc<Mutex<CursorSnapshot>>,
        log_config: LogConfig,
    ) -> Self {
        CursorChannel {
            stream,
            events,
            buffer: Vec::with_capacity(65536),
            cursor_cache: HashMap::new(),
            capture,
            byte_counter,
            traffic,
            log_config,
            snapshot,
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
            last_recv_ts_secs: None,
            last_send_ts_secs: None,
            ping_recv_count: 0,
            pong_send_count: 0,
            last_ping_recv_ts_secs: None,
            capture_dropped_count: 0,
            opcodes: OpcodeCounters::new(
                message_names::cursor_server,
                message_names::cursor_client,
            ),
        }
    }

    /// Run the cursor channel event loop. Wraps `run_loop` so
    /// errors propagating out of the inner select! arms are
    /// logged before the task ends — see `MainChannel::run`
    /// for the rationale (including the `Box::pin` reason).
    pub async fn run(&mut self) -> Result<()> {
        let result = Box::pin(self.run_loop()).await;
        match &result {
            Ok(()) => info!("cursor: run loop exited cleanly"),
            Err(e) => error!("cursor: run loop exited with error: {:#}", e),
        }
        result
    }

    async fn run_loop(&mut self) -> Result<()> {
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
                SpiceStream::TlsServer(s) => {
                    use tokio::io::AsyncReadExt;
                    s.read(&mut chunk).await?
                }
            };

            if n == 0 {
                info!("cursor: channel disconnected");
                self.events
                    .emit(ChannelEvent::Disconnected(ChannelType::Cursor))
                    .await;
                break;
            }

            self.byte_counter.add(n as u64);
            if let Some(ref c) = self.capture {
                if !c.packet_received("cursor", &chunk[..n]) {
                    self.capture_dropped_count = self.capture_dropped_count.saturating_add(1);
                }
            }
            self.buffer.extend_from_slice(&chunk[..n]);
            self.bytes_in += n as u64;
            self.last_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

            // Process complete messages
            self.process_messages().await?;
        }

        Ok(())
    }

    async fn process_messages(&mut self) -> Result<()> {
        while let Some(message) = take_message(&mut self.buffer, MAX_MESSAGE_BODY)? {
            let msg_type = message.header.message_type;
            self.traffic.record_received(
                "cursor",
                msg_type,
                message_names::cursor_server(msg_type),
                &message.raw,
            );

            self.message_count += 1;
            self.handle_message(msg_type, message.payload()).await?;

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
        if self.log_config.verbose {
            logging::log_message(
                "received",
                "cursor",
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
            // INIT and SET are read in two parts, the fixed fields and then
            // the SpiceCursor (CursorInit and CursorSet are the two
            // composed). Short fixed fields end the channel; a malformed
            // cursor after them loses only the shape, and the position is
            // still applied.
            //
            // spice.proto's Point16 is signed. Ryll has always handed the
            // position on as u16, and the `as u16` casts keep those bits.
            cursor_server::INIT => {
                let mut r = BoundedReader::new(payload);
                let init = CursorInitHead::read(&mut r).context("malformed INIT")?;
                debug!(
                    "cursor: init: pos=({},{}), visible={}, payload_size={}",
                    init.x as u16,
                    init.y as u16,
                    init.visible,
                    payload.len()
                );

                self.events
                    .emit(ChannelEvent::CursorPosition {
                        x: init.x as u16,
                        y: init.y as u16,
                        visible: init.visible != 0,
                    })
                    .await;

                self.parse_and_emit_cursor(&mut r).await;
            }

            cursor_server::SET => {
                let mut r = BoundedReader::new(payload);
                let set = CursorSetHead::read(&mut r).context("malformed SET")?;
                debug!(
                    "cursor: set: pos=({},{}), visible={}, payload_size={}",
                    set.x as u16,
                    set.y as u16,
                    set.visible,
                    payload.len()
                );

                self.events
                    .emit(ChannelEvent::CursorPosition {
                        x: set.x as u16,
                        y: set.y as u16,
                        visible: set.visible != 0,
                    })
                    .await;

                self.parse_and_emit_cursor(&mut r).await;
            }

            cursor_server::MOVE => {
                // A short MOVE is ignored.
                if let Ok(mv) = CursorMove::decode(payload) {
                    let (x, y) = (mv.x as u16, mv.y as u16);
                    debug!("cursor: move: ({},{})", x, y);

                    self.events
                        .emit(ChannelEvent::CursorPosition {
                            x,
                            y,
                            visible: true,
                        })
                        .await;
                }
            }

            cursor_server::HIDE => {
                info!("cursor: hide");
                self.events
                    .emit(ChannelEvent::CursorPosition {
                        x: 0,
                        y: 0,
                        visible: false,
                    })
                    .await;
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
                // A short INVAL_ONE is ignored.
                if let Ok(CursorInvalOne { id }) = CursorInvalOne::decode(payload) {
                    debug!("cursor: invalidate_one: id={}", id);
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
                let set_ack = SetAck::decode(payload).context("malformed SET_ACK")?;

                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "generation={}, window={}",
                        set_ack.generation, set_ack.window
                    ));
                }

                self.ack_generation = set_ack.generation;
                self.ack_window = set_ack.window;

                // Send ack_sync response
                let mut ack_payload = Vec::new();
                set_ack.ack_sync().write(&mut ack_payload);
                let response = make_message(cursor_client::ACK_SYNC, &ack_payload);
                self.send_with_log(cursor_client::ACK_SYNC, &response)
                    .await?;
            }

            cursor_server::PING => {
                self.ping_recv_count = self.ping_recv_count.saturating_add(1);
                self.last_ping_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

                let ping = Ping::decode(payload).context("malformed PING")?;

                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "ping_id={}, timestamp={}",
                        ping.id, ping.timestamp
                    ));
                }

                let mut pong_payload = Vec::new();
                ping.pong().write(&mut pong_payload);
                let response = make_message(cursor_client::PONG, &pong_payload);
                self.send_with_log(cursor_client::PONG, &response).await?;
                self.pong_send_count = self.pong_send_count.saturating_add(1);
            }

            cursor_server::NOTIFY => {
                let notify = NotifyMessage::decode(payload).context("malformed NOTIFY")?;
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
                        warn!("cursor: server notify (error): {}", message)
                    }
                    NotifySeverity::Warn => {
                        warn!("cursor: server notify (warn): {}", message)
                    }
                    NotifySeverity::Info => {
                        info!("cursor: server notify: {}", message)
                    }
                }
                let mut entry = NotificationEntry::new(
                    severity,
                    NotificationSource::Spice {
                        channel: ChannelType::Cursor,
                        what: notify.what,
                    },
                    message,
                );
                if let Some(v) = notify.visibility_kind() {
                    entry = entry.with_visibility(v);
                }
                self.events.emit(ChannelEvent::Notification(entry)).await;
            }

            unknown => {
                // Unknown message — log hex once, silent on repeat.
                logging::log_unknown_once("cursor", unknown, payload);
                self.opcodes.note_unknown(unknown);
            }
        }

        Ok(())
    }

    /// Read the SpiceCursor at the end of INIT or SET from `r`, and emit a
    /// CursorShape event if it decodes. A missing cursor is ignored
    /// silently and a truncated header with a warning; neither ends the
    /// channel.
    async fn parse_and_emit_cursor(&mut self, r: &mut BoundedReader<'_>) {
        if r.remaining() < SpiceCursor::FLAGS_SIZE {
            return;
        }

        let cursor = match SpiceCursorRef::read(r) {
            Ok(c) => c,
            Err(e) => {
                warn!("cursor: failed to parse SpiceCursor: {}", e);
                return;
            }
        };
        let Some(header) = &cursor.header else {
            debug!("cursor: FLAG_NONE set, no cursor data");
            return;
        };

        let from_cache = cursor.has_flag(cursor_flags::FROM_CACHE);
        let cache_me = cursor.has_flag(cursor_flags::CACHE_ME);

        debug!(
            "cursor: shape: type={}, {}x{}, hot=({},{}), id={}, flags={:#x} (cache_me={}, from_cache={})",
            header.cursor_type,
            header.width,
            header.height,
            header.hot_spot_x,
            header.hot_spot_y,
            header.unique_id,
            cursor.flags,
            cache_me,
            from_cache,
        );

        if from_cache {
            if let Some(img) = self.cursor_cache.get(&header.unique_id) {
                debug!("cursor: using cached cursor id={}", header.unique_id);
                self.events
                    .emit(ChannelEvent::CursorShape(img.clone()))
                    .await;
            } else {
                warn!(
                    "cursor: cache miss for id={} (cache has {} entries)",
                    header.unique_id,
                    self.cursor_cache.len()
                );
            }
            return;
        }

        let image = decode_cursor_pixels(header, cursor.data);

        if let Some(img) = image {
            if cache_me {
                debug!("cursor: caching cursor id={}", header.unique_id);
                self.cursor_cache.insert(header.unique_id, img.clone());
            }
            self.events.emit(ChannelEvent::CursorShape(img)).await;
        }
    }

    /// Sync local state to the shared snapshot.
    fn update_snapshot(&self) {
        let mut snap = self.snapshot.lock().expect("lock poisoned");
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
        snap.last_recv_ts_secs = self.last_recv_ts_secs;
        snap.last_send_ts_secs = self.last_send_ts_secs;
        snap.ping_recv_count = self.ping_recv_count;
        snap.pong_send_count = self.pong_send_count;
        snap.last_ping_recv_ts_secs = self.last_ping_recv_ts_secs;
        snap.writer_dropped_count = self.capture_dropped_count;
        self.opcodes.publish_into(&mut *snap);
    }

    async fn send_ack(&mut self) -> Result<()> {
        let msg = make_message(cursor_client::ACK, &[]);
        self.send_with_log(cursor_client::ACK, &msg).await?;
        self.last_ack = self.message_count;
        Ok(())
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let msg_name = message_names::cursor_client(msg_type);
        if self.log_config.verbose {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "cursor", msg_type, msg_name, payload_size);
        }
        self.traffic.record_sent("cursor", msg_type, msg_name, data);
        // Single send path, so this is the only send-count site.
        self.opcodes.record_send(msg_type);
        let result = self.send(data).await;
        self.update_snapshot();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            if !c.packet_sent("cursor", data) {
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

/// Decode cursor pixel data based on cursor_type, returning RGBA pixels.
fn decode_cursor_pixels(header: &CursorHeader, pixel_data: &[u8]) -> Option<CursorImage> {
    let w = header.width as usize;
    let h = header.height as usize;

    if w == 0 || h == 0 {
        return None;
    }

    // Bytes per source pixel, whether the fourth byte is alpha, and
    // the format's name for the warnings below.
    let (src_bpp, has_alpha, format_name) = match header.cursor_type {
        // Alpha: 32-bit ARGB per pixel
        cursor_type::ALPHA => (4, true, "alpha"),
        // Color24: 24-bit BGR per pixel
        cursor_type::COLOR24 => (3, false, "color24"),
        // Color32: 32-bit xRGB per pixel (x is padding, not alpha)
        cursor_type::COLOR32 => (4, false, "color32"),
        other => {
            warn!("cursor: unsupported cursor type {} ({}x{})", other, w, h);
            return None;
        }
    };

    // The dimensions are server-chosen u16s, so size the buffer with
    // the shared limit, and refuse a short message before allocating
    // anything: a 65535x65535 header with no pixel data must not cost
    // 16 GiB.
    let Some(rgba_size) = limits::rgba_len(w, h) else {
        warn!("cursor: dimensions refused: {}x{}", w, h);
        return None;
    };
    let pixel_count = rgba_size / 4;
    let needed = pixel_count * src_bpp;
    if pixel_data.len() < needed {
        warn!(
            "cursor: {} data too short (have {}, need {})",
            format_name,
            pixel_data.len(),
            needed
        );
        return None;
    }

    let mut rgba = vec![0u8; rgba_size];
    let (dst_pixels, _) = rgba.as_chunks_mut::<4>();
    for (src, dst) in pixel_data.chunks_exact(src_bpp).zip(dst_pixels) {
        // BGR(A/X) -> RGBA
        dst[0] = src[2]; // R
        dst[1] = src[1]; // G
        dst[2] = src[0]; // B
        dst[3] = if has_alpha { src[3] } else { 255 }; // A
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
    use crate::channels::test_support::{loopback, NullTraffic, TestChannelPeers};

    /// A cursor shape as the protocol crate's writer puts it on the wire,
    /// read back the way the channel reads it.
    fn wire_cursor(
        cursor_type: u8,
        width: u16,
        height: u16,
        flags: u16,
        pixel_data: &[u8],
    ) -> SpiceCursor {
        let cursor = SpiceCursor {
            flags,
            header: (flags & cursor_flags::NONE == 0).then_some(CursorHeader {
                unique_id: 1,
                cursor_type,
                width,
                height,
                hot_spot_x: 0,
                hot_spot_y: 0,
            }),
            data: pixel_data.to_vec(),
        };
        let mut body = Vec::new();
        cursor.write(&mut body);
        SpiceCursor::decode(&body).expect("a written cursor reads back")
    }

    /// Decode a shaped cursor's pixels as `parse_and_emit_cursor` does.
    fn decode(cursor: &SpiceCursor) -> Option<CursorImage> {
        decode_cursor_pixels(cursor.header.as_ref().expect("a shape"), &cursor.data)
    }

    #[test]
    fn test_alpha_cursor_argb_to_rgba() {
        let pixels: Vec<u8> = vec![
            0x10, 0x20, 0x30, 0x80, // B=0x10, G=0x20, R=0x30, A=0x80
            0x40, 0x50, 0x60, 0xFF, // B=0x40, G=0x50, R=0x60, A=0xFF
            0x00, 0x00, 0x00, 0x00, // transparent black
            0xFF, 0xFF, 0xFF, 0xFF, // opaque white
        ];
        let cursor = wire_cursor(cursor_type::ALPHA, 2, 2, 0, &pixels);
        let result = decode(&cursor);
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
        let cursor = wire_cursor(cursor_type::COLOR32, 1, 1, 0, &pixels);
        let result = decode(&cursor);
        assert!(result.is_some());

        let img = result.unwrap();
        assert_eq!(img.pixels, vec![0xCC, 0xBB, 0xAA, 0xFF]); // RGBA with A=255
    }

    #[test]
    fn test_color24_cursor_bgr_to_rgba() {
        let pixels: Vec<u8> = vec![
            0x11, 0x22, 0x33, // B=11, G=22, R=33
            0x44, 0x55, 0x66, // B=44, G=55, R=66
        ];
        let cursor = wire_cursor(cursor_type::COLOR24, 2, 1, 0, &pixels);
        let img = decode(&cursor).unwrap();
        assert_eq!(
            img.pixels,
            vec![0x33, 0x22, 0x11, 0xFF, 0x66, 0x55, 0x44, 0xFF]
        );
    }

    #[test]
    fn test_short_cursor_data_returns_none() {
        // One byte short of a 2x2 cursor, for each format.
        for (cursor_type, bpp) in [
            (cursor_type::ALPHA, 4usize),
            (cursor_type::COLOR24, 3),
            (cursor_type::COLOR32, 4),
        ] {
            let pixels = vec![0u8; 2 * 2 * bpp - 1];
            let cursor = wire_cursor(cursor_type, 2, 2, 0, &pixels);
            assert!(
                decode(&cursor).is_none(),
                "cursor type {} with short data must be refused",
                cursor_type
            );
        }
    }

    #[test]
    fn test_huge_cursor_with_short_data_returns_none() {
        // #177: a 65535x65535 header with four bytes of pixel data.
        // Before the fix this allocated 16 GiB of RGBA and only then
        // noticed the data was short.
        for cursor_type in [
            cursor_type::ALPHA,
            cursor_type::COLOR24,
            cursor_type::COLOR32,
        ] {
            let cursor = wire_cursor(cursor_type, 65535, 65535, 0, &[0u8; 4]);
            assert!(decode(&cursor).is_none());
        }
    }

    #[test]
    fn test_cursor_over_dimension_cap_returns_none() {
        // Full pixel data does not rescue a cursor over the shared
        // per-side limit; one at the limit still decodes.
        let max = limits::MAX_IMAGE_DIMENSION as u16;
        let pixels = vec![0u8; (max as usize + 1) * 4];

        let cursor = wire_cursor(cursor_type::ALPHA, max + 1, 1, 0, &pixels);
        assert!(decode(&cursor).is_none());

        let cursor = wire_cursor(cursor_type::ALPHA, max, 1, 0, &pixels);
        let img = decode(&cursor).unwrap();
        assert_eq!(img.pixels.len(), max as usize * 4);
    }

    #[test]
    fn test_from_cache_flag_no_pixel_data() {
        let cursor = wire_cursor(cursor_type::ALPHA, 24, 24, cursor_flags::FROM_CACHE, &[]);
        assert!(cursor.has_flag(cursor_flags::FROM_CACHE));
        let header = cursor.header.as_ref().expect("FROM_CACHE keeps the header");
        assert_eq!(header.width, 24);
        assert_eq!(header.height, 24);
        assert!(cursor.data.is_empty());
    }

    #[test]
    fn test_flag_none_returns_none() {
        let cursor = wire_cursor(cursor_type::ALPHA, 24, 24, cursor_flags::NONE, &[]);
        assert!(cursor.header.is_none());
    }

    #[test]
    fn test_unsupported_cursor_types_return_none() {
        // Only ALPHA, COLOR24 and COLOR32 decode; the rest are refused
        // even with ample data.
        for cursor_type in [
            cursor_type::MONO,
            cursor_type::COLOR4,
            cursor_type::COLOR8,
            cursor_type::COLOR16,
            7,
            0xff,
        ] {
            let cursor = wire_cursor(cursor_type, 2, 2, 0, &[0u8; 64]);
            assert!(decode(&cursor).is_none(), "cursor type {}", cursor_type);
        }
    }

    #[test]
    fn test_zero_dimension_cursor_returns_none() {
        let cursor = wire_cursor(cursor_type::ALPHA, 0, 0, 0, &[]);
        let result = decode(&cursor);
        assert!(result.is_none());
    }

    // Failure policy for malformed messages: which end the channel and
    // which are skipped.

    async fn test_cursor_channel() -> (CursorChannel, TestChannelPeers) {
        let (stream, events, peers) = loopback().await;
        let channel = CursorChannel::new(
            stream,
            events,
            None,
            Arc::new(ByteCounter::new()),
            Arc::new(NullTraffic::new()),
            Arc::new(Mutex::new(CursorSnapshot::default())),
            LogConfig::default(),
        );
        (channel, peers)
    }

    #[tokio::test]
    async fn short_init_and_set_end_the_channel() {
        let (mut channel, _peers) = test_cursor_channel().await;
        let init = vec![0; CursorInitHead::SIZE - 1];
        assert!(channel
            .handle_message(cursor_server::INIT, &init)
            .await
            .is_err());
        let set = vec![0; CursorSetHead::SIZE - 1];
        assert!(channel
            .handle_message(cursor_server::SET, &set)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn short_move_and_inval_one_are_ignored() {
        let (mut channel, mut peers) = test_cursor_channel().await;
        let cached = CursorImage {
            width: 1,
            height: 1,
            hot_spot_x: 0,
            hot_spot_y: 0,
            pixels: vec![0; 4],
        };
        channel.cursor_cache.insert(0, cached);

        channel
            .handle_message(cursor_server::MOVE, &[1, 0, 2])
            .await
            .expect("a short MOVE is ignored");
        channel
            .handle_message(
                cursor_server::INVALIDATE_ONE,
                &[0; CursorInvalOne::SIZE - 1],
            )
            .await
            .expect("a short INVAL_ONE is ignored");

        assert!(
            peers.events.try_recv().is_err(),
            "a short MOVE moves nothing"
        );
        assert!(
            channel.cursor_cache.contains_key(&0),
            "a short INVAL_ONE invalidates nothing"
        );
    }
}
