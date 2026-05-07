/// Display channel handler - surfaces, image rendering
use anyhow::Result;
use flate2::read::ZlibDecoder;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

use crate::snapshots::{DecodeResult, DisplaySnapshot};
use crate::{
    ByteCounter, CaptureSink, LogConfig, NotificationEntry, NotificationSource, TrafficSink,
};
use shakenfist_spice_compression::{
    decompress_glz, decompress_lz, decompress_spice_lz4, quic_decode, DecompressedImage,
    GlzDictionary,
};
use shakenfist_spice_protocol::constants::ropd;
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, DisplayInit, DrawBase, ImageDescriptor, MessageHeader, Notify as NotifyMessage,
    Ping, SetAck, SpiceAlphaBlend, SpiceBlackness, SpiceBrush, SpiceFill, SpiceOpaque, SpicePoint,
    SpiceTransparent, SurfaceCreate,
};
use shakenfist_spice_protocol::parse::{read_i32_le, read_u16_le, read_u32_le, read_u64_le};
use shakenfist_spice_protocol::{
    display_client, display_server, warn_once, ChannelType, ImageType, NotifySeverity,
    IMAGE_FLAGS_CACHE_ME,
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

/// What we decided to do with a DRAW_FILL after classifying its
/// rop/brush/mask. Extracted from `handle_draw_fill` so the
/// parse-and-classify logic is independently testable without
/// standing up a full `DisplayChannel`.
#[derive(Debug, Clone)]
enum FillOutcome {
    /// Happy path: paint `colour` (RGBA) into `base.rect` with
    /// `base.clip_rects`.
    Paint {
        base: DrawBase,
        colour: [u8; 4],
        /// True when a non-null mask was present; we still paint,
        /// but unmasked, and the caller has already warn_once'd.
        masked_fallback: bool,
    },
    /// ROP descriptor wasn't SPICE_ROPD_OP_PUT — skip.
    SkipNonOpPut { rop: u16 },
    /// Brush type was NONE — skip.
    SkipNoneBrush,
    /// Brush type was PATTERN — skip (not yet supported).
    SkipPatternBrush,
}

/// Emit a one-line "surface / rect / clip_type" preview of a draw-op
/// payload when `-v` verbose mode is set. Handlers call this as a
/// cheap header on entry; the real decoding still happens in the
/// `decode_*` classifier. Doing it here keeps the nine image- and
/// mask-bearing handlers from each copy-pasting the same eight
/// lines.
fn log_draw_base_if_verbose(log_config: LogConfig, payload: &[u8], op_name: &str) {
    if !log_config.verbose {
        return;
    }
    if let Ok(base) = DrawBase::read(payload) {
        logging::log_detail(&format!(
            "{}: surface={}, rect=({},{})-({},{}), clip_type={}",
            op_name, base.surface_id, base.left, base.top, base.right, base.bottom, base.clip_type,
        ));
    }
}

fn decode_draw_fill(payload: &[u8]) -> std::io::Result<FillOutcome> {
    let base = DrawBase::read(payload)?;
    let (fill, _consumed) = SpiceFill::read(&payload[base.end_offset..])?;

    if fill.rop_descriptor != ropd::OP_PUT {
        return Ok(FillOutcome::SkipNonOpPut {
            rop: fill.rop_descriptor,
        });
    }

    let color = match fill.brush {
        SpiceBrush::Solid { color } => color,
        SpiceBrush::None => return Ok(FillOutcome::SkipNoneBrush),
        SpiceBrush::Pattern { .. } => return Ok(FillOutcome::SkipPatternBrush),
    };

    let masked_fallback = fill.mask.flags != 0 || fill.mask.bitmap_offset != 0;

    let rgba = [
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
        0xff,
    ];

    Ok(FillOutcome::Paint {
        base,
        colour: rgba,
        masked_fallback,
    })
}

/// Outcome of decoding a DRAW_BLACKNESS or DRAW_WHITENESS payload.
///
/// Both opcodes share the same 13-byte body (SpiceQMask) and both
/// reduce to painting a solid RGBA rect, so the decoder returns the
/// geometry and whether a non-null mask was present; the caller
/// supplies the colour.
#[derive(Debug, Clone)]
enum SolidFillOutcome {
    Paint {
        base: DrawBase,
        masked_fallback: bool,
    },
}

fn decode_draw_solid_fill(payload: &[u8]) -> std::io::Result<SolidFillOutcome> {
    let base = DrawBase::read(payload)?;
    let body = SpiceBlackness::read(&payload[base.end_offset..])?;
    let masked_fallback = body.mask.flags != 0 || body.mask.bitmap_offset != 0;
    Ok(SolidFillOutcome::Paint {
        base,
        masked_fallback,
    })
}

/// Outcome of decoding a COPY_BITS payload.
///
/// COPY_BITS has no mask, brush, or rop — every payload is an
/// intra-surface pixel copy — so the decoder just classifies the
/// geometry and leaves the actual copy to `DisplaySurface::copy_bits`.
#[derive(Debug, Clone)]
enum CopyBitsOutcome {
    Copy {
        base: DrawBase,
        src_x: u32,
        src_y: u32,
    },
}

fn decode_copy_bits(payload: &[u8]) -> std::io::Result<CopyBitsOutcome> {
    let base = DrawBase::read(payload)?;
    let src_pos = SpicePoint::read(&payload[base.end_offset..])?;
    // Wire type is int32; source coords are logically unsigned
    // indices into the surface buffer. Clamp negatives to 0.
    let src_x = src_pos.x.max(0) as u32;
    let src_y = src_pos.y.max(0) as u32;
    Ok(CopyBitsOutcome::Copy { base, src_x, src_y })
}

/// Outcome of decoding a DRAW_BLEND payload.
///
/// DRAW_BLEND shares the 36-byte SpiceCopy/SpiceBlend header with
/// DRAW_COPY (draw.h defines the latter as a typedef for the
/// former). On OP_PUT the blend is identical to a DRAW_COPY; any
/// other ROP would require compositing that we don't implement, so
/// we warn_once and skip.
#[derive(Debug, Clone)]
enum BlendOutcome {
    Paint {
        base: DrawBase,
        src_bitmap_offset: usize,
        src_top: u32,
        src_left: u32,
        src_bottom: u32,
        src_right: u32,
    },
    SkipNonOpPut {
        rop: u16,
    },
}

fn decode_draw_blend(payload: &[u8]) -> std::io::Result<BlendOutcome> {
    let base = DrawBase::read(payload)?;
    let copy_start = base.end_offset;
    if payload.len() < copy_start + 36 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Not enough data for SpiceBlend",
        ));
    }
    let src_bitmap_offset = read_u32_le(payload, copy_start) as usize;
    let src_top = read_u32_le(payload, copy_start + 4);
    let src_left = read_u32_le(payload, copy_start + 8);
    let src_bottom = read_u32_le(payload, copy_start + 12);
    let src_right = read_u32_le(payload, copy_start + 16);
    let rop = read_u16_le(payload, copy_start + 20);
    // scale_mode and mask are parsed-through silently, matching
    // how handle_draw_copy treats them today.

    if rop != ropd::OP_PUT {
        return Ok(BlendOutcome::SkipNonOpPut { rop });
    }
    Ok(BlendOutcome::Paint {
        base,
        src_bitmap_offset,
        src_top,
        src_left,
        src_bottom,
        src_right,
    })
}

