/// Display channel handler - surfaces, image rendering
use anyhow::Result;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::decompression::{decompress_glz, decompress_lz, DecompressedImage};
use crate::protocol::link::SpiceStream;
use crate::protocol::logging::{self, message_names};
use crate::protocol::messages::{
    make_message, DisplayInit, DrawCopyBase, ImageDescriptor, MessageHeader, Ping, SetAck,
    SurfaceCreate,
};
use crate::protocol::{display_client, display_server, ChannelType, ImageType};
use crate::settings;

use super::ChannelEvent;

pub struct DisplayChannel {
    stream: SpiceStream,
    event_tx: mpsc::Sender<ChannelEvent>,
    buffer: Vec<u8>,
    previous_images: HashMap<u64, Vec<u8>>,
    previous_images_order: Vec<u64>,
    max_cached_images: usize,
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,
    bytes_in: u64,
    bytes_out: u64,
}

impl DisplayChannel {
    pub fn new(stream: SpiceStream, event_tx: mpsc::Sender<ChannelEvent>) -> Self {
        DisplayChannel {
            stream,
            event_tx,
            buffer: Vec::with_capacity(1024 * 1024), // 1MB buffer for images
            previous_images: HashMap::new(),
            previous_images_order: Vec::new(),
            max_cached_images: 100,
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
                break;
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
                    "display: surface created: id={}, {}x{}",
                    surface.surface_id, surface.width, surface.height
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
                        surface_id: surface.surface_id,
                        width: surface.width,
                        height: surface.height,
                    })
                    .await
                    .ok();
            }

            display_server::SURFACE_DESTROY => {
                if payload.len() >= 4 {
                    let surface_id =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    info!("display: surface destroyed: id={}", surface_id);

                    if settings::is_verbose() {
                        logging::log_detail(&format!("surface_id={}", surface_id));
                    }

                    self.event_tx
                        .send(ChannelEvent::SurfaceDestroyed { surface_id })
                        .await
                        .ok();
                }
            }

            display_server::DRAW_COPY => {
                self.handle_draw_copy(payload).await?;
            }

            display_server::DRAW_COMPOSITE => {
                debug!("display: draw_composite (not yet implemented, skipping)");
            }

            display_server::MARK => {
                debug!("display: mark");
                self.event_tx.send(ChannelEvent::DisplayMark).await.ok();
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

            display_server::INVALIDATE_LIST | display_server::INVAL_ALL_PIXMAPS => {
                debug!("display: invalidate pixmaps received (opcode {})", msg_type);

                if settings::is_verbose() {
                    logging::log_detail(&format!(
                        "clearing {} cached images",
                        self.previous_images.len()
                    ));
                }

                // Clear cached images
                self.previous_images.clear();
                self.previous_images_order.clear();
            }

            display_server::RESET => {
                info!("display: reset");
                self.previous_images.clear();
                self.previous_images_order.clear();
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
        if payload.len() < DrawCopyBase::SIZE {
            warn!("display: draw_copy payload too short");
            return Ok(());
        }

        let base = DrawCopyBase::read(payload)?;
        let left = base.left;
        let top = base.top;

        if settings::is_verbose() {
            logging::log_detail(&format!(
                "surface={}, rect=({},{}) to ({},{}), clip_type={}",
                base.surface_id, left, top, base.right, base.bottom, base.clip_type
            ));
        }

        // After DrawCopyBase the SpiceCopy structure is (mini-header mode):
        //   src_bitmap  SPICE_ADDRESS (u32)   4 bytes
        //   src_area    SpiceRect (4 x i32)  16 bytes
        //   rop_descriptor  u16               2 bytes
        //   scale_mode      u8                1 byte
        //   mask.flags      u8                1 byte
        //   mask.pos        2 x i32           8 bytes
        //   mask.bitmap     SPICE_ADDRESS     4 bytes
        // Total SpiceCopy header = 36 bytes
        // The SpiceImage (ImageDescriptor + data) follows inline.
        let copy_header_size: usize = 4 + 16 + 2 + 1 + 1 + 8 + 4;
        let image_start = DrawCopyBase::SIZE + copy_header_size;

        if payload.len() < image_start + ImageDescriptor::SIZE {
            warn!(
                "display: draw_copy: payload too short for image descriptor \
                 (have {}, need {})",
                payload.len(),
                image_start + ImageDescriptor::SIZE
            );
            return Ok(());
        }

        let img_desc = ImageDescriptor::read(&payload[image_start..])?;
        let image_type = ImageType::from_u8(img_desc.image_type);

        // Image data starts after the descriptor
        let image_data_start = image_start + ImageDescriptor::SIZE;
        if image_data_start >= payload.len() {
            warn!("display: draw_copy: no image data");
            return Ok(());
        }

        let image_data = &payload[image_data_start..];

        info!(
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
                // Raw 32-bit BGRX pixel data — convert to RGBA
                let width = img_desc.width;
                let height = img_desc.height;
                let expected = (width as usize)
                    .checked_mul(height as usize)
                    .and_then(|n| n.checked_mul(4))
                    .unwrap_or(0);
                if expected > 0 && image_data.len() >= expected {
                    let mut rgba = vec![0u8; expected];
                    for i in 0..(width as usize * height as usize) {
                        let src = i * 4;
                        let dst = i * 4;
                        rgba[dst] = image_data[src + 2]; // R
                        rgba[dst + 1] = image_data[src + 1]; // G
                        rgba[dst + 2] = image_data[src]; // B
                        rgba[dst + 3] = 255; // A
                    }
                    Some(DecompressedImage {
                        width,
                        height,
                        pixels: rgba,
                        image_id: img_desc.image_id,
                    })
                } else {
                    warn!(
                        "display: pixmap data too short (have {}, need {})",
                        image_data.len(),
                        expected
                    );
                    None
                }
            }
            Some(ImageType::GlzRgb) => {
                // Skip 4-byte data_size prefix before the GLZ header
                if image_data.len() < 4 {
                    warn!("display: GLZ image data too short");
                    None
                } else {
                    match decompress_glz(&image_data[4..], &self.previous_images) {
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
            Some(ImageType::FromCache) => {
                // Look up in cache
                if let Some(pixels) = self.previous_images.get(&img_desc.image_id) {
                    Some(DecompressedImage {
                        width: img_desc.width,
                        height: img_desc.height,
                        pixels: pixels.clone(),
                        image_id: img_desc.image_id,
                    })
                } else {
                    warn!("display: image {} not in cache", img_desc.image_id);
                    None
                }
            }
            _ => {
                warn!(
                    "display: unsupported image type: {:?} (raw byte={})",
                    image_type, img_desc.image_type
                );
                None
            }
        };

        if decompressed.is_none() {
            info!(
                "display: draw_copy: no pixels produced for type={:?}",
                image_type
            );
        }

        if let Some(img) = decompressed {
            // Cache for GLZ dictionary
            if img.image_id != 0 {
                self.cache_image(img.image_id, img.pixels.clone());
            }

            // Send to UI
            self.event_tx
                .send(ChannelEvent::ImageReady {
                    surface_id: base.surface_id,
                    left,
                    top,
                    width: img.width,
                    height: img.height,
                    pixels: img.pixels,
                    image_id: img.image_id,
                })
                .await
                .ok();
        }

        Ok(())
    }

    fn cache_image(&mut self, image_id: u64, pixels: Vec<u8>) {
        // Add to cache
        self.previous_images.insert(image_id, pixels);
        self.previous_images_order.push(image_id);

        // Evict old entries if over limit
        while self.previous_images_order.len() > self.max_cached_images {
            if let Some(old_id) = self.previous_images_order.first().copied() {
                self.previous_images_order.remove(0);
                self.previous_images.remove(&old_id);
            }
        }
    }

    async fn send_ack(&mut self) -> Result<()> {
        let msg = make_message(display_client::ACK, &[]);
        self.send_with_log(display_client::ACK, &msg).await?;
        self.last_ack = self.message_count;
        Ok(())
    }

    async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
        if settings::is_verbose() {
            let msg_type_str = message_names::display_client(msg_type);
            let payload_size = data.len().saturating_sub(6) as u32;
            logging::log_message("sent", "display", msg_type, msg_type_str, payload_size);
        }
        self.send(data).await
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }
}
