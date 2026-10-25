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
    make_message, CursorHeader, CursorInitHead, CursorInvalOne, CursorMove, CursorSetHead,
    Notify as NotifyMessage, Ping, ReceiveBuffer, SetAck, SpiceCursor, SpiceCursorRef, WireType,
};
use shakenfist_spice_protocol::reader::BoundedReader;
use shakenfist_spice_protocol::{cursor_client, cursor_server, ChannelType, NotifySeverity};

use super::{ChannelEvent, CursorImage, DisconnectReason, EventSink, MAX_MESSAGE_BODY};

pub struct CursorChannel {
    stream: SpiceStream,
    events: EventSink,
    buffer: ReceiveBuffer,
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
            buffer: ReceiveBuffer::with_capacity(65536),
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
                    .emit(ChannelEvent::Disconnected(
                        ChannelType::Cursor,
                        DisconnectReason::ServerClosed,
                    ))
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
        while let Some(message) = self.buffer.take_message(MAX_MESSAGE_BODY)? {
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

/// The cursor formats ryll decodes. COLOR8 is not one of them, as it is
/// not one of spice-gtk's.
#[derive(Clone, Copy)]
enum CursorFormat {
    /// 32-bit BGRA per pixel, straight alpha.
    Alpha,
    /// An AND plane then an XOR plane, one bit per pixel, rows padded to
    /// a whole byte.
    Mono,
    /// Four-bit palette indices, then a 16-entry BGRx palette, then the
    /// AND mask.
    Color4,
    /// 15-bit xRGB per pixel, little-endian, then the AND mask.
    Color16,
    /// 24-bit BGR per pixel. spice-gtk has no decoder for it.
    Color24,
    /// 32-bit BGRx per pixel, then the AND mask.
    Color32,
}

/// Bytes in a COLOR4 cursor's palette: 16 entries of 32 bits.
const COLOR4_PALETTE_LEN: usize = 16 * 4;

impl CursorFormat {
    fn from_type(cursor_type: u8) -> Option<Self> {
        match cursor_type {
            cursor_type::ALPHA => Some(Self::Alpha),
            cursor_type::MONO => Some(Self::Mono),
            cursor_type::COLOR4 => Some(Self::Color4),
            cursor_type::COLOR16 => Some(Self::Color16),
            cursor_type::COLOR24 => Some(Self::Color24),
            cursor_type::COLOR32 => Some(Self::Color32),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Mono => "mono",
            Self::Color4 => "color4",
            Self::Color16 => "color16",
            Self::Color24 => "color24",
            Self::Color32 => "color32",
        }
    }

    /// The bytes of cursor data a `width` x `height` image needs in this
    /// format, or None if the sum overflows.
    ///
    /// The AND mask of the colour formats has no per-row padding: spice-gtk
    /// indexes it by pixel number (`get_pix_mask`), so it is
    /// `ceil(width * height / 8)` bytes. COLOR4's indices are sized with
    /// rows rounded up to a whole byte, as spice-gtk sizes them, although
    /// it too reads them by pixel number.
    fn data_len(self, width: usize, height: usize) -> Option<usize> {
        let pixels = width.checked_mul(height)?;
        let mask = pixels.div_ceil(8);
        match self {
            Self::Alpha => pixels.checked_mul(4),
            Self::Mono => width.div_ceil(8).checked_mul(height)?.checked_mul(2),
            Self::Color4 => width
                .div_ceil(2)
                .checked_mul(height)?
                .checked_add(COLOR4_PALETTE_LEN)?
                .checked_add(mask),
            Self::Color16 => pixels.checked_mul(2)?.checked_add(mask),
            Self::Color24 => pixels.checked_mul(3),
            Self::Color32 => pixels.checked_mul(4)?.checked_add(mask),
        }
    }
}

/// Decode cursor pixel data based on cursor_type, returning RGBA pixels.
/// Mono and the masked colour formats follow spice-gtk's `set_cursor`
/// pixel for pixel, so a cursor looks the same as in remote-viewer.
fn decode_cursor_pixels(header: &CursorHeader, pixel_data: &[u8]) -> Option<CursorImage> {
    let w = header.width as usize;
    let h = header.height as usize;

    if w == 0 || h == 0 {
        return None;
    }

    let Some(format) = CursorFormat::from_type(header.cursor_type) else {
        // COLOR8, and types spice.proto does not define.
        warn!(
            "cursor: unsupported cursor type {} ({}x{})",
            header.cursor_type, w, h
        );
        return None;
    };

    // The dimensions are server-chosen u16s, so size the buffer with
    // the shared limit, and refuse a short message before allocating
    // anything: a 65535x65535 header with no pixel data must not cost
    // 16 GiB.
    let (Some(rgba_size), Some(needed)) = (limits::rgba_len(w, h), format.data_len(w, h)) else {
        warn!("cursor: dimensions refused: {}x{}", w, h);
        return None;
    };
    if pixel_data.len() < needed {
        warn!(
            "cursor: {} data too short (have {}, need {})",
            format.name(),
            pixel_data.len(),
            needed
        );
        return None;
    }

    let mut rgba = vec![0u8; rgba_size];
    let (dst_pixels, _) = rgba.as_chunks_mut::<4>();
    let pixel_count = dst_pixels.len();
    match format {
        CursorFormat::Alpha | CursorFormat::Color24 => {
            let (src_bpp, has_alpha) = match format {
                CursorFormat::Alpha => (4, true),
                _ => (3, false),
            };
            for (src, dst) in pixel_data.chunks_exact(src_bpp).zip(dst_pixels) {
                // BGR(A) -> RGBA
                dst[0] = src[2]; // R
                dst[1] = src[1]; // G
                dst[2] = src[0]; // B
                dst[3] = if has_alpha { src[3] } else { 255 }; // A
            }
        }
        CursorFormat::Mono => mono_edge_highlight(w, h, pixel_data, dst_pixels),
        CursorFormat::Color4 => {
            let (indices, rest) = pixel_data.split_at(w.div_ceil(2) * h);
            let (palette, mask) = rest.split_at(COLOR4_PALETTE_LEN);
            let (palette, _) = palette.as_chunks::<4>();
            for (i, dst) in dst_pixels.iter_mut().enumerate() {
                // Two pixels a byte, the first in the high nibble.
                let byte = indices[i / 2];
                let index = if i % 2 == 0 { byte >> 4 } else { byte & 0x0f };
                let entry = palette[index as usize];
                *dst = masked_pixel(
                    [entry[2], entry[1], entry[0]],
                    entry == [0xff, 0xff, 0xff, 0x00],
                    entry[3],
                    mask_bit(mask, i),
                    i,
                    w,
                );
            }
        }
        CursorFormat::Color16 => {
            let (pixels, mask) = pixel_data.split_at(pixel_count * 2);
            let (pixels, _) = pixels.as_chunks::<2>();
            for (i, (src, dst)) in pixels.iter().zip(dst_pixels).enumerate() {
                // Five bits a channel, widened by shifting left as
                // spice-gtk does; the top bit is unused.
                let pixel = u16::from_le_bytes(*src);
                let channel = |shift: u16| (((pixel >> shift) & 0x1f) as u8) << 3;
                *dst = masked_pixel(
                    [channel(10), channel(5), channel(0)],
                    pixel == 0x7fff,
                    0,
                    mask_bit(mask, i),
                    i,
                    w,
                );
            }
        }
        CursorFormat::Color32 => {
            let (pixels, mask) = pixel_data.split_at(pixel_count * 4);
            let (pixels, _) = pixels.as_chunks::<4>();
            for (i, (src, dst)) in pixels.iter().zip(dst_pixels).enumerate() {
                *dst = masked_pixel(
                    [src[2], src[1], src[0]],
                    *src == [0xff, 0xff, 0xff, 0x00],
                    src[3],
                    mask_bit(mask, i),
                    i,
                    w,
                );
            }
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

/// Whether pixel `index`'s bit is set in a colour format's AND mask,
/// which is indexed by pixel number with no per-row padding (spice-gtk's
/// `get_pix_mask`).
fn mask_bit(mask: &[u8], index: usize) -> bool {
    mask[index / 8] & (0x80 >> (index % 8)) != 0
}

/// One RGBA pixel of a colour format with an AND mask, resolved as
/// spice-gtk's `set_cursor` resolves it. A clear mask bit makes the
/// colour opaque. A set one over white asks for the screen to be
/// inverted, which RGBA cannot express, so it becomes spice-gtk's
/// translucent grey checker (`get_pix_hack`). A set one over any other
/// colour keeps the colour with `masked_alpha`: zero for COLOR16, and
/// for COLOR32 and COLOR4 the colour's fourth byte, which is zero in
/// practice.
fn masked_pixel(
    rgb: [u8; 3],
    white: bool,
    masked_alpha: u8,
    masked: bool,
    index: usize,
    width: usize,
) -> [u8; 4] {
    let [r, g, b] = rgb;
    match (masked, white) {
        (false, _) => [r, g, b, 0xff],
        (true, true) if ((index % width) ^ (index / width)) & 1 == 1 => [0x30, 0x30, 0x30, 0xc0],
        (true, true) => [0x50, 0x50, 0x50, 0x30],
        (true, false) => [r, g, b, masked_alpha],
    }
}

/// Decode a mono cursor into `dst`, porting spice-gtk's
/// `spice_mono_edge_highlight`. A clear AND bit draws the XOR bit as
/// white or black; a set one leaves the screen alone (transparent) where
/// the XOR bit is clear and inverts it where it is set. RGBA cannot
/// invert, so an inverting pixel is drawn white, and when the AND plane
/// is entirely ones (padding bits included) the clear pixels bordering a
/// set XOR pixel are drawn black, so that a text cursor stays visible
/// over white.
fn mono_edge_highlight(width: usize, height: usize, data: &[u8], dst: &mut [[u8; 4]]) {
    const BLACK: [u8; 4] = [0x00, 0x00, 0x00, 0xff];
    const WHITE: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    const TRANSPARENT: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

    let stride = width.div_ceil(8);
    let (and, xor) = data.split_at(stride * height);
    let and_ones = and.iter().all(|&byte| byte == 0xff);
    let bit = |plane: &[u8], x: usize, y: usize| plane[stride * y + x / 8] & (0x80 >> (x % 8)) != 0;
    // A clear XOR pixel with a set one among its eight neighbours. The
    // image's outermost pixels are never edges.
    let is_edge = |x: usize, y: usize| {
        if x == 0 || x == width - 1 || y == 0 || y == height - 1 {
            return false;
        }
        !bit(xor, x, y) && (y - 1..=y + 1).any(|ny| (x - 1..=x + 1).any(|nx| bit(xor, nx, ny)))
    };

    for y in 0..height {
        for x in 0..width {
            dst[width * y + x] = if and_ones && is_edge(x, y) {
                BLACK
            } else {
                match (bit(and, x, y), bit(xor, x, y)) {
                    (_, true) => WHITE,
                    (true, false) => TRANSPARENT,
                    (false, false) => BLACK,
                }
            };
        }
    }
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
        let pixels: Vec<u8> = vec![
            0xAA, 0xBB, 0xCC, 0x00, // B=AA, G=BB, R=CC, x=00
            0x00, // AND mask: clear, so opaque
        ];
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
        // Each format at exactly the length it needs decodes, and one
        // byte short is refused. Pixels, then COLOR4's 64-byte palette,
        // then the colour formats' AND mask of one bit per pixel without
        // row padding; mono's two planes are padded to a byte per row.
        for (cursor_type, width, height, needed) in [
            (cursor_type::ALPHA, 2, 2, 16),
            (cursor_type::COLOR24, 2, 2, 12),
            (cursor_type::COLOR32, 2, 2, 16 + 1),
            (cursor_type::COLOR32, 5, 3, 60 + 2),
            (cursor_type::COLOR16, 2, 2, 8 + 1),
            (cursor_type::COLOR16, 5, 3, 30 + 2),
            (cursor_type::COLOR4, 2, 2, 2 + 64 + 1),
            (cursor_type::COLOR4, 5, 3, 9 + 64 + 2),
            (cursor_type::MONO, 2, 2, 2 * 2),
            (cursor_type::MONO, 9, 3, 2 * 6),
        ] {
            let pixels = vec![0u8; needed];
            let cursor = wire_cursor(cursor_type, width, height, 0, &pixels);
            assert!(
                decode(&cursor).is_some(),
                "cursor type {} {}x{} with {} bytes must decode",
                cursor_type,
                width,
                height,
                needed
            );
            let cursor = wire_cursor(cursor_type, width, height, 0, &pixels[..needed - 1]);
            assert!(
                decode(&cursor).is_none(),
                "cursor type {} {}x{} with short data must be refused",
                cursor_type,
                width,
                height
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
            cursor_type::MONO,
            cursor_type::COLOR4,
            cursor_type::COLOR16,
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
    fn test_cursor_over_pixel_cap_returns_none() {
        // Each side is within the per-side limit, but together they are
        // one row over the pixel cap. Mono needs only 2 bits a pixel, so
        // the data is complete and only the cap can refuse it.
        let side = 8192usize;
        assert_eq!(side * side, limits::MAX_IMAGE_PIXELS);
        let needed = CursorFormat::Mono.data_len(side, side + 1).unwrap();
        let data = vec![0u8; needed];

        let cursor = wire_cursor(cursor_type::MONO, side as u16, side as u16 + 1, 0, &data);
        assert!(decode(&cursor).is_none());
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
    fn test_color8_and_unknown_cursor_types_return_none() {
        // COLOR8 and types spice.proto does not define are refused even
        // with ample data.
        for cursor_type in [cursor_type::COLOR8, 7, 0xff] {
            let cursor = wire_cursor(cursor_type, 2, 2, 0, &[0u8; 2048]);
            assert!(decode(&cursor).is_none(), "cursor type {}", cursor_type);
        }
    }

    // The masked colour formats. Each test builds a `width`x2 image in
    // which pixel `width + 1` is masked over a colour (transparent), and
    // pixels 1 and `width + 3` are masked over white (inverting, one of
    // each checker colour). Two of the three sit on the second row, so a
    // mask read with per-row padding would misplace them unless the
    // width is a multiple of 8; each test runs with widths 5 and 8.

    const MASKED_TRANSPARENT: usize = 1;
    const MASKED_INVERTING: [usize; 2] = [1, 3];

    /// The masked pixels of a `width`x2 test image: the transparent one
    /// and the two inverting ones.
    fn masked_pixels(width: usize) -> (usize, [usize; 2]) {
        (
            width + MASKED_TRANSPARENT,
            [MASKED_INVERTING[0], width + MASKED_INVERTING[1]],
        )
    }

    /// An AND mask for `count` pixels with the given ones set, packed
    /// eight pixels a byte, most significant bit first, with no row
    /// padding.
    fn and_mask(count: usize, set: &[usize]) -> Vec<u8> {
        let mut mask = vec![0u8; count.div_ceil(8)];
        for &i in set {
            mask[i / 8] |= 0x80 >> (i % 8);
        }
        mask
    }

    /// The RGBA a masked test image decodes to, given each pixel's
    /// colour. The checker colours are spice-gtk's, by the parity of
    /// x ^ y: (1, 0) is odd and (3, 1) even.
    fn expected_masked(width: usize, colour: impl Fn(usize) -> [u8; 3]) -> Vec<u8> {
        let (transparent, inverting) = masked_pixels(width);
        (0..width * 2)
            .flat_map(|i| {
                let [r, g, b] = colour(i);
                if i == inverting[0] {
                    [0x30, 0x30, 0x30, 0xc0]
                } else if i == inverting[1] {
                    [0x50, 0x50, 0x50, 0x30]
                } else if i == transparent {
                    [r, g, b, 0x00]
                } else {
                    [r, g, b, 0xff]
                }
            })
            .collect()
    }

    #[test]
    fn test_color32_cursor_mask() {
        for width in [5usize, 8] {
            let (transparent, inverting) = masked_pixels(width);
            let colour = |i: usize| [0x10 + i as u8, 0x40 + i as u8, 0xc0 - i as u8];
            let mut data = Vec::new();
            for i in 0..width * 2 {
                let [r, g, b] = if inverting.contains(&i) {
                    [0xff; 3]
                } else {
                    colour(i)
                };
                data.extend_from_slice(&[b, g, r, 0x00]);
            }
            data.extend(and_mask(
                width * 2,
                &[transparent, inverting[0], inverting[1]],
            ));

            let cursor = wire_cursor(cursor_type::COLOR32, width as u16, 2, 0, &data);
            let img = decode(&cursor).expect("a masked COLOR32 cursor decodes");
            assert_eq!(
                img.pixels,
                expected_masked(width, colour),
                "width {}",
                width
            );
        }
    }

    #[test]
    fn test_color32_cursor_unmasked_white_is_opaque() {
        // White inverts only where the mask is set.
        let data = [0xff, 0xff, 0xff, 0x00, 0x00];
        let cursor = wire_cursor(cursor_type::COLOR32, 1, 1, 0, &data);
        assert_eq!(decode(&cursor).unwrap().pixels, [0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn test_color16_cursor_mask() {
        for width in [5usize, 8] {
            let (transparent, inverting) = masked_pixels(width);
            // Five-bit channels, all different, widened by << 3.
            let channels = |i: usize| [i as u16 + 1, i as u16 + 12, 30 - i as u16];
            let colour = |i: usize| channels(i).map(|c| (c << 3) as u8);
            let mut data = Vec::new();
            for i in 0..width * 2 {
                let pixel = if inverting.contains(&i) {
                    0x7fff
                } else {
                    let [r, g, b] = channels(i);
                    (r << 10) | (g << 5) | b
                };
                data.extend_from_slice(&u16::to_le_bytes(pixel));
            }
            data.extend(and_mask(
                width * 2,
                &[transparent, inverting[0], inverting[1]],
            ));

            let cursor = wire_cursor(cursor_type::COLOR16, width as u16, 2, 0, &data);
            let img = decode(&cursor).expect("a COLOR16 cursor decodes");
            assert_eq!(
                img.pixels,
                expected_masked(width, colour),
                "width {}",
                width
            );
        }
    }

    #[test]
    fn test_color16_cursor_channel_order() {
        // Red, green and blue at full intensity, then white with the
        // unused top bit set: masked, it is not the 0x7fff that inverts,
        // so it is transparent white.
        let mut data = Vec::new();
        for pixel in [0x7c00u16, 0x03e0, 0x001f, 0xffff] {
            data.extend_from_slice(&pixel.to_le_bytes());
        }
        data.push(0b0001_0000);
        let cursor = wire_cursor(cursor_type::COLOR16, 4, 1, 0, &data);
        assert_eq!(
            decode(&cursor).unwrap().pixels,
            [
                0xf8, 0x00, 0x00, 0xff, // red
                0x00, 0xf8, 0x00, 0xff, // green
                0x00, 0x00, 0xf8, 0xff, // blue
                0xf8, 0xf8, 0xf8, 0x00, // masked white with the top bit
            ]
        );
    }

    #[test]
    fn test_color4_cursor_mask() {
        for width in [5usize, 8] {
            let (transparent, inverting) = masked_pixels(width);
            // Palette entry k is BGRx with asymmetric channels; entry 15
            // is white. Neighbouring pixels use different entries, so
            // reading the nibbles in the wrong order shows.
            let entry = |k: usize| [0x10 * k as u8 + 1, 0x80, 0xe0 - 0x08 * k as u8];
            let index = |i: usize| {
                if inverting.contains(&i) {
                    15
                } else {
                    i % 15
                }
            };
            let colour = |i: usize| entry(index(i));

            // Indices by pixel number, two a byte, high nibble first,
            // then zeros up to spice-gtk's row-padded length.
            let mut data = vec![0u8; width.div_ceil(2) * 2];
            for i in 0..width * 2 {
                data[i / 2] |= (index(i) as u8) << if i % 2 == 0 { 4 } else { 0 };
            }
            for k in 0..15 {
                let [r, g, b] = entry(k);
                data.extend_from_slice(&[b, g, r, 0x00]);
            }
            data.extend_from_slice(&[0xff, 0xff, 0xff, 0x00]);
            data.extend(and_mask(
                width * 2,
                &[transparent, inverting[0], inverting[1]],
            ));

            let cursor = wire_cursor(cursor_type::COLOR4, width as u16, 2, 0, &data);
            let img = decode(&cursor).expect("a COLOR4 cursor decodes");
            assert_eq!(
                img.pixels,
                expected_masked(width, colour),
                "width {}",
                width
            );
        }
    }

    /// One plane of a mono cursor from rows of '0' and '1', each row
    /// padded to a whole byte with `pad`.
    fn mono_plane(rows: &[&str], pad: bool) -> Vec<u8> {
        let mut plane = Vec::new();
        for row in rows {
            let mut bits: Vec<bool> = row.chars().map(|c| c == '1').collect();
            bits.resize(row.len().div_ceil(8) * 8, pad);
            plane.extend(bits.chunks(8).map(|byte| {
                byte.iter()
                    .fold(0u8, |acc, &bit| (acc << 1) | u8::from(bit))
            }));
        }
        plane
    }

    /// A decoded mono cursor as rows of characters: '#' opaque black,
    /// 'o' opaque white, '.' transparent.
    fn mono_picture(img: &CursorImage) -> Vec<String> {
        let (pixels, _) = img.pixels.as_chunks::<4>();
        pixels
            .chunks(img.width as usize)
            .map(|row| {
                row.iter()
                    .map(|pixel| match pixel {
                        [0x00, 0x00, 0x00, 0xff] => '#',
                        [0xff, 0xff, 0xff, 0xff] => 'o',
                        [0x00, 0x00, 0x00, 0x00] => '.',
                        other => panic!("unexpected mono pixel {:?}", other),
                    })
                    .collect()
            })
            .collect()
    }

    fn decode_mono(and: &[u8], xor: &[u8], width: u16, height: u16) -> CursorImage {
        let data = [and, xor].concat();
        let cursor = wire_cursor(cursor_type::MONO, width, height, 0, &data);
        decode(&cursor).expect("a mono cursor decodes")
    }

    #[test]
    fn test_mono_cursor_and_xor() {
        // Each AND/XOR combination: AND 0 draws XOR as black or white,
        // AND 1 is transparent over XOR 0 and inverting (white) over
        // XOR 1. The AND plane is not all ones, so there is no edge.
        for (and, xor, picture) in [
            (
                ["00110", "00000", "11111"],
                ["01010", "00000", "00000"],
                ["#o.o#", "#####", "....."],
            ),
            (
                ["00110011", "00000000", "11111111"],
                ["01010101", "00000000", "00000000"],
                ["#o.o#o.o", "########", "........"],
            ),
        ] {
            let width = and[0].len() as u16;
            let img = decode_mono(&mono_plane(&and, false), &mono_plane(&xor, false), width, 3);
            assert_eq!(mono_picture(&img), picture);
        }
    }

    #[test]
    fn test_mono_cursor_edge_highlight() {
        // spice-gtk's own example (spice-util.c): with the AND plane all
        // ones, the inverting pixels turn white and the clear pixels
        // around them black, except on the image's outermost pixels.
        let xor = ["00000", "00000", "00100", "00100", "00000", "00000"];
        let picture = [".....", ".###.", ".#o#.", ".#o#.", ".###.", "....."];
        let and = vec![0xffu8; 6];
        let img = decode_mono(&and, &mono_plane(&xor, false), 5, 6);
        assert_eq!(mono_picture(&img), picture);

        // The same at a width of 8, with the shape beside the last
        // column and the first row, which stay transparent.
        let xor = ["00000000", "00000010", "00000000", "00000000"];
        let picture = ["........", ".....#o.", ".....##.", "........"];
        let img = decode_mono(&[0xffu8; 4], &mono_plane(&xor, false), 8, 4);
        assert_eq!(mono_picture(&img), picture);
    }

    #[test]
    fn test_mono_cursor_padding_bits_count_for_edge_highlight() {
        // spice-gtk tests whole bytes of the AND plane for all ones, so
        // a 5-wide plane whose padding bits are clear gets no edge.
        let xor = ["00000", "00000", "00100", "00100", "00000", "00000"];
        let and = mono_plane(&["11111"; 6], false);
        let img = decode_mono(&and, &mono_plane(&xor, false), 5, 6);
        assert_eq!(
            mono_picture(&img),
            [".....", ".....", "..o..", "..o..", ".....", "....."]
        );
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