/// Outcome of decoding a DRAW_OPAQUE payload.
///
/// DRAW_OPAQUE carries a brush between src_area and rop_descriptor,
/// parsed via the phase-1 SpiceOpaque struct. On OP_PUT the brush
/// is irrelevant (per draw.h semantics: the brush only participates
/// in ROPs that reference the pattern source), so we ignore it
/// silently. On any other ROP we warn_once and skip — the rop/brush
/// combination would require compositing we don't implement.
#[derive(Debug, Clone)]
enum OpaqueOutcome {
    Paint {
        base: DrawBase,
        src_bitmap_offset: usize,
        src_top: u32,
        src_left: u32,
        src_bottom: u32,
        src_right: u32,
    },
    SkipNonOpPut {
        rop: u16,
    },
}

fn decode_draw_opaque(payload: &[u8]) -> std::io::Result<OpaqueOutcome> {
    let base = DrawBase::read(payload)?;
    let (opaque, _consumed) = SpiceOpaque::read(&payload[base.end_offset..])?;

    if opaque.rop_descriptor != ropd::OP_PUT {
        return Ok(OpaqueOutcome::SkipNonOpPut {
            rop: opaque.rop_descriptor,
        });
    }
    Ok(OpaqueOutcome::Paint {
        base,
        src_bitmap_offset: opaque.src_bitmap as usize,
        src_top: opaque.src_top,
        src_left: opaque.src_left,
        src_bottom: opaque.src_bottom,
        src_right: opaque.src_right,
    })
}

/// Outcome of decoding a DRAW_TRANSPARENT payload.
///
/// Chroma-key blit with no skip case: every payload is paintable
/// (the compositor at the surface side is what inspects the chroma
/// colour against each pixel).
#[derive(Debug, Clone)]
enum TransparentOutcome {
    Paint {
        base: DrawBase,
        chroma_rgba: [u8; 4],
        src_bitmap_offset: usize,
        src_top: u32,
        src_left: u32,
        src_bottom: u32,
        src_right: u32,
    },
}

fn decode_draw_transparent(payload: &[u8]) -> std::io::Result<TransparentOutcome> {
    let base = DrawBase::read(payload)?;
    let transparent = SpiceTransparent::read(&payload[base.end_offset..])?;
    // src_color is BGRX little-endian, same convention as brush colour.
    let chroma_rgba = [
        ((transparent.src_color >> 16) & 0xff) as u8,
        ((transparent.src_color >> 8) & 0xff) as u8,
        (transparent.src_color & 0xff) as u8,
        0xff,
    ];
    Ok(TransparentOutcome::Paint {
        base,
        chroma_rgba,
        src_bitmap_offset: transparent.src_bitmap as usize,
        src_top: transparent.src_top,
        src_left: transparent.src_left,
        src_bottom: transparent.src_bottom,
        src_right: transparent.src_right,
    })
}

/// Outcome of decoding a DRAW_ALPHA_BLEND payload.
///
/// `alpha == 0` is short-circuited here (matches canvas_base.c
/// which early-returns without touching the destination); the
/// handler simply returns without decoding the image. `alpha_flags`
/// is carried through so the handler can warn_once on non-zero
/// values even though we paint anyway.
#[derive(Debug, Clone)]
enum AlphaBlendOutcome {
    Paint {
        base: DrawBase,
        alpha: u8,
        alpha_flags: u16,
        src_bitmap_offset: usize,
        src_top: u32,
        src_left: u32,
        src_bottom: u32,
        src_right: u32,
    },
    SkipZeroAlpha,
}

fn decode_draw_alpha_blend(payload: &[u8]) -> std::io::Result<AlphaBlendOutcome> {
    let base = DrawBase::read(payload)?;
    let ab = SpiceAlphaBlend::read(&payload[base.end_offset..])?;
    if ab.alpha == 0 {
        return Ok(AlphaBlendOutcome::SkipZeroAlpha);
    }
    Ok(AlphaBlendOutcome::Paint {
        base,
        alpha: ab.alpha,
        alpha_flags: ab.alpha_flags,
        src_bitmap_offset: ab.src_bitmap as usize,
        src_top: ab.src_top,
        src_left: ab.src_left,
        src_bottom: ab.src_bottom,
        src_right: ab.src_right,
    })
}

/// How `decode_image_and_emit` should composite the decoded
/// source pixels into the destination surface.
#[derive(Debug, Clone, Copy)]
enum CompositeMode {
    /// Straight overwrite — emits ChannelEvent::ImageReady.
    /// Used by DRAW_COPY, DRAW_BLEND, DRAW_OPAQUE.
    Overwrite,
    /// Chroma-key — emits ChannelEvent::ImageReadyChroma.
    /// Used by DRAW_TRANSPARENT.
    ChromaKey { chroma_rgba: [u8; 4] },
    /// Constant-alpha source-over — emits ChannelEvent::ImageReadyAlpha.
    /// Used by DRAW_ALPHA_BLEND.
    AlphaBlend { alpha: u8 },
}

