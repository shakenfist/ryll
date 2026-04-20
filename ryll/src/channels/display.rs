/// Display channel handler - surfaces, image rendering
use anyhow::Result;
use flate2::read::ZlibDecoder;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

use crate::app::ByteCounter;
use crate::bugreport::{DecodeResult, DisplaySnapshot, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::settings;
use shakenfist_spice_compression::{
    decompress_glz, decompress_lz, decompress_spice_lz4, quic_decode, DecompressedImage,
    GlzDictionary,
};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, DisplayInit, DrawBase, ImageDescriptor, MessageHeader, Ping, SetAck,
    SurfaceCreate,
};
use shakenfist_spice_protocol::parse::{read_i32_le, read_u16_le, read_u32_le, read_u64_le};
use shakenfist_spice_protocol::{
    display_client, display_server, ChannelType, ImageType, IMAGE_FLAGS_CACHE_ME,
};

use super::ChannelEvent;

const SPICE_VIDEO_CODEC_TYPE_MJPEG: u8 = 1;

pub(crate) fn extract_dht_segments(jpeg: &[u8]) -> Vec<u8> {
    let mut dht = Vec::new();
    let mut i = 0;
    while i + 3 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xC4 {
            let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
            if i + seg_len <= jpeg.len() {
                dht.extend_from_slice(&jpeg[i..i + seg_len]);
            }
            i += seg_len;
        } else if marker == 0xD8 || marker == 0x00 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
        } else if marker == 0xD9 || marker == 0xDA {
            break;
        } else if i + 3 < jpeg.len() {
            let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize + 2;
            i += seg_len;
        } else {
            break;
        }
    }
    dht
}

pub(crate) fn inject_dht(jpeg: &[u8], dht: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(jpeg.len() + dht.len());
    let mut i = 0;
    // SOI marker
    if jpeg.len() >= 2 && jpeg[0] == 0xFF && jpeg[1] == 0xD8 {
        out.extend_from_slice(&jpeg[..2]);
        i = 2;
    }
    // skip APP0/APP1 if present (bounds check matches extract_dht_segments)
    while i + 3 < jpeg.len() && jpeg[i] == 0xFF && (jpeg[i + 1] == 0xE0 || jpeg[i + 1] == 0xE1) {
        let seg_len = (u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize).saturating_add(2);
        if i + seg_len > jpeg.len() {
            break;
        }
        out.extend_from_slice(&jpeg[i..i + seg_len]);
        i += seg_len;
    }
    out.extend_from_slice(dht);
    out.extend_from_slice(&jpeg[i..]);
    out
}

pub(crate) fn decode_mjpeg_frame(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    let pixels = match decoder.decode() {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "MJPEG decode error: {}, data_len={}, header={:02x?}",
                e,
                data.len(),
                &data[..data.len().min(16)]
            );
            return None;
        }
    };
    let info = decoder.info()?;
    let w = info.width as u32;
    let h = info.height as u32;

    let rgba = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            let mut out = Vec::with_capacity(pixels.len() * 4 / 3);
            for chunk in pixels.chunks(3) {
                out.push(chunk[0]);
                out.push(chunk[1]);
                out.push(chunk[2]);
                out.push(255);
            }
            out
        }
        jpeg_decoder::PixelFormat::L8 => {
            let mut out = Vec::with_capacity(pixels.len() * 4);
            for &gray in &pixels {
                out.push(gray);
                out.push(gray);
                out.push(gray);
                out.push(255);
            }
            out
        }
        other => {
            warn!("MJPEG decode: unsupported pixel format {:?}", other);
            return None;
        }
    };

    Some((rgba, w, h))
}

struct StreamState {
    surface_id: u32,
    codec_type: u8,
    dest_top: u32,
    dest_left: u32,
    dest_bottom: u32,
    dest_right: u32,
    cached_dht: Option<Vec<u8>>,
}

/// Maximum number of recent decode results to keep in the snapshot.
const MAX_RECENT_DECODES: usize = 20;

/// GLZ dictionary shared across all display channels.
pub type SharedGlzDictionary = Arc<GlzDictionary>;

pub struct DisplayChannel {
    channel_id: u8,
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    repaint_notify: Arc<Notify>,
    buffer: Vec<u8>,
    glz_dictionary: SharedGlzDictionary,
    image_cache: HashMap<u64, Vec<u8>>,
    streams: HashMap<u32, StreamState>,
    capture: Option<Arc<CaptureSession>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<TrafficBuffers>,
    snapshot: Arc<Mutex<DisplaySnapshot>>,
    recent_decodes: VecDeque<DecodeResult>,
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,
    bytes_in: u64,
    bytes_out: u64,
}

