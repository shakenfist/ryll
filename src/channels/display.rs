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
        info!("Display channel started");

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
                info!("Display channel disconnected");
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
                    "Surface created: id={}, {}x{}",
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
                    info!("Surface destroyed: id={}", surface_id);

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

            display_server::MARK => {
                debug!("Display mark");
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

            display_server::INVALIDATE_LIST => {
                debug!("Invalidate list received");

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
                info!("Display reset");
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
            warn!("draw_copy payload too short");
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

        // Skip clip data based on clip_type
        let clip_offset = if base.clip_type == 0 {
            DrawCopyBase::SIZE
        } else {
            // Has clip data - skip it (simplified)
            DrawCopyBase::SIZE + 4 // Minimal clip skip
        };

        // Find image descriptor
        // The layout after clip is: rop_descriptor (2 bytes), scale_mode (1 byte),
        // mask offset (4 bytes), then image offset (4 bytes)
        let image_offset_pos = clip_offset + 7;

        if payload.len() < image_offset_pos + 4 {
            warn!("draw_copy: not enough data for image offset");
            return Ok(());
        }

        let image_offset = u32::from_le_bytes([
            payload[image_offset_pos],
            payload[image_offset_pos + 1],
            payload[image_offset_pos + 2],
            payload[image_offset_pos + 3],
        ]) as usize;

        // Adjust offset relative to start of payload
        let actual_offset = if image_offset > 0 {
            image_offset - MessageHeader::SIZE - DrawCopyBase::SIZE
        } else {
            image_offset_pos + 4
        };

        if payload.len() < actual_offset + ImageDescriptor::SIZE {
            warn!("draw_copy: not enough data for image descriptor");
            return Ok(());
        }

        let img_desc = ImageDescriptor::read(&payload[actual_offset..])?;

        let image_type = ImageType::from_u8(img_desc.image_type);

        if settings::is_verbose() {
            logging::log_detail(&format!(
                "image: type={:?}, size={}x{}, id={}, flags={}",
                image_type, img_desc.width, img_desc.height, img_desc.image_id, img_desc.flags
            ));
        } else {
            debug!(
                "draw_copy: surface={}, pos=({},{}), image_type={:?}, size={}x{}",
                base.surface_id, left, top, image_type, img_desc.width, img_desc.height
            );
        }

        // Image data starts after descriptor
        let image_data_start = actual_offset + ImageDescriptor::SIZE;
        if image_data_start >= payload.len() {
            warn!("draw_copy: no image data");
            return Ok(());
        }

        let image_data = &payload[image_data_start..];

        // Decompress based on type
        let decompressed: Option<DecompressedImage> = match image_type {
            Some(ImageType::GlzRgb) => match decompress_glz(image_data, &self.previous_images) {
                Ok(img) => Some(img),
                Err(e) => {
                    warn!("GLZ decompression failed: {}", e);
                    None
                }
            },
            Some(ImageType::LzRgb) => match decompress_lz(image_data) {
                Ok(img) => Some(img),
                Err(e) => {
                    warn!("LZ decompression failed: {}", e);
                    None
                }
            },
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
                    warn!("Image {} not in cache", img_desc.image_id);
                    None
                }
            }
            _ => {
                debug!("Unsupported image type: {:?}", image_type);
                None
            }
        };

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