#[allow(clippy::too_many_arguments)]
fn build_image_event(
    composite: CompositeMode,
    display_channel_id: u8,
    surface_id: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    image_id: u64,
) -> ChannelEvent {
    match composite {
        CompositeMode::Overwrite => ChannelEvent::ImageReady {
            display_channel_id,
            surface_id,
            left,
            top,
            width,
            height,
            pixels,
            image_id,
        },
        CompositeMode::ChromaKey { chroma_rgba } => ChannelEvent::ImageReadyChroma {
            display_channel_id,
            surface_id,
            left,
            top,
            width,
            height,
            pixels,
            chroma_rgba,
            image_id,
        },
        CompositeMode::AlphaBlend { alpha } => ChannelEvent::ImageReadyAlpha {
            display_channel_id,
            surface_id,
            left,
            top,
            width,
            height,
            pixels,
            alpha,
            image_id,
        },
    }
}

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
    capture: Option<Arc<dyn CaptureSink>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    log_config: LogConfig,
    snapshot: Arc<Mutex<DisplaySnapshot>>,
    recent_decodes: VecDeque<DecodeResult>,
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
        capture: Option<Arc<dyn CaptureSink>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<dyn TrafficSink>,
        snapshot: Arc<Mutex<DisplaySnapshot>>,
        glz_dictionary: SharedGlzDictionary,
        log_config: LogConfig,
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
            log_config,
            snapshot,
            recent_decodes: VecDeque::new(),
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
            self.last_recv_ts_secs = Some(self.traffic.elapsed().as_secs_f64());

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

        if self.log_config.verbose {
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
        if self.log_config.verbose {
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

                if self.log_config.verbose {
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

                    if self.log_config.verbose {
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

            display_server::COPY_BITS => {
                self.handle_copy_bits(payload).await?;
            }

            display_server::DRAW_FILL => {
                self.handle_draw_fill(payload).await?;
            }

            display_server::DRAW_OPAQUE => {
                self.handle_draw_opaque(payload).await?;
            }

            display_server::DRAW_COPY => {
                self.handle_draw_copy(payload).await?;
            }

            display_server::DRAW_BLEND => {
                self.handle_draw_blend(payload).await?;
            }

            display_server::DRAW_BLACKNESS => {
                self.handle_draw_blackness(payload).await?;
            }

            display_server::DRAW_WHITENESS => {
                self.handle_draw_whiteness(payload).await?;
            }

            display_server::DRAW_INVERS => {
                self.handle_draw_invers(payload).await?;
            }

            display_server::DRAW_TRANSPARENT => {
                self.handle_draw_transparent(payload).await?;
            }

            display_server::DRAW_ALPHA_BLEND => {
                self.handle_draw_alpha_blend(payload).await?;
            }

            display_server::DRAW_ROP3 => {
                warn_once!(
                    "display:unimpl:draw_rop3",
                    "display: draw_rop3: unimplemented, skipping (256-entry ROP truth-table evaluator not yet ported)"
                );
                logging::log_unknown_once("display", msg_type, payload);
            }

            display_server::DRAW_STROKE => {
                warn_once!(
                    "display:unimpl:draw_stroke",
                    "display: draw_stroke: unimplemented, skipping (line/path rasteriser not yet ported)"
                );
                logging::log_unknown_once("display", msg_type, payload);
            }

            display_server::DRAW_TEXT => {
                warn_once!(
                    "display:unimpl:draw_text",
                    "display: draw_text: unimplemented, skipping (glyph rendering not yet ported)"
                );
                logging::log_unknown_once("display", msg_type, payload);
            }

            display_server::DRAW_COMPOSITE => {
                warn_once!(
                    "display:unimpl:draw_composite",
                    "display: draw_composite: unimplemented, skipping"
                );
                logging::log_unknown_once("display", msg_type, payload);
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
                SetAck::write_ack_sync(set_ack.generation, &mut ack_payload)?;
                let response = make_message(display_client::ACK_SYNC, &ack_payload);
                self.send_with_log(display_client::ACK_SYNC, &response)
                    .await?;
            }

            display_server::PING => {
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
                let response = make_message(display_client::PONG, &pong_payload);
                self.send_with_log(display_client::PONG, &response).await?;
                self.pong_send_count = self.pong_send_count.saturating_add(1);
            }

            display_server::NOTIFY => {
                let notify = NotifyMessage::read(payload)?;
                if self.log_config.verbose {
                    logging::log_detail(&format!(
                        "severity={:?}, visibility={:?}, what={}, message=\"{}\"",
                        notify.severity, notify.visibility, notify.what, notify.message,
                    ));
                }
                match notify.severity {
                    NotifySeverity::Error => {
                        warn!("display: server notify (error): {}", notify.message)
                    }
                    NotifySeverity::Warn => {
                        warn!("display: server notify (warn): {}", notify.message)
                    }
                    NotifySeverity::Info => {
                        info!("display: server notify: {}", notify.message)
                    }
                }
                let mut entry = NotificationEntry::new(
                    notify.severity,
                    NotificationSource::Spice {
                        channel: ChannelType::Display,
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
                // Unknown opcode — log hex once per msg_type, silent on repeat.
                logging::log_unknown_once("display", msg_type, payload);
            }
        }

        Ok(())
    }

    async fn handle_draw_copy(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 21 {
            warn_once!(
                "display:decode_failure:draw_copy:short_payload",
                "display: draw_copy payload too short"
            );
            return Ok(());
        }

        let base = DrawBase::read(payload)?;
        let left = base.left;
        let top = base.top;

        if self.log_config.verbose {
            logging::log_detail(&format!(
                "surface={}, rect=({},{}) to ({},{}), clip_type={}",
                base.surface_id, left, top, base.right, base.bottom, base.clip_type
            ));
        }

        // SpiceCopy starts with src_bitmap offset (u32) pointing to SpiceImage
        let copy_start = base.end_offset;
        if payload.len() < copy_start + 4 {
            warn_once!(
                "display:decode_failure:draw_copy:short_spice_copy",
                "display: draw_copy: payload too short for SpiceCopy"
            );
            return Ok(());
        }

        let src_bitmap_offset = read_u32_le(payload, copy_start) as usize;

        if payload.len() < copy_start + 36 {
            warn_once!(
                "display:decode_failure:draw_copy:short_spice_copy_header",
                "display: draw_copy: payload too short for SpiceCopy header"
            );
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

        if self.log_config.verbose {
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

        self.decode_image_and_emit(
            payload,
            "draw_copy",
            &base,
            src_bitmap_offset,
            src_top,
            src_left,
            src_bottom,
            src_right,
            CompositeMode::Overwrite,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn decode_image_and_emit(
        &mut self,
        payload: &[u8],
        op_name: &str,
        base: &DrawBase,
        src_bitmap_offset: usize,
        src_top: u32,
        src_left: u32,
        src_bottom: u32,
        src_right: u32,
        composite: CompositeMode,
    ) -> Result<()> {
        if src_bitmap_offset == 0 {
            logging::warn_once_impl(
                logging::intern_key(format!(
                    "display:decode_failure:{}:null_src_bitmap",
                    op_name
                )),
                &format!("display: {}: null src_bitmap", op_name),
            );
            return Ok(());
        }

        let image_start = src_bitmap_offset;
        if payload.len() < image_start + ImageDescriptor::SIZE {
            logging::warn_once_impl(
                logging::intern_key(format!(
                    "display:decode_failure:{}:short_payload_img_desc",
                    op_name
                )),
                &format!(
                    "display: {}: payload too short for image descriptor \
                     (have {}, need {}, offset={})",
                    op_name,
                    payload.len(),
                    image_start + ImageDescriptor::SIZE,
                    src_bitmap_offset
                ),
            );
            return Ok(());
        }

        let img_desc = ImageDescriptor::read(&payload[image_start..])?;
        let image_type = ImageType::from_u8(img_desc.image_type);

        let image_data_start = image_start + ImageDescriptor::SIZE;
        if image_data_start >= payload.len() {
            logging::warn_once_impl(
                logging::intern_key(format!("display:decode_failure:{}:no_image_data", op_name)),
                &format!("display: {}: no image data", op_name),
            );
            return Ok(());
        }

        let image_data = &payload[image_data_start..];

        debug!(
            "display: {}: surface={}, pos=({},{}), size={}x{}, type={:?}, id={}, \
             flags={}, data_bytes={}",
            op_name,
            base.surface_id,
            base.left,
            base.top,
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
                    warn_once!(
                        "display:decode_failure:pixmap:short_bitmap_data",
                        "display: pixmap BitmapData header too short"
                    );
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
                        warn_once!(
                            "display:decode_failure:pixmap:format_unsupported",
                            "display: pixmap format {} not supported (only 32-bit)",
                            bmp_fmt
                        );
                        return Ok(());
                    }

                    let width = bmp_width;
                    let height = bmp_height;
                    let stride = bmp_stride as usize;
                    let width_usize = width as usize;
                    let height_usize = height as usize;

                    // Guard every width/height/stride multiplication against
                    // overflow — a malicious server can send u32::MAX on
                    // any of these, and unchecked arithmetic on usize would
                    // either panic (debug) or wrap silently (release) and
                    // allow the short-data check below to pass before we
                    // index out-of-bounds in the blit loop.
                    let Some(pixel_count) = width_usize.checked_mul(height_usize) else {
                        warn_once!(
                            "display:decode_failure:pixmap:dimension_overflow",
                            "display: pixmap dimensions overflow ({} × {}), skipping",
                            width,
                            height
                        );
                        return Ok(());
                    };
                    // Cap pixel count at 64M (= 8192 × 8192 worth of RGBA,
                    // i.e. 256 MiB). No realistic SPICE pixmap draw needs
                    // more; a larger value means the server is malformed
                    // or adversarial and we refuse to allocate against
                    // attacker-controlled dimensions.
                    const MAX_PIXMAP_PIXELS: usize = 64 * 1024 * 1024;
                    if pixel_count > MAX_PIXMAP_PIXELS {
                        warn_once!(
                            "display:decode_failure:pixmap:too_large",
                            "display: pixmap {} pixels exceeds {} cap, skipping",
                            pixel_count,
                            MAX_PIXMAP_PIXELS
                        );
                        return Ok(());
                    }
                    let Some(expected_pixels) = pixel_count.checked_mul(4) else {
                        warn_once!(
                            "display:decode_failure:pixmap:dimension_overflow",
                            "display: pixmap pixel bytes overflow ({} × {} × 4), skipping",
                            width,
                            height
                        );
                        return Ok(());
                    };
                    let Some(needed_bytes) = stride.checked_mul(height_usize) else {
                        warn_once!(
                            "display:decode_failure:pixmap:dimension_overflow",
                            "display: pixmap stride × height overflow (stride={}, height={}), skipping",
                            stride,
                            height
                        );
                        return Ok(());
                    };

                    if needed_bytes > pixel_data.len() {
                        warn_once!(
                            "display:decode_failure:pixmap:short_pixel_data",
                            "display: pixmap data too short (have {}, need {})",
                            pixel_data.len(),
                            needed_bytes
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
                    warn_once!(
                        "display:decode_failure:glz:short_data",
                        "display: GLZ image data too short"
                    );
                    None
                } else {
                    match decompress_glz(&image_data[4..], &self.glz_dictionary).await {
                        Ok(img) => Some(img),
                        Err(e) => {
                            warn_once!(
                                "display:decode_failure:glz:decompress_failed",
                                "display: GLZ decompression failed: {}",
                                e
                            );
                            None
                        }
                    }
                }
            }
            Some(ImageType::LzRgb) => {
                // Skip 4-byte data_size prefix before the LZ header
                if image_data.len() < 4 {
                    warn_once!(
                        "display:decode_failure:lz:short_data",
                        "display: LZ image data too short"
                    );
                    None
                } else {
                    match decompress_lz(&image_data[4..]) {
                        Ok(img) => Some(img),
                        Err(e) => {
                            warn_once!(
                                "display:decode_failure:lz:decompress_failed",
                                "display: LZ decompression failed: {}",
                                e
                            );
                            None
                        }
                    }
                }
            }
            Some(ImageType::ZlibGlzRgb) => {
                // Zlib-compressed GLZ data: glz_data_size (u32 LE) +
                // compressed_size (u32 LE) + zlib-compressed GLZ stream
                if image_data.len() < 8 {
                    warn_once!(
                        "display:decode_failure:zlib_glz:short_data",
                        "display: ZLIB_GLZ_RGB data too short"
                    );
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
                                warn_once!(
                                    "display:decode_failure:zlib_glz:glz_failed",
                                    "display: ZLIB_GLZ_RGB GLZ decompression failed: {}",
                                    e
                                );
                                None
                            }
                        },
                        Err(e) => {
                            warn_once!(
                                "display:decode_failure:zlib_glz:zlib_failed",
                                "display: ZLIB_GLZ_RGB zlib decompression failed: {}",
                                e
                            );
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
                    warn_once!(
                        "display:decode_failure:from_cache:miss",
                        "display: image {} not in cache",
                        img_desc.image_id
                    );
                    None
                }
            }
            Some(ImageType::Jpeg) => {
                // JPEG: BinaryData wrapper (4-byte data_size + JPEG stream)
                if image_data.len() < 4 {
                    warn_once!(
                        "display:decode_failure:jpeg:short_data",
                        "display: JPEG data too short"
                    );
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
                            warn_once!(
                                "display:decode_failure:jpeg:decode_failed",
                                "display: JPEG decode failed: {}",
                                e
                            );
                            None
                        }
                    }
                }
            }
            Some(ImageType::Quic) => {
                if image_data.len() < 4 {
                    warn_once!(
                        "display:decode_failure:quic:short_data",
                        "display: QUIC data too short"
                    );
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
                            warn_once!(
                                "display:decode_failure:quic:decode_failed",
                                "display: QUIC decode failed"
                            );
                            None
                        }
                    }
                }
            }
            Some(ImageType::LzPalette) => {
                warn_once!(
                    "display:decode_failure:lz_palette:unsupported",
                    "display: LzPalette images require palette data (not yet implemented), \
                     id={}",
                    img_desc.image_id
                );
                None
            }
            Some(ImageType::Surface) => {
                warn_once!(
                    "display:decode_failure:surface:unsupported",
                    "display: Surface-to-surface copy (not yet implemented), id={}",
                    img_desc.image_id
                );
                None
            }
            Some(ImageType::FromCacheLossless) => {
                warn_once!(
                    "display:decode_failure:from_cache_lossless:unsupported",
                    "display: FromCacheLossless (not yet implemented), id={}",
                    img_desc.image_id
                );
                None
            }
            Some(ImageType::JpegAlpha) => {
                warn_once!(
                    "display:decode_failure:jpeg_alpha:unsupported",
                    "display: JpegAlpha requires separate alpha plane (not yet implemented), \
                     id={}",
                    img_desc.image_id
                );
                None
            }
            None => {
                warn_once!(
                    "display:decode_failure:image_type:unknown",
                    "display: unknown image type byte: {}",
                    img_desc.image_type
                );
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
                "display: {}: no pixels produced for type={:?}",
                op_name, image_type
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

            let dest_left = base.left;
            let dest_top = base.top;
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
                        .send(build_image_event(
                            composite,
                            self.channel_id,
                            base.surface_id,
                            il,
                            it,
                            sub_w as u32,
                            sub_h as u32,
                            sub_pixels,
                            img.image_id,
                        ))
                        .await
                        .ok();
                    self.repaint_notify.notify_one();
                }
            } else {
                self.event_tx
                    .send(build_image_event(
                        composite,
                        self.channel_id,
                        base.surface_id,
                        base.left,
                        base.top,
                        out_width,
                        out_height,
                        out_pixels,
                        img.image_id,
                    ))
                    .await
                    .ok();
                self.repaint_notify.notify_one();
            }
        }

        Ok(())
    }

    async fn handle_draw_fill(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "draw_fill");
        let outcome = decode_draw_fill(payload)?;
        match outcome {
            FillOutcome::SkipNonOpPut { rop } => {
                warn_once!(
                    "display:draw_fill:non_op_put",
                    "display: draw_fill: unhandled ROP descriptor {:#x}, skipping",
                    rop
                );
            }
            FillOutcome::SkipNoneBrush => {
                warn_once!(
                    "display:draw_fill:none_brush",
                    "display: draw_fill: NONE brush, skipping"
                );
            }
            FillOutcome::SkipPatternBrush => {
                warn_once!(
                    "display:draw_fill:pattern_brush",
                    "display: draw_fill: PATTERN brush not yet supported, skipping"
                );
            }
            FillOutcome::Paint {
                base,
                colour,
                masked_fallback,
            } => {
                if masked_fallback {
                    warn_once!(
                        "display:draw_fill:mask_present",
                        "display: draw_fill: non-null mask, painting unmasked"
                    );
                }
                self.event_tx
                    .send(ChannelEvent::FillRect {
                        display_channel_id: self.channel_id,
                        surface_id: base.surface_id,
                        rect: (base.left, base.top, base.right, base.bottom),
                        colour,
                        clip: base.clip_rects,
                    })
                    .await
                    .ok();
                self.repaint_notify.notify_one();
            }
        }

        Ok(())
    }

    async fn handle_draw_solid_fill(
        &mut self,
        payload: &[u8],
        op_name: &'static str,
        mask_warn_key: &'static str,
        colour: [u8; 4],
    ) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, op_name);
        let SolidFillOutcome::Paint {
            base,
            masked_fallback,
        } = decode_draw_solid_fill(payload)?;

        if masked_fallback {
            logging::warn_once_impl(
                mask_warn_key,
                &format!("display: {}: non-null mask, painting unmasked", op_name),
            );
        }

        self.event_tx
            .send(ChannelEvent::FillRect {
                display_channel_id: self.channel_id,
                surface_id: base.surface_id,
                rect: (base.left, base.top, base.right, base.bottom),
                colour,
                clip: base.clip_rects,
            })
            .await
            .ok();
        self.repaint_notify.notify_one();

        Ok(())
    }

    async fn handle_draw_blackness(&mut self, payload: &[u8]) -> Result<()> {
        self.handle_draw_solid_fill(
            payload,
            "draw_blackness",
            "display:draw_blackness:mask_present",
            [0, 0, 0, 0xff],
        )
        .await
    }

    async fn handle_draw_whiteness(&mut self, payload: &[u8]) -> Result<()> {
        self.handle_draw_solid_fill(
            payload,
            "draw_whiteness",
            "display:draw_whiteness:mask_present",
            [0xff, 0xff, 0xff, 0xff],
        )
        .await
    }

    async fn handle_draw_invers(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "draw_invers");
        // DRAW_INVERS shares its wire format (DrawBase + SpiceQMask) with
        // DRAW_BLACKNESS / DRAW_WHITENESS, so the phase-3 solid-fill
        // decoder slots in unchanged — only the paint semantic differs.
        let SolidFillOutcome::Paint {
            base,
            masked_fallback,
        } = decode_draw_solid_fill(payload)?;

        if masked_fallback {
            warn_once!(
                "display:draw_invers:mask_present",
                "display: draw_invers: non-null mask, inverting unmasked"
            );
        }

        self.event_tx
            .send(ChannelEvent::Invert {
                display_channel_id: self.channel_id,
                surface_id: base.surface_id,
                rect: (base.left, base.top, base.right, base.bottom),
                clip: base.clip_rects,
            })
            .await
            .ok();
        self.repaint_notify.notify_one();

        Ok(())
    }

    async fn handle_copy_bits(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "copy_bits");
        let CopyBitsOutcome::Copy { base, src_x, src_y } = decode_copy_bits(payload)?;

        self.event_tx
            .send(ChannelEvent::CopyBits {
                display_channel_id: self.channel_id,
                surface_id: base.surface_id,
                src_x,
                src_y,
                dest_rect: (base.left, base.top, base.right, base.bottom),
                clip: base.clip_rects,
            })
            .await
            .ok();
        self.repaint_notify.notify_one();

        Ok(())
    }

    async fn handle_draw_opaque(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "draw_opaque");
        match decode_draw_opaque(payload)? {
            OpaqueOutcome::SkipNonOpPut { rop } => {
                warn_once!(
                    "display:draw_opaque:non_op_put",
                    "display: draw_opaque: unhandled ROP descriptor {:#x}, skipping",
                    rop
                );
                Ok(())
            }
            OpaqueOutcome::Paint {
                base,
                src_bitmap_offset,
                src_top,
                src_left,
                src_bottom,
                src_right,
            } => {
                self.decode_image_and_emit(
                    payload,
                    "draw_opaque",
                    &base,
                    src_bitmap_offset,
                    src_top,
                    src_left,
                    src_bottom,
                    src_right,
                    CompositeMode::Overwrite,
                )
                .await
            }
        }
    }

    async fn handle_draw_blend(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "draw_blend");
        match decode_draw_blend(payload)? {
            BlendOutcome::SkipNonOpPut { rop } => {
                warn_once!(
                    "display:draw_blend:non_op_put",
                    "display: draw_blend: unhandled ROP descriptor {:#x}, skipping",
                    rop
                );
                Ok(())
            }
            BlendOutcome::Paint {
                base,
                src_bitmap_offset,
                src_top,
                src_left,
                src_bottom,
                src_right,
            } => {
                self.decode_image_and_emit(
                    payload,
                    "draw_blend",
                    &base,
                    src_bitmap_offset,
                    src_top,
                    src_left,
                    src_bottom,
                    src_right,
                    CompositeMode::Overwrite,
                )
                .await
            }
        }
    }

    async fn handle_draw_transparent(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "draw_transparent");
        let TransparentOutcome::Paint {
            base,
            chroma_rgba,
            src_bitmap_offset,
            src_top,
            src_left,
            src_bottom,
            src_right,
        } = decode_draw_transparent(payload)?;
        self.decode_image_and_emit(
            payload,
            "draw_transparent",
            &base,
            src_bitmap_offset,
            src_top,
            src_left,
            src_bottom,
            src_right,
            CompositeMode::ChromaKey { chroma_rgba },
        )
        .await
    }

    async fn handle_draw_alpha_blend(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "draw_alpha_blend");
        match decode_draw_alpha_blend(payload)? {
            AlphaBlendOutcome::SkipZeroAlpha => Ok(()),
            AlphaBlendOutcome::Paint {
                base,
                alpha,
                alpha_flags,
                src_bitmap_offset,
                src_top,
                src_left,
                src_bottom,
                src_right,
            } => {
                if alpha_flags != 0 {
                    warn_once!(
                        "display:draw_alpha_blend:alpha_flags",
                        "display: draw_alpha_blend: non-zero alpha_flags {:#x} ignored, painting with straight alpha",
                        alpha_flags
                    );
                }
                self.decode_image_and_emit(
                    payload,
                    "draw_alpha_blend",
                    &base,
                    src_bitmap_offset,
                    src_top,
                    src_left,
                    src_bottom,
                    src_right,
                    CompositeMode::AlphaBlend { alpha },
                )
                .await
            }
        }
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
        snap.last_recv_ts_secs = self.last_recv_ts_secs;
        snap.last_send_ts_secs = self.last_send_ts_secs;
        snap.ping_recv_count = self.ping_recv_count;
        snap.pong_send_count = self.pong_send_count;
        snap.last_ping_recv_ts_secs = self.last_ping_recv_ts_secs;
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
        if self.log_config.verbose {
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
        self.last_send_ts_secs = Some(self.traffic.elapsed().as_secs_f64());
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

    // -------------------------------------------------------------------------
    // decode_draw_fill tests
    // -------------------------------------------------------------------------

    /// Build a DRAW_FILL payload:
    ///   DrawBase (21 bytes, clip_type=0, no clip rects)
    ///     + SpiceBrush
    ///     + rop_descriptor (u16 LE)
    ///     + SpiceQMask (flags u8 + pos i32 i32 + bitmap_offset u32 = 13 bytes)
    fn build_draw_fill_payload(
        brush: &[u8],
        rop_descriptor: u16,
        mask_flags: u8,
        mask_bitmap_offset: u32,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        // DrawBase: surface_id, top, left, bottom, right (all u32 LE), clip_type u8
        v.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        v.extend_from_slice(&10u32.to_le_bytes()); // top
        v.extend_from_slice(&20u32.to_le_bytes()); // left
        v.extend_from_slice(&30u32.to_le_bytes()); // bottom
        v.extend_from_slice(&40u32.to_le_bytes()); // right
        v.push(0); // clip_type = SPICE_CLIP_TYPE_NONE

        // Brush (tag + body).
        v.extend_from_slice(brush);

        // rop_descriptor (u16 LE).
        v.extend_from_slice(&rop_descriptor.to_le_bytes());

        // SpiceQMask: flags u8 + pos (i32, i32) + bitmap_offset u32.
        v.push(mask_flags);
        v.extend_from_slice(&0i32.to_le_bytes()); // pos.x
        v.extend_from_slice(&0i32.to_le_bytes()); // pos.y
        v.extend_from_slice(&mask_bitmap_offset.to_le_bytes());

        v
    }

    fn solid_brush(color: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(1); // SPICE_BRUSH_TYPE_SOLID
        b.extend_from_slice(&color.to_le_bytes());
        b
    }

    fn none_brush() -> Vec<u8> {
        vec![0] // SPICE_BRUSH_TYPE_NONE
    }

    #[test]
    fn decode_draw_fill_happy_path() {
        // colour = 0x00123456 → R=0x12, G=0x34, B=0x56
        let brush = solid_brush(0x0012_3456);
        let payload = build_draw_fill_payload(
            &brush,
            ropd::OP_PUT,
            0, // mask flags
            0, // bitmap_offset (null)
        );

        match decode_draw_fill(&payload).expect("decode failed") {
            FillOutcome::Paint {
                base,
                colour,
                masked_fallback,
            } => {
                assert_eq!(colour, [0x12, 0x34, 0x56, 0xff]);
                assert!(!masked_fallback);
                assert_eq!(base.surface_id, 0);
                assert_eq!(base.top, 10);
                assert_eq!(base.left, 20);
                assert_eq!(base.bottom, 30);
                assert_eq!(base.right, 40);
            }
            other => panic!("expected Paint, got {:?}", other),
        }
    }

    #[test]
    fn decode_draw_fill_masked_fallback() {
        // Same as happy path, but with a non-null mask bitmap_offset.
        let brush = solid_brush(0x0012_3456);
        let payload = build_draw_fill_payload(&brush, ropd::OP_PUT, 0, 0x100);

        match decode_draw_fill(&payload).expect("decode failed") {
            FillOutcome::Paint {
                masked_fallback,
                colour,
                ..
            } => {
                assert!(masked_fallback, "expected masked_fallback = true");
                assert_eq!(colour, [0x12, 0x34, 0x56, 0xff]);
            }
            other => panic!("expected Paint, got {:?}", other),
        }
    }

    #[test]
    fn decode_draw_fill_non_op_put() {
        let brush = solid_brush(0x0012_3456);
        // 0x10 = OP_OR.
        let payload = build_draw_fill_payload(&brush, 0x10, 0, 0);

        match decode_draw_fill(&payload).expect("decode failed") {
            FillOutcome::SkipNonOpPut { rop } => {
                assert_eq!(rop, 0x10);
            }
            other => panic!("expected SkipNonOpPut, got {:?}", other),
        }
    }

    #[test]
    fn decode_draw_fill_none_brush() {
        let brush = none_brush();
        let payload = build_draw_fill_payload(&brush, ropd::OP_PUT, 0, 0);

        match decode_draw_fill(&payload).expect("decode failed") {
            FillOutcome::SkipNoneBrush => {}
            other => panic!("expected SkipNoneBrush, got {:?}", other),
        }
    }

    // -------------------------------------------------------------------------
    // decode_draw_solid_fill tests (DRAW_BLACKNESS / DRAW_WHITENESS)
    // -------------------------------------------------------------------------

    /// Build a DRAW_BLACKNESS / DRAW_WHITENESS payload:
    ///   DrawBase (21 bytes, clip_type=0, no clip rects)
    ///     + SpiceQMask (flags u8 + pos i32 i32 + bitmap_offset u32 = 13 bytes)
    fn build_draw_solid_fill_payload(mask_flags: u8, mask_bitmap_offset: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        v.extend_from_slice(&10u32.to_le_bytes()); // top
        v.extend_from_slice(&20u32.to_le_bytes()); // left
        v.extend_from_slice(&30u32.to_le_bytes()); // bottom
        v.extend_from_slice(&40u32.to_le_bytes()); // right
        v.push(0); // clip_type = SPICE_CLIP_TYPE_NONE

        v.push(mask_flags);
        v.extend_from_slice(&0i32.to_le_bytes()); // pos.x
        v.extend_from_slice(&0i32.to_le_bytes()); // pos.y
        v.extend_from_slice(&mask_bitmap_offset.to_le_bytes());

        v
    }

    #[test]
    fn decode_draw_solid_fill_happy_path() {
        let payload = build_draw_solid_fill_payload(0, 0);
        match decode_draw_solid_fill(&payload).expect("decode failed") {
            SolidFillOutcome::Paint {
                base,
                masked_fallback,
            } => {
                assert!(!masked_fallback);
                assert_eq!(base.surface_id, 0);
                assert_eq!(base.top, 10);
                assert_eq!(base.left, 20);
                assert_eq!(base.bottom, 30);
                assert_eq!(base.right, 40);
            }
        }
    }

    #[test]
    fn decode_draw_solid_fill_masked_fallback() {
        let payload = build_draw_solid_fill_payload(0, 0x200);
        match decode_draw_solid_fill(&payload).expect("decode failed") {
            SolidFillOutcome::Paint {
                masked_fallback, ..
            } => assert!(masked_fallback),
        }
    }

    #[test]
    fn decode_draw_solid_fill_rejects_short_payload() {
        // 21 bytes of DrawBase but no SpiceQMask body.
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.push(0);
        // 12 bytes of mask instead of 13.
        v.extend_from_slice(&[0u8; 12]);

        let result = decode_draw_solid_fill(&v);
        assert!(result.is_err(), "expected short-payload error");
    }

    // -------------------------------------------------------------------------
    // decode_copy_bits tests
    // -------------------------------------------------------------------------

    /// Build a COPY_BITS payload:
    ///   DrawBase (21 bytes, clip_type=0, no clip rects)
    ///     + SpicePoint (i32 x, i32 y = 8 bytes)
    fn build_copy_bits_payload(src_x: i32, src_y: i32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        v.extend_from_slice(&50u32.to_le_bytes()); // top
        v.extend_from_slice(&100u32.to_le_bytes()); // left
        v.extend_from_slice(&70u32.to_le_bytes()); // bottom
        v.extend_from_slice(&200u32.to_le_bytes()); // right
        v.push(0); // clip_type = SPICE_CLIP_TYPE_NONE
        v.extend_from_slice(&src_x.to_le_bytes());
        v.extend_from_slice(&src_y.to_le_bytes());
        v
    }

    #[test]
    fn decode_copy_bits_happy_path() {
        let payload = build_copy_bits_payload(15, 7);
        match decode_copy_bits(&payload).expect("decode failed") {
            CopyBitsOutcome::Copy { base, src_x, src_y } => {
                assert_eq!(src_x, 15);
                assert_eq!(src_y, 7);
                assert_eq!(base.surface_id, 0);
                assert_eq!(base.top, 50);
                assert_eq!(base.left, 100);
                assert_eq!(base.bottom, 70);
                assert_eq!(base.right, 200);
            }
        }
    }

    #[test]
    fn decode_copy_bits_negative_src_clamped() {
        let payload = build_copy_bits_payload(-3, -2);
        match decode_copy_bits(&payload).expect("decode failed") {
            CopyBitsOutcome::Copy { src_x, src_y, .. } => {
                assert_eq!(src_x, 0);
                assert_eq!(src_y, 0);
            }
        }
    }

    #[test]
    fn decode_copy_bits_rejects_short_payload() {
        // 21-byte DrawBase + only 7 bytes of SpicePoint (one byte short).
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.push(0);
        v.extend_from_slice(&[0u8; 7]);

        let result = decode_copy_bits(&v);
        assert!(result.is_err(), "expected short-payload error");
    }

    // -------------------------------------------------------------------------
    // decode_draw_blend tests
    // -------------------------------------------------------------------------

    /// Build a DRAW_BLEND payload = DrawBase (21 bytes, clip_type=0) +
    /// SpiceCopy (36 bytes: src_bitmap + src_area + rop + scale + mask).
    fn build_draw_blend_payload(rop_descriptor: u16) -> Vec<u8> {
        let mut v = Vec::new();
        // DrawBase
        v.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        v.extend_from_slice(&10u32.to_le_bytes()); // top
        v.extend_from_slice(&20u32.to_le_bytes()); // left
        v.extend_from_slice(&30u32.to_le_bytes()); // bottom
        v.extend_from_slice(&40u32.to_le_bytes()); // right
        v.push(0); // clip_type = SPICE_CLIP_TYPE_NONE

        // SpiceCopy header
        v.extend_from_slice(&0x100u32.to_le_bytes()); // src_bitmap offset
        v.extend_from_slice(&1u32.to_le_bytes()); // src_top
        v.extend_from_slice(&2u32.to_le_bytes()); // src_left
        v.extend_from_slice(&3u32.to_le_bytes()); // src_bottom
        v.extend_from_slice(&4u32.to_le_bytes()); // src_right
        v.extend_from_slice(&rop_descriptor.to_le_bytes());
        v.push(0); // scale_mode
                   // SpiceQMask (13 bytes)
        v.push(0); // flags
        v.extend_from_slice(&0i32.to_le_bytes()); // pos.x
        v.extend_from_slice(&0i32.to_le_bytes()); // pos.y
        v.extend_from_slice(&0u32.to_le_bytes()); // bitmap_offset

        v
    }

    #[test]
    fn decode_draw_blend_happy_path_op_put() {
        let payload = build_draw_blend_payload(ropd::OP_PUT);
        match decode_draw_blend(&payload).expect("decode failed") {
            BlendOutcome::Paint {
                base,
                src_bitmap_offset,
                src_top,
                src_left,
                src_bottom,
                src_right,
            } => {
                assert_eq!(base.surface_id, 0);
                assert_eq!(base.top, 10);
                assert_eq!(base.left, 20);
                assert_eq!(base.bottom, 30);
                assert_eq!(base.right, 40);
                assert_eq!(src_bitmap_offset, 0x100);
                assert_eq!(src_top, 1);
                assert_eq!(src_left, 2);
                assert_eq!(src_bottom, 3);
                assert_eq!(src_right, 4);
            }
            other => panic!("expected Paint, got {:?}", other),
        }
    }

    #[test]
    fn decode_draw_blend_non_op_put_skips() {
        // 0x10 = OP_OR.
        let payload = build_draw_blend_payload(0x10);
        match decode_draw_blend(&payload).expect("decode failed") {
            BlendOutcome::SkipNonOpPut { rop } => assert_eq!(rop, 0x10),
            other => panic!("expected SkipNonOpPut, got {:?}", other),
        }
    }

    // -------------------------------------------------------------------------
    // decode_draw_opaque tests
    // -------------------------------------------------------------------------

    /// Build a DRAW_OPAQUE payload = DrawBase (21 bytes, clip_type=0) +
    /// SpiceOpaque (src_bitmap + src_area + SOLID brush + rop + scale + mask).
    /// SOLID brush body is 4 bytes of colour, so the variable portion is
    /// 5 bytes total (1-byte tag + 4-byte body).
    fn build_draw_opaque_payload(rop_descriptor: u16) -> Vec<u8> {
        let mut v = Vec::new();
        // DrawBase
        v.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        v.extend_from_slice(&10u32.to_le_bytes()); // top
        v.extend_from_slice(&20u32.to_le_bytes()); // left
        v.extend_from_slice(&30u32.to_le_bytes()); // bottom
        v.extend_from_slice(&40u32.to_le_bytes()); // right
        v.push(0); // clip_type = SPICE_CLIP_TYPE_NONE

        // SpiceOpaque: src_bitmap + src_area (20 bytes)
        v.extend_from_slice(&0x100u32.to_le_bytes()); // src_bitmap offset
        v.extend_from_slice(&1u32.to_le_bytes()); // src_top
        v.extend_from_slice(&2u32.to_le_bytes()); // src_left
        v.extend_from_slice(&3u32.to_le_bytes()); // src_bottom
        v.extend_from_slice(&4u32.to_le_bytes()); // src_right

        // SOLID brush (5 bytes: type + u32 colour).
        v.push(1); // brush type = SOLID
        v.extend_from_slice(&0u32.to_le_bytes()); // colour

        // rop + scale + mask.
        v.extend_from_slice(&rop_descriptor.to_le_bytes());
        v.push(0); // scale_mode
        v.push(0); // mask.flags
        v.extend_from_slice(&0i32.to_le_bytes()); // mask.pos.x
        v.extend_from_slice(&0i32.to_le_bytes()); // mask.pos.y
        v.extend_from_slice(&0u32.to_le_bytes()); // mask.bitmap_offset

        v
    }

    #[test]
    fn decode_draw_opaque_happy_path_op_put() {
        let payload = build_draw_opaque_payload(ropd::OP_PUT);
        match decode_draw_opaque(&payload).expect("decode failed") {
            OpaqueOutcome::Paint {
                base,
                src_bitmap_offset,
                src_top,
                src_left,
                src_bottom,
                src_right,
            } => {
                assert_eq!(base.surface_id, 0);
                assert_eq!(base.top, 10);
                assert_eq!(base.left, 20);
                assert_eq!(base.bottom, 30);
                assert_eq!(base.right, 40);
                assert_eq!(src_bitmap_offset, 0x100);
                assert_eq!(src_top, 1);
                assert_eq!(src_left, 2);
                assert_eq!(src_bottom, 3);
                assert_eq!(src_right, 4);
            }
            other => panic!("expected Paint, got {:?}", other),
        }
    }

    #[test]
    fn decode_draw_opaque_non_op_put_skips() {
        // 0x10 = OP_OR.
        let payload = build_draw_opaque_payload(0x10);
        match decode_draw_opaque(&payload).expect("decode failed") {
            OpaqueOutcome::SkipNonOpPut { rop } => assert_eq!(rop, 0x10),
            other => panic!("expected SkipNonOpPut, got {:?}", other),
        }
    }

    // -------------------------------------------------------------------------
    // decode_draw_transparent tests
    // -------------------------------------------------------------------------

    /// Build a DRAW_TRANSPARENT payload: DrawBase (21 bytes, clip_type=0) +
    /// SpiceTransparent (28 bytes: src_bitmap u32 + src_area 4×u32 +
    /// src_color u32 + true_color u32).
    fn build_draw_transparent_payload(src_color: u32) -> Vec<u8> {
        let mut v = Vec::new();
        // DrawBase
        v.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        v.extend_from_slice(&10u32.to_le_bytes()); // top
        v.extend_from_slice(&20u32.to_le_bytes()); // left
        v.extend_from_slice(&30u32.to_le_bytes()); // bottom
        v.extend_from_slice(&40u32.to_le_bytes()); // right
        v.push(0); // clip_type = NONE

        // SpiceTransparent
        v.extend_from_slice(&0x100u32.to_le_bytes()); // src_bitmap
        v.extend_from_slice(&1u32.to_le_bytes()); // src_top
        v.extend_from_slice(&2u32.to_le_bytes()); // src_left
        v.extend_from_slice(&3u32.to_le_bytes()); // src_bottom
        v.extend_from_slice(&4u32.to_le_bytes()); // src_right
        v.extend_from_slice(&src_color.to_le_bytes()); // src_color (BGRX)
        v.extend_from_slice(&0u32.to_le_bytes()); // true_color (deprecated; ignored)

        v
    }

    #[test]
    fn decode_draw_transparent_converts_bgrx_to_rgba() {
        // src_color = 0x00AB_CDEF → wire bytes [EF, CD, AB, 00]
        // RGBA conversion = R=0xAB, G=0xCD, B=0xEF, A=0xFF.
        let payload = build_draw_transparent_payload(0x00AB_CDEF);
        match decode_draw_transparent(&payload).expect("decode failed") {
            TransparentOutcome::Paint {
                base,
                chroma_rgba,
                src_bitmap_offset,
                src_top,
                src_left,
                src_bottom,
                src_right,
            } => {
                assert_eq!(chroma_rgba, [0xAB, 0xCD, 0xEF, 0xFF]);
                assert_eq!(base.surface_id, 0);
                assert_eq!(base.top, 10);
                assert_eq!(src_bitmap_offset, 0x100);
                assert_eq!(src_top, 1);
                assert_eq!(src_left, 2);
                assert_eq!(src_bottom, 3);
                assert_eq!(src_right, 4);
            }
        }
    }

    // -------------------------------------------------------------------------
    // decode_draw_alpha_blend tests
    // -------------------------------------------------------------------------

    /// Build a DRAW_ALPHA_BLEND payload: DrawBase (21 bytes) +
    /// SpiceAlphaBlend (23 bytes: alpha_flags u16 + alpha u8 +
    /// src_bitmap u32 + src_area 4×u32).
    fn build_draw_alpha_blend_payload(alpha: u8, alpha_flags: u16) -> Vec<u8> {
        let mut v = Vec::new();
        // DrawBase
        v.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        v.extend_from_slice(&10u32.to_le_bytes()); // top
        v.extend_from_slice(&20u32.to_le_bytes()); // left
        v.extend_from_slice(&30u32.to_le_bytes()); // bottom
        v.extend_from_slice(&40u32.to_le_bytes()); // right
        v.push(0); // clip_type = NONE

        // SpiceAlphaBlend
        v.extend_from_slice(&alpha_flags.to_le_bytes());
        v.push(alpha);
        v.extend_from_slice(&0x200u32.to_le_bytes()); // src_bitmap
        v.extend_from_slice(&1u32.to_le_bytes()); // src_top
        v.extend_from_slice(&2u32.to_le_bytes()); // src_left
        v.extend_from_slice(&3u32.to_le_bytes()); // src_bottom
        v.extend_from_slice(&4u32.to_le_bytes()); // src_right

        v
    }

    #[test]
    fn decode_draw_alpha_blend_happy_path() {
        let payload = build_draw_alpha_blend_payload(128, 0);
        match decode_draw_alpha_blend(&payload).expect("decode failed") {
            AlphaBlendOutcome::Paint {
                base,
                alpha,
                alpha_flags,
                src_bitmap_offset,
                ..
            } => {
                assert_eq!(alpha, 128);
                assert_eq!(alpha_flags, 0);
                assert_eq!(base.surface_id, 0);
                assert_eq!(src_bitmap_offset, 0x200);
            }
            other => panic!("expected Paint, got {:?}", other),
        }
    }

    #[test]
    fn decode_draw_alpha_blend_zero_alpha_skips() {
        let payload = build_draw_alpha_blend_payload(0, 0);
        match decode_draw_alpha_blend(&payload).expect("decode failed") {
            AlphaBlendOutcome::SkipZeroAlpha => {}
            other => panic!("expected SkipZeroAlpha, got {:?}", other),
        }
    }

    #[test]
    fn decode_draw_alpha_blend_carries_alpha_flags() {
        // alpha_flags != 0 is surfaced through Paint so the handler
        // can warn_once and still paint. The decoder does not skip.
        let payload = build_draw_alpha_blend_payload(128, 0x02);
        match decode_draw_alpha_blend(&payload).expect("decode failed") {
            AlphaBlendOutcome::Paint {
                alpha, alpha_flags, ..
            } => {
                assert_eq!(alpha, 128);
                assert_eq!(alpha_flags, 0x02);
            }
            other => panic!("expected Paint, got {:?}", other),
        }
    }
}