impl DisplayChannel {
    pub fn new_shared_glz_dictionary() -> SharedGlzDictionary {
        Arc::new(GlzDictionary::new())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: u8,
        stream: SpiceStream,
        event_tx: mpsc::Sender<ChannelEvent>,
        repaint_notify: Arc<Notify>,
        capture: Option<Arc<CaptureSession>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<TrafficBuffers>,
        snapshot: Arc<Mutex<DisplaySnapshot>>,
        glz_dictionary: SharedGlzDictionary,
    ) -> Self {
        DisplayChannel {
            channel_id,
            stream,
            event_tx,
            repaint_notify,
            buffer: Vec::with_capacity(1024 * 1024),
            glz_dictionary,
            image_cache: HashMap::new(),
            streams: HashMap::new(),
            capture,
            byte_counter,
            traffic,
            snapshot,
            recent_decodes: VecDeque::new(),
            ack_generation: 0,
            ack_window: 0,
            message_count: 0,
            last_ack: 0,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    /// Run the display channel event loop
    pub async fn run(&mut self) -> Result<()> {
        info!("display: channel started");

        // Send display init message
        self.send_init().await?;

        loop {
            // Read data into buffer
            let mut chunk = [0u8; 262144]; // 256KB chunks for images
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
                info!("display: channel disconnected");
                self.event_tx
                    .send(ChannelEvent::Disconnected(ChannelType::Display))
                    .await
                    .ok();
                self.repaint_notify.notify_one();
                break;
            }

            self.byte_counter.add(n as u64);
            if let Some(ref c) = self.capture {
                c.packet_received("display", &chunk[..n]);
            }
            self.buffer.extend_from_slice(&chunk[..n]);
            self.bytes_in += n as u64;

            // Process complete messages
            self.process_messages().await?;
        }

        Ok(())
    }

    async fn send_init(&mut self) -> Result<()> {
        let init = DisplayInit {
            cache_id: 1,
            cache_size: 20 * 1024 * 1024, // 20MB
            glz_dict_id: 1,
            glz_dict_window: 3 * 1024 * 1024, // 3MB
        };

        let mut payload = Vec::new();
        init.write(&mut payload)?;
        let msg = make_message(display_client::INIT, &payload);

        if settings::is_verbose() {
            logging::log_detail(&format!(
                "cache_id={}, cache_size={}, glz_dict_id={}, glz_dict_window={}",
                init.cache_id, init.cache_size, init.glz_dict_id, init.glz_dict_window
            ));
        }

        self.send_with_log(display_client::INIT, &msg).await
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
                "display",
                header.message_type,
                message_names::display_server(header.message_type),
                &raw,
            );

            // Extract message payload
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
        let msg_type_str = message_names::display_server(msg_type);

        // Log all messages in verbose mode
        if settings::is_verbose() {
            logging::log_message(
                "received",
                "display",
                msg_type,
                msg_type_str,
                payload.len() as u32,
            );
        }

        match msg_type {
            display_server::SURFACE_CREATE => {
                let surface = SurfaceCreate::read(payload)?;
                info!(
                    "display: surface created: channel={}, id={}, {}x{}",
                    self.channel_id, surface.surface_id, surface.width, surface.height
                );

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "surface_id={}, width={}, height={}, format={}, flags={}",
                        surface.surface_id,
                        surface.width,
                        surface.height,
                        surface.format,
                        surface.flags
                    ));
                }

                self.event_tx
                    .send(ChannelEvent::SurfaceCreated {
                        display_channel_id: self.channel_id,
                        surface_id: surface.surface_id,
                        width: surface.width,
                        height: surface.height,
                    })
                    .await
                    .ok();
                self.repaint_notify.notify_one();
            }

            display_server::SURFACE_DESTROY => {
                if payload.len() >= 4 {
                    let surface_id = read_u32_le(payload, 0);
                    info!("display: surface destroyed: id={}", surface_id);

                    if settings::is_verbose() {
                        logging::log_detail(&format!("surface_id={}", surface_id));
                    }

                    self.event_tx
                        .send(ChannelEvent::SurfaceDestroyed {
                            display_channel_id: self.channel_id,
                            surface_id,
                        })
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                }
            }

            display_server::DRAW_COPY => {
                self.handle_draw_copy(payload).await?;
            }

            display_server::DRAW_COMPOSITE => {
                debug!("display: draw_composite (not yet implemented, skipping)");
            }

            display_server::MONITORS_CONFIG => {
                if payload.len() >= 8 {
                    let count = read_u16_le(payload, 0);
                    let max_allowed = read_u16_le(payload, 2);
                    info!(
                        "display: monitors_config: count={}, max_allowed={}, channel_id={}",
                        count, max_allowed, self.channel_id
                    );
                    let mut offset = 4;
                    for i in 0..count {
                        if offset + 28 > payload.len() {
                            break;
                        }
                        let head_id = read_u32_le(payload, offset);
                        let surface_id = read_u32_le(payload, offset + 4);
                        let width = read_u32_le(payload, offset + 8);
                        let height = read_u32_le(payload, offset + 12);
                        let x = read_u32_le(payload, offset + 16);
                        let y = read_u32_le(payload, offset + 20);
                        let flags = read_u32_le(payload, offset + 24);
                        info!(
                            "display: monitors_config[{}]: head_id={}, surface_id={}, {}x{}, pos=({},{}), flags={:#x}",
                            i, head_id, surface_id, width, height, x, y, flags
                        );
                        offset += 28;
                    }
                }
            }

            display_server::MARK => {
                debug!("display: mark");
                self.event_tx.send(ChannelEvent::DisplayMark).await.ok();
                self.repaint_notify.notify_one();
            }

            display_server::SET_ACK => {
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
                let response = make_message(display_client::ACK_SYNC, &ack_payload);
                self.send_with_log(display_client::ACK_SYNC, &response)
                    .await?;
            }

            display_server::PING => {
                let ping = Ping::read(payload)?;

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "ping_id={}, timestamp={}",
                        ping.id, ping.timestamp
                    ));
                }

                let mut pong_payload = Vec::new();
                ping.write_pong(&mut pong_payload)?;
                let response = make_message(display_client::PONG, &pong_payload);
                self.send_with_log(display_client::PONG, &response).await?;
            }

            display_server::INVALIDATE_LIST => {
                // Wire: u16 count + (u8 type + u64 id) per entry
                if payload.len() >= 2 {
                    let count = read_u16_le(payload, 0) as usize;
                    let entry_size = 9; // u8 type + u64 id
                    let mut removed = 0usize;
                    let mut glz_removed = 0usize;
                    for i in 0..count {
                        let offset = 2 + i * entry_size + 1; // skip type byte
                        if offset + 8 > payload.len() {
                            break;
                        }
                        let id = read_u64_le(payload, offset);
                        if self.image_cache.remove(&id).is_some() {
                            removed += 1;
                        }
                        if self.glz_dictionary.remove(&id) {
                            glz_removed += 1;
                        }
                    }
                    debug!(
                        "display: inval_list: removed {}/{} from image_cache, \
                         {}/{} from glz_dict (cache now {}, glz now {})",
                        removed,
                        count,
                        glz_removed,
                        count,
                        self.image_cache.len(),
                        self.glz_dictionary.len()
                    );
                }
            }

            display_server::INVAL_ALL_PIXMAPS => {
                let glz_len = self.glz_dictionary.len();
                debug!(
                    "display: inval_all_pixmaps: clearing {} cached images + {} glz entries",
                    self.image_cache.len(),
                    glz_len
                );
                self.image_cache.clear();
                self.glz_dictionary.clear();
            }

            display_server::RESET => {
                info!("display: reset");
                self.image_cache.clear();
                self.glz_dictionary.clear();
            }

            display_server::STREAM_CREATE => {
                if payload.len() >= 50 {
                    let surface_id = read_u32_le(payload, 0);
                    let stream_id = read_u32_le(payload, 4);
                    let _flags = payload[8];
                    let codec_type = payload[9];
                    let stream_w = read_u32_le(payload, 18);
                    let stream_h = read_u32_le(payload, 22);
                    let dest_top = read_u32_le(payload, 34);
                    let dest_left = read_u32_le(payload, 38);
                    let dest_bottom = read_u32_le(payload, 42);
                    let dest_right = read_u32_le(payload, 46);

                    info!(
                        "display: stream_create: id={}, surface={}, codec={}, {}x{}, \
                         dest=({},{})→({},{})",
                        stream_id,
                        surface_id,
                        codec_type,
                        stream_w,
                        stream_h,
                        dest_left,
                        dest_top,
                        dest_right,
                        dest_bottom
                    );

                    self.streams.insert(
                        stream_id,
                        StreamState {
                            surface_id,
                            codec_type,
                            dest_top,
                            dest_left,
                            dest_bottom,
                            dest_right,
                            cached_dht: None,
                        },
                    );
                }
            }

            display_server::STREAM_DATA | display_server::STREAM_DATA_SIZED => {
                let (stream_id, dest, jpeg_data) = if msg_type == display_server::STREAM_DATA_SIZED
                {
                    if payload.len() < 36 {
                        return Ok(());
                    }
                    let id = read_u32_le(payload, 0);
                    let dest_top = read_u32_le(payload, 16);
                    let dest_left = read_u32_le(payload, 20);
                    let dest_bottom = read_u32_le(payload, 24);
                    let dest_right = read_u32_le(payload, 28);
                    let data_size = read_u32_le(payload, 32) as usize;
                    let data = &payload[36..36 + data_size.min(payload.len() - 36)];
                    (
                        id,
                        Some((dest_top, dest_left, dest_bottom, dest_right)),
                        data,
                    )
                } else {
                    if payload.len() < 12 {
                        return Ok(());
                    }
                    let id = read_u32_le(payload, 0);
                    let data_size = read_u32_le(payload, 8) as usize;
                    let data = &payload[12..12 + data_size.min(payload.len() - 12)];
                    (id, None, data)
                };

                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    if stream.codec_type == SPICE_VIDEO_CODEC_TYPE_MJPEG {
                        let (top, left, bottom, right) = dest.unwrap_or((
                            stream.dest_top,
                            stream.dest_left,
                            stream.dest_bottom,
                            stream.dest_right,
                        ));
                        let w = right.saturating_sub(left);
                        let h = bottom.saturating_sub(top);

                        let dht = extract_dht_segments(jpeg_data);
                        let decode_data;
                        let frame_data = if !dht.is_empty() {
                            stream.cached_dht = Some(dht);
                            jpeg_data
                        } else if let Some(ref cached) = stream.cached_dht {
                            decode_data = inject_dht(jpeg_data, cached);
                            &decode_data
                        } else {
                            jpeg_data
                        };

                        match decode_mjpeg_frame(frame_data) {
                            Some((rgba, fw, fh)) => {
                                debug!(
                                    "display: stream {} MJPEG frame {}x{} → ({},{})",
                                    stream_id, fw, fh, left, top
                                );
                                self.event_tx
                                    .send(ChannelEvent::ImageReady {
                                        display_channel_id: self.channel_id,
                                        surface_id: stream.surface_id,
                                        left,
                                        top,
                                        width: fw.min(w),
                                        height: fh.min(h),
                                        pixels: rgba,
                                        image_id: 0,
                                    })
                                    .await
                                    .ok();
                                self.repaint_notify.notify_one();
                            }
                            None => {
                                debug!("display: stream {} MJPEG decode failed", stream_id);
                            }
                        }
                    } else {
                        debug!(
                            "display: stream {} unsupported codec {}",
                            stream_id, stream.codec_type
                        );
                    }
                }
            }

            display_server::STREAM_CLIP => {
                if payload.len() >= 4 {
                    let stream_id = read_u32_le(payload, 0);
                    debug!("display: stream_clip id={}", stream_id);
                }
            }

            display_server::STREAM_DESTROY => {
                if payload.len() >= 4 {
                    let stream_id = read_u32_le(payload, 0);
                    info!("display: stream_destroy id={}", stream_id);
                    self.streams.remove(&stream_id);
                }
            }

            display_server::STREAM_ACTIVATE_REPORT => {
                debug!("display: stream_activate_report");
            }

            _ => {
                // Unknown message - log with hex dump
                logging::log_unknown(
                    "display",
                    "received",
                    msg_type,
                    payload.len() as u32,
                    payload,
                );
            }
        }

        Ok(())
    }

    async fn handle_draw_copy(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 21 {
            warn!("display: draw_copy payload too short");
            return Ok(());
        }

        let base = DrawBase::read(payload)?;
        let left = base.left;
        let top = base.top;

        if settings::is_verbose() {
            logging::log_detail(&format!(
                "surface={}, rect=({},{}) to ({},{}), clip_type={}",
                base.surface_id, left, top, base.right, base.bottom, base.clip_type
            ));
        }

        // SpiceCopy starts with src_bitmap offset (u32) pointing to SpiceImage
        let copy_start = base.end_offset;
        if payload.len() < copy_start + 4 {
            warn!("display: draw_copy: payload too short for SpiceCopy");
            return Ok(());
        }

        let src_bitmap_offset = read_u32_le(payload, copy_start) as usize;

        if payload.len() < copy_start + 36 {
            warn!("display: draw_copy: payload too short for SpiceCopy header");
            return Ok(());
        }

        let src_top = read_u32_le(payload, copy_start + 4);
        let src_left = read_u32_le(payload, copy_start + 8);
        let src_bottom = read_u32_le(payload, copy_start + 12);
        let src_right = read_u32_le(payload, copy_start + 16);
        let rop_descriptor = read_u16_le(payload, copy_start + 20);
        let scale_mode = payload[copy_start + 22];
        let mask_flags = payload[copy_start + 23];
        let mask_pos_x = read_i32_le(payload, copy_start + 24);
        let mask_pos_y = read_i32_le(payload, copy_start + 28);
        let mask_bitmap_offset = read_u32_le(payload, copy_start + 32) as usize;

        if settings::is_verbose() {
            debug!(
                "display: draw_copy detail: rop={:#x}, scale={}, mask={:#x}, \
                 pos=({},{}), mask_bmp={}, clip_type={}, clip_rects={}",
                rop_descriptor,
                scale_mode,
                mask_flags,
                mask_pos_x,
                mask_pos_y,
                mask_bitmap_offset,
                base.clip_type,
                base.clip_rects.len()
            );
        }

        if src_bitmap_offset == 0 {
            warn!("display: draw_copy: null src_bitmap");
            return Ok(());
        }

        let image_start = src_bitmap_offset;
        if payload.len() < image_start + ImageDescriptor::SIZE {
            warn!(
                "display: draw_copy: payload too short for image descriptor \
                 (have {}, need {}, offset={})",
                payload.len(),
                image_start + ImageDescriptor::SIZE,
                src_bitmap_offset
            );
            return Ok(());
        }

        let img_desc = ImageDescriptor::read(&payload[image_start..])?;
        let image_type = ImageType::from_u8(img_desc.image_type);

        let image_data_start = image_start + ImageDescriptor::SIZE;
        if image_data_start >= payload.len() {
            warn!("display: draw_copy: no image data");
            return Ok(());
        }

        let image_data = &payload[image_data_start..];

        debug!(
            "display: draw_copy: surface={}, pos=({},{}), size={}x{}, type={:?}, id={}, \
             flags={}, data_bytes={}",
            base.surface_id,
            left,
            top,
            img_desc.width,
            img_desc.height,
            image_type,
            img_desc.image_id,
            img_desc.flags,
            image_data.len()
        );

        // Decode/decompress based on type
        let decompressed: Option<DecompressedImage> = match image_type {
            Some(ImageType::Pixmap) => {
                // BitmapData: format(u8) + flags(u8) + x(u32) +
                // y(u32) + stride(u32) + palette_addr(u32) = 18 bytes,
                // then raw pixel rows.
                if image_data.len() < 18 {
                    warn!("display: pixmap BitmapData header too short");
                    None
                } else {
                    let bmp_fmt = image_data[0];
                    let bmp_flags = image_data[1];
                    let bmp_width = read_u32_le(image_data, 2);
                    let bmp_height = read_u32_le(image_data, 6);
                    let bmp_stride = read_u32_le(image_data, 10);
                    let top_down = (bmp_flags & 0x04) != 0;
                    // palette_addr at offset 14..18 (ignored for 32-bit)
                    let pixel_data = &image_data[18..];

                    debug!(
                        "display: pixmap fmt={}, flags={:#x}, {}x{}, stride={}, top_down={}",
                        bmp_fmt, bmp_flags, bmp_width, bmp_height, bmp_stride, top_down
                    );

                    // Only 32-bit BGRX (fmt=8) and RGBA (fmt=9) are supported
                    if bmp_fmt != 8 && bmp_fmt != 9 {
                        warn!(
                            "display: pixmap format {} not supported (only 32-bit)",
                            bmp_fmt
                        );
                        return Ok(());
                    }

                    let width = bmp_width;
                    let height = bmp_height;
                    let stride = bmp_stride as usize;
                    let pixel_count = (width as usize) * (height as usize);
                    let expected_pixels = pixel_count * 4;

                    if stride * (height as usize) > pixel_data.len() {
                        warn!(
                            "display: pixmap data too short (have {}, need {})",
                            pixel_data.len(),
                            stride * (height as usize)
                        );
                        None
                    } else {
                        let mut rgba = vec![0u8; expected_pixels];
                        let row_bytes = (width as usize) * 4;
                        for y in 0..height as usize {
                            // Rows may be bottom-up unless TOP_DOWN flag is set
                            let src_y = if top_down {
                                y
                            } else {
                                (height as usize) - 1 - y
                            };
                            let src_row = &pixel_data[src_y * stride..src_y * stride + row_bytes];
                            let dst_start = y * row_bytes;
                            for x in 0..width as usize {
                                let si = x * 4;
                                let di = dst_start + x * 4;
                                // BGRX/BGRA -> RGBA
                                rgba[di] = src_row[si + 2]; // R
                                rgba[di + 1] = src_row[si + 1]; // G
                                rgba[di + 2] = src_row[si]; // B
                                rgba[di + 3] = if bmp_fmt == 9 { src_row[si + 3] } else { 255 };
                            }
                        }
                        Some(DecompressedImage::new(
                            width,
                            height,
                            rgba,
                            img_desc.image_id,
                        ))
                    }
                }
            }
            Some(ImageType::GlzRgb) => {
                // Skip 4-byte data_size prefix before the GLZ header
                if image_data.len() < 4 {
                    warn!("display: GLZ image data too short");
                    None
                } else {
                    match decompress_glz(&image_data[4..], &self.glz_dictionary).await {
                        Ok(img) => Some(img),
                        Err(e) => {
                            warn!("display: GLZ decompression failed: {}", e);
                            None
                        }
                    }
                }
            }
            Some(ImageType::LzRgb) => {
                // Skip 4-byte data_size prefix before the LZ header
                if image_data.len() < 4 {
                    warn!("display: LZ image data too short");
                    None
                } else {
                    match decompress_lz(&image_data[4..]) {
                        Ok(img) => Some(img),
                        Err(e) => {
                            warn!("display: LZ decompression failed: {}", e);
                            None
                        }
                    }
                }
            }
            Some(ImageType::ZlibGlzRgb) => {
                // Zlib-compressed GLZ data: glz_data_size (u32 LE) +
                // compressed_size (u32 LE) + zlib-compressed GLZ stream
                if image_data.len() < 8 {
                    warn!("display: ZLIB_GLZ_RGB data too short");
                    None
                } else {
                    let _glz_size = read_u32_le(image_data, 0) as usize;
                    let zlib_size = read_u32_le(image_data, 4) as usize;

                    let zlib_data = &image_data[8..8 + zlib_size.min(image_data.len() - 8)];
                    let mut decoder = ZlibDecoder::new(zlib_data);
                    let mut glz_data = Vec::new();
                    match decoder.read_to_end(&mut glz_data) {
                        Ok(_) => match decompress_glz(&glz_data, &self.glz_dictionary).await {
                            Ok(img) => Some(img),
                            Err(e) => {
                                warn!("display: ZLIB_GLZ_RGB GLZ decompression failed: {}", e);
                                None
                            }
                        },
                        Err(e) => {
                            warn!("display: ZLIB_GLZ_RGB zlib decompression failed: {}", e);
                            None
                        }
                    }
                }
            }
            Some(ImageType::Lz4) => {
                // SPICE LZ4 format:
                //   1 byte: top_down flag
                //   1 byte: spice bitmap format
                //   then per-row blocks: 4-byte BE size + LZ4 compressed row
                //
                // Note: unlike LZ_RGB/GLZ_RGB, the LZ4 data does NOT have
                // a data_size u32 prefix — the pixel data starts immediately
                // after the ImageDescriptor.
                let width = img_desc.width as usize;
                let height = img_desc.height as usize;
                decompress_spice_lz4(image_data, width, height)
            }
            Some(ImageType::FromCache) => {
                // Look up in cache
                if let Some(pixels) = self.image_cache.get(&img_desc.image_id) {
                    Some(DecompressedImage::new(
                        img_desc.width,
                        img_desc.height,
                        pixels.clone(),
                        img_desc.image_id,
                    ))
                } else {
                    warn!("display: image {} not in cache", img_desc.image_id);
                    None
                }
            }
            Some(ImageType::Jpeg) => {
                // JPEG: BinaryData wrapper (4-byte data_size + JPEG stream)
                if image_data.len() < 4 {
                    warn!("display: JPEG data too short");
                    None
                } else {
                    let data_size = read_u32_le(image_data, 0) as usize;
                    let jpeg_data = &image_data[4..4 + data_size.min(image_data.len() - 4)];
                    match image::load_from_memory_with_format(jpeg_data, image::ImageFormat::Jpeg) {
                        Ok(img) => {
                            let rgba = img.to_rgba8();
                            Some(DecompressedImage::new(
                                rgba.width(),
                                rgba.height(),
                                rgba.into_raw(),
                                img_desc.image_id,
                            ))
                        }
                        Err(e) => {
                            warn!("display: JPEG decode failed: {}", e);
                            None
                        }
                    }
                }
            }
            Some(ImageType::Quic) => {
                if image_data.len() < 4 {
                    warn!("display: QUIC data too short");
                    None
                } else {
                    let data_size = read_u32_le(image_data, 0) as usize;
                    let quic_data = &image_data[4..4 + data_size.min(image_data.len() - 4)];
                    match quic_decode(quic_data, img_desc.width, img_desc.height) {
                        Some(rgba) => Some(DecompressedImage::new(
                            img_desc.width,
                            img_desc.height,
                            rgba,
                            img_desc.image_id,
                        )),
                        None => {
                            warn!("display: QUIC decode failed");
                            None
                        }
                    }
                }
            }
            Some(ImageType::LzPalette) => {
                warn!(
                    "display: LzPalette images require palette data (not yet implemented), \
                     id={}",
                    img_desc.image_id
                );
                None
            }
            Some(ImageType::Surface) => {
                warn!(
                    "display: Surface-to-surface copy (not yet implemented), id={}",
                    img_desc.image_id
                );
                None
            }
            Some(ImageType::FromCacheLossless) => {
                warn!(
                    "display: FromCacheLossless (not yet implemented), id={}",
                    img_desc.image_id
                );
                None
            }
            Some(ImageType::JpegAlpha) => {
                warn!(
                    "display: JpegAlpha requires separate alpha plane (not yet implemented), \
                     id={}",
                    img_desc.image_id
                );
                None
            }
            None => {
                warn!("display: unknown image type byte: {}", img_desc.image_type);
                None
            }
        };

        // Record this decode attempt in the snapshot history.
        let is_from_cache = matches!(image_type, Some(ImageType::FromCache));
        self.record_decode(DecodeResult {
            image_type: format!("{:?}", image_type),
            image_id: img_desc.image_id,
            width: img_desc.width,
            height: img_desc.height,
            from_cache: is_from_cache,
            success: decompressed.is_some(),
            timestamp_secs: self.traffic.elapsed().as_secs_f64(),
        });

        if decompressed.is_none() {
            info!(
                "display: draw_copy: no pixels produced for type={:?}",
                image_type
            );
        }

        if let Some(img) = decompressed {
            let is_glz = matches!(
                image_type,
                Some(ImageType::GlzRgb) | Some(ImageType::ZlibGlzRgb)
            );
            if is_glz {
                // GLZ images are always cached -- they form the shared
                // dictionary that cross-frame references depend on.
                // insert() also notifies any waiters blocked on a
                // cross-frame reference.
                self.glz_dictionary.insert(img.image_id, img.pixels.clone());

                // Evict images outside the sliding window. The server
                // only generates cross-frame references to images
                // within win_head_dist of the current image_id.
                if img.win_head_dist > 0 {
                    let oldest_valid = img.image_id.saturating_sub(img.win_head_dist as u64);
                    let evicted = self.glz_dictionary.evict_older_than(oldest_valid);
                    if evicted > 0 {
                        debug!(
                            "display: glz eviction: removed {} entries older than id {} \
                             (win_head_dist={}, dict now {})",
                            evicted,
                            oldest_valid,
                            img.win_head_dist,
                            self.glz_dictionary.len()
                        );
                    }
                }
            } else if (img_desc.flags & IMAGE_FLAGS_CACHE_ME) != 0 {
                // Only cache non-GLZ images when the server requests it.
                self.image_cache.insert(img.image_id, img.pixels.clone());
            }

            let mut out_width = img.width;
            let mut out_height = img.height;
            let mut out_pixels = img.pixels;

            let crop_w = src_right.saturating_sub(src_left);
            let crop_h = src_bottom.saturating_sub(src_top);
            if crop_w > 0
                && crop_h > 0
                && (src_left != 0 || src_top != 0 || crop_w != out_width || crop_h != out_height)
            {
                let src_w = out_width as usize;
                let src_h = out_height as usize;
                let left_px = (src_left as usize).min(src_w);
                let top_px = (src_top as usize).min(src_h);
                let right_px = (src_right as usize).min(src_w);
                let bottom_px = (src_bottom as usize).min(src_h);

                if right_px > left_px && bottom_px > top_px {
                    let new_w = right_px - left_px;
                    let new_h = bottom_px - top_px;
                    let mut cropped = vec![0u8; new_w * new_h * 4];
                    for y in 0..new_h {
                        let src_off = ((top_px + y) * src_w + left_px) * 4;
                        let dst_off = y * new_w * 4;
                        cropped[dst_off..dst_off + new_w * 4]
                            .copy_from_slice(&out_pixels[src_off..src_off + new_w * 4]);
                    }
                    out_width = new_w as u32;
                    out_height = new_h as u32;
                    out_pixels = cropped;
                }
            }

            let dest_left = left;
            let dest_top = top;
            let dest_right = dest_left.saturating_add(out_width);
            let dest_bottom = dest_top.saturating_add(out_height);

            if base.clip_type == 1 && !base.clip_rects.is_empty() {
                for (clip_left, clip_top, clip_right, clip_bottom) in &base.clip_rects {
                    let il = dest_left.max(*clip_left);
                    let it = dest_top.max(*clip_top);
                    let ir = dest_right.min(*clip_right);
                    let ib = dest_bottom.min(*clip_bottom);
                    if ir <= il || ib <= it {
                        continue;
                    }

                    let sub_w = (ir - il) as usize;
                    let sub_h = (ib - it) as usize;
                    let x_off = (il - dest_left) as usize;
                    let y_off = (it - dest_top) as usize;
                    let out_w_usize = out_width as usize;
                    let mut sub_pixels = vec![0u8; sub_w * sub_h * 4];

                    for y in 0..sub_h {
                        let src_off = ((y_off + y) * out_w_usize + x_off) * 4;
                        let dst_off = y * sub_w * 4;
                        sub_pixels[dst_off..dst_off + sub_w * 4]
                            .copy_from_slice(&out_pixels[src_off..src_off + sub_w * 4]);
                    }

                    self.event_tx
                        .send(ChannelEvent::ImageReady {
                            display_channel_id: self.channel_id,
                            surface_id: base.surface_id,
                            left: il,
                            top: it,
                            width: sub_w as u32,
                            height: sub_h as u32,
                            pixels: sub_pixels,
                            image_id: img.image_id,
                        })
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                }
            } else {
                self.event_tx
                    .send(ChannelEvent::ImageReady {
                        display_channel_id: self.channel_id,
                        surface_id: base.surface_id,
                        left,
                        top,
                        width: out_width,
                        height: out_height,
                        pixels: out_pixels,
                        image_id: img.image_id,
                    })
                    .await
                    .ok();
                self.repaint_notify.notify_one();
            }
        }

        Ok(())
    }

    /// Record a decode result and update the snapshot.
    fn record_decode(&mut self, decode: DecodeResult) {
        self.recent_decodes.push_back(decode);
        if self.recent_decodes.len() > MAX_RECENT_DECODES {
            self.recent_decodes.pop_front();
        }
    }

    /// Sync local state to the shared snapshot.
    fn update_snapshot(&self) {
        let mut snap = self.snapshot.lock().unwrap();
        snap.ack_generation = self.ack_generation;
        snap.ack_window = self.ack_window;
        snap.message_count = self.message_count;
        snap.last_ack = self.last_ack;
        snap.bytes_in = self.bytes_in;
        snap.bytes_out = self.bytes_out;
        let glz_len = self.glz_dictionary.len();
        let glz_bytes = self.glz_dictionary.total_bytes();
        let glz_ids = self.glz_dictionary.image_ids();
        snap.image_cache_entries = self.image_cache.len() + glz_len;
        snap.image_cache_bytes =
            self.image_cache.values().map(|v| v.len()).sum::<usize>() + glz_bytes;
        snap.image_cache_ids = {
            let mut ids: Vec<u64> = self.image_cache.keys().copied().chain(glz_ids).collect();
            ids.sort_unstable();
            ids
        };
        snap.recent_decodes = self.recent_decodes.clone();
    }

    async fn send_ack(&mut self) -> Result<()> {
        let msg = make_message(display_client::ACK, &[]);
        self.send_with_log(display_client::ACK, &msg).await?;
        self.last_ack = self.message_count;
        Ok(())
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        let msg_name = message_names::display_client(msg_type);
        if settings::is_verbose() {
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "display", msg_type, msg_name, payload_size);
        }
        self.traffic
            .record_sent("display", msg_type, msg_name, data);
        let result = self.send(data).await;
        self.update_snapshot();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            c.packet_sent("display", data);
        }
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // extract_dht_segments tests
    // -------------------------------------------------------------------------

    /// Build a minimal JPEG byte sequence:
    ///   SOI (FF D8) + one segment (marker + 2-byte BE length + payload) + EOI (FF D9)
    ///
    /// `length` in the JPEG encoding is `payload.len() + 2` (includes the 2 length bytes).
    fn make_jpeg_with_marker(marker: u8, payload: &[u8]) -> Vec<u8> {
        let seg_len = (payload.len() + 2) as u16;
        let mut jpeg = vec![0xFF, 0xD8]; // SOI
        jpeg.push(0xFF);
        jpeg.push(marker);
        jpeg.extend_from_slice(&seg_len.to_be_bytes());
        jpeg.extend_from_slice(payload);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    #[test]
    fn extract_dht_segments_finds_dht_marker() {
        // Build a JPEG with a DHT segment (marker 0xC4).
        let payload = vec![0x01, 0x02, 0x03, 0x04];
        let jpeg = make_jpeg_with_marker(0xC4, &payload);

        let dht = extract_dht_segments(&jpeg);

        // The returned data should start with the FF C4 marker.
        assert!(!dht.is_empty(), "expected non-empty DHT output");
        assert_eq!(dht[0], 0xFF);
        assert_eq!(dht[1], 0xC4);
        // Length field (big-endian) = payload.len() + 2
        let encoded_len = u16::from_be_bytes([dht[2], dht[3]]) as usize;
        assert_eq!(encoded_len, payload.len() + 2);
        // Payload bytes are present.
        assert_eq!(&dht[4..], payload.as_slice());
    }

    #[test]
    fn extract_dht_segments_no_dht_returns_empty() {
        // Build a JPEG with a comment segment (0xFE) — not a DHT.
        let jpeg = make_jpeg_with_marker(0xFE, b"hello");
        let dht = extract_dht_segments(&jpeg);
        assert!(dht.is_empty(), "expected empty Vec when no DHT present");
    }

    #[test]
    fn extract_dht_segments_empty_input_returns_empty() {
        let dht = extract_dht_segments(&[]);
        assert!(dht.is_empty());
    }

    // -------------------------------------------------------------------------
    // inject_dht tests
    // -------------------------------------------------------------------------

    #[test]
    fn inject_dht_inserts_after_soi_when_no_app_markers() {
        // Plain JPEG: SOI + some non-APP data + EOI
        let jpeg = vec![0xFF, 0xD8, 0xAA, 0xBB, 0xCC, 0xFF, 0xD9];
        let dht = vec![0xFF, 0xC4, 0x00, 0x04, 0x01, 0x02];

        let out = inject_dht(&jpeg, &dht);

        // Output should start with SOI.
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        // Immediately followed by the injected DHT.
        assert_eq!(&out[2..2 + dht.len()], dht.as_slice());
        // Then the remaining data (everything after SOI in the original).
        assert_eq!(&out[2 + dht.len()..], &jpeg[2..]);
    }

    #[test]
    fn inject_dht_inserts_after_app0_segment() {
        // Build a JPEG: SOI + APP0 (0xE0) segment + remaining data.
        let app0_payload = vec![0x4A, 0x46, 0x49, 0x46, 0x00]; // "JFIF\0"
        let app0_seg_len = (app0_payload.len() + 2) as u16;
        let mut jpeg = vec![0xFF, 0xD8]; // SOI
        jpeg.push(0xFF);
        jpeg.push(0xE0); // APP0
        jpeg.extend_from_slice(&app0_seg_len.to_be_bytes());
        jpeg.extend_from_slice(&app0_payload);
        // Some remaining data
        jpeg.extend_from_slice(&[0xDE, 0xAD]);

        let dht = vec![0xFF, 0xC4, 0x00, 0x04, 0x01, 0x02];

        let out = inject_dht(&jpeg, &dht);

        // Output should start with SOI.
        assert_eq!(&out[..2], &[0xFF, 0xD8]);

        // Next: APP0 segment (marker + length bytes + payload).
        let app0_total = 2 + 2 + app0_payload.len(); // FF E0 + len_bytes + payload
        let app0_in_out = &out[2..2 + app0_total];
        assert_eq!(app0_in_out[0], 0xFF);
        assert_eq!(app0_in_out[1], 0xE0);

        // After APP0: injected DHT.
        let dht_start = 2 + app0_total;
        assert_eq!(&out[dht_start..dht_start + dht.len()], dht.as_slice());

        // After DHT: the remaining bytes from the original JPEG.
        let remaining_start = dht_start + dht.len();
        assert_eq!(&out[remaining_start..], &[0xDE, 0xAD]);
    }

    #[test]
    fn inject_dht_output_structure_soi_app_dht_rest() {
        // Quick structural check: SOI, then optional APP markers, then DHT, then rest.
        let jpeg = vec![0xFF, 0xD8, 0x01, 0x02, 0x03];
        let dht = vec![0xFF, 0xC4, 0x00, 0x02];

        let out = inject_dht(&jpeg, &dht);

        // SOI must be first.
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        // DHT must appear before the original trailing bytes.
        let dht_pos = out.windows(dht.len()).position(|w| w == dht.as_slice());
        assert!(dht_pos.is_some(), "DHT not found in output");
        let dht_end = dht_pos.unwrap() + dht.len();
        // The remaining original data (after SOI) should follow the DHT.
        assert_eq!(&out[dht_end..], &jpeg[2..]);
    }

    // -------------------------------------------------------------------------
    // decode_mjpeg_frame tests
    // -------------------------------------------------------------------------

    #[test]
    fn decode_mjpeg_frame_valid_jpeg_returns_rgba() {
        use image::{DynamicImage, RgbImage};
        use std::io::Cursor;

        // Create a tiny 2×2 solid-red image and encode it as JPEG.
        let rgb = RgbImage::from_fn(2, 2, |_x, _y| image::Rgb([255u8, 0, 0]));
        let img = DynamicImage::ImageRgb8(rgb);
        let mut jpeg_data = Vec::new();
        img.write_to(&mut Cursor::new(&mut jpeg_data), image::ImageFormat::Jpeg)
            .expect("failed to encode test JPEG");

        let result = decode_mjpeg_frame(&jpeg_data);
        assert!(result.is_some(), "expected Some for valid JPEG");

        let (rgba, w, h) = result.unwrap();
        assert_eq!(w, 2, "width should be 2");
        assert_eq!(h, 2, "height should be 2");
        // RGBA: 4 bytes per pixel.
        assert_eq!(rgba.len(), 2 * 2 * 4, "expected 16 bytes of RGBA data");
    }

    #[test]
    fn decode_mjpeg_frame_empty_input_returns_none() {
        let result = decode_mjpeg_frame(&[]);
        assert!(result.is_none(), "expected None for empty input");
    }

    #[test]
    fn decode_mjpeg_frame_truncated_input_returns_none() {
        // A few bytes that look like a JPEG start but are truncated.
        let result = decode_mjpeg_frame(&[0xFF, 0xD8, 0xFF, 0xE0]);
        assert!(result.is_none(), "expected None for truncated JPEG");
    }
}
