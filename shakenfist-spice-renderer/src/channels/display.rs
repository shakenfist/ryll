/// Display channel handler - surfaces, image rendering
use anyhow::{Context, Result};
use flate2::read::ZlibDecoder;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, error, info, warn};

use crate::display::clip::{clip_area, ClipArea, Ltrb, MAX_CLIP_RECTS};
use crate::image_cache::BoundedImageCache;
use crate::mm_clock::MmClock;
use crate::opcode_counters::OpcodeCounters;
use crate::snapshots::{DecodeResult, DisplaySnapshot, StreamSnapshot};
use crate::{
    ByteCounter, CaptureSink, LogConfig, NotificationEntry, NotificationSource, TrafficSink,
};
use shakenfist_spice_compression::{
    best_for_platform, decompress_glz, decompress_lz, decompress_spice_lz4, limits, quic_decode,
    video, DecompressedImage, GlzDictionary, JpegDecoder, VideoDecoder, VideoDecoderError,
    SPICE_VIDEO_CODEC_TYPE_H264, SPICE_VIDEO_CODEC_TYPE_MJPEG,
};
use shakenfist_spice_protocol::constants::{bitmap_flags, bitmap_fmt, ropd};
use shakenfist_spice_protocol::link::{H264Offer, SpiceStream};
use shakenfist_spice_protocol::logging::{self, message_names};
use shakenfist_spice_protocol::messages::{
    make_message, BinaryData, BitmapHeader, DisplayInit, DisplayMonitorsConfig, DrawBase,
    ImageDescriptor, Notify as NotifyMessage, Ping, PreferredCompression, PreferredVideoCodecType,
    ReceiveBuffer, Rect, SetAck, SpiceAlphaBlend, SpiceBlackness, SpiceBrush, SpiceCopy, SpiceFill,
    SpiceOpaque, SpicePoint, SpiceTransparent, StreamActivateReport, StreamClip, StreamCreate,
    StreamDataRef, StreamDataSizedRef, StreamDestroy, StreamReport, SurfaceCreate, SurfaceDestroy,
    WireType,
};
use shakenfist_spice_protocol::parse::{read_u16_le, read_u32_le, read_u64_le};
use shakenfist_spice_protocol::reader::{BoundedReader, LinkError};
use shakenfist_spice_protocol::{
    display_client, display_server, warn_once, ChannelType, ImageType, NotifySeverity,
    IMAGE_FLAGS_CACHE_ME, IMAGE_FLAGS_CACHE_REPLACE_ME,
};

use super::{ChannelEvent, DisconnectReason, EventSink, MAX_MESSAGE_BODY};

struct StreamState {
    surface_id: u32,
    codec_type: u8,
    stream_width: u32,
    stream_height: u32,
    dest_top: u32,
    dest_left: u32,
    dest_bottom: u32,
    dest_right: u32,
    /// Per-stream video decoder, selected at `STREAM_CREATE` by
    /// [`shakenfist_spice_compression::video::for_stream`]. Holds any
    /// codec-specific state (e.g. the MJPEG DHT cache, or H.264 reference
    /// frames). The boxed trait object is moved when the stream is retired.
    video_decoder: Box<dyn VideoDecoder>,
    /// Session-relative seconds at `STREAM_CREATE`.
    created_at_secs: f64,
    /// Counters mirrored into `StreamSnapshot` by `update_snapshot`.
    /// See snapshot field docs for semantics.
    frames_received: u64,
    frames_decoded_ok: u64,
    frames_decode_failed: u64,
    last_frame_ts_secs: Option<f64>,
    last_decode_ok_ts_secs: Option<f64>,
    last_decode_duration_us: u32,
    /// Set once this stream has been warned about as an H.264 stream
    /// that decodes nothing; see [`warn_if_h264_never_decodes`].
    warned_never_decodes: bool,
    // Report state — populated by STREAM_ACTIVATE_REPORT; reset
    // to defaults at STREAM_CREATE. See spice.proto's
    // SpiceMsgDisplayStreamActivateReport and
    // SpiceMsgcDisplayStreamReport.
    report_is_active: bool,
    report_unique_id: u32,
    report_max_window_size: u32,
    report_timeout_ms: u32,
    // Rolling window counters, updated per frame and reset on each
    // STREAM_REPORT send.
    report_num_frames: u32,
    report_num_drops: u32,
    report_drops_seq_len: u32,
    report_start_frame_mm_time: u32,
    report_end_frame_mm_time: u32,
    report_start_now_mm_time: u32,
    // Cumulative — don't reset.
    report_send_count: u32,
    last_report_sent_ts_secs: Option<f64>,
    // Mirrors of the last sent report's values (for snapshots).
    last_report_num_frames: u32,
    last_report_num_drops: u32,
    last_report_last_frame_delay: i32,
}

/// Maximum number of recent decode results to keep in the snapshot.
const MAX_RECENT_DECODES: usize = 20;

/// Maximum number of concurrently-open server video streams.
///
/// `stream_id` is a server-chosen `u32`, so without a cap a server can
/// mint streams until the client runs out of memory. Each open stream
/// now owns a `Box<dyn VideoDecoder>`, and an H.264 decoder is an
/// openh264 instance costing megabytes, so the cost per surplus stream
/// is no longer the cached DHT it used to be.
///
/// spice-gtk's practical ceiling is single digits — one stream per
/// promoted video region, and a server that promotes more than a
/// handful at once is misbehaving. 16 leaves generous headroom over
/// anything legitimate while bounding worst-case decoder memory at
/// tens of megabytes rather than gigabytes.
const MAX_CONCURRENT_STREAMS: usize = 16;

/// Number of image-cache keys published into `DisplaySnapshot`.
///
/// The cache can hold millions of entries and the snapshot is
/// republished on every send, so materialising and sorting every key
/// was per-send work proportional to cache size, under the snapshot
/// mutex. The 64 most recently used keys answer the question
/// the field exists for ("what is this cache holding, and is it
/// churning?"); `image_cache_entries` carries the true total.
const MAX_SNAPSHOT_IMAGE_CACHE_IDS: usize = 64;

/// Minimum interval between snapshot publishes on the *send* path.
///
/// `update_snapshot` rebuilds several bounded rings and clones the
/// per-stream state, and a server can drive one send per inbound
/// 18-byte `STREAM_DATA` via the `STREAM_REPORT` trigger. Reads still
/// publish unthrottled, so a bug report is never more than one read
/// batch stale; this only collapses bursts of send-side republishes.
const SNAPSHOT_SEND_PUBLISH_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(50);

/// Substituted for a `STREAM_ACTIVATE_REPORT` `max_window_size` of
/// zero. Zero makes the `num_frames >= max_window_size` trigger
/// always true (`report_num_frames` is already >= 1 when the
/// predicate runs), so a server could force a STREAM_REPORT marshal,
/// a socket flush and a snapshot publish for every inbound
/// `STREAM_DATA`. 5 is spice-server's own default.
const STREAM_REPORT_DEFAULT_WINDOW_SIZE: u32 = 5;

/// Clamp range for a `STREAM_ACTIVATE_REPORT` `timeout_ms`.
///
/// The floor keeps a server from requesting a report per frame; at
/// 60 fps a 100 ms window is ~6 frames, comparable to the default
/// window size of 5. The ceiling keeps the value inside `i32`, which
/// the trigger predicate compares against: unclamped, any value from
/// `0x8000_0000` up casts to a negative `i32` and makes the elapsed
/// check unconditionally true. A server wanting reports less often
/// than once a minute is served just as well by the window-size
/// trigger.
const STREAM_REPORT_MIN_TIMEOUT_MS: u32 = 100;
const STREAM_REPORT_MAX_TIMEOUT_MS: u32 = 60_000;

/// Maximum number of recently-destroyed streams retained for
/// post-mortem diagnostics. Streams flap fast enough on a misbehaving
/// spice-server (observed: stream every ~15 s with ~2 s lifetime)
/// that 16 entries comfortably covers a few minutes of session
/// time without bloating channel-state.json.
const MAX_RECENT_DESTROYED_STREAMS: usize = 16;

/// The video codecs ryll asks the server to stream, most preferred
/// first, sent once at link-up in `PREFERRED_VIDEO_CODEC_TYPE`.
///
/// MJPEG then H.264 by default. spice-server tries its codecs in this
/// order, so it streams MJPEG, avoiding its GStreamer H.264, which does
/// not decode in ryll (shakenfist/ryll#398), while a server that only
/// encodes H.264 can still stream that. The capability bit, not this
/// list, is what makes H.264 eligible at all (`link::display_channel_caps`).
/// `--experimental-h264` puts H.264 first, and `--no-h264` drops it.
fn preferred_video_codecs(h264: H264Offer) -> &'static [u8] {
    match h264 {
        H264Offer::Off => &[SPICE_VIDEO_CODEC_TYPE_MJPEG],
        H264Offer::AfterMjpeg => &[SPICE_VIDEO_CODEC_TYPE_MJPEG, SPICE_VIDEO_CODEC_TYPE_H264],
        H264Offer::Preferred => &[SPICE_VIDEO_CODEC_TYPE_H264, SPICE_VIDEO_CODEC_TYPE_MJPEG],
    }
}

/// The H.264 offer this build can honour: `requested` when the
/// renderer has an H.264 decoder (the `h264-decode` feature), and
/// [`H264Offer::Off`] when it does not.
///
/// Without a decoder, every H.264 stream the server chose would be
/// refused at `STREAM_CREATE` and its frames dropped, leaving that
/// part of the screen blank. `Off` withholds both the capability bit
/// (`link::display_channel_caps`) and the codec preference
/// ([`preferred_video_codecs`]), so the server streams MJPEG or sends
/// plain draw commands instead. The session applies this before it
/// builds the display channel's link caps, and
/// [`DisplayChannel::new`] applies it again so a channel built any
/// other way cannot ask for what it cannot decode.
pub(crate) fn supported_h264_offer(requested: H264Offer) -> H264Offer {
    if cfg!(feature = "h264-decode") {
        requested
    } else {
        H264Offer::Off
    }
}

/// Sliding-window threshold for triggering a STREAM_REPORT
/// early due to consecutive frame drops. Matches spice-gtk's
/// `STREAM_REPORT_DROP_SEQ_LEN_LIMIT` at
/// channel-display.c:1532.
const STREAM_REPORT_DROP_SEQ_LEN_LIMIT: u32 = 3;

/// Trigger predicate for STREAM_REPORT, extracted to a
/// free function so each OR branch is unit-testable in
/// isolation. Mirrors spice-gtk's check at
/// channel-display.c:1559-1561.
///
/// `max_window_size` and `timeout_ms` are server-supplied and are
/// clamped where `STREAM_ACTIVATE_REPORT` is parsed, not here: callers
/// must pass a `max_window_size` of at least 1 (zero makes the first
/// branch unconditionally true) and a `timeout_ms` of at most
/// `i32::MAX` (larger values cast negative and make the second branch
/// unconditionally true). See `STREAM_REPORT_DEFAULT_WINDOW_SIZE` and
/// `STREAM_REPORT_MAX_TIMEOUT_MS`.
fn stream_report_should_send(
    num_frames: u32,
    max_window_size: u32,
    elapsed_since_window_start: i32,
    timeout_ms: u32,
    drops_seq_len: u32,
) -> bool {
    num_frames >= max_window_size
        || elapsed_since_window_start >= timeout_ms as i32
        || drops_seq_len >= STREAM_REPORT_DROP_SEQ_LEN_LIMIT
}

/// Clamp a server-supplied `STREAM_ACTIVATE_REPORT` trigger pair into
/// the range `stream_report_should_send` can safely use. Returns
/// `(max_window_size, timeout_ms)`.
///
/// Both values arrive raw from a 16-byte server payload and both feed
/// the send predicate, where an out-of-range value makes a branch
/// unconditionally true and lets the server force a STREAM_REPORT
/// marshal, a socket flush and a snapshot publish for every inbound
/// 18-byte STREAM_DATA. See the constants above for each bound.
fn clamp_stream_report_params(max_window_size: u32, timeout_ms: u32) -> (u32, u32) {
    let window = if max_window_size == 0 {
        STREAM_REPORT_DEFAULT_WINDOW_SIZE
    } else {
        max_window_size
    };
    (
        window,
        timeout_ms.clamp(STREAM_REPORT_MIN_TIMEOUT_MS, STREAM_REPORT_MAX_TIMEOUT_MS),
    )
}

/// Maximum number of consecutive ACK-send intervals retained in
/// the snapshot for "video not keeping up" diagnostics. With a
/// typical ACK window of a few hundred messages on a busy
/// display session, 32 intervals covers tens of seconds of
/// recent activity — enough to see whether ACK cadence paused
/// without bloating channel-state.json.
const RECENT_ACK_INTERVALS_CAP: usize = 32;

/// Push an ACK-send interval into the bounded ring, evicting
/// the oldest entry when the cap is exceeded. Factored out of
/// `send_ack` so the cap behaviour is unit-testable without
/// standing up a live channel.
fn push_ack_interval(ring: &mut VecDeque<f64>, interval_secs: f64) {
    ring.push_back(interval_secs);
    if ring.len() > RECENT_ACK_INTERVALS_CAP {
        ring.pop_front();
    }
}

/// Min / max / mean of `decode_duration_us` over the recent
/// decode ring, excluding cache hits and failures so the result
/// characterises actual decoder cost. Returns `(0, 0, 0)` when
/// no qualifying entries are present.
fn recent_decode_duration_stats(decodes: &VecDeque<DecodeResult>) -> (u32, u32, u32) {
    let mut count: u64 = 0;
    let mut sum: u64 = 0;
    let mut min: u32 = u32::MAX;
    let mut max: u32 = 0;
    for d in decodes.iter() {
        if d.from_cache || !d.success {
            continue;
        }
        count += 1;
        sum += u64::from(d.decode_duration_us);
        if d.decode_duration_us < min {
            min = d.decode_duration_us;
        }
        if d.decode_duration_us > max {
            max = d.decode_duration_us;
        }
    }
    match sum.checked_div(count) {
        None => (0, 0, 0),
        Some(mean_u64) => {
            let mean = u32::try_from(mean_u64).unwrap_or(u32::MAX);
            (min, max, mean)
        }
    }
}

/// Min / max / mean of a ring of raw microsecond durations.
/// Returns `(0, 0, 0)` when the ring is empty.
fn mjpeg_duration_stats(ring: &VecDeque<u32>) -> (u32, u32, u32) {
    if ring.is_empty() {
        return (0, 0, 0);
    }
    let mut min = u32::MAX;
    let mut max = 0u32;
    let mut sum = 0u64;
    for &us in ring {
        if us < min {
            min = us;
        }
        if us > max {
            max = us;
        }
        sum += u64::from(us);
    }
    let mean = u32::try_from(sum / ring.len() as u64).unwrap_or(u32::MAX);
    (min, max, mean)
}

/// What we decided to do with a DRAW_FILL after classifying its
/// rop/brush/mask. Extracted from `handle_draw_fill` so the
/// parse-and-classify logic is independently testable without
/// standing up a full `DisplayChannel`.
#[derive(Debug, Clone)]
enum FillOutcome {
    /// Happy path: paint `colour` (RGBA) into `base.rect` with
    /// `base.clip`.
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

/// Why a ZLIB_GLZ_RGB payload was refused before GLZ decoding.
#[derive(Debug)]
enum InflateGlzError {
    /// `rgba_len` refused the descriptor's dimensions.
    DimensionsRefused,
    /// The declared GLZ size exceeds the limit for the dimensions.
    DeclaredTooLarge,
    /// The zlib stream inflated past the declared GLZ size.
    ExceedsDeclared,
    /// The stream ended short of the declared GLZ size.
    SizeMismatch { inflated: usize },
    /// The zlib stream was malformed.
    Zlib(std::io::Error),
}

/// Fixed allowance for the GLZ header, which is 33 bytes, plus slack.
const GLZ_STREAM_FIXED_OVERHEAD: usize = 64;

/// Upper bound on the inflated GLZ stream for a `width` x `height` image.
///
/// A GLZ stream can be slightly larger than the RGBA it decodes to:
/// incompressible RGBA costs about 4 + 2/32 bytes a pixel plus a 33
/// byte header (spice/server/glz-encode.tmpl.c). A quarter of the RGBA
/// size plus a fixed allowance covers that.
///
/// `None` when `rgba_len` refuses the dimensions.
fn glz_stream_limit(width: usize, height: usize) -> Option<usize> {
    let rgba = limits::rgba_len(width, height)?;
    Some(rgba + rgba / 4 + GLZ_STREAM_FIXED_OVERHEAD)
}

/// Inflate the zlib layer of a ZLIB_GLZ_RGB payload, refusing a
/// decompression bomb.
///
/// `declared_glz_size` is the wire's `glz_data_size`: the exact length
/// of the GLZ stream after inflating (see `glz_size` in the server's
/// image-encoders.cpp, and canvas_base.c in spice-common, which
/// allocates exactly that many bytes for the inflate). It counts
/// compressed GLZ bytes, not RGBA bytes. It must fit within the limit
/// and match the inflated length, and the inflate stops one byte past
/// it.
fn inflate_glz_stream(
    zlib_data: &[u8],
    declared_glz_size: usize,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, InflateGlzError> {
    let limit = glz_stream_limit(width, height).ok_or(InflateGlzError::DimensionsRefused)?;
    if declared_glz_size > limit {
        return Err(InflateGlzError::DeclaredTooLarge);
    }

    let mut decoder = ZlibDecoder::new(zlib_data);
    let mut glz_data = Vec::new();
    // The declared size is exact and already within the limit, so one
    // byte past it tells "exactly the size" from "over it" without
    // inflating any further.
    decoder
        .by_ref()
        .take(declared_glz_size as u64 + 1)
        .read_to_end(&mut glz_data)
        .map_err(InflateGlzError::Zlib)?;
    if glz_data.len() > declared_glz_size {
        return Err(InflateGlzError::ExceedsDeclared);
    }
    if glz_data.len() != declared_glz_size {
        return Err(InflateGlzError::SizeMismatch {
            inflated: glz_data.len(),
        });
    }
    Ok(glz_data)
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
    if let Ok(base) = DrawBase::decode(payload) {
        let (left, top, right, bottom) = ltrb(&base.bbox);
        logging::log_detail(&format!(
            "{}: surface={}, rect=({},{})-({},{}), clip_type={}",
            op_name, base.surface_id, left, top, right, bottom, base.clip.clip_type,
        ));
    }
}

/// A rectangle as the (left, top, right, bottom) tuple ryll's events
/// carry. spice.proto's edges are signed; ryll has always read them as
/// `u32`, and the casts keep those bits.
fn ltrb(rect: &Rect) -> (u32, u32, u32, u32) {
    (
        rect.left as u32,
        rect.top as u32,
        rect.right as u32,
        rect.bottom as u32,
    )
}

/// Crop a decoded stream frame to its top-left `dest_w` x `dest_h`
/// corner, or less where the frame is smaller.
///
/// `ImageReady` pixels are tightly packed, `width * 4` bytes a row. A
/// frame wider than its dest (an H.264 frame rounded up to whole
/// macroblocks, or a server's frame overhanging its dest rect) has rows
/// longer than that, so each kept row is moved down to the cropped
/// stride. Narrowing only the reported width would leave the rows at
/// the frame's stride, and the frame would paint sheared. A frame no
/// wider than its dest only loses its surplus rows.
///
/// A frame whose pixels are shorter than its dimensions claim is a
/// decode failure. Neither decoder produces one.
fn crop_frame_to_dest(
    frame: video::DecodedFrame,
    dest_w: u32,
    dest_h: u32,
) -> Result<video::DecodedFrame, VideoDecoderError> {
    let video::DecodedFrame {
        mut rgba,
        width: frame_w,
        height: frame_h,
    } = frame;
    let src_stride = (frame_w as usize) * 4;
    if !matches!(src_stride.checked_mul(frame_h as usize), Some(n) if n <= rgba.len()) {
        return Err(VideoDecoderError::Decode(format!(
            "{}x{} frame carries only {} bytes of pixels",
            frame_w,
            frame_h,
            rgba.len()
        )));
    }

    let width = frame_w.min(dest_w);
    let height = frame_h.min(dest_h);
    let row_bytes = (width as usize) * 4;
    if width < frame_w {
        // Row y moves from y * src_stride down to y * row_bytes, which
        // is never later, so a forward pass never overwrites a row it
        // has yet to move.
        for y in 1..height as usize {
            let src = y * src_stride;
            rgba.copy_within(src..src + row_bytes, y * row_bytes);
        }
    }
    rgba.truncate(row_bytes * height as usize);
    Ok(video::DecodedFrame {
        rgba,
        width,
        height,
    })
}

/// Frames an H.264 stream may receive without a decoded picture
/// before [`warn_if_h264_never_decodes`] says so.
const H264_NO_PICTURE_WARN_FRAMES: u64 = 10;

/// Warn, once per stream, when an H.264 stream's first
/// [`H264_NO_PICTURE_WARN_FRAMES`] frames have produced no picture.
///
/// ryll offers H.264 by default, after MJPEG, and spice-server's
/// display channel honours that order. A guest's spice-streaming-agent
/// does not: it may choose H.264, and spice-server's GStreamer H.264
/// does not decode in ryll (ryll#398, ryll#547), which leaves the
/// streamed region black or stale with nothing to say why. A frame
/// that fails and a packet that completes no picture count alike,
/// because which of the two openh264 reports for such a stream varies.
fn warn_if_h264_never_decodes(stream_id: u32, stream: &mut StreamState) {
    if stream.codec_type != SPICE_VIDEO_CODEC_TYPE_H264
        || stream.warned_never_decodes
        || stream.frames_decoded_ok > 0
        || stream.frames_received < H264_NO_PICTURE_WARN_FRAMES
    {
        return;
    }
    stream.warned_never_decodes = true;
    warn!(
        "display: H.264 stream {} has decoded no picture from its first {} frames \
         ({} failed); this server's H.264 may not decode in ryll (ryll#398, ryll#547). \
         Restart ryll with --no-h264 to ask for MJPEG instead",
        stream_id, stream.frames_received, stream.frames_decode_failed
    );
}

/// A draw's clip as the rectangle list its event carries: empty for an
/// unclipped draw, otherwise the clip's union with `dest` as disjoint
/// rectangles inside it (see [`clip_area`]). `None` means the draw
/// paints nothing, either because the clip misses `dest` or because it
/// needs more than [`MAX_CLIP_RECTS`] rectangles, which is warned about
/// once per op.
fn event_clip(base: &DrawBase, dest: Ltrb, op_name: &str) -> Option<Vec<Ltrb>> {
    match clip_area(&base.clip, dest) {
        ClipArea::Whole => Some(Vec::new()),
        ClipArea::Rects(rects) => Some(rects),
        ClipArea::Nothing => None,
        ClipArea::TooComplex { sent } => {
            logging::warn_once_impl(
                logging::intern_key(format!(
                    "display:decode_failure:{}:clip_too_complex",
                    op_name
                )),
                &format!(
                    "display: {}: clip of {} rectangles needs more than {} once merged; \
                     skipping the draw",
                    op_name, sent, MAX_CLIP_RECTS
                ),
            );
            None
        }
    }
}

/// Read the `DrawBase` that opens a draw payload, returning it and the
/// draw-specific body after it.
fn read_draw_base(payload: &[u8]) -> std::io::Result<(DrawBase, &[u8])> {
    let mut r = BoundedReader::new(payload);
    let base = DrawBase::read(&mut r)?;
    let body = r.read_bytes(r.remaining())?;
    Ok((base, body))
}

fn decode_draw_fill(payload: &[u8]) -> std::io::Result<FillOutcome> {
    let (base, body) = read_draw_base(payload)?;
    let (fill, _consumed) = SpiceFill::read(body)?;

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
    let (base, body) = read_draw_base(payload)?;
    let body = SpiceBlackness::read(body)?;
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
    let (base, body) = read_draw_base(payload)?;
    let src_pos = SpicePoint::decode(body)?;
    // Wire type is int32; source coords are logically unsigned
    // indices into the surface buffer. Clamp negatives to 0.
    let src_x = src_pos.x.max(0) as u32;
    let src_y = src_pos.y.max(0) as u32;
    Ok(CopyBitsOutcome::Copy { base, src_x, src_y })
}

/// Outcome of decoding a DRAW_BLEND payload.
///
/// DRAW_BLEND's body is DRAW_COPY's (spice.proto's `Blend` is
/// `@ctype(SpiceCopy)`), read here as a [`SpiceCopy`]. On OP_PUT the
/// blend is identical to a DRAW_COPY; any other ROP would require
/// compositing that we don't implement, so we warn_once and skip.
#[derive(Debug, Clone)]
enum BlendOutcome {
    Paint {
        base: DrawBase,
        copy: SpiceCopy,
        /// The length of the base and the copy, after which its images
        /// start.
        fixed_len: usize,
    },
    SkipNonOpPut {
        rop: u16,
    },
}

fn decode_draw_blend(payload: &[u8]) -> std::io::Result<BlendOutcome> {
    let mut r = BoundedReader::new(payload);
    let base = DrawBase::read(&mut r)?;
    // scale_mode and mask are parsed-through silently, matching
    // how handle_draw_copy treats them.
    let copy = SpiceCopy::read(&mut r)?;

    if copy.rop_descriptor != ropd::OP_PUT {
        return Ok(BlendOutcome::SkipNonOpPut {
            rop: copy.rop_descriptor,
        });
    }
    Ok(BlendOutcome::Paint {
        base,
        copy,
        fixed_len: r.position(),
    })
}

/// Outcome of decoding a DRAW_OPAQUE payload.
///
/// DRAW_OPAQUE carries a brush between src_area and rop_descriptor,
/// parsed via the SpiceOpaque struct. On OP_PUT the brush
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
    let (base, body) = read_draw_base(payload)?;
    let (opaque, _consumed) = SpiceOpaque::read(body)?;

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
    let (base, body) = read_draw_base(payload)?;
    let transparent = SpiceTransparent::read(body)?;
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
    let (base, body) = read_draw_base(payload)?;
    let ab = SpiceAlphaBlend::read(body)?;
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

/// Warn, once per draw op, that its source image pointer is null.
fn warn_null_src_bitmap(op_name: &str) {
    logging::warn_once_impl(
        logging::intern_key(format!(
            "display:decode_failure:{}:null_src_bitmap",
            op_name
        )),
        &format!("display: {}: null src_bitmap", op_name),
    );
}

/// Warn, once per draw op, that its source image has no room for a
/// descriptor: `have` bytes are available to an image at `offset`.
fn warn_short_img_desc(op_name: &str, have: usize, offset: usize) {
    logging::warn_once_impl(
        logging::intern_key(format!(
            "display:decode_failure:{}:short_payload_img_desc",
            op_name
        )),
        &format!(
            "display: {}: payload too short for image descriptor \
             (have {}, need {}, offset={})",
            op_name,
            have,
            offset + ImageDescriptor::SIZE,
            offset
        ),
    );
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
    produced_at_secs: f64,
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
            produced_at_secs,
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
            produced_at_secs,
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
            produced_at_secs,
        },
    }
}

/// GLZ dictionary shared across all display channels.
pub type SharedGlzDictionary = Arc<GlzDictionary>;

pub struct DisplayChannel {
    channel_id: u8,
    stream: SpiceStream,
    events: EventSink,
    buffer: ReceiveBuffer,
    glz_dictionary: SharedGlzDictionary,
    /// MJPEG decoder backend selected once at construction via
    /// `best_for_platform()`. Shared as `Arc<dyn JpegDecoder>` so
    /// faster backends (ImageIO, WIC, VA-API) can be swapped in
    /// without changing call sites. See
    /// `docs/plans/PLAN-stream-caps-and-flap.md`.
    jpeg_decoder: Arc<dyn JpegDecoder>,
    image_cache: BoundedImageCache,
    streams: HashMap<u32, StreamState>,
    capture: Option<Arc<dyn CaptureSink>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    log_config: LogConfig,
    snapshot: Arc<Mutex<DisplaySnapshot>>,
    recent_decodes: VecDeque<DecodeResult>,
    ack_generation: u32,
    ack_window: u32,
    /// Messages read since the channel linked. u64 so that a long soak
    /// cannot wrap it, as `stats` reports rates from its differences.
    message_count: u64,
    last_ack: u64,
    bytes_in: u64,
    bytes_out: u64,
    /// Local cache of disconnect-cause diagnostic fields,
    /// flushed to `snapshot` by `update_snapshot()`.
    last_recv_ts_secs: Option<f64>,
    last_send_ts_secs: Option<f64>,
    ping_recv_count: u32,
    pong_send_count: u32,
    last_ping_recv_ts_secs: Option<f64>,
    /// "Video not keeping up" instrumentation. See
    /// `docs/plans/PLAN-video-keeping-up.md`.
    decode_total_count: u64,
    decode_failed_count: u64,
    decode_from_cache_count: u64,
    socket_read_count: u64,
    socket_reads_at_chunk_cap: u64,
    socket_max_chunk_bytes: u32,
    ack_send_count: u32,
    last_ack_send_ts_secs: Option<f64>,
    recent_ack_intervals_secs: VecDeque<f64>,
    /// Count of pcap-capture packets rejected by the writer task's
    /// queue. Mirrored into
    /// `DisplaySnapshot::writer_dropped_count`.
    capture_dropped_count: u64,
    /// Stream-channel diagnostics mirrored into
    /// `DisplaySnapshot::streams_created_total` etc. Added so a
    /// bug report can answer "did MJPEG frames reach blit?"
    /// without the user enabling debug logging.
    streams_created_total: u64,
    streams_destroyed_total: u64,
    stream_data_orphan_count: u64,
    /// Cumulative count of STREAM_REPORT messages sent to the
    /// server since session start. Mirrored into
    /// `DisplaySnapshot::stream_reports_sent_total`.
    stream_reports_sent_total: u64,
    /// Cumulative count of `STREAM_CREATE` messages refused by
    /// `MAX_CONCURRENT_STREAMS`. Mirrored into
    /// `DisplaySnapshot::streams_rejected_total`.
    streams_rejected_total: u64,
    /// Bounded per-opcode message counters; flushed to the
    /// snapshot by `update_snapshot`. See `OpcodeCounters`.
    opcodes: OpcodeCounters,
    /// When the send path last published the snapshot. Drives the
    /// `SNAPSHOT_SEND_PUBLISH_MIN_INTERVAL` throttle; `None` until
    /// the first send.
    last_send_snapshot_publish: Option<Instant>,
    /// Bounded ring of recently-destroyed `StreamState`s captured
    /// at teardown so per-stream counters survive `STREAM_DESTROY`.
    /// Without this, a bug report filed between flap cycles loses
    /// the diagnostic data we just added.
    recently_destroyed_streams: VecDeque<StreamSnapshot>,
    /// Shared mm_time clock — reader side. `mm_clock.now()`
    /// evaluates the STREAM_REPORT trigger predicate and computes
    /// `last_frame_delay` at send time.
    mm_clock: Arc<MmClock>,
    /// How the session offers H.264 (`--no-h264`,
    /// `--experimental-h264`). Picks the link-up video codec
    /// preference; the matching capability bit was chosen when the
    /// channel linked.
    h264: H264Offer,
    /// Bounded ring of the most recent MJPEG decode durations in
    /// microseconds, newest at the back. Capped at `MAX_RECENT_DECODES`
    /// to match the non-stream recent-decode ring. Used by
    /// `update_snapshot` to compute
    /// `mjpeg_decode_recent_min/max/mean_us`.
    mjpeg_recent_durations: VecDeque<u32>,
    /// Cumulative count of MJPEG decode attempts (success +
    /// failure) since session start.
    mjpeg_decode_total_count: u64,
    /// Cumulative count of MJPEG decode attempts that
    /// returned `None` since session start.
    mjpeg_decode_failed_count: u64,
    /// Bounded ring of the most recent H.264 decode durations in
    /// microseconds, newest at the back. Capped at `MAX_RECENT_DECODES`.
    /// Used by `update_snapshot` to compute
    /// `h264_decode_recent_min/max/mean_us`. Parallel to
    /// `mjpeg_recent_durations`; the dispatch site selects which ring
    /// receives the sample based on `stream.codec_type`.
    h264_recent_durations: VecDeque<u32>,
    /// Cumulative count of H.264 decode attempts (success +
    /// failure) since session start.
    h264_decode_total_count: u64,
    /// Cumulative count of H.264 decode attempts that returned `Err`
    /// since session start. `Ok(None)` (needs more data) is not counted
    /// as a failure.
    h264_decode_failed_count: u64,
    /// Set to true after we have successfully sent the link-up
    /// `PREFERRED_COMPRESSION` (opcode 103) message. Mirrored into
    /// `DisplaySnapshot::pref_compression_sent` so a bug report can
    /// confirm the preference actually went out. One-shot per channel
    /// lifetime.
    pref_compression_sent: bool,
    /// `SPICE_IMAGE_COMPRESSION_*` value sent in the link-up
    /// `PREFERRED_COMPRESSION` message; chosen by `--preferred-compression`.
    preferred_compression: u8,
    /// Set to true after we have successfully sent the link-up
    /// `PREFERRED_VIDEO_CODEC_TYPE` (opcode 105) message. Mirrored into
    /// `DisplaySnapshot::pref_video_codec_type_sent`. One-shot per channel
    /// lifetime.
    pref_video_codec_type_sent: bool,
}

impl DisplayChannel {
    pub fn new_shared_glz_dictionary(cap_bytes: usize) -> SharedGlzDictionary {
        Arc::new(GlzDictionary::with_cap(cap_bytes))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: u8,
        stream: SpiceStream,
        events: EventSink,
        capture: Option<Arc<dyn CaptureSink>>,
        byte_counter: Arc<ByteCounter>,
        traffic: Arc<dyn TrafficSink>,
        snapshot: Arc<Mutex<DisplaySnapshot>>,
        glz_dictionary: SharedGlzDictionary,
        log_config: LogConfig,
        mm_clock: Arc<MmClock>,
        h264: H264Offer,
        image_cache_cap_bytes: usize,
        preferred_compression: u8,
    ) -> Self {
        DisplayChannel {
            channel_id,
            stream,
            events,
            buffer: ReceiveBuffer::with_capacity(1024 * 1024),
            glz_dictionary,
            jpeg_decoder: best_for_platform(),
            image_cache: BoundedImageCache::new(image_cache_cap_bytes),
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
            decode_total_count: 0,
            decode_failed_count: 0,
            decode_from_cache_count: 0,
            socket_read_count: 0,
            socket_reads_at_chunk_cap: 0,
            socket_max_chunk_bytes: 0,
            ack_send_count: 0,
            last_ack_send_ts_secs: None,
            recent_ack_intervals_secs: VecDeque::new(),
            capture_dropped_count: 0,
            streams_created_total: 0,
            streams_destroyed_total: 0,
            stream_data_orphan_count: 0,
            stream_reports_sent_total: 0,
            streams_rejected_total: 0,
            opcodes: OpcodeCounters::new(
                message_names::display_server,
                message_names::display_client,
            ),
            last_send_snapshot_publish: None,
            recently_destroyed_streams: VecDeque::new(),
            mm_clock,
            h264: supported_h264_offer(h264),
            mjpeg_recent_durations: VecDeque::new(),
            mjpeg_decode_total_count: 0,
            mjpeg_decode_failed_count: 0,
            h264_recent_durations: VecDeque::new(),
            h264_decode_total_count: 0,
            h264_decode_failed_count: 0,
            preferred_compression,
            pref_compression_sent: false,
            pref_video_codec_type_sent: false,
        }
    }

    /// Build a `StreamSnapshot` from a live `StreamState`.
    /// `destroyed_at` is `None` for active streams (entries in
    /// `streams_active`) and `Some(now)` for entries being
    /// moved into `recently_destroyed_streams`.
    fn stream_state_to_snapshot(
        id: u32,
        s: &StreamState,
        destroyed_at: Option<f64>,
    ) -> StreamSnapshot {
        StreamSnapshot {
            stream_id: id,
            surface_id: s.surface_id,
            codec_type: s.codec_type,
            stream_width: s.stream_width,
            stream_height: s.stream_height,
            dest_top: s.dest_top,
            dest_left: s.dest_left,
            dest_bottom: s.dest_bottom,
            dest_right: s.dest_right,
            created_at_secs: s.created_at_secs,
            frames_received: s.frames_received,
            frames_decoded_ok: s.frames_decoded_ok,
            frames_decode_failed: s.frames_decode_failed,
            last_frame_ts_secs: s.last_frame_ts_secs,
            last_decode_ok_ts_secs: s.last_decode_ok_ts_secs,
            last_decode_duration_us: s.last_decode_duration_us,
            destroyed_at_secs: destroyed_at,
            report_is_active: s.report_is_active,
            report_unique_id: s.report_unique_id,
            report_max_window_size: s.report_max_window_size,
            report_timeout_ms: s.report_timeout_ms,
            report_send_count: s.report_send_count,
            last_report_sent_ts_secs: s.last_report_sent_ts_secs,
            last_report_num_frames: s.last_report_num_frames,
            last_report_num_drops: s.last_report_num_drops,
            last_report_last_frame_delay: s.last_report_last_frame_delay,
            // video_decoder_backend is the general field: always the
            // active decoder's name regardless of codec (e.g.
            // "libjpeg-turbo", "ImageIO", "H264 (openh264)").
            video_decoder_backend: s.video_decoder.name().to_string(),
            // mjpeg_decoder_backend is the backwards-compat field for
            // existing bug-report consumers. Populated only for MJPEG
            // streams; empty string for H.264 and any other codec so
            // consumers can distinguish "MJPEG with a named backend"
            // from "this field doesn't apply to this stream's codec".
            mjpeg_decoder_backend: if s.codec_type == SPICE_VIDEO_CODEC_TYPE_MJPEG {
                s.video_decoder.name().to_string()
            } else {
                String::new()
            },
        }
    }

    /// Snapshot a dying stream into the recently-destroyed ring,
    /// evicting the oldest entry when the cap is exceeded, and
    /// log the final per-stream counters at INFO so the console
    /// is useful even without a bug report.
    fn retire_stream(&mut self, stream_id: u32, state: &StreamState, destroyed_at: f64) {
        let lifetime = destroyed_at - state.created_at_secs;
        info!(
            "display: stream_destroy id={} (lifetime={:.2}s, received={}, \
             decoded_ok={}, decode_failed={}, last_frame_age={})",
            stream_id,
            lifetime,
            state.frames_received,
            state.frames_decoded_ok,
            state.frames_decode_failed,
            state
                .last_frame_ts_secs
                .map(|t| format!("{:.2}s", destroyed_at - t))
                .unwrap_or_else(|| "never".to_string()),
        );
        self.recently_destroyed_streams
            .push_back(Self::stream_state_to_snapshot(
                stream_id,
                state,
                Some(destroyed_at),
            ));
        if self.recently_destroyed_streams.len() > MAX_RECENT_DESTROYED_STREAMS {
            self.recently_destroyed_streams.pop_front();
        }
    }

    /// Run the display channel event loop. Wraps `run_loop`
    /// so errors propagating out of the inner select! arms
    /// are logged before the task ends — see `MainChannel::run`
    /// for the rationale (including the `Box::pin` reason).
    pub async fn run(&mut self) -> Result<()> {
        let result = Box::pin(self.run_loop()).await;
        match &result {
            Ok(()) => info!("display: run loop exited cleanly"),
            Err(e) => error!("display: run loop exited with error: {:#}", e),
        }
        result
    }

    /// Send the one-shot link-up preference messages: image compression
    /// then video codec types.
    async fn send_link_up_preferences(&mut self) -> Result<()> {
        // One-shot link-up preference messages. spice-gtk fires both right
        // after the channel reaches STATE_LINKED (channel-display.c:984-995).
        // In ryll the channel is already linked by the time run_loop starts
        // (link/auth happened during session connect), so just after INIT is
        // the equivalent point. Failures here are not fatal — the server
        // falls back to its default codec / compression choice if the
        // messages never arrive — but we propagate any IO error since it
        // indicates the socket is unhealthy and the read loop is about to
        // fail anyway. The compression scheme is whatever the session was
        // configured with (`--preferred-compression`); why the default is
        // `auto-glz` is in docs/configuration.md.
        self.send_preferred_compression(self.preferred_compression)
            .await?;
        self.send_preferred_video_codec_type(preferred_video_codecs(self.h264))
            .await?;
        Ok(())
    }

    async fn run_loop(&mut self) -> Result<()> {
        info!("display: channel started");

        // Send display init message
        self.send_init().await?;

        self.send_link_up_preferences().await?;

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
                SpiceStream::TlsServer(s) => {
                    use tokio::io::AsyncReadExt;
                    s.read(&mut chunk).await?
                }
            };

            if n == 0 {
                info!("display: channel disconnected");
                self.events
                    .emit(ChannelEvent::Disconnected(
                        ChannelType::Display,
                        DisconnectReason::ServerClosed,
                    ))
                    .await;
                break;
            }

            // Socket-read fill stats. A read that comes back at the full chunk
            // size means the OS recv buffer had at least that much waiting when
            // we read, which is a cheap proxy for "the read loop is behind the
            // arrival rate". See `docs/plans/PLAN-video-keeping-up.md`.
            self.socket_read_count = self.socket_read_count.saturating_add(1);
            if n == chunk.len() {
                self.socket_reads_at_chunk_cap = self.socket_reads_at_chunk_cap.saturating_add(1);
            }
            let n_u32 = u32::try_from(n).unwrap_or(u32::MAX);
            if n_u32 > self.socket_max_chunk_bytes {
                self.socket_max_chunk_bytes = n_u32;
            }

            self.byte_counter.add(n as u64);
            if let Some(ref c) = self.capture {
                if !c.packet_received("display", &chunk[..n]) {
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

    async fn send_init(&mut self) -> Result<()> {
        let init = DisplayInit {
            cache_id: 1,
            cache_size: 20 * 1024 * 1024, // 20MB
            glz_dict_id: 1,
            glz_dict_window: 3 * 1024 * 1024, // 3MB
        };

        let mut payload = Vec::new();
        init.write(&mut payload);
        let msg = make_message(display_client::INIT, &payload);

        if self.log_config.verbose {
            logging::log_detail(&format!(
                "cache_id={}, cache_size={}, glz_dict_id={}, glz_dict_window={}",
                init.cache_id, init.cache_size, init.glz_dict_id, init.glz_dict_window
            ));
        }

        self.send_with_log(display_client::INIT, &msg).await
    }

    /// Send `SPICE_MSGC_DISPLAY_PREFERRED_COMPRESSION` (opcode
    /// 103). Payload is a single u8 from the
    /// `SPICE_IMAGE_COMPRESSION_*` enum (spice.proto:1028-1030).
    /// Called once at link-up from `run_loop`. The server uses
    /// this to pick how it encodes server-driven image fills
    /// (LZ vs GLZ vs QUIC etc).
    async fn send_preferred_compression(&mut self, scheme: u8) -> Result<()> {
        let mut payload = Vec::new();
        PreferredCompression {
            image_compression: scheme,
        }
        .write(&mut payload);
        let msg = make_message(display_client::PREFERRED_COMPRESSION, &payload);
        if self.log_config.verbose {
            logging::log_detail(&format!("image_compression={}", scheme));
        }
        self.send_with_log(display_client::PREFERRED_COMPRESSION, &msg)
            .await?;
        self.pref_compression_sent = true;
        Ok(())
    }

    /// Send `SPICE_MSGC_DISPLAY_PREFERRED_VIDEO_CODEC_TYPE`
    /// (opcode 105). Payload is `u8 num_of_codecs` followed by
    /// that many `video_codec_type` u8s in preference order
    /// (spice.proto:1035-1037). spice-gtk's
    /// `spice_display_send_client_preferred_video_codecs`
    /// (channel-display.c:621-642) is the reference encoder.
    /// Called once at link-up from `run_loop`.
    async fn send_preferred_video_codec_type(&mut self, codecs: &[u8]) -> Result<()> {
        // The count is a u8, so at most the first 255 codecs are sent.
        let num = u8::try_from(codecs.len()).unwrap_or(u8::MAX);
        let mut payload = Vec::with_capacity(1 + num as usize);
        PreferredVideoCodecType {
            codecs: codecs[..num as usize].to_vec(),
        }
        .write(&mut payload);
        let msg = make_message(display_client::PREFERRED_VIDEO_CODEC_TYPE, &payload);
        if self.log_config.verbose {
            logging::log_detail(&format!(
                "num_of_codecs={}, codecs={:?}",
                num,
                &codecs[..num as usize]
            ));
        }
        self.send_with_log(display_client::PREFERRED_VIDEO_CODEC_TYPE, &msg)
            .await?;
        self.pref_video_codec_type_sent = true;
        Ok(())
    }

    async fn process_messages(&mut self) -> Result<()> {
        while let Some(message) = self.buffer.take_message(MAX_MESSAGE_BODY)? {
            let msg_type = message.header.message_type;
            self.traffic.record_received(
                "display",
                msg_type,
                message_names::display_server(msg_type),
                &message.raw,
            );

            self.message_count += 1;
            self.handle_message(msg_type, message.payload()).await?;

            // Send ACK if needed
            if self.ack_window > 0
                && self.message_count - self.last_ack >= u64::from(self.ack_window)
            {
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

        // Count before dispatch so known and unknown opcodes are
        // counted uniformly.
        self.opcodes.record_recv(msg_type);

        match msg_type {
            display_server::SURFACE_CREATE => {
                let surface = SurfaceCreate::decode(payload).context("malformed SURFACE_CREATE")?;
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

                self.events
                    .emit(ChannelEvent::SurfaceCreated {
                        display_channel_id: self.channel_id,
                        surface_id: surface.surface_id,
                        width: surface.width,
                        height: surface.height,
                    })
                    .await;
            }

            display_server::SURFACE_DESTROY => {
                // A short SURFACE_DESTROY is ignored.
                if let Ok(SurfaceDestroy { surface_id }) = SurfaceDestroy::decode(payload) {
                    info!("display: surface destroyed: id={}", surface_id);

                    if self.log_config.verbose {
                        logging::log_detail(&format!("surface_id={}", surface_id));
                    }

                    self.events
                        .emit(ChannelEvent::SurfaceDestroyed {
                            display_channel_id: self.channel_id,
                            surface_id,
                        })
                        .await;
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
                // Only logged, so a malformed one is only noted.
                match DisplayMonitorsConfig::decode(payload) {
                    Ok(config) => {
                        info!(
                            "display: monitors_config: count={}, max_allowed={}, channel_id={}",
                            config.heads.len(),
                            config.max_allowed,
                            self.channel_id
                        );
                        for (i, head) in config.heads.iter().enumerate() {
                            info!(
                                "display: monitors_config[{}]: head_id={}, surface_id={}, {}x{}, pos=({},{}), flags={:#x}",
                                i,
                                head.monitor_id,
                                head.surface_id,
                                head.width,
                                head.height,
                                head.x,
                                head.y,
                                head.flags
                            );
                        }
                    }
                    Err(e) => debug!("display: malformed monitors_config: {}", e),
                }
            }

            display_server::MARK => {
                debug!("display: mark");
                self.events
                    .emit(ChannelEvent::DisplayMark {
                        produced_at_secs: self.traffic.elapsed().as_secs_f64(),
                    })
                    .await;
            }

            display_server::SET_ACK => {
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
                let response = make_message(display_client::ACK_SYNC, &ack_payload);
                self.send_with_log(display_client::ACK_SYNC, &response)
                    .await?;
            }

            display_server::PING => {
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
                let response = make_message(display_client::PONG, &pong_payload);
                self.send_with_log(display_client::PONG, &response).await?;
                self.pong_send_count = self.pong_send_count.saturating_add(1);
            }

            display_server::NOTIFY => {
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
                        warn!("display: server notify (error): {}", message)
                    }
                    NotifySeverity::Warn => {
                        warn!("display: server notify (warn): {}", message)
                    }
                    NotifySeverity::Info => {
                        info!("display: server notify: {}", message)
                    }
                }
                let mut entry = NotificationEntry::new(
                    severity,
                    NotificationSource::Spice {
                        channel: ChannelType::Display,
                        what: notify.what,
                    },
                    message,
                );
                if let Some(v) = notify.visibility_kind() {
                    entry = entry.with_visibility(v);
                }
                self.events.emit(ChannelEvent::Notification(entry)).await;
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
                        if self.image_cache.remove(&id) {
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

            // Palettes are only used by the 1/4/8-bit bitmap formats and
            // LZ_PLT images, none of which we decode, so there is no palette
            // cache to invalidate. spice-server sends INVAL_ALL_PALETTES on
            // every connect; handling it here keeps it out of the unknown-
            // opcode path, which raised a Gap warning each time (#446).
            display_server::INVAL_PALETTE | display_server::INVAL_ALL_PALETTES => {
                debug!("display: palette invalidation (no palette cache), ignoring");
            }

            display_server::RESET => {
                info!("display: reset");
                self.image_cache.clear();
                self.glz_dictionary.clear();
            }

            display_server::STREAM_CREATE => {
                // A malformed STREAM_CREATE is ignored, as a short one always
                // was. The clip type is part of the message, so one that
                // stops before it is malformed.
                if let Ok(create) = StreamCreate::decode(payload) {
                    let surface_id = create.surface_id;
                    let stream_id = create.id;
                    let codec_type = create.codec_type;
                    let stream_w = create.stream_width;
                    let stream_h = create.stream_height;
                    // The wire's edges are signed; ryll keeps their bits.
                    let (dest_left, dest_top, dest_right, dest_bottom) = ltrb(&create.dest);

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

                    // Cap concurrent streams. `stream_id` is
                    // server-chosen and each entry owns a decoder, so
                    // an uncapped map is a memory-exhaustion primitive.
                    //
                    // A re-CREATE on an id already held replaces that
                    // entry rather than adding one, so it cannot grow
                    // the map and is exempt. Checking before the
                    // decoder is built matters: `video::for_stream`
                    // allocates an openh264 instance costing
                    // megabytes, and a server flooding CREATEs past
                    // the cap must not be able to drive that
                    // allocation once per message.
                    let replacing_live_stream = self.streams.contains_key(&stream_id);
                    if !replacing_live_stream && self.streams.len() >= MAX_CONCURRENT_STREAMS {
                        self.streams_rejected_total = self.streams_rejected_total.saturating_add(1);
                        // Warn on the first refusal only: a server that
                        // hits the cap once will usually keep hitting
                        // it, and the counter carries the rest.
                        if self.streams_rejected_total == 1 {
                            warn!(
                                "display: stream_create: id={} refused, {} streams already \
                                 open (cap {}); further refusals logged at debug",
                                stream_id,
                                self.streams.len(),
                                MAX_CONCURRENT_STREAMS,
                            );
                        } else {
                            debug!(
                                "display: stream_create: id={} refused (cap {}); \
                                 rejected_total={}",
                                stream_id, MAX_CONCURRENT_STREAMS, self.streams_rejected_total,
                            );
                        }
                        return Ok(());
                    }

                    // Select the video decoder for this stream's codec.
                    // If the codec is unsupported, log and skip the stream
                    // (preserving the pre-refactor behaviour where
                    // unsupported codecs were ignored).
                    //
                    // Bound *before* any teardown below. A re-CREATE
                    // naming a codec this build cannot decode must
                    // leave the working stream it names alone: retiring
                    // first and discovering the codec second would
                    // blank the promoted region for the rest of the
                    // session, which is strictly worse than ignoring
                    // the message.
                    let video_decoder =
                        match video::for_stream(codec_type, self.jpeg_decoder.clone()) {
                            Ok(dec) => dec,
                            Err(VideoDecoderError::UnsupportedCodec(ct)) => {
                                warn!(
                                    "display: stream_create: unsupported codec {} \
                                     for stream {} — skipping",
                                    ct, stream_id
                                );
                                return Ok(());
                            }
                            Err(e) => {
                                warn!(
                                    "display: stream_create: failed to create decoder \
                                     for stream {}: {}",
                                    stream_id, e
                                );
                                return Ok(());
                            }
                        };

                    // A re-CREATE on a live id replaces the entry.
                    // Route it through the teardown path so the
                    // outgoing stream's counters reach the
                    // recently-destroyed ring and
                    // `streams_destroyed_total` stays paired with
                    // `streams_created_total`; dropping the old
                    // `StreamState` silently would lose both, along
                    // with the decoder it owns.
                    let now = self.traffic.elapsed().as_secs_f64();
                    if let Some(previous) = self.streams.remove(&stream_id) {
                        warn!(
                            "display: stream_create: id={} re-created while still \
                             open — retiring the previous stream",
                            stream_id
                        );
                        self.retire_stream(stream_id, &previous, now);
                        self.streams_destroyed_total =
                            self.streams_destroyed_total.saturating_add(1);
                    }

                    self.streams.insert(
                        stream_id,
                        StreamState {
                            surface_id,
                            codec_type,
                            stream_width: stream_w,
                            stream_height: stream_h,
                            dest_top,
                            dest_left,
                            dest_bottom,
                            dest_right,
                            video_decoder,
                            created_at_secs: now,
                            frames_received: 0,
                            frames_decoded_ok: 0,
                            frames_decode_failed: 0,
                            last_frame_ts_secs: None,
                            last_decode_ok_ts_secs: None,
                            last_decode_duration_us: 0,
                            warned_never_decodes: false,
                            report_is_active: false,
                            report_unique_id: 0,
                            report_max_window_size: 0,
                            report_timeout_ms: 0,
                            report_num_frames: 0,
                            report_num_drops: 0,
                            report_drops_seq_len: 0,
                            report_start_frame_mm_time: 0,
                            report_end_frame_mm_time: 0,
                            report_start_now_mm_time: 0,
                            report_send_count: 0,
                            last_report_sent_ts_secs: None,
                            last_report_num_frames: 0,
                            last_report_num_drops: 0,
                            last_report_last_frame_delay: 0,
                        },
                    );
                    self.streams_created_total = self.streams_created_total.saturating_add(1);
                } else {
                    warn_once!(
                        "display:decode_failure:stream_create:malformed",
                        "display: stream_create: malformed message ignored ({} bytes)",
                        payload.len()
                    );
                }
            }

            display_server::STREAM_DATA | display_server::STREAM_DATA_SIZED => {
                // A malformed frame is ignored, as a short one always was.
                // One whose data_size runs past the body is malformed. The
                // frame is borrowed from the payload, not copied.
                let mut r = BoundedReader::new(payload);
                let frame = if msg_type == display_server::STREAM_DATA_SIZED {
                    StreamDataSizedRef::read(&mut r).map(|frame| {
                        // The wire's edges are signed; ryll keeps their bits.
                        let (left, top, right, bottom) = ltrb(&frame.dest);
                        (frame.base, Some((top, left, bottom, right)), frame.data)
                    })
                } else {
                    StreamDataRef::read(&mut r).map(|frame| (frame.base, None, frame.data))
                };
                let Ok((base, dest, jpeg_data)) = frame else {
                    warn_once!(
                        "display:decode_failure:stream_data:malformed",
                        "display: stream_data: malformed frame dropped ({} bytes)",
                        payload.len()
                    );
                    return Ok(());
                };
                let (stream_id, frame_mm_time) = (base.id, base.multi_media_time);

                // Evaluate STREAM_REPORT bookkeeping BEFORE the MJPEG
                // decode dispatch — `report_num_frames` counts every
                // STREAM_DATA for an active stream, irrespective of
                // decode outcome. The borrow on `self.streams` is
                // released at the end of the `if let Some(stream)`
                // scope so we can call `self.send_stream_report`
                // afterwards without a borrow conflict.
                let now_mm_time = self.mm_clock.now();
                let report_action: Option<u32> =
                    if let Some(stream) = self.streams.get_mut(&stream_id) {
                        let now_secs = self.traffic.elapsed().as_secs_f64();
                        stream.frames_received = stream.frames_received.saturating_add(1);
                        stream.last_frame_ts_secs = Some(now_secs);

                        let mut send: Option<u32> = None;
                        if stream.report_is_active {
                            if stream.report_num_frames == 0 {
                                stream.report_start_frame_mm_time = frame_mm_time;
                                stream.report_start_now_mm_time = now_mm_time;
                            }
                            stream.report_num_frames = stream.report_num_frames.saturating_add(1);
                            stream.report_end_frame_mm_time = frame_mm_time;

                            // Modular i32 subtraction; mm_time wraps at
                            // 2^32 ms so we cast through i64 and narrow.
                            // Matches spice-gtk's spice_mmtime_diff
                            // helper at channel-display.c:1482. Used
                            // here only for the drop-counter check;
                            // STREAM_REPORT's `last_frame_delay` field
                            // is recomputed at send time inside
                            // `send_stream_report`.
                            let last_frame_delay: i32 =
                                (frame_mm_time as i64).wrapping_sub(now_mm_time as i64) as i32;

                            if last_frame_delay < 0 {
                                stream.report_num_drops = stream.report_num_drops.saturating_add(1);
                                stream.report_drops_seq_len =
                                    stream.report_drops_seq_len.saturating_add(1);
                            } else {
                                stream.report_drops_seq_len = 0;
                            }

                            let elapsed_since_window_start: i32 = (now_mm_time as i64)
                                .wrapping_sub(stream.report_start_now_mm_time as i64)
                                as i32;

                            if stream_report_should_send(
                                stream.report_num_frames,
                                stream.report_max_window_size,
                                elapsed_since_window_start,
                                stream.report_timeout_ms,
                                stream.report_drops_seq_len,
                            ) {
                                send = Some(stream_id);
                            }
                        }
                        send
                    } else {
                        None
                    };

                if let Some(sid) = report_action {
                    self.send_stream_report(sid).await?;
                }

                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    let now_secs = self.traffic.elapsed().as_secs_f64();

                    let (top, left, bottom, right) = dest.unwrap_or((
                        stream.dest_top,
                        stream.dest_left,
                        stream.dest_bottom,
                        stream.dest_right,
                    ));
                    let w = right.saturating_sub(left);
                    let h = bottom.saturating_sub(top);

                    // Codec-agnostic decode dispatch. Pre-refactor
                    // per-codec logic (DHT extract/inject for MJPEG)
                    // has been absorbed into `MJpegVideoDecoder`.
                    let decode_start = std::time::Instant::now();
                    let decode_result = stream.video_decoder.decode(jpeg_data);
                    let decode_duration_us =
                        u32::try_from(decode_start.elapsed().as_micros()).unwrap_or(u32::MAX);
                    // Crop to the dest before the counters below, so a
                    // frame too short for its own dimensions counts as
                    // the decode failure it is.
                    let decode_result = decode_result.and_then(|frame| {
                        frame
                            .map(|frame| crop_frame_to_dest(frame, w, h))
                            .transpose()
                    });

                    // Aggregate decode duration tracking per codec. Gate on
                    // codec_type so each ring receives only its own samples —
                    // keeping the two streams separate lets bug reports tell
                    // MJPEG and H.264 cost apart. `Ok(None)` (H.264 "needs more
                    // data") counts toward total but not toward failures.
                    if stream.codec_type == SPICE_VIDEO_CODEC_TYPE_MJPEG {
                        self.mjpeg_decode_total_count =
                            self.mjpeg_decode_total_count.saturating_add(1);
                        self.mjpeg_recent_durations.push_back(decode_duration_us);
                        if self.mjpeg_recent_durations.len() > MAX_RECENT_DECODES {
                            self.mjpeg_recent_durations.pop_front();
                        }
                        if decode_result.is_err() {
                            self.mjpeg_decode_failed_count =
                                self.mjpeg_decode_failed_count.saturating_add(1);
                        }
                    } else if stream.codec_type == SPICE_VIDEO_CODEC_TYPE_H264 {
                        self.h264_decode_total_count =
                            self.h264_decode_total_count.saturating_add(1);
                        self.h264_recent_durations.push_back(decode_duration_us);
                        if self.h264_recent_durations.len() > MAX_RECENT_DECODES {
                            self.h264_recent_durations.pop_front();
                        }
                        if decode_result.is_err() {
                            self.h264_decode_failed_count =
                                self.h264_decode_failed_count.saturating_add(1);
                        }
                    }

                    match decode_result {
                        Ok(Some(frame)) => {
                            debug!(
                                "display: stream {} {} frame, cropped to {}x{} → ({},{})",
                                stream_id,
                                stream.video_decoder.name(),
                                frame.width,
                                frame.height,
                                left,
                                top
                            );
                            stream.frames_decoded_ok = stream.frames_decoded_ok.saturating_add(1);
                            stream.last_decode_ok_ts_secs = Some(now_secs);
                            stream.last_decode_duration_us = decode_duration_us;
                            let surface_id = stream.surface_id;
                            self.events
                                .emit(ChannelEvent::ImageReady {
                                    display_channel_id: self.channel_id,
                                    surface_id,
                                    left,
                                    top,
                                    width: frame.width,
                                    height: frame.height,
                                    pixels: frame.rgba,
                                    image_id: 0,
                                    produced_at_secs: now_secs,
                                })
                                .await;
                        }
                        Ok(None) => {
                            // No complete frame assembled yet — this is
                            // normal for H.264 (needs multiple packets
                            // per frame) and should not occur for MJPEG.
                            debug!(
                                "display: stream {} decoder returned no frame \
                                 (codec={})",
                                stream_id, stream.codec_type
                            );
                        }
                        Err(VideoDecoderError::Decode(ref msg)) => {
                            debug!("display: stream {} decode failed: {}", stream_id, msg);
                            stream.frames_decode_failed =
                                stream.frames_decode_failed.saturating_add(1);
                        }
                        Err(VideoDecoderError::UnsupportedCodec(codec)) => {
                            // Should not happen: `for_stream` only
                            // constructs a decoder for supported codecs
                            // and STREAM_CREATE skips the rest, so no
                            // stream should exist whose decoder can
                            // return this. Treat it as a decode failure
                            // rather than a panic — this is a
                            // server-message handler, and the
                            // convention that makes it impossible lives
                            // 200 lines away in another function.
                            warn_once!(
                                "display:decode_failure:stream_data:unsupported_codec",
                                "display: stream decoder reported an unsupported codec; \
                                 treating the frame as a decode failure"
                            );
                            debug!(
                                "display: stream {} decoder reported unsupported codec {}",
                                stream_id, codec
                            );
                            stream.frames_decode_failed =
                                stream.frames_decode_failed.saturating_add(1);
                        }
                    }
                    warn_if_h264_never_decodes(stream_id, stream);
                } else {
                    self.stream_data_orphan_count = self.stream_data_orphan_count.saturating_add(1);
                    debug!(
                        "display: stream_data for unknown stream {} \
                         (orphan_count={})",
                        stream_id, self.stream_data_orphan_count
                    );
                }
            }

            display_server::STREAM_CLIP => {
                // Only logged; a malformed one is ignored.
                if let Ok(clip) = StreamClip::decode(payload) {
                    debug!("display: stream_clip id={}", clip.id);
                }
            }

            display_server::STREAM_DESTROY => {
                // A short STREAM_DESTROY is ignored.
                if let Ok(StreamDestroy { id: stream_id }) = StreamDestroy::decode(payload) {
                    let now = self.traffic.elapsed().as_secs_f64();
                    if let Some(state) = self.streams.remove(&stream_id) {
                        self.retire_stream(stream_id, &state, now);
                        self.streams_destroyed_total =
                            self.streams_destroyed_total.saturating_add(1);
                    } else {
                        // Server destroyed a stream we never saw
                        // created — log and move on. Not counted as
                        // a real destruction.
                        info!("display: stream_destroy id={} (unknown stream)", stream_id);
                    }
                }
            }

            display_server::STREAM_DESTROY_ALL => {
                // Empty payload — server signals "tear down every
                // active stream", typically before a resolution
                // change or surface reconfiguration. Equivalent to
                // spice-gtk's clear_streams() at
                // channel-display.c:1855.
                let cleared = self.streams.len() as u64;
                info!("display: stream_destroy_all (clearing {} streams)", cleared);
                let now = self.traffic.elapsed().as_secs_f64();
                // Drain the map into the recently-destroyed ring so
                // each stream's final counters survive teardown.
                // Sort by id for stable retire order in the log.
                let mut drained: Vec<(u32, StreamState)> = self.streams.drain().collect();
                drained.sort_by_key(|(id, _)| *id);
                for (id, state) in drained {
                    self.retire_stream(id, &state, now);
                }
                self.streams_destroyed_total = self.streams_destroyed_total.saturating_add(cleared);
            }

            display_server::STREAM_ACTIVATE_REPORT => {
                let Ok(StreamActivateReport {
                    stream_id,
                    unique_id,
                    max_window_size: raw_max_window_size,
                    timeout_ms: raw_timeout_ms,
                }) = StreamActivateReport::decode(payload)
                else {
                    warn!(
                        "display: short stream_activate_report payload ({} bytes)",
                        payload.len()
                    );
                    return Ok(());
                };

                let (max_window_size, timeout_ms) =
                    clamp_stream_report_params(raw_max_window_size, raw_timeout_ms);
                if max_window_size != raw_max_window_size || timeout_ms != raw_timeout_ms {
                    warn!(
                        "display: stream_activate_report: id={} clamped window {}→{} \
                         timeout_ms {}→{}",
                        stream_id, raw_max_window_size, max_window_size, raw_timeout_ms, timeout_ms
                    );
                }

                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    info!(
                        "display: stream_activate_report: id={} unique_id={} window={} timeout_ms={}",
                        stream_id, unique_id, max_window_size, timeout_ms
                    );
                    stream.report_is_active = true;
                    stream.report_unique_id = unique_id;
                    stream.report_max_window_size = max_window_size;
                    stream.report_timeout_ms = timeout_ms;
                    // Reset rolling counters so the first frame starts a
                    // fresh window. Cumulative counters left alone.
                    stream.report_num_frames = 0;
                    stream.report_num_drops = 0;
                    stream.report_drops_seq_len = 0;
                    stream.report_start_frame_mm_time = 0;
                    stream.report_end_frame_mm_time = 0;
                    stream.report_start_now_mm_time = 0;
                } else {
                    warn!(
                        "display: stream_activate_report for unknown stream id={}",
                        stream_id
                    );
                }
            }

            _ => {
                // Unknown opcode — log hex once per msg_type, silent on repeat.
                logging::log_unknown_once("display", msg_type, payload);
                self.opcodes.note_unknown(msg_type);
            }
        }

        Ok(())
    }

    async fn handle_draw_copy(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < DrawBase::MIN_SIZE {
            warn_once!(
                "display:decode_failure:draw_copy:short_payload",
                "display: draw_copy payload too short"
            );
            return Ok(());
        }

        // A DrawBase whose clip rectangles are cut short ends the channel,
        // as every DrawBase failure does.
        let mut r = BoundedReader::new(payload);
        let base = DrawBase::read(&mut r).context("malformed DRAW_COPY base")?;

        if self.log_config.verbose {
            let (left, top, right, bottom) = ltrb(&base.bbox);
            logging::log_detail(&format!(
                "surface={}, rect=({},{}) to ({},{}), clip_type={}",
                base.surface_id, left, top, right, bottom, base.clip.clip_type
            ));
        }

        // The SpiceCopy follows: a src_bitmap offset pointing to the
        // SpiceImage, then the rest of its 36 bytes. A body too short for
        // the offset and one too short for the rest warn separately.
        let short_of_offset = r.remaining() < 4;
        let Ok(copy) = SpiceCopy::read(&mut r) else {
            if short_of_offset {
                warn_once!(
                    "display:decode_failure:draw_copy:short_spice_copy",
                    "display: draw_copy: payload too short for SpiceCopy"
                );
            } else {
                warn_once!(
                    "display:decode_failure:draw_copy:short_spice_copy_header",
                    "display: draw_copy: payload too short for SpiceCopy header"
                );
            }
            return Ok(());
        };

        if self.log_config.verbose {
            debug!(
                "display: draw_copy detail: rop={:#x}, scale={}, mask={:#x}, \
                 pos=({},{}), mask_bmp={}, clip_type={}, clip_rects={}",
                copy.rop_descriptor,
                copy.scale_mode,
                copy.mask.flags,
                copy.mask.pos.x,
                copy.mask.pos.y,
                copy.mask.bitmap_offset,
                base.clip.clip_type,
                base.clip.rects.len()
            );
        }

        self.decode_copy_image_and_emit(payload, "draw_copy", &base, &copy, r.position())
            .await
    }

    /// Decode and draw the source image of a DRAW_COPY or DRAW_BLEND,
    /// found through the protocol crate's pointer resolution.
    ///
    /// The image's bytes run to the mask image, if it comes next, or to
    /// the end of the payload. A pointer into the draw's fixed fields is
    /// refused.
    async fn decode_copy_image_and_emit(
        &mut self,
        payload: &[u8],
        op_name: &str,
        base: &DrawBase,
        copy: &SpiceCopy,
        fixed_len: usize,
    ) -> Result<()> {
        let src_bitmap_offset = copy.src_bitmap as usize;
        let mut image = match copy.src_bitmap_reader(payload, fixed_len) {
            Ok(Some(image)) => image,
            Ok(None) => {
                warn_null_src_bitmap(op_name);
                return Ok(());
            }
            Err(LinkError::PointerIntoFixedPart { .. }) => {
                logging::warn_once_impl(
                    logging::intern_key(format!(
                        "display:decode_failure:{}:src_bitmap_in_fixed_part",
                        op_name
                    )),
                    &format!(
                        "display: {}: src_bitmap {} points into the first {} bytes, \
                         which are fixed fields",
                        op_name, src_bitmap_offset, fixed_len
                    ),
                );
                return Ok(());
            }
            Err(_) => {
                warn_short_img_desc(op_name, payload.len(), src_bitmap_offset);
                return Ok(());
            }
        };
        let available = image.remaining();
        let Ok(img_desc) = ImageDescriptor::read(&mut image) else {
            warn_short_img_desc(op_name, src_bitmap_offset + available, src_bitmap_offset);
            return Ok(());
        };
        let image_data = image.read_bytes(image.remaining())?;

        self.decode_image_and_emit(
            op_name,
            base,
            &img_desc,
            image_data,
            &copy.src_area,
            CompositeMode::Overwrite,
        )
        .await
    }

    /// Decode and draw the source image of a DRAW_OPAQUE,
    /// DRAW_TRANSPARENT or DRAW_ALPHA_BLEND, at an offset into the
    /// payload. Its bytes run to the end of the payload.
    #[allow(clippy::too_many_arguments)]
    async fn decode_image_at_offset_and_emit(
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
            warn_null_src_bitmap(op_name);
            return Ok(());
        }

        let image_start = src_bitmap_offset;
        if payload.len() < image_start + ImageDescriptor::SIZE {
            warn_short_img_desc(op_name, payload.len(), src_bitmap_offset);
            return Ok(());
        }

        let img_desc = ImageDescriptor::decode(&payload[image_start..])?;
        let image_data = &payload[image_start + ImageDescriptor::SIZE..];
        // The source rect as spice.proto's signed Rect, with the same
        // bits.
        let src_area = Rect {
            top: src_top as i32,
            left: src_left as i32,
            bottom: src_bottom as i32,
            right: src_right as i32,
        };
        self.decode_image_and_emit(op_name, base, &img_desc, image_data, &src_area, composite)
            .await
    }

    /// Decode `image_data`, the bytes after `img_desc`, and draw
    /// `src_area` of it into `base`'s box.
    ///
    /// `image_data` may be empty. A FromCache image is only its
    /// descriptor, and spice-server marshals the source image after the
    /// draw's fixed fields and before any mask, so a cache hit on a draw
    /// without a mask ends the payload. Each arm below checks the length
    /// of the data it reads.
    async fn decode_image_and_emit(
        &mut self,
        op_name: &str,
        base: &DrawBase,
        img_desc: &ImageDescriptor,
        image_data: &[u8],
        src_area: &Rect,
        composite: CompositeMode,
    ) -> Result<()> {
        let image_type = ImageType::from_u8(img_desc.image_type);
        let (src_left, src_top, src_right, src_bottom) = ltrb(src_area);

        debug!(
            "display: {}: surface={}, pos=({},{}), size={}x{}, type={:?}, id={}, \
             flags={}, data_bytes={}",
            op_name,
            base.surface_id,
            base.bbox.left as u32,
            base.bbox.top as u32,
            img_desc.width,
            img_desc.height,
            image_type,
            img_desc.image_id,
            img_desc.flags,
            image_data.len()
        );

        // Decode/decompress based on type. The bracket here measures
        // only the decompression dispatch, not header parsing or the
        // downstream emit; that scope matches the diagnostic question
        // "how long does decode itself take per image".
        let decode_start = Instant::now();
        let decompressed: Option<DecompressedImage> = match image_type {
            Some(ImageType::Pixmap) => {
                // BitmapData: a BitmapHeader (18 bytes, or 22 when the
                // palette comes from the cache), then raw pixel rows. The
                // rows are checked below rather than by the protocol
                // crate's BitmapPayload, so that each way they can be
                // wrong keeps its own warning.
                let mut bitmap = BoundedReader::new(image_data);
                if let Ok(header) = BitmapHeader::read(&mut bitmap) {
                    let bmp_fmt = header.format;
                    let bmp_flags = header.flags;
                    let bmp_width = header.x;
                    let bmp_height = header.y;
                    let bmp_stride = header.stride;
                    let top_down = (bmp_flags & bitmap_flags::TOP_DOWN) != 0;
                    // The palette is ignored: 32-bit formats have none.
                    let pixel_data = bitmap.read_bytes(bitmap.remaining())?;

                    debug!(
                        "display: pixmap fmt={}, flags={:#x}, {}x{}, stride={}, top_down={}",
                        bmp_fmt, bmp_flags, bmp_width, bmp_height, bmp_stride, top_down
                    );

                    // Only 32-bit BGRX (fmt=8) and RGBA (fmt=9) are supported
                    if bmp_fmt != bitmap_fmt::BIT32 && bmp_fmt != bitmap_fmt::RGBA {
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

                    // A server can send u32::MAX on any of these. `rgba_len`
                    // bounds the allocation below; stride * height is checked
                    // separately so it cannot wrap past the short-data check.
                    let Some(expected_pixels) = limits::rgba_len(width_usize, height_usize) else {
                        warn_once!(
                            "display:decode_failure:pixmap:too_large",
                            "display: pixmap {}x{} outside the {} pixel / {} per-side limits, skipping",
                            width,
                            height,
                            limits::MAX_IMAGE_PIXELS,
                            limits::MAX_IMAGE_DIMENSION
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

                    // Each row copies width * 4 bytes, so a row must fit
                    // its stride or the copy slices past the pixel data
                    // even when stride * height fits it (a 4-byte stride
                    // with a 16384-pixel width).
                    let row_fits_stride = width_usize
                        .checked_mul(4)
                        .is_some_and(|row_bytes| row_bytes <= stride);

                    if needed_bytes > pixel_data.len() {
                        warn_once!(
                            "display:decode_failure:pixmap:short_pixel_data",
                            "display: pixmap data too short (have {}, need {})",
                            pixel_data.len(),
                            needed_bytes
                        );
                        None
                    } else if !row_fits_stride {
                        warn_once!(
                            "display:decode_failure:pixmap:stride_too_small",
                            "display: pixmap stride {} too small for width {}, skipping",
                            stride,
                            width
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
                                rgba[di + 3] = if bmp_fmt == bitmap_fmt::RGBA {
                                    src_row[si + 3]
                                } else {
                                    255
                                };
                            }
                        }
                        let image = DecompressedImage::new(width, height, rgba, img_desc.image_id);
                        if image.is_none() {
                            warn_once!(
                                "display:decode_failure:pixmap:dimensions_refused",
                                "display: pixmap dimensions {}x{} refused, skipping",
                                width,
                                height
                            );
                        }
                        image
                    }
                } else {
                    warn_once!(
                        "display:decode_failure:pixmap:short_bitmap_data",
                        "display: pixmap BitmapData header too short"
                    );
                    None
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
                    let declared_glz_size = read_u32_le(image_data, 0) as usize;
                    let zlib_size = read_u32_le(image_data, 4) as usize;

                    let zlib_data = &image_data[8..8 + zlib_size.min(image_data.len() - 8)];
                    match inflate_glz_stream(
                        zlib_data,
                        declared_glz_size,
                        img_desc.width as usize,
                        img_desc.height as usize,
                    ) {
                        Ok(glz_data) => match decompress_glz(&glz_data, &self.glz_dictionary).await
                        {
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
                        Err(InflateGlzError::DimensionsRefused) => {
                            warn_once!(
                                "display:decode_failure:zlib_glz:dimensions_refused",
                                "display: ZLIB_GLZ_RGB image dimensions refused: {}x{}",
                                img_desc.width,
                                img_desc.height
                            );
                            None
                        }
                        Err(InflateGlzError::DeclaredTooLarge) => {
                            warn_once!(
                                "display:decode_failure:zlib_glz:declared_too_large",
                                "display: ZLIB_GLZ_RGB declared GLZ size {} exceeds limit for {}x{}",
                                declared_glz_size,
                                img_desc.width,
                                img_desc.height
                            );
                            None
                        }
                        Err(InflateGlzError::ExceedsDeclared) => {
                            warn_once!(
                                "display:decode_failure:zlib_glz:inflate_too_large",
                                "display: ZLIB_GLZ_RGB inflated past its declared GLZ size {}",
                                declared_glz_size
                            );
                            None
                        }
                        Err(InflateGlzError::SizeMismatch { inflated }) => {
                            warn_once!(
                                "display:decode_failure:zlib_glz:size_mismatch",
                                "display: ZLIB_GLZ_RGB declared GLZ size {} but inflated {} bytes",
                                declared_glz_size,
                                inflated
                            );
                            None
                        }
                        Err(InflateGlzError::Zlib(e)) => {
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
                // LZ4: BinaryData wrapper (4-byte data_size, then a
                // top-down byte, a bitmap format byte and big-endian
                // length-prefixed LZ4 blocks, which the decoder reads).
                if let Ok(lz4_data) = BinaryData::read_data(&mut BoundedReader::new(image_data)) {
                    let decoded = decompress_spice_lz4(
                        lz4_data,
                        img_desc.width as usize,
                        img_desc.height as usize,
                    );
                    // The decoder logs which check failed; this records
                    // the gap once a session.
                    if decoded.is_none() {
                        warn_once!(
                            "display:decode_failure:lz4:decode_failed",
                            "display: LZ4 decode failed ({}x{})",
                            img_desc.width,
                            img_desc.height
                        );
                    }
                    decoded
                } else {
                    // Shorter than its size field, or than the size it
                    // declares.
                    warn_once!(
                        "display:decode_failure:lz4:short_data",
                        "display: LZ4 data too short"
                    );
                    None
                }
            }
            Some(ImageType::FromCache) | Some(ImageType::FromCacheLossless) => {
                // FromCacheLossless names an entry the server knows is
                // lossless, either sent that way or since replaced through
                // CACHE_REPLACE_ME, so it is the same lookup.
                if let Some(pixels) = self.image_cache.get(&img_desc.image_id) {
                    // The cache holds pixels without dimensions, so these
                    // come from the descriptor and the server can name
                    // any size for a cached id. Check the fit before
                    // cloning, so a mismatched id cannot make every draw
                    // copy a large buffer only to throw it away; the
                    // constructor's own check stays as the backstop.
                    let cached_len = pixels.len();
                    let fits = limits::rgba_len(img_desc.width as usize, img_desc.height as usize)
                        == Some(cached_len);
                    let image = if fits {
                        DecompressedImage::new(
                            img_desc.width,
                            img_desc.height,
                            pixels.clone(),
                            img_desc.image_id,
                        )
                    } else {
                        None
                    };
                    if image.is_none() {
                        warn_once!(
                            "display:decode_failure:from_cache:size_mismatch",
                            "display: cached image {} is {} bytes, does not fit {}x{}",
                            img_desc.image_id,
                            cached_len,
                            img_desc.width,
                            img_desc.height
                        );
                    }
                    image
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
                // JPEG: BinaryData wrapper (4-byte data_size + JPEG stream).
                // Route through the per-platform `JpegDecoder` selected in
                // `best_for_platform()` (ImageIO / WIC / mozjpeg / pure-Rust)
                // rather than the `image` crate's pure-Rust path. Session 006
                // measured the old path at ~263 ms / frame at 1920×1472 on a
                // Mac that has ImageIO available.
                if let Ok(jpeg_data) = BinaryData::read_data(&mut BoundedReader::new(image_data)) {
                    let decoded = self.jpeg_decoder.decode(jpeg_data).and_then(|dec| {
                        DecompressedImage::new(dec.width, dec.height, dec.rgba, img_desc.image_id)
                    });
                    // `new` refusing is a backstop: `DecodedJpeg` already
                    // applied `rgba_len`.
                    if decoded.is_none() {
                        warn_once!(
                            "display:decode_failure:jpeg:decode_failed",
                            "display: JPEG decode failed (backend {})",
                            self.jpeg_decoder.name()
                        );
                    }
                    decoded
                } else {
                    // Shorter than its size field, or than the size it
                    // declares.
                    warn_once!(
                        "display:decode_failure:jpeg:short_data",
                        "display: JPEG data too short"
                    );
                    None
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
                    let decoded =
                        quic_decode(quic_data, img_desc.width, img_desc.height).and_then(|rgba| {
                            DecompressedImage::new(
                                img_desc.width,
                                img_desc.height,
                                rgba,
                                img_desc.image_id,
                            )
                        });
                    // `new` refusing is a backstop: `quic_decode` already
                    // applied `rgba_len`.
                    if decoded.is_none() {
                        warn_once!(
                            "display:decode_failure:quic:decode_failed",
                            "display: QUIC decode failed"
                        );
                    }
                    decoded
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
        let is_from_cache = matches!(
            image_type,
            Some(ImageType::FromCache) | Some(ImageType::FromCacheLossless)
        );
        let decode_duration_us = if is_from_cache {
            0
        } else {
            u32::try_from(decode_start.elapsed().as_micros()).unwrap_or(u32::MAX)
        };
        self.record_decode(DecodeResult {
            image_type: format!("{:?}", image_type),
            image_id: img_desc.image_id,
            width: img_desc.width,
            height: img_desc.height,
            from_cache: is_from_cache,
            success: decompressed.is_some(),
            timestamp_secs: self.traffic.elapsed().as_secs_f64(),
            decode_duration_us,
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
            // A GLZ image's id comes from its own header, and names its
            // entry in the GLZ dictionary. Every other image is known by
            // its descriptor's id: some decoders (LZ, LZ4) do not set
            // one, and the descriptor's id is what a later FromCache
            // names.
            let image_id = if is_glz {
                img.image_id
            } else {
                img_desc.image_id
            };
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
            } else if (img_desc.flags & (IMAGE_FLAGS_CACHE_ME | IMAGE_FLAGS_CACHE_REPLACE_ME)) != 0
            {
                // Only cache non-GLZ images when the server requests it.
                // insert() replaces an existing entry, which is all
                // CACHE_REPLACE_ME asks for.
                let _ = self.image_cache.insert(image_id, img.pixels.clone());
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

                // In bounds for any source rect: DecompressedImage
                // guarantees out_pixels is src_w * src_h * 4 bytes, and
                // the rect has just been clamped to src_w x src_h.
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

            let (dest_left, dest_top, _, _) = ltrb(&base.bbox);
            let dest_right = dest_left.saturating_add(out_width);
            let dest_bottom = dest_top.saturating_add(out_height);

            let dest = (dest_left, dest_top, dest_right, dest_bottom);
            let Some(clip) = event_clip(base, dest, op_name) else {
                return Ok(());
            };
            if !clip.is_empty() {
                for (il, it, ir, ib) in clip {
                    // event_clip's rectangles are disjoint and inside the
                    // destination box, which is at most out_width x
                    // out_height (less where saturating_add clipped it), so
                    // like the crop above each sub-copy stays inside
                    // out_pixels, and together they copy at most the
                    // image once.
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

                    self.events
                        .emit(build_image_event(
                            composite,
                            self.channel_id,
                            base.surface_id,
                            il,
                            it,
                            sub_w as u32,
                            sub_h as u32,
                            sub_pixels,
                            image_id,
                            self.traffic.elapsed().as_secs_f64(),
                        ))
                        .await;
                }
            } else {
                self.events
                    .emit(build_image_event(
                        composite,
                        self.channel_id,
                        base.surface_id,
                        dest_left,
                        dest_top,
                        out_width,
                        out_height,
                        out_pixels,
                        image_id,
                        self.traffic.elapsed().as_secs_f64(),
                    ))
                    .await;
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
                let rect = ltrb(&base.bbox);
                let Some(clip) = event_clip(&base, rect, "draw_fill") else {
                    return Ok(());
                };
                self.events
                    .emit(ChannelEvent::FillRect {
                        display_channel_id: self.channel_id,
                        surface_id: base.surface_id,
                        rect,
                        colour,
                        clip,
                        produced_at_secs: self.traffic.elapsed().as_secs_f64(),
                    })
                    .await;
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

        let rect = ltrb(&base.bbox);
        let Some(clip) = event_clip(&base, rect, op_name) else {
            return Ok(());
        };
        self.events
            .emit(ChannelEvent::FillRect {
                display_channel_id: self.channel_id,
                surface_id: base.surface_id,
                rect,
                colour,
                clip,
                produced_at_secs: self.traffic.elapsed().as_secs_f64(),
            })
            .await;

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
        // DRAW_BLACKNESS / DRAW_WHITENESS, so the shared solid-fill decoder slots in
        // unchanged — only the paint semantic differs.
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

        let rect = ltrb(&base.bbox);
        let Some(clip) = event_clip(&base, rect, "draw_invers") else {
            return Ok(());
        };
        self.events
            .emit(ChannelEvent::Invert {
                display_channel_id: self.channel_id,
                surface_id: base.surface_id,
                rect,
                clip,
                produced_at_secs: self.traffic.elapsed().as_secs_f64(),
            })
            .await;

        Ok(())
    }

    async fn handle_copy_bits(&mut self, payload: &[u8]) -> Result<()> {
        log_draw_base_if_verbose(self.log_config, payload, "copy_bits");
        let CopyBitsOutcome::Copy { base, src_x, src_y } = decode_copy_bits(payload)?;
        let dest_rect = ltrb(&base.bbox);
        let Some(clip) = event_clip(&base, dest_rect, "copy_bits") else {
            return Ok(());
        };

        self.events
            .emit(ChannelEvent::CopyBits {
                display_channel_id: self.channel_id,
                surface_id: base.surface_id,
                src_x,
                src_y,
                dest_rect,
                clip,
                produced_at_secs: self.traffic.elapsed().as_secs_f64(),
            })
            .await;

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
                self.decode_image_at_offset_and_emit(
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
                copy,
                fixed_len,
            } => {
                self.decode_copy_image_and_emit(payload, "draw_blend", &base, &copy, fixed_len)
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
        self.decode_image_at_offset_and_emit(
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
                self.decode_image_at_offset_and_emit(
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
        self.decode_total_count = self.decode_total_count.saturating_add(1);
        if !decode.success {
            self.decode_failed_count = self.decode_failed_count.saturating_add(1);
        }
        if decode.from_cache {
            self.decode_from_cache_count = self.decode_from_cache_count.saturating_add(1);
        }
        self.recent_decodes.push_back(decode);
        if self.recent_decodes.len() > MAX_RECENT_DECODES {
            self.recent_decodes.pop_front();
        }
    }

    /// Sync local state to the shared snapshot.
    fn update_snapshot(&self) {
        let mut snap = self.snapshot.lock().expect("lock poisoned");
        snap.channel_id = self.channel_id;
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
        // The image_cache_* snapshot fields are scoped to the renderer's
        // BoundedImageCache only. Summing the GLZ dictionary into them
        // made a 5 GiB image_cache_bytes reading ambiguous between the
        // two caches and so untriageable from a bug report alone. The
        // GLZ dictionary has its own parallel snapshot fields.
        snap.image_cache_entries = self.image_cache.len();
        snap.image_cache_bytes = self.image_cache.bytes();
        // MRU-first and truncated: `keys()` already yields MRU→LRU,
        // so this is O(MAX_SNAPSHOT_IMAGE_CACHE_IDS) rather than
        // O(entries) plus a sort on every publish. See
        // `MAX_SNAPSHOT_IMAGE_CACHE_IDS`.
        snap.image_cache_ids = self
            .image_cache
            .keys()
            .take(MAX_SNAPSHOT_IMAGE_CACHE_IDS)
            .copied()
            .collect();
        snap.image_cache_evictions_total = self.image_cache.evictions_total();
        snap.image_cache_evicted_bytes_total = self.image_cache.evicted_bytes_total();
        snap.image_cache_cap_bytes = self.image_cache.cap_bytes() as u64;
        snap.glz_dictionary_entries = self.glz_dictionary.len();
        snap.glz_dictionary_bytes = self.glz_dictionary.total_bytes();
        snap.glz_dictionary_cap_bytes = self.glz_dictionary.cap_bytes() as u64;
        snap.glz_dictionary_evictions_total = self.glz_dictionary.evictions_total();
        snap.glz_dictionary_evicted_bytes_total = self.glz_dictionary.evicted_bytes_total();
        snap.recent_decodes = self.recent_decodes.clone();

        // Cumulative decode counters and recent-window decode duration
        // stats. The recent-window aggregate excludes cache hits and
        // failures so it characterises actual decoder cost.
        snap.decode_total_count = self.decode_total_count;
        snap.decode_failed_count = self.decode_failed_count;
        snap.decode_from_cache_count = self.decode_from_cache_count;
        let (min_us, max_us, mean_us) = recent_decode_duration_stats(&self.recent_decodes);
        snap.decode_recent_min_us = min_us;
        snap.decode_recent_max_us = max_us;
        snap.decode_recent_mean_us = mean_us;

        // Socket-read fill stats and ACK-send stats.
        snap.socket_read_count = self.socket_read_count;
        snap.socket_reads_at_chunk_cap = self.socket_reads_at_chunk_cap;
        snap.socket_max_chunk_bytes = self.socket_max_chunk_bytes;
        snap.ack_send_count = self.ack_send_count;
        snap.last_ack_send_ts_secs = self.last_ack_send_ts_secs;
        snap.recent_ack_intervals_secs = self.recent_ack_intervals_secs.clone();

        // Pcap writer-queue drop counter.
        snap.writer_dropped_count = self.capture_dropped_count;

        // Stream diagnostics: copy per-stream counters and the
        // aggregate totals so a bug report can answer "did MJPEG
        // frames arrive / decode / paint?" directly. Active
        // streams are listed in stream_id order for stable JSON
        // output; recently-destroyed entries retain insertion
        // (chronological) order from the ring.
        let mut stream_ids: Vec<u32> = self.streams.keys().copied().collect();
        stream_ids.sort_unstable();
        snap.streams_active = stream_ids
            .into_iter()
            .map(|id| Self::stream_state_to_snapshot(id, &self.streams[&id], None))
            .collect();
        snap.streams_created_total = self.streams_created_total;
        snap.streams_destroyed_total = self.streams_destroyed_total;
        snap.streams_rejected_total = self.streams_rejected_total;
        snap.stream_data_orphan_count = self.stream_data_orphan_count;
        snap.streams_recently_destroyed = self.recently_destroyed_streams.clone();
        snap.stream_reports_sent_total = self.stream_reports_sent_total;
        snap.stream_reports_unsupported_signals_sent = 0; // never written yet

        // Aggregate MJPEG decode duration stats. Mirrors the non-stream
        // decode_recent_* pattern but draws from the MJPEG-only duration ring
        // rather than the per-image decode ring.
        let (mjpeg_min, mjpeg_max, mjpeg_mean) = mjpeg_duration_stats(&self.mjpeg_recent_durations);
        snap.mjpeg_decode_recent_min_us = mjpeg_min;
        snap.mjpeg_decode_recent_max_us = mjpeg_max;
        snap.mjpeg_decode_recent_mean_us = mjpeg_mean;
        snap.mjpeg_decode_total_count = self.mjpeg_decode_total_count;
        snap.mjpeg_decode_failed_count = self.mjpeg_decode_failed_count;

        // Aggregate H.264 decode duration stats. Parallel to the MJPEG
        // block above. Reuses the same `mjpeg_duration_stats` helper
        // (renaming would just add churn; the function is codec-agnostic).
        let (h264_min, h264_max, h264_mean) = mjpeg_duration_stats(&self.h264_recent_durations);
        snap.h264_decode_recent_min_us = h264_min;
        snap.h264_decode_recent_max_us = h264_max;
        snap.h264_decode_recent_mean_us = h264_mean;
        snap.h264_decode_total_count = self.h264_decode_total_count;
        snap.h264_decode_failed_count = self.h264_decode_failed_count;

        // Link-up preference-message send markers. Both flip from false to
        // true once and never reset for the life of the channel — they let a
        // bug report confirm the preferences actually went out without
        // having to read the pcap.
        snap.pref_compression_sent = self.pref_compression_sent;
        snap.pref_video_codec_type_sent = self.pref_video_codec_type_sent;
        snap.h264_offer = self.h264;

        self.opcodes.publish_into(&mut *snap);
    }

    /// Publish the snapshot from the send path, at most once per
    /// `SNAPSHOT_SEND_PUBLISH_MIN_INTERVAL`.
    ///
    /// The read loop calls `update_snapshot` directly and is not
    /// throttled, so this only collapses server-driven bursts of
    /// sends (`STREAM_REPORT` in particular) into one publish.
    fn update_snapshot_throttled(&mut self) {
        let now = Instant::now();
        let due = self
            .last_send_snapshot_publish
            .is_none_or(|last| now.duration_since(last) >= SNAPSHOT_SEND_PUBLISH_MIN_INTERVAL);
        if due {
            self.last_send_snapshot_publish = Some(now);
            self.update_snapshot();
        }
    }

    /// Send a STREAM_REPORT for `stream_id`. Marshals the 32-byte
    /// LE payload per `SpiceMsgcDisplayStreamReport`
    /// (spice.proto:1004-1026), updates the per-stream mirrors,
    /// resets the rolling-window counters, and bumps the
    /// cumulative `stream_reports_sent_total` counter.
    ///
    /// `last_frame_delay` is recomputed here at send time as
    /// `report_end_frame_mm_time - mm_clock.now()` to match
    /// spice-gtk's "margin from the most recent frame, relative
    /// to now" semantic (channel-display.c:1572).
    async fn send_stream_report(&mut self, stream_id: u32) -> Result<()> {
        // Snapshot the values we need into locals so the mutable
        // borrow on `self.streams` ends before we call
        // `send_with_log` (which takes `&mut self`).
        let now_mm_time = self.mm_clock.now();
        let now_secs = self.traffic.elapsed().as_secs_f64();

        let payload = if let Some(stream) = self.streams.get_mut(&stream_id) {
            let last_frame_delay: i32 =
                (stream.report_end_frame_mm_time as i64).wrapping_sub(now_mm_time as i64) as i32;

            let mut buf = Vec::with_capacity(StreamReport::SIZE);
            StreamReport {
                stream_id,
                unique_id: stream.report_unique_id,
                start_frame_mm_time: stream.report_start_frame_mm_time,
                end_frame_mm_time: stream.report_end_frame_mm_time,
                num_frames: stream.report_num_frames,
                num_drops: stream.report_num_drops,
                last_frame_delay,
                // audio_delay = UINT32_MAX: no audio latency is
                // surfaced yet, and measuring it was left out of
                // scope for the stream-caps work.
                audio_delay: u32::MAX,
            }
            .write(&mut buf);

            // Mirror counters into last_report_* and reset rolling.
            stream.last_report_num_frames = stream.report_num_frames;
            stream.last_report_num_drops = stream.report_num_drops;
            stream.last_report_last_frame_delay = last_frame_delay;
            stream.report_send_count = stream.report_send_count.saturating_add(1);
            stream.last_report_sent_ts_secs = Some(now_secs);
            stream.report_num_frames = 0;
            stream.report_num_drops = 0;
            stream.report_drops_seq_len = 0;
            stream.report_start_frame_mm_time = 0;
            stream.report_end_frame_mm_time = 0;
            stream.report_start_now_mm_time = 0;

            Some(buf)
        } else {
            None
        };

        if let Some(payload) = payload {
            let msg = make_message(display_client::STREAM_REPORT, &payload);
            self.send_with_log(display_client::STREAM_REPORT, &msg)
                .await?;
            self.stream_reports_sent_total = self.stream_reports_sent_total.saturating_add(1);
        }

        Ok(())
    }

    async fn send_ack(&mut self) -> Result<()> {
        let msg = make_message(display_client::ACK, &[]);
        self.send_with_log(display_client::ACK, &msg).await?;
        self.last_ack = self.message_count;

        // Record ACK cadence so a bug report can show whether ACK sends
        // stalled. See `docs/plans/PLAN-video-keeping-up.md`.
        let now = self.traffic.elapsed().as_secs_f64();
        if let Some(prev) = self.last_ack_send_ts_secs {
            push_ack_interval(&mut self.recent_ack_intervals_secs, now - prev);
        }
        self.last_ack_send_ts_secs = Some(now);
        self.ack_send_count = self.ack_send_count.saturating_add(1);
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
        self.opcodes.record_send(msg_type);
        let result = self.send(data).await;
        self.update_snapshot_throttled();
        result
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref c) = self.capture {
            if !c.packet_sent("display", data) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::test_support::{loopback, NullTraffic, TestChannelPeers};
    use shakenfist_spice_protocol::constants::{clip_type, image_compression};
    use shakenfist_spice_protocol::messages::{
        BitmapPalette, BitmapPayload, Clip, DrawCopyBuilder, ImagePayload, SpiceImage,
        StreamDataHeader, StreamDataSized,
    };

    // -------------------------------------------------------------------------
    // Note: extract_dht_segments / inject_dht tests have moved to
    // shakenfist_spice_compression::video (video.rs) alongside the functions
    // themselves.
    // -------------------------------------------------------------------------

    // -------------------------------------------------------------------------
    // JpegDecoderRsDecoder tests (replaces old decode_mjpeg_frame tests;
    // the function moved to shakenfist-spice-compression::jpeg).
    // -------------------------------------------------------------------------

    #[test]
    fn jpeg_decoder_rs_valid_jpeg_returns_rgba() {
        use image::{DynamicImage, RgbImage};
        use shakenfist_spice_compression::{DecodedJpeg, JpegDecoder, JpegDecoderRsDecoder};
        use std::io::Cursor;

        // Create a tiny 2×2 solid-red image and encode it as JPEG.
        let rgb = RgbImage::from_fn(2, 2, |_x, _y| image::Rgb([255u8, 0, 0]));
        let img = DynamicImage::ImageRgb8(rgb);
        let mut jpeg_data = Vec::new();
        img.write_to(&mut Cursor::new(&mut jpeg_data), image::ImageFormat::Jpeg)
            .expect("failed to encode test JPEG");

        let decoder = JpegDecoderRsDecoder::new();
        let result = decoder.decode(&jpeg_data);
        assert!(result.is_some(), "expected Some for valid JPEG");

        let DecodedJpeg {
            rgba,
            width,
            height,
        } = result.unwrap();
        assert_eq!(width, 2, "width should be 2");
        assert_eq!(height, 2, "height should be 2");
        // RGBA: 4 bytes per pixel.
        assert_eq!(rgba.len(), 2 * 2 * 4, "expected 16 bytes of RGBA data");
    }

    #[test]
    fn jpeg_decoder_rs_empty_input_returns_none() {
        use shakenfist_spice_compression::{JpegDecoder, JpegDecoderRsDecoder};
        let decoder = JpegDecoderRsDecoder::new();
        let result = decoder.decode(&[]);
        assert!(result.is_none(), "expected None for empty input");
    }

    #[test]
    fn jpeg_decoder_rs_truncated_input_returns_none() {
        use shakenfist_spice_compression::{JpegDecoder, JpegDecoderRsDecoder};
        // A few bytes that look like a JPEG start but are truncated.
        let decoder = JpegDecoderRsDecoder::new();
        let result = decoder.decode(&[0xFF, 0xD8, 0xFF, 0xE0]);
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
                assert_eq!(base.bbox.top, 10);
                assert_eq!(base.bbox.left, 20);
                assert_eq!(base.bbox.bottom, 30);
                assert_eq!(base.bbox.right, 40);
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
                assert_eq!(base.bbox.top, 10);
                assert_eq!(base.bbox.left, 20);
                assert_eq!(base.bbox.bottom, 30);
                assert_eq!(base.bbox.right, 40);
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
                assert_eq!(base.bbox.top, 50);
                assert_eq!(base.bbox.left, 100);
                assert_eq!(base.bbox.bottom, 70);
                assert_eq!(base.bbox.right, 200);
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
                copy,
                fixed_len,
            } => {
                assert_eq!(base.surface_id, 0);
                assert_eq!(base.bbox.top, 10);
                assert_eq!(base.bbox.left, 20);
                assert_eq!(base.bbox.bottom, 30);
                assert_eq!(base.bbox.right, 40);
                assert_eq!(copy.src_bitmap, 0x100);
                assert_eq!(copy.src_area.top, 1);
                assert_eq!(copy.src_area.left, 2);
                assert_eq!(copy.src_area.bottom, 3);
                assert_eq!(copy.src_area.right, 4);
                assert_eq!(fixed_len, payload.len());
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
                assert_eq!(base.bbox.top, 10);
                assert_eq!(base.bbox.left, 20);
                assert_eq!(base.bbox.bottom, 30);
                assert_eq!(base.bbox.right, 40);
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
                assert_eq!(base.bbox.top, 10);
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

    // -------------------------------------------------------------------------
    // "Video not keeping up" instrumentation
    // -------------------------------------------------------------------------

    fn decode(success: bool, from_cache: bool, decode_duration_us: u32) -> DecodeResult {
        DecodeResult {
            image_type: "GlzRgb".to_string(),
            image_id: 0,
            width: 0,
            height: 0,
            from_cache,
            success,
            timestamp_secs: 0.0,
            decode_duration_us,
        }
    }

    #[test]
    fn recent_decode_duration_stats_empty_ring_returns_zeros() {
        let ring = VecDeque::new();
        assert_eq!(recent_decode_duration_stats(&ring), (0, 0, 0));
    }

    #[test]
    fn recent_decode_duration_stats_ignores_cache_hits_and_failures() {
        let mut ring = VecDeque::new();
        // Success, non-cache: counted.
        ring.push_back(decode(true, false, 100));
        ring.push_back(decode(true, false, 300));
        ring.push_back(decode(true, false, 200));
        // Cache hit: ignored.
        ring.push_back(decode(true, true, 9999));
        // Failure: ignored.
        ring.push_back(decode(false, false, 9999));
        let (min, max, mean) = recent_decode_duration_stats(&ring);
        assert_eq!(min, 100);
        assert_eq!(max, 300);
        assert_eq!(mean, 200);
    }

    #[test]
    fn recent_decode_duration_stats_all_excluded_returns_zeros() {
        let mut ring = VecDeque::new();
        ring.push_back(decode(true, true, 500));
        ring.push_back(decode(false, false, 1000));
        assert_eq!(recent_decode_duration_stats(&ring), (0, 0, 0));
    }

    #[test]
    fn push_ack_interval_caps_ring_keeping_most_recent() {
        let mut ring: VecDeque<f64> = VecDeque::new();
        // Push 40 distinct intervals.
        for i in 0..40 {
            push_ack_interval(&mut ring, i as f64);
        }
        // Cap is 32; we should have intervals 8..40 in order.
        assert_eq!(ring.len(), RECENT_ACK_INTERVALS_CAP);
        assert_eq!(ring.front().copied(), Some(8.0));
        assert_eq!(ring.back().copied(), Some(39.0));
        let observed: Vec<f64> = ring.iter().copied().collect();
        let expected: Vec<f64> = (8..40).map(|i| i as f64).collect();
        assert_eq!(observed, expected);
    }

    #[test]
    fn push_ack_interval_under_cap_retains_all() {
        let mut ring: VecDeque<f64> = VecDeque::new();
        for i in 0..5 {
            push_ack_interval(&mut ring, i as f64);
        }
        assert_eq!(ring.len(), 5);
        assert_eq!(ring.front().copied(), Some(0.0));
        assert_eq!(ring.back().copied(), Some(4.0));
    }

    // -------------------------------------------------------------------------
    // stream_report_should_send tests
    // -------------------------------------------------------------------------

    #[test]
    fn stream_report_predicate_fires_on_frame_window() {
        assert!(stream_report_should_send(5, 5, 100, 1000, 0));
        assert!(!stream_report_should_send(4, 5, 100, 1000, 0));
    }

    #[test]
    fn stream_report_predicate_fires_on_timeout() {
        assert!(stream_report_should_send(1, 5, 1000, 1000, 0));
        assert!(!stream_report_should_send(1, 5, 999, 1000, 0));
    }

    #[test]
    fn stream_report_predicate_fires_on_drop_sequence() {
        assert!(stream_report_should_send(
            1,
            5,
            100,
            1000,
            STREAM_REPORT_DROP_SEQ_LEN_LIMIT
        ));
        assert!(!stream_report_should_send(
            1,
            5,
            100,
            1000,
            STREAM_REPORT_DROP_SEQ_LEN_LIMIT - 1
        ));
    }

    #[test]
    fn stream_report_predicate_does_not_fire_idle() {
        assert!(!stream_report_should_send(0, 5, 0, 1000, 0));
    }

    // -------------------------------------------------------------------------
    // clamp_stream_report_params tests
    // -------------------------------------------------------------------------

    #[test]
    fn clamp_leaves_sane_activate_report_values_alone() {
        assert_eq!(clamp_stream_report_params(5, 1000), (5, 1000));
    }

    #[test]
    fn clamp_replaces_a_zero_window_with_the_server_default() {
        // Zero makes `num_frames >= max_window_size` true on the very
        // first frame, since report_num_frames is already >= 1 when
        // the predicate runs.
        let (window, _) = clamp_stream_report_params(0, 1000);
        assert_eq!(window, STREAM_REPORT_DEFAULT_WINDOW_SIZE);
        assert!(!stream_report_should_send(1, window, 0, 1000, 0));
    }

    #[test]
    fn clamp_keeps_timeout_inside_i32() {
        // Unclamped, anything from 0x8000_0000 up casts to a negative
        // i32 in the predicate and makes the elapsed check always true.
        for raw in [0x8000_0000u32, 0xFFFF_FFFF, u32::MAX / 2 + 1] {
            let (_, timeout_ms) = clamp_stream_report_params(5, raw);
            assert_eq!(timeout_ms, STREAM_REPORT_MAX_TIMEOUT_MS);
            assert!(timeout_ms as i32 > 0, "clamped timeout must stay positive");
            assert!(!stream_report_should_send(1, 5, 0, timeout_ms, 0));
        }
    }

    #[test]
    fn clamp_raises_a_too_small_timeout_to_the_floor() {
        assert_eq!(
            clamp_stream_report_params(5, 0),
            (5, STREAM_REPORT_MIN_TIMEOUT_MS)
        );
        assert_eq!(
            clamp_stream_report_params(5, 1),
            (5, STREAM_REPORT_MIN_TIMEOUT_MS)
        );
    }

    // -------------------------------------------------------------------------
    // STREAM_REPORT wire-format round-trip
    // -------------------------------------------------------------------------

    #[test]
    fn stream_report_payload_round_trip() {
        // Hand-built payload using known values; assert each
        // offset decodes back to the input. Layout per
        // spice.proto's SpiceMsgcDisplayStreamReport
        // (spice-common spice.proto:1004-1026).
        let stream_id: u32 = 0x1111_2222;
        let unique_id: u32 = 0xDEAD_BEEF;
        let start_mm: u32 = 100;
        let end_mm: u32 = 200;
        let num_frames: u32 = 5;
        let num_drops: u32 = 1;
        let last_frame_delay: i32 = -42;
        let audio_delay: u32 = u32::MAX;

        let mut buf = Vec::with_capacity(32);
        buf.extend_from_slice(&stream_id.to_le_bytes());
        buf.extend_from_slice(&unique_id.to_le_bytes());
        buf.extend_from_slice(&start_mm.to_le_bytes());
        buf.extend_from_slice(&end_mm.to_le_bytes());
        buf.extend_from_slice(&num_frames.to_le_bytes());
        buf.extend_from_slice(&num_drops.to_le_bytes());
        buf.extend_from_slice(&last_frame_delay.to_le_bytes());
        buf.extend_from_slice(&audio_delay.to_le_bytes());

        assert_eq!(buf.len(), 32);
        assert_eq!(read_u32_le(&buf, 0), stream_id);
        assert_eq!(read_u32_le(&buf, 4), unique_id);
        assert_eq!(read_u32_le(&buf, 8), start_mm);
        assert_eq!(read_u32_le(&buf, 12), end_mm);
        assert_eq!(read_u32_le(&buf, 16), num_frames);
        assert_eq!(read_u32_le(&buf, 20), num_drops);
        // i32 round-trip via u32 reinterpretation — the same 4
        // bytes; signedness is purely interpretation.
        assert_eq!(read_u32_le(&buf, 24) as i32, last_frame_delay);
        assert_eq!(read_u32_le(&buf, 28), audio_delay);
    }

    // -------------------------------------------------------------------------
    // STREAM_CREATE bounds and ordering
    //
    // These drive `handle_message` directly rather than testing the
    // constants, because the defects worth catching here are ordering
    // defects: which of the cap check, the decoder construction and the
    // teardown of a previous stream runs first decides whether a
    // hostile CREATE can evict a working stream or force a megabyte
    // allocation per message.
    // -------------------------------------------------------------------------

    /// Build a `DisplayChannel` wired to a loopback socket.
    async fn test_display_channel() -> (DisplayChannel, TestChannelPeers) {
        let (stream, events, peers) = loopback().await;
        let channel = DisplayChannel::new(
            0,
            stream,
            events,
            None,
            Arc::new(ByteCounter::new()),
            Arc::new(NullTraffic::new()),
            Arc::new(Mutex::new(DisplaySnapshot::default())),
            DisplayChannel::new_shared_glz_dictionary(1024 * 1024),
            LogConfig::default(),
            Arc::new(MmClock::new()),
            H264Offer::default(),
            1024 * 1024,
            image_compression::AUTO_GLZ,
        );
        (channel, peers)
    }

    // spice-server sends INVAL_ALL_PALETTES on every connect (#446). With
    // no palette cache it is a no-op, and must not be treated as an
    // unknown message, which raised a Gap warning.
    #[tokio::test]
    async fn palette_invalidations_are_handled_not_unknown() {
        let (mut channel, _peer) = test_display_channel().await;

        channel
            .handle_message(display_server::INVAL_ALL_PALETTES, &[])
            .await
            .expect("inval_all_palettes must not error");
        channel
            .handle_message(display_server::INVAL_PALETTE, &0u64.to_le_bytes())
            .await
            .expect("inval_palette must not error");

        assert_eq!(channel.opcodes.unknown_count(), 0);
        let keys = logging::warn_once_keys();
        assert!(!keys.contains(&"display:hexdump:107"));
        assert!(!keys.contains(&"display:hexdump:108"));
    }

    /// A minimal 64x64 `SpiceMsgDisplayStreamCreate` payload, with no clip.
    fn stream_create_payload(stream_id: u32, codec_type: u8) -> Vec<u8> {
        let mut v = Vec::new();
        StreamCreate {
            surface_id: 0,
            id: stream_id,
            flags: 0,
            codec_type,
            stamp: 0,
            stream_width: 64,
            stream_height: 64,
            src_width: 0,
            src_height: 0,
            dest: Rect {
                top: 0,
                left: 0,
                bottom: 64,
                right: 64,
            },
            clip: Clip::none(),
        }
        .write(&mut v);
        v
    }

    /// Codec 2 is VP8: a real SPICE codec type that `video::for_stream`
    /// does not build a decoder for, so it takes the UnsupportedCodec
    /// arm without being obvious junk.
    const CODEC_VP8_UNSUPPORTED: u8 = 2;

    #[tokio::test]
    async fn stream_create_past_the_cap_is_refused_and_counted() {
        let (mut channel, _peer) = test_display_channel().await;

        for stream_id in 0..MAX_CONCURRENT_STREAMS as u32 {
            channel
                .handle_message(
                    display_server::STREAM_CREATE,
                    &stream_create_payload(stream_id, SPICE_VIDEO_CODEC_TYPE_MJPEG),
                )
                .await
                .expect("stream_create must not error");
        }
        assert_eq!(channel.streams.len(), MAX_CONCURRENT_STREAMS);
        assert_eq!(
            channel.streams_rejected_total, 0,
            "the cap is not reached yet"
        );

        // One past the cap, on a fresh id.
        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(MAX_CONCURRENT_STREAMS as u32, SPICE_VIDEO_CODEC_TYPE_MJPEG),
            )
            .await
            .expect("a refused stream_create is not an error");

        assert_eq!(
            channel.streams.len(),
            MAX_CONCURRENT_STREAMS,
            "the map must not grow past the cap"
        );
        assert_eq!(channel.streams_rejected_total, 1);
        assert_eq!(
            channel.streams_created_total, MAX_CONCURRENT_STREAMS as u64,
            "a refusal must not count as a creation"
        );
    }

    #[tokio::test]
    async fn a_re_create_at_the_cap_is_not_refused() {
        // A re-CREATE replaces an entry rather than adding one, so it
        // cannot grow the map and must be exempt from the cap. Getting
        // this wrong is silent: the server's stream simply stops
        // updating once the cap is reached.
        let (mut channel, _peer) = test_display_channel().await;

        for stream_id in 0..MAX_CONCURRENT_STREAMS as u32 {
            channel
                .handle_message(
                    display_server::STREAM_CREATE,
                    &stream_create_payload(stream_id, SPICE_VIDEO_CODEC_TYPE_MJPEG),
                )
                .await
                .expect("stream_create must not error");
        }

        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(0, SPICE_VIDEO_CODEC_TYPE_MJPEG),
            )
            .await
            .expect("re-create must not error");

        assert_eq!(
            channel.streams_rejected_total, 0,
            "a re-create is not a refusal"
        );
        assert_eq!(channel.streams.len(), MAX_CONCURRENT_STREAMS);
        assert!(
            channel.streams.contains_key(&0),
            "the re-created id must be live"
        );
    }

    #[tokio::test]
    async fn re_create_on_a_live_id_retires_the_previous_stream() {
        let (mut channel, _peer) = test_display_channel().await;

        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(7, SPICE_VIDEO_CODEC_TYPE_MJPEG),
            )
            .await
            .expect("stream_create must not error");
        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(7, SPICE_VIDEO_CODEC_TYPE_MJPEG),
            )
            .await
            .expect("re-create must not error");

        assert_eq!(
            channel.streams.len(),
            1,
            "the id must hold exactly one entry"
        );
        assert_eq!(
            channel.streams_created_total, 2,
            "both creates count; the replacement is a real stream"
        );
        assert_eq!(
            channel.streams_destroyed_total, 1,
            "destroyed_total must stay paired with created_total"
        );
        assert_eq!(
            channel.recently_destroyed_streams.len(),
            1,
            "the outgoing stream's counters must reach the recently-destroyed ring \
             rather than being dropped with its StreamState"
        );
    }

    #[tokio::test]
    async fn re_create_with_an_unsupported_codec_leaves_the_live_stream_alone() {
        // The ordering defect this pins: if the teardown runs before
        // `video::for_stream`, a re-CREATE naming a codec this build
        // cannot decode retires the working stream and then declines
        // to install a replacement, blanking the promoted region for
        // the rest of the session. Ignoring the message is strictly
        // better.
        let (mut channel, _peer) = test_display_channel().await;

        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(3, SPICE_VIDEO_CODEC_TYPE_MJPEG),
            )
            .await
            .expect("stream_create must not error");

        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(3, CODEC_VP8_UNSUPPORTED),
            )
            .await
            .expect("an unsupported codec is not an error");

        let surviving = channel
            .streams
            .get(&3)
            .expect("the working stream must survive a re-create it cannot honour");
        assert_eq!(
            surviving.codec_type, SPICE_VIDEO_CODEC_TYPE_MJPEG,
            "the surviving stream must be the original, not a half-built replacement"
        );
        assert_eq!(
            channel.streams_destroyed_total, 0,
            "nothing was destroyed, so nothing may be counted as destroyed"
        );
        assert!(
            channel.recently_destroyed_streams.is_empty(),
            "an ignored re-create must not retire anything"
        );
    }

    #[tokio::test]
    async fn an_unsupported_codec_on_a_fresh_id_creates_nothing() {
        let (mut channel, _peer) = test_display_channel().await;

        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(1, CODEC_VP8_UNSUPPORTED),
            )
            .await
            .expect("an unsupported codec is not an error");

        assert!(channel.streams.is_empty());
        assert_eq!(channel.streams_created_total, 0);
        assert_eq!(
            channel.streams_rejected_total, 0,
            "an unsupported codec is not a cap refusal; the counters mean different things"
        );
    }

    // -------------------------------------------------------------------------
    // Failure policy for malformed messages. Some end the channel (Err),
    // the rest are skipped; these pin which is which, so that a change
    // in either direction is deliberate.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn short_messages_that_end_the_channel() {
        let (mut channel, _peer) = test_display_channel().await;
        for (msg_type, len) in [
            (display_server::SURFACE_CREATE, SurfaceCreate::SIZE - 1),
            (display_server::SET_ACK, SetAck::SIZE - 1),
            (display_server::PING, Ping::SIZE - 1),
            (display_server::NOTIFY, NotifyMessage::MIN_SIZE - 1),
        ] {
            assert!(
                channel
                    .handle_message(msg_type, &vec![0; len])
                    .await
                    .is_err(),
                "a short message {} must end the channel",
                msg_type
            );
        }
    }

    #[tokio::test]
    async fn short_surface_destroy_and_stream_clip_are_ignored() {
        let (mut channel, mut peers) = test_display_channel().await;
        channel
            .handle_message(display_server::SURFACE_DESTROY, &[1, 0, 0])
            .await
            .expect("a short SURFACE_DESTROY is ignored");
        channel
            .handle_message(display_server::STREAM_CLIP, &[1, 0, 0])
            .await
            .expect("a short STREAM_CLIP is ignored");
        assert!(
            peers.events.try_recv().is_err(),
            "an ignored message emits nothing"
        );
    }

    #[tokio::test]
    async fn short_stream_messages_are_ignored() {
        let (mut channel, _peer) = test_display_channel().await;
        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(7, SPICE_VIDEO_CODEC_TYPE_MJPEG),
            )
            .await
            .expect("stream_create must not error");

        // A frame whose data_size runs past the body.
        let mut frame = Vec::new();
        for v in [7u32, 0, 10] {
            frame.extend_from_slice(&v.to_le_bytes());
        }
        frame.extend_from_slice(&[0xff, 0xd8]);
        channel
            .handle_message(display_server::STREAM_DATA, &frame)
            .await
            .expect("a short STREAM_DATA is ignored");
        channel
            .handle_message(display_server::STREAM_DATA_SIZED, &frame)
            .await
            .expect("a short STREAM_DATA_SIZED is ignored");
        assert_eq!(
            channel.streams[&7].frames_received, 0,
            "an ignored frame is not counted"
        );
        assert!(logging::warn_once_keys().contains(&"display:decode_failure:stream_data:malformed"));

        channel
            .handle_message(display_server::STREAM_DESTROY, &[7, 0, 0])
            .await
            .expect("a short STREAM_DESTROY is ignored");
        assert!(channel.streams.contains_key(&7));
        assert_eq!(channel.streams_destroyed_total, 0);
    }

    // A frame wider than its dest used to be painted with its width
    // narrowed but its rows left at the frame's stride, so each row
    // started further into the frame than the last and the frame
    // painted sheared. This drives a real frame through the channel and
    // the surface mirror every mode paints with.
    #[tokio::test]
    async fn stream_frame_larger_than_its_dest_paints_its_top_left_corner() {
        use crate::surface_mirror::SurfaceMirror;
        use image::codecs::jpeg::JpegEncoder;

        const SURFACE: u32 = 32;
        const FRAME_W: u32 = 24;
        const FRAME_H: u32 = 16;
        let (left, top, dest_w, dest_h) = (4u32, 2u32, 16u32, 8u32);

        // Each pixel differs from its neighbours, so a row read from
        // the wrong offset shows.
        let rgb: Vec<u8> = (0..FRAME_H)
            .flat_map(|y| (0..FRAME_W).flat_map(move |x| [(x * 10) as u8, (y * 15) as u8, 128]))
            .collect();
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(&rgb, FRAME_W, FRAME_H, image::ExtendedColorType::Rgb8)
            .expect("encode the test frame");

        let (mut channel, mut peers) = test_display_channel().await;
        // JPEG is lossy, so the expected pixels are a separate decode.
        let decoded = video::for_stream(SPICE_VIDEO_CODEC_TYPE_MJPEG, channel.jpeg_decoder.clone())
            .expect("an MJPEG decoder")
            .decode(&jpeg)
            .expect("the test frame decodes")
            .expect("MJPEG yields a frame per packet");
        assert_eq!((decoded.width, decoded.height), (FRAME_W, FRAME_H));

        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(7, SPICE_VIDEO_CODEC_TYPE_MJPEG),
            )
            .await
            .expect("stream_create must not error");
        let mut frame = Vec::new();
        StreamDataSized {
            base: StreamDataHeader {
                id: 7,
                multi_media_time: 0,
            },
            width: FRAME_W,
            height: FRAME_H,
            dest: Rect {
                top: top as i32,
                left: left as i32,
                bottom: (top + dest_h) as i32,
                right: (left + dest_w) as i32,
            },
            data: jpeg,
        }
        .write(&mut frame);
        channel
            .handle_message(display_server::STREAM_DATA_SIZED, &frame)
            .await
            .expect("stream_data_sized must not error");

        let mut mirror = SurfaceMirror::new();
        mirror.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: SURFACE,
            height: SURFACE,
        });
        let mut painted = 0;
        while let Ok(event) = peers.events.try_recv() {
            if matches!(event, ChannelEvent::ImageReady { .. }) {
                mirror.apply_event(&event);
                painted += 1;
            }
        }
        assert_eq!(painted, 1, "the frame is painted once");

        let surface = mirror.primary_surface().expect("the surface exists");
        let pixel = |pixels: &[u8], stride: u32, x: u32, y: u32| {
            let i = ((y * stride + x) * 4) as usize;
            pixels[i..i + 4].to_vec()
        };
        for y in 0..SURFACE {
            for x in 0..SURFACE {
                let in_dest =
                    (left..left + dest_w).contains(&x) && (top..top + dest_h).contains(&y);
                let expected = if in_dest {
                    pixel(&decoded.rgba, FRAME_W, x - left, y - top)
                } else {
                    vec![0, 0, 0, 255]
                };
                assert_eq!(
                    pixel(surface.pixels(), SURFACE, x, y),
                    expected,
                    "surface pixel ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn a_frame_within_its_dest_is_not_cropped() {
        let rgba: Vec<u8> = (0..2 * 3 * 4).map(|b| b as u8).collect();
        let frame = video::DecodedFrame {
            rgba: rgba.clone(),
            width: 2,
            height: 3,
        };
        let cropped = crop_frame_to_dest(frame, 4, 3).expect("a whole frame crops");
        assert_eq!((cropped.width, cropped.height), (2, 3));
        assert_eq!(cropped.rgba, rgba);
    }

    #[test]
    fn a_frame_short_of_its_dimensions_is_a_decode_failure() {
        let frame = video::DecodedFrame {
            rgba: vec![0; 2 * 2 * 4 - 1],
            width: 2,
            height: 2,
        };
        assert!(matches!(
            crop_frame_to_dest(frame, 1, 1),
            Err(VideoDecoderError::Decode(_))
        ));
    }

    /// `count` 64x64 H.264 frames, each decodable on its own after the
    /// first (an IDR), from the encoder a SPICE server would use.
    #[cfg(all(feature = "h264-decode", feature = "encoder"))]
    fn h264_frames(count: usize) -> Vec<Vec<u8>> {
        use shakenfist_spice_compression::{Bgrx, H264Config, H264Encoder};
        let mut config = H264Config::new(64, 64, 30);
        config.slices = 1;
        config.skip_frames = false;
        let mut encoder = H264Encoder::new(config).expect("an encoder");
        (0..count)
            .map(|i| {
                let pixels: Vec<u8> = (0..64 * 64 * 4).map(|b| (b * 7 + i * 13) as u8).collect();
                let image = Bgrx::new(&pixels, 64, 64, 64 * 4).expect("a valid image");
                encoder
                    .encode(&image)
                    .expect("encode")
                    .expect("frame skipping is off")
                    .data
            })
            .collect()
    }

    /// A `STREAM_DATA` body carrying `data` for stream `id`.
    #[cfg(feature = "h264-decode")]
    fn stream_data_payload(id: u32, data: &[u8]) -> Vec<u8> {
        use shakenfist_spice_protocol::messages::StreamData;
        let mut v = Vec::new();
        StreamData {
            base: StreamDataHeader {
                id,
                multi_media_time: 0,
            },
            data: data.to_vec(),
        }
        .write(&mut v);
        v
    }

    /// Send each of `frames` to stream `id`.
    #[cfg(feature = "h264-decode")]
    async fn send_stream_frames(channel: &mut DisplayChannel, id: u32, frames: &[Vec<u8>]) {
        for frame in frames {
            channel
                .handle_message(display_server::STREAM_DATA, &stream_data_payload(id, frame))
                .await
                .expect("stream_data must not error");
        }
    }

    // A streaming agent may pick H.264 although ryll asks for MJPEG
    // first, and spice-server's H.264 does not decode in ryll (#398).
    // Nothing shows on screen to say why, so ryll warns, once per
    // stream, naming --no-h264. Needs the H.264 decoder: without it the
    // stream is refused at STREAM_CREATE and there is nothing to warn
    // about.
    #[cfg(feature = "h264-decode")]
    #[tokio::test]
    async fn an_h264_stream_that_never_decodes_is_warned_about_once() {
        let (mut channel, _peers) = test_display_channel().await;
        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(7, SPICE_VIDEO_CODEC_TYPE_H264),
            )
            .await
            .expect("stream_create must not error");
        let junk = vec![vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0x55, 0xAA]; 9];
        send_stream_frames(&mut channel, 7, &junk).await;
        assert!(
            !channel.streams[&7].warned_never_decodes,
            "warned before the threshold"
        );
        send_stream_frames(&mut channel, 7, &junk[..1]).await;
        let stream = &channel.streams[&7];
        assert_eq!(stream.frames_decoded_ok, 0);
        assert_eq!(stream.frames_received, H264_NO_PICTURE_WARN_FRAMES);
        assert!(stream.warned_never_decodes, "not warned at the threshold");
    }

    #[cfg(all(feature = "h264-decode", feature = "encoder"))]
    #[tokio::test]
    async fn streams_that_decode_or_are_mjpeg_are_not_warned_about() {
        let (mut channel, _peers) = test_display_channel().await;
        for (id, codec) in [
            (7, SPICE_VIDEO_CODEC_TYPE_H264),
            (8, SPICE_VIDEO_CODEC_TYPE_MJPEG),
        ] {
            channel
                .handle_message(
                    display_server::STREAM_CREATE,
                    &stream_create_payload(id, codec),
                )
                .await
                .expect("stream_create must not error");
        }
        send_stream_frames(&mut channel, 7, &h264_frames(12)).await;
        send_stream_frames(&mut channel, 8, &vec![vec![0xFF, 0xD8, 0x00]; 12]).await;
        assert!(channel.streams[&7].frames_decoded_ok > 0);
        assert!(!channel.streams[&7].warned_never_decodes);
        assert_eq!(channel.streams[&8].frames_decoded_ok, 0);
        assert!(!channel.streams[&8].warned_never_decodes);
    }

    // The control socket's `stats` reads what the channel publishes. Fill
    // the snapshot through the channel itself, not by hand, so that a
    // change to the opcode key or the publish path shows here. Drives a
    // real H.264 stream, so it needs both the encoder and the decoder.
    #[cfg(all(unix, feature = "h264-decode", feature = "encoder"))]
    #[tokio::test]
    async fn stats_reports_what_the_channel_publishes() {
        use crate::control::protocol::{CodecDecodeStats, DisplayStats};

        let (mut channel, _peers) = test_display_channel().await;
        channel
            .handle_message(display_server::MARK, &[])
            .await
            .expect("mark must not error");
        channel
            .handle_message(
                display_server::STREAM_CREATE,
                &stream_create_payload(7, SPICE_VIDEO_CODEC_TYPE_H264),
            )
            .await
            .expect("stream_create must not error");
        send_stream_frames(&mut channel, 7, &h264_frames(3)).await;
        channel.update_snapshot();

        let stats = DisplayStats::from(&*channel.snapshot.lock().expect("snapshot"));
        assert_eq!(stats.channel_id, 0);
        assert_eq!(stats.marks_received, 1);
        assert_eq!(stats.streams_active, 1);
        assert_eq!(stats.streams_created_total, 1);
        assert_eq!(
            stats.decodes.get("h264"),
            Some(&CodecDecodeStats {
                decodes_total: 3,
                decode_failures_total: 0,
            })
        );
        let stream = &stats.streams[0];
        assert_eq!(
            (stream.stream_id, stream.codec.as_str(), stream.codec_type),
            (7, "h264", SPICE_VIDEO_CODEC_TYPE_H264)
        );
        assert_eq!((stream.width, stream.height), (64, 64));
        assert_eq!(
            (
                stream.frames_received,
                stream.frames_decoded_ok,
                stream.frames_decode_failed
            ),
            (3, 3, 0)
        );
        assert_eq!(stream.destroyed_at_secs, None);
    }

    #[tokio::test]
    async fn stream_create_without_its_clip_is_ignored() {
        let (mut channel, _peer) = test_display_channel().await;
        let mut create = stream_create_payload(7, SPICE_VIDEO_CODEC_TYPE_MJPEG);
        assert_eq!(create.len(), StreamCreate::MIN_SIZE);
        create.pop();
        channel
            .handle_message(display_server::STREAM_CREATE, &create)
            .await
            .expect("a STREAM_CREATE without its clip is ignored");

        // A RECTS clip claiming one rectangle, and no rectangle.
        create.push(clip_type::RECTS);
        create.extend_from_slice(&1u32.to_le_bytes());
        channel
            .handle_message(display_server::STREAM_CREATE, &create)
            .await
            .expect("a STREAM_CREATE with its clip rects cut short is ignored");
        assert!(channel.streams.is_empty());
        assert_eq!(channel.streams_created_total, 0);
        assert!(
            logging::warn_once_keys().contains(&"display:decode_failure:stream_create:malformed")
        );
    }

    #[tokio::test]
    async fn draw_copy_shorter_than_its_base_warns_and_is_skipped() {
        let (mut channel, _peer) = test_display_channel().await;
        channel
            .handle_message(display_server::DRAW_COPY, &[0; DrawBase::MIN_SIZE - 1])
            .await
            .expect("a DRAW_COPY shorter than its DrawBase is skipped");
        assert!(
            logging::warn_once_keys().contains(&"display:decode_failure:draw_copy:short_payload")
        );
    }

    #[tokio::test]
    async fn draw_copy_with_its_clip_rects_cut_short_ends_the_channel() {
        let (mut channel, _peer) = test_display_channel().await;
        // surface_id, bbox, a RECTS clip claiming one rectangle, and no
        // rectangle: long enough for DrawBase::MIN_SIZE, short of the clip.
        let mut payload = vec![0; 4 + Rect::SIZE];
        payload.push(clip_type::RECTS);
        payload.extend_from_slice(&1u32.to_le_bytes());
        assert!(payload.len() >= DrawBase::MIN_SIZE);
        assert!(channel
            .handle_message(display_server::DRAW_COPY, &payload)
            .await
            .is_err());
    }

    // -------------------------------------------------------------------------
    // ZLIB_GLZ_RGB inflate bounds (#176)
    // -------------------------------------------------------------------------

    fn deflate(data: &[u8]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn inflate_glz_stream_refuses_bomb_past_limit() {
        // A 1x1 image has a limit of 4 + 1 + 64 bytes. 1 MiB of zeros
        // deflates to about a kilobyte and must be refused, even when
        // the header claims the most the limit allows.
        let zlib = deflate(&vec![0u8; 1024 * 1024]);
        assert!(zlib.len() < 4096);
        let limit = glz_stream_limit(1, 1).unwrap();
        assert!(matches!(
            inflate_glz_stream(&zlib, limit, 1, 1),
            Err(InflateGlzError::ExceedsDeclared)
        ));
    }

    #[test]
    fn inflate_glz_stream_accepts_limit_refuses_one_over() {
        // The boundary: a stream exactly at the limit is accepted, one
        // byte over it is refused.
        let limit = glz_stream_limit(1, 1).unwrap();
        let ok = deflate(&vec![7u8; limit]);
        assert_eq!(inflate_glz_stream(&ok, limit, 1, 1).unwrap().len(), limit);
        let over = deflate(&vec![7u8; limit + 1]);
        assert!(matches!(
            inflate_glz_stream(&over, limit, 1, 1),
            Err(InflateGlzError::ExceedsDeclared)
        ));
    }

    #[test]
    fn inflate_glz_stream_stops_at_the_declared_size_not_the_limit() {
        // Two full stored blocks, then a block of the reserved type.
        // Inflated whole, the stream is a zlib error; stopped one byte
        // past a declared size of 10, it never reaches the bad block and
        // is refused as exceeding the declared size. The stored data is
        // well over 32 KiB because miniz_oxide, flate2's default backend,
        // inflates a window ahead of what the reader asks for.
        let mut zlib = vec![0x78, 0x01];
        for _ in 0..2 {
            zlib.extend_from_slice(&[0x00, 0xFF, 0xFF, 0x00, 0x00]);
            zlib.extend_from_slice(&[0u8; 0xFFFF]);
        }
        zlib.push(0x07);
        let stored = 2 * 0xFFFF;
        assert!(glz_stream_limit(256, 256).unwrap() > stored);
        assert!(matches!(
            inflate_glz_stream(&zlib, 10, 256, 256),
            Err(InflateGlzError::ExceedsDeclared)
        ));
        assert!(matches!(
            inflate_glz_stream(&zlib, stored + 1, 256, 256),
            Err(InflateGlzError::Zlib(_))
        ));
    }

    #[test]
    fn inflate_glz_stream_refuses_oversized_declared_size() {
        let zlib = deflate(&[1, 2, 3]);
        assert!(matches!(
            inflate_glz_stream(&zlib, usize::MAX, 2, 2),
            Err(InflateGlzError::DeclaredTooLarge)
        ));
    }

    #[test]
    fn inflate_glz_stream_refuses_declared_size_mismatch() {
        let zlib = deflate(&[1, 2, 3]);
        assert!(matches!(
            inflate_glz_stream(&zlib, 4, 2, 2),
            Err(InflateGlzError::SizeMismatch { inflated: 3 })
        ));
        assert_eq!(inflate_glz_stream(&zlib, 3, 2, 2).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn inflate_glz_stream_refuses_absurd_dimensions() {
        let zlib = deflate(&[1, 2, 3]);
        assert!(matches!(
            inflate_glz_stream(&zlib, 3, 65535, 65535),
            Err(InflateGlzError::DimensionsRefused)
        ));
    }

    // -------------------------------------------------------------------------
    // DRAW_COPY image decode and placement
    //
    // These drive `handle_message` with DRAW_COPY payloads from the
    // protocol crate's builder, so the whole of `decode_image_and_emit`
    // runs: the decode arm, the cache, the source-rect crop and the
    // clip-rect split.
    // -------------------------------------------------------------------------

    /// SpiceRect as (top, left, bottom, right), the wire order.
    type WireRect = (u32, u32, u32, u32);

    fn rect((top, left, bottom, right): WireRect) -> Rect {
        Rect {
            top: top as i32,
            left: left as i32,
            bottom: bottom as i32,
            right: right as i32,
        }
    }

    /// A DRAW_COPY payload drawing `image` at (0, 0) from `src_rect`.
    ///
    /// An empty `clip_rects` sends clip type NONE, otherwise RECTS.
    fn draw_copy_payload(
        src_rect: WireRect,
        clip_rects: &[WireRect],
        image: &SpiceImage,
    ) -> Vec<u8> {
        let base = DrawBase {
            surface_id: 0,
            bbox: rect((0, 0, 1, 1)),
            clip: if clip_rects.is_empty() {
                Clip::none()
            } else {
                Clip::rects(clip_rects.iter().copied().map(rect).collect())
            },
        };
        DrawCopyBuilder::new(&base, image, rect(src_rect)).build()
    }

    fn image_descriptor(
        id: u64,
        image_type: ImageType,
        flags: u8,
        width: u32,
        height: u32,
    ) -> ImageDescriptor {
        ImageDescriptor {
            image_id: id,
            image_type: image_type as u8,
            flags,
            width,
            height,
        }
    }

    /// A top-down 32-bit BGRX Pixmap SpiceImage.
    fn pixmap_image(
        id: u64,
        flags: u8,
        width: u32,
        height: u32,
        stride: u32,
        pixels: &[u8],
    ) -> SpiceImage {
        SpiceImage {
            descriptor: image_descriptor(id, ImageType::Pixmap, flags, width, height),
            payload: ImagePayload::Bitmap(BitmapPayload {
                header: BitmapHeader {
                    format: bitmap_fmt::BIT32,
                    flags: bitmap_flags::TOP_DOWN,
                    x: width,
                    y: height,
                    stride,
                    palette: BitmapPalette::None,
                },
                data: pixels.to_vec(),
            }),
        }
    }

    /// A cache hit on `id`, claiming `width` x `height`.
    ///
    /// Only the descriptor: FromCache carries no data after it, so with
    /// no mask following, the image ends the payload as it does on the
    /// wire.
    fn from_cache_image(image_type: ImageType, id: u64, width: u32, height: u32) -> SpiceImage {
        SpiceImage {
            descriptor: image_descriptor(id, image_type, 0, width, height),
            payload: ImagePayload::FromCache,
        }
    }

    /// (left, top, width, height, pixels) of each ImageReady emitted so
    /// far.
    fn drain_image_events(peers: &mut TestChannelPeers) -> Vec<(u32, u32, u32, u32, Vec<u8>)> {
        let mut out = Vec::new();
        while let Ok(event) = peers.events.try_recv() {
            if let ChannelEvent::ImageReady {
                left,
                top,
                width,
                height,
                pixels,
                ..
            } = event
            {
                out.push((left, top, width, height, pixels));
            }
        }
        out
    }

    #[tokio::test]
    async fn pixmap_with_padded_stride_skips_the_padding() {
        // 2x2, stride 12: each row is 8 bytes of BGRX then 4 of padding.
        let (mut channel, mut peers) = test_display_channel().await;
        let pixels = [
            1, 2, 3, 0, 4, 5, 6, 0, 0xEE, 0xEE, 0xEE, 0xEE, //
            7, 8, 9, 0, 10, 11, 12, 0, 0xEE, 0xEE, 0xEE, 0xEE,
        ];
        let image = pixmap_image(1, 0, 2, 2, 12, &pixels);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (_, _, width, height, rgba) = &events[0];
        assert_eq!((*width, *height), (2, 2));
        assert_eq!(
            rgba,
            &vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }

    #[tokio::test]
    async fn pixmap_with_stride_narrower_than_a_row_is_refused() {
        // #173: 16384 is the per-side cap, so this passes `rgba_len`.
        // With stride 4 and four bytes of data it also passes the
        // stride * height check, and the row copy then sliced 65536
        // bytes out of four and panicked.
        let (mut channel, mut peers) = test_display_channel().await;
        let image = pixmap_image(1, 0, 16384, 1, 4, &[1, 2, 3, 4]);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 1, 16384), &[], &image),
            )
            .await
            .expect("a refused pixmap is not an error");

        assert!(drain_image_events(&mut peers).is_empty());
    }

    /// Draw a 2x2 Pixmap with CACHE_ME, so it is cached as `id`. Its
    /// BGRX bytes are 1 to 16 in order.
    async fn cache_2x2_pixmap(channel: &mut DisplayChannel, peers: &mut TestChannelPeers, id: u64) {
        let pixels: Vec<u8> = (1..=16).collect();
        let image = pixmap_image(id, IMAGE_FLAGS_CACHE_ME, 2, 2, 8, &pixels);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");
        assert_eq!(
            drain_image_events(peers).len(),
            1,
            "the pixmap itself is drawn"
        );
    }

    #[tokio::test]
    async fn from_cache_larger_than_the_cached_image_is_refused() {
        // #174: cache a 2x2 pixmap as id 42 (16 bytes), then draw it
        // from the cache claiming a larger size. Before the fix the
        // crop sliced out of the 16 bytes and panicked, and the clip
        // path indexed the same way, so each size is also tried with a
        // full-image source rect, which skips the crop, clipped to a
        // strip. 3x3 is inside the pixel cap, so only the cache-size
        // check refuses it; 10000x10000 is over the cap and is
        // refused earlier.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        for size in [3u32, 10000] {
            let image = from_cache_image(ImageType::FromCache, 42, size, size);
            let strip: WireRect = (0, 0, 1, size);
            let whole: WireRect = (0, 0, size, size);
            for (src_rect, clip_rects) in [(strip, &[][..]), (whole, &[strip][..])] {
                channel
                    .handle_message(
                        display_server::DRAW_COPY,
                        &draw_copy_payload(src_rect, clip_rects, &image),
                    )
                    .await
                    .expect("a refused cache hit is not an error");
                assert!(
                    drain_image_events(&mut peers).is_empty(),
                    "{size}x{size} over a cached 2x2 must be refused"
                );
            }
        }
    }

    /// A 2x2 GLZ stream: the 33 byte header, then one control byte for
    /// four literal BGR pixels. Decodes to the RGBA returned alongside.
    fn glz_2x2_stream() -> (Vec<u8>, Vec<u8>) {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"  ZL");
        stream.extend_from_slice(&[0, 1, 0, 0]); // version 1.0
        stream.push(0x10); // top down
        stream.extend_from_slice(&2u32.to_be_bytes()); // width
        stream.extend_from_slice(&2u32.to_be_bytes()); // height
        stream.extend_from_slice(&8u32.to_be_bytes()); // stride
        stream.extend_from_slice(&1u64.to_be_bytes()); // image id
        stream.extend_from_slice(&0u32.to_be_bytes()); // window head dist
        stream.push(3); // four literal pixels
        let mut rgba = Vec::new();
        for i in 0..4u8 {
            let (b, g, r) = (i * 3 + 1, i * 3 + 2, i * 3 + 3);
            stream.extend_from_slice(&[b, g, r]);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
        (stream, rgba)
    }

    /// A 2x2 ZLIB_GLZ_RGB SpiceImage whose wire header claims
    /// `declared_glz_size` bytes of GLZ stream.
    fn zlib_glz_image(declared_glz_size: u32, glz_stream: &[u8]) -> SpiceImage {
        let zlib = deflate(glz_stream);
        let mut data = Vec::new();
        data.extend_from_slice(&declared_glz_size.to_le_bytes());
        data.extend_from_slice(&(zlib.len() as u32).to_le_bytes());
        data.extend_from_slice(&zlib);
        SpiceImage {
            descriptor: image_descriptor(7, ImageType::ZlibGlzRgb, 0, 2, 2),
            payload: ImagePayload::Other(data),
        }
    }

    #[tokio::test]
    async fn zlib_glz_image_is_inflated_decoded_and_drawn() {
        let (mut channel, mut peers) = test_display_channel().await;
        let (stream, rgba) = glz_2x2_stream();
        let image = zlib_glz_image(stream.len() as u32, &stream);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (_, _, width, height, pixels) = &events[0];
        assert_eq!((*width, *height), (2, 2));
        assert_eq!(pixels, &rgba);
    }

    #[tokio::test]
    async fn zlib_glz_image_with_a_wrong_declared_size_is_refused() {
        let (mut channel, mut peers) = test_display_channel().await;
        let (stream, _) = glz_2x2_stream();
        let image = zlib_glz_image(stream.len() as u32 + 1, &stream);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("a refused image is not an error");

        assert!(drain_image_events(&mut peers).is_empty());
    }

    #[tokio::test]
    async fn draw_copy_with_a_million_duplicate_clip_rects_copies_once() {
        // #476: one DRAW_COPY of a cache hit with a million copies of a
        // full-size clip rectangle. Applied per listed rectangle that
        // copied the image a million times; merged, it is one copy.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let image = from_cache_image(ImageType::FromCache, 42, 2, 2);
        let full: WireRect = (0, 0, 2, 2);
        let clip_rects = vec![full; 1_000_000];
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload(full, &clip_rects, &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (left, top, width, height, _) = &events[0];
        assert_eq!((*left, *top, *width, *height), (0, 0, 2, 2));
    }

    #[tokio::test]
    async fn draw_copy_with_overlapping_clip_rects_copies_each_pixel_once() {
        // An L of two overlapping rectangles: the top row and the left
        // column of a 2x2 image. Three pixels, each copied once.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let image = from_cache_image(ImageType::FromCache, 42, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[(0, 0, 1, 2), (0, 0, 2, 1)], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        let rects: Vec<_> = events
            .iter()
            .map(|(l, t, w, h, _)| (*l, *t, *w, *h))
            .collect();
        assert_eq!(rects, vec![(0, 0, 2, 1), (0, 1, 1, 1)]);
    }

    #[tokio::test]
    async fn draw_copy_with_clip_rects_missing_the_destination_draws_nothing() {
        // A RECTS clip with no rectangle over the destination, and one
        // with no rectangles at all, both clip away the whole draw, as
        // spice-common's region intersection does.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let image = from_cache_image(ImageType::FromCache, 42, 2, 2);
        let mut empty_rects = draw_copy_payload((0, 0, 2, 2), &[(0, 0, 1, 1)], &image);
        // Rewrite the one-rectangle clip as a RECTS clip of none: drop
        // the rectangle and zero the count. The clip starts after the
        // surface id and the box.
        let clip_at = 4 + Rect::SIZE;
        assert_eq!(empty_rects[clip_at], clip_type::RECTS);
        empty_rects.drain(clip_at + 5..clip_at + 5 + Rect::SIZE);
        empty_rects[clip_at + 1..clip_at + 5].copy_from_slice(&0u32.to_le_bytes());

        for payload in [
            draw_copy_payload((0, 0, 2, 2), &[(10, 10, 20, 20)], &image),
            empty_rects,
        ] {
            channel
                .handle_message(display_server::DRAW_COPY, &payload)
                .await
                .expect("draw_copy must not error");
            assert!(drain_image_events(&mut peers).is_empty());
        }
    }

    /// A DRAW_BLACKNESS payload over `bbox` with `clip` and a null mask.
    fn blackness_payload(bbox: WireRect, clip: Clip) -> Vec<u8> {
        let mut v = Vec::new();
        DrawBase {
            surface_id: 0,
            bbox: rect(bbox),
            clip,
        }
        .write(&mut v);
        v.extend_from_slice(&[0u8; SpiceBlackness::SIZE]);
        v
    }

    /// The clip of each FillRect emitted so far.
    fn drain_fill_clips(peers: &mut TestChannelPeers) -> Vec<Vec<(u32, u32, u32, u32)>> {
        let mut out = Vec::new();
        while let Ok(event) = peers.events.try_recv() {
            if let ChannelEvent::FillRect { clip, .. } = event {
                out.push(clip);
            }
        }
        out
    }

    #[tokio::test]
    async fn fill_clip_is_merged_and_clamped_to_its_box() {
        let (mut channel, mut peers) = test_display_channel().await;
        let bbox: WireRect = (0, 0, 100, 100);
        let clip = Clip::rects(vec![
            rect((0, 0, 50, 50)),
            rect((0, 0, 50, 50)),
            rect((0, 50, 50, 500)),
        ]);
        channel
            .handle_message(
                display_server::DRAW_BLACKNESS,
                &blackness_payload(bbox, clip),
            )
            .await
            .expect("draw_blackness must not error");
        assert_eq!(drain_fill_clips(&mut peers), vec![vec![(0, 0, 100, 50)]]);
    }

    #[tokio::test]
    async fn fill_with_too_complex_a_clip_is_skipped() {
        let (mut channel, mut peers) = test_display_channel().await;
        let bbox: WireRect = (0, 0, 4096, 4096);
        let rects: Vec<Rect> = (0..=MAX_CLIP_RECTS as u32)
            .map(|i| rect((i, i, i + 1, i + 1)))
            .collect();
        channel
            .handle_message(
                display_server::DRAW_BLACKNESS,
                &blackness_payload(bbox, Clip::rects(rects)),
            )
            .await
            .expect("a refused clip is not an error");
        assert!(drain_fill_clips(&mut peers).is_empty());
    }

    #[tokio::test]
    async fn from_cache_crops_a_source_rect_past_the_image_to_the_image() {
        // The crop clamps the source rect to the image, so a rect
        // running off the right and bottom yields the part that
        // exists rather than reading past the cached pixels.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let image = from_cache_image(ImageType::FromCache, 42, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((1, 1, 100, 100), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (_, _, width, height, rgba) = &events[0];
        assert_eq!((*width, *height), (1, 1));
        // Bottom-right source pixel: BGRX (13, 14, 15, 16) -> RGBA.
        assert_eq!(rgba, &vec![15, 14, 13, 255]);
    }

    #[tokio::test]
    async fn from_cache_ending_the_payload_is_drawn() {
        // #442: spice-server marshals a cache hit as the bare
        // descriptor, after the draw's fixed fields and before the
        // (here absent) mask, so it ends the payload. A guard that
        // wanted data after the descriptor dropped every such draw.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let image = from_cache_image(ImageType::FromCache, 42, 2, 2);
        let payload = draw_copy_payload((0, 0, 2, 2), &[], &image);
        let mut image_bytes = Vec::new();
        image.write(&mut image_bytes);
        assert!(payload.ends_with(&image_bytes));
        channel
            .handle_message(display_server::DRAW_COPY, &payload)
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (_, _, width, height, rgba) = &events[0];
        assert_eq!((*width, *height), (2, 2));
        assert_eq!(
            rgba,
            &vec![3, 2, 1, 255, 7, 6, 5, 255, 11, 10, 9, 255, 15, 14, 13, 255]
        );
    }

    #[tokio::test]
    async fn from_cache_ending_a_draw_transparent_payload_is_drawn() {
        // DRAW_TRANSPARENT has no mask, so its source image always ends
        // the payload and every cache hit on it hit the #442 guard.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // surface_id
        for edge in [0u32, 0, 2, 2] {
            payload.extend_from_slice(&edge.to_le_bytes()); // dest box
        }
        payload.push(0); // clip_type NONE

        // SpiceTransparent: src_bitmap, src_area, src_color, true_color
        // = 28 bytes, then the image.
        let src_bitmap = (payload.len() + 28) as u32;
        payload.extend_from_slice(&src_bitmap.to_le_bytes());
        for edge in [0u32, 0, 2, 2] {
            payload.extend_from_slice(&edge.to_le_bytes()); // src_area
        }
        payload.extend_from_slice(&[0u8; 8]);
        from_cache_image(ImageType::FromCache, 42, 2, 2).write(&mut payload);
        channel
            .handle_message(display_server::DRAW_TRANSPARENT, &payload)
            .await
            .expect("draw_transparent must not error");

        let mut chroma_draws = 0;
        while let Ok(event) = peers.events.try_recv() {
            if matches!(event, ChannelEvent::ImageReadyChroma { .. }) {
                chroma_draws += 1;
            }
        }
        assert_eq!(chroma_draws, 1);
    }

    #[tokio::test]
    async fn from_cache_lossless_reads_the_image_cache() {
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let image = from_cache_image(ImageType::FromCacheLossless, 42, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((1, 1, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (_, _, _, _, rgba) = &events[0];
        // Bottom-right source pixel: BGRX (13, 14, 15, 16) -> RGBA.
        assert_eq!(rgba, &vec![15, 14, 13, 255]);
    }

    #[tokio::test]
    async fn cache_replace_me_replaces_the_cached_image() {
        // spice-server resends a lossy cached image losslessly with
        // CACHE_REPLACE_ME (not CACHE_ME) before it names it with
        // FromCacheLossless, so the resend must overwrite the entry.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let replacement: Vec<u8> = (101..=116).collect();
        let image = pixmap_image(42, IMAGE_FLAGS_CACHE_REPLACE_ME, 2, 2, 8, &replacement);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");
        assert_eq!(drain_image_events(&mut peers).len(), 1);

        let image = from_cache_image(ImageType::FromCacheLossless, 42, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((1, 1, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (_, _, _, _, rgba) = &events[0];
        assert_eq!(rgba, &vec![115, 114, 113, 255]);
    }

    /// A top-down 32-bit LZ4 image of `pixels` (B,G,R,X), as one block.
    fn lz4_image(id: u64, flags: u8, width: u32, height: u32, pixels: &[u8]) -> SpiceImage {
        let block = lz4_flex::block::compress(pixels);
        let mut body = vec![1, bitmap_fmt::BIT32];
        body.extend_from_slice(&(block.len() as u32).to_be_bytes());
        body.extend_from_slice(&block);
        SpiceImage {
            descriptor: image_descriptor(id, ImageType::Lz4, flags, width, height),
            payload: ImagePayload::Lz4(BinaryData { data: body }),
        }
    }

    #[tokio::test]
    async fn lz4_image_is_drawn_and_cached_under_its_descriptor_id() {
        // The LZ4 decoder does not know the image's id, so the cache
        // must key it by the descriptor's, which is what a later
        // FromCache names.
        let (mut channel, mut peers) = test_display_channel().await;
        let pixels: Vec<u8> = (1..=16).collect();
        let expected = vec![3, 2, 1, 255, 7, 6, 5, 255, 11, 10, 9, 255, 15, 14, 13, 255];
        let image = lz4_image(0x1234, IMAGE_FLAGS_CACHE_ME, 2, 2, &pixels);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        let (_, _, width, height, rgba) = &events[0];
        assert_eq!((*width, *height), (2, 2));
        assert_eq!(rgba, &expected);

        let image = from_cache_image(ImageType::FromCache, 0x1234, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1, "the cache hit is drawn");
        assert_eq!(events[0].4, expected);
    }

    #[tokio::test]
    async fn pixmap_image_is_drawn_and_cached_under_its_descriptor_id() {
        // Non-GLZ images are cached under the id in their descriptor,
        // which is what a later FromCache names.
        let (mut channel, mut peers) = test_display_channel().await;
        let pixels: Vec<u8> = (1..=16).collect();
        let expected = vec![3, 2, 1, 255, 7, 6, 5, 255, 11, 10, 9, 255, 15, 14, 13, 255];
        let image = pixmap_image(0x1234, IMAGE_FLAGS_CACHE_ME, 2, 2, 8, &pixels);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].4, expected);

        let image = from_cache_image(ImageType::FromCache, 0x1234, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1, "the cache hit is drawn");
        assert_eq!(events[0].4, expected);
    }

    /// A top-down 2x2 LZ_RGB32 image of four literal BGR pixels: a
    /// little-endian data_size, the 28-byte big-endian LZ header, one
    /// control byte (four literals follow), then the pixels.
    fn lz_rgb_image(id: u64, flags: u8, bgr: &[u8; 12]) -> SpiceImage {
        let mut lz = Vec::new();
        lz.extend_from_slice(b"  ZL");
        lz.extend_from_slice(&1u16.to_be_bytes());
        lz.extend_from_slice(&0u16.to_be_bytes());
        lz.extend_from_slice(&[0, 0, 0]);
        lz.push(8); // LZ_IMAGE_TYPE_RGB32
        for value in [2u32, 2, 8, 1] {
            // width, height, stride, top_down
            lz.extend_from_slice(&value.to_be_bytes());
        }
        lz.push(3);
        lz.extend_from_slice(bgr);
        let mut data = (lz.len() as u32).to_le_bytes().to_vec();
        data.extend_from_slice(&lz);
        SpiceImage {
            descriptor: image_descriptor(id, ImageType::LzRgb, flags, 2, 2),
            payload: ImagePayload::Other(data),
        }
    }

    #[tokio::test]
    async fn lz_image_is_drawn_and_cached_under_its_descriptor_id() {
        // Like LZ4, the LZ decoder does not know the image's id.
        let (mut channel, mut peers) = test_display_channel().await;
        let bgr: [u8; 12] = std::array::from_fn(|i| i as u8 + 1);
        let expected = vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255];
        let image = lz_rgb_image(0x1234, IMAGE_FLAGS_CACHE_ME, &bgr);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].4, expected);

        let image = from_cache_image(ImageType::FromCache, 0x1234, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1, "the cache hit is drawn");
        assert_eq!(events[0].4, expected);
    }

    #[tokio::test]
    async fn lz4_data_size_past_the_payload_warns_and_is_skipped() {
        let (mut channel, mut peers) = test_display_channel().await;
        let pixels: Vec<u8> = (1..=16).collect();
        let image = lz4_image(1, 0, 2, 2, &pixels);
        let ImagePayload::Lz4(lz4) = &image.payload else {
            unreachable!()
        };
        // The image ends the payload, so its data_size is just before
        // its body; claim one byte more than there is.
        let mut payload = draw_copy_payload((0, 0, 2, 2), &[], &image);
        let size_at = payload.len() - lz4.data.len() - 4;
        payload[size_at..size_at + 4].copy_from_slice(&(lz4.data.len() as u32 + 1).to_le_bytes());
        channel
            .handle_message(display_server::DRAW_COPY, &payload)
            .await
            .expect("a short LZ4 image is not an error");

        assert!(drain_image_events(&mut peers).is_empty());
        assert!(logging::warn_once_keys().contains(&"display:decode_failure:lz4:short_data"));
    }

    #[tokio::test]
    async fn lz4_image_that_does_not_decode_warns_and_is_skipped() {
        // Three pixels' worth of data for a 2x2 image.
        let (mut channel, mut peers) = test_display_channel().await;
        let image = lz4_image(1, 0, 2, 2, &[0; 12]);
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("an undecodable LZ4 image is not an error");

        assert!(drain_image_events(&mut peers).is_empty());
        assert!(logging::warn_once_keys().contains(&"display:decode_failure:lz4:decode_failed"));
    }

    #[tokio::test]
    async fn draw_blend_with_op_put_draws_like_a_copy() {
        // DRAW_BLEND's body is DRAW_COPY's, and its image is found the
        // same way.
        let (mut channel, mut peers) = test_display_channel().await;
        cache_2x2_pixmap(&mut channel, &mut peers, 42).await;

        let image = from_cache_image(ImageType::FromCache, 42, 2, 2);
        channel
            .handle_message(
                display_server::DRAW_BLEND,
                &draw_copy_payload((1, 1, 2, 2), &[], &image),
            )
            .await
            .expect("draw_blend must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].4, vec![15, 14, 13, 255]);
    }

    #[tokio::test]
    async fn draw_copy_src_bitmap_into_its_fixed_fields_is_refused() {
        // spice.proto puts a draw's images after its fixed fields; a
        // pointer back into them names no image.
        let (mut channel, mut peers) = test_display_channel().await;
        let pixels: Vec<u8> = (1..=16).collect();
        let image = pixmap_image(1, 0, 2, 2, 8, &pixels);
        let mut payload = draw_copy_payload((0, 0, 2, 2), &[], &image);
        let copy_at = DrawBase::MIN_SIZE;
        payload[copy_at..copy_at + 4].copy_from_slice(&1u32.to_le_bytes());
        channel
            .handle_message(display_server::DRAW_COPY, &payload)
            .await
            .expect("a refused pointer is not an error");

        assert!(drain_image_events(&mut peers).is_empty());
        assert!(logging::warn_once_keys()
            .contains(&"display:decode_failure:draw_copy:src_bitmap_in_fixed_part"));
    }

    #[tokio::test]
    async fn pixmap_with_a_cached_palette_reads_rows_after_the_palette_id() {
        // PAL_FROM_CACHE makes BitmapData's palette field a u64 cache id,
        // so the rows start 22 bytes in, not 18.
        let (mut channel, mut peers) = test_display_channel().await;
        let pixels: Vec<u8> = (1..=16).collect();
        let mut image = pixmap_image(1, 0, 2, 2, 8, &pixels);
        if let ImagePayload::Bitmap(bitmap) = &mut image.payload {
            bitmap.header.flags |= bitmap_flags::PAL_FROM_CACHE;
            bitmap.header.palette = BitmapPalette::FromCache(5);
        }
        channel
            .handle_message(
                display_server::DRAW_COPY,
                &draw_copy_payload((0, 0, 2, 2), &[], &image),
            )
            .await
            .expect("draw_copy must not error");

        let events = drain_image_events(&mut peers);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].4,
            vec![3, 2, 1, 255, 7, 6, 5, 255, 11, 10, 9, 255, 15, 14, 13, 255]
        );
    }

    // -------------------------------------------------------------------------
    // The bytes the display channel sends
    //
    // Pinned from what ryll sent before its client messages moved onto the
    // protocol crate's writers, so that the move cannot change them.
    // -------------------------------------------------------------------------

    // The scheme asked for at link-up comes from configuration: AUTO_GLZ by
    // default, and exactly LZ4 when asked, since spice-server only sends LZ4
    // images to a client that requests that scheme.
    #[tokio::test]
    async fn link_up_sends_the_configured_compression() {
        for (scheme, wire) in [
            (image_compression::AUTO_GLZ, 2u8),
            (image_compression::LZ4, 7u8),
        ] {
            let (mut channel, mut peers) = test_display_channel().await;
            channel.preferred_compression = scheme;
            channel
                .send_link_up_preferences()
                .await
                .expect("send link-up preferences");
            assert_eq!(peers.read_sent(7).await, vec![103, 0, 1, 0, 0, 0, wire]);
            if cfg!(feature = "h264-decode") {
                // MJPEG (1) then H.264 (3); see preferred_video_codecs.
                assert_eq!(peers.read_sent(9).await, vec![105, 0, 3, 0, 0, 0, 2, 1, 3]);
            } else {
                // No decoder, so no H.264: MJPEG (1) only.
                assert_eq!(peers.read_sent(8).await, vec![105, 0, 2, 0, 0, 0, 1, 1]);
            }
        }
    }

    #[tokio::test]
    async fn link_up_messages_are_unchanged() {
        let (mut channel, mut peers) = test_display_channel().await;

        channel.send_init().await.expect("send INIT");
        assert_eq!(
            peers.read_sent(6 + 14).await,
            vec![
                101, 0, 14, 0, 0, 0, // mini header: INIT, 14 bytes
                1, // pixmap_cache_id
                0x00, 0x00, 0x40, 0x01, 0, 0, 0, 0, // pixmap_cache_size: 20 MiB
                1, // glz_dictionary_id
                0x00, 0x00, 0x30, 0x00, // glz_dictionary_window_size: 3 MiB
            ]
        );

        channel
            .send_preferred_compression(image_compression::AUTO_GLZ)
            .await
            .expect("send PREFERRED_COMPRESSION");
        assert_eq!(peers.read_sent(7).await, vec![103, 0, 1, 0, 0, 0, 2]);

        // What run_loop sends at link-up by default: MJPEG (1) then
        // H.264 (3). See preferred_video_codecs.
        channel
            .send_preferred_video_codec_type(preferred_video_codecs(H264Offer::default()))
            .await
            .expect("send PREFERRED_VIDEO_CODEC_TYPE");
        assert_eq!(peers.read_sent(9).await, vec![105, 0, 3, 0, 0, 0, 2, 1, 3]);

        // With --experimental-h264: H.264 (3) then MJPEG (1), the
        // multi-codec list keeping its order on the wire.
        channel
            .send_preferred_video_codec_type(preferred_video_codecs(H264Offer::Preferred))
            .await
            .expect("send PREFERRED_VIDEO_CODEC_TYPE");
        assert_eq!(peers.read_sent(9).await, vec![105, 0, 3, 0, 0, 0, 2, 3, 1]);

        // With --no-h264: MJPEG (1) only.
        channel
            .send_preferred_video_codec_type(preferred_video_codecs(H264Offer::Off))
            .await
            .expect("send PREFERRED_VIDEO_CODEC_TYPE");
        assert_eq!(peers.read_sent(8).await, vec![105, 0, 2, 0, 0, 0, 1, 1]);

        // More codecs than the u8 count can say: the first 255 are sent.
        let codecs: Vec<u8> = (0..300).map(|i| (i % 7) as u8).collect();
        channel
            .send_preferred_video_codec_type(&codecs)
            .await
            .expect("send PREFERRED_VIDEO_CODEC_TYPE");
        let sent = peers.read_sent(6 + 256).await;
        assert_eq!(&sent[..7], &[105, 0, 0, 1, 0, 0, 255]);
        assert_eq!(&sent[7..], &codecs[..255]);
    }

    #[test]
    fn the_h264_offer_is_off_without_a_decoder() {
        for requested in [H264Offer::Off, H264Offer::AfterMjpeg, H264Offer::Preferred] {
            let expected = if cfg!(feature = "h264-decode") {
                requested
            } else {
                H264Offer::Off
            };
            assert_eq!(supported_h264_offer(requested), expected);
        }
    }

    #[tokio::test]
    async fn a_display_channel_only_offers_h264_it_can_decode() {
        // test_display_channel() asks for the default offer.
        let (channel, _peers) = test_display_channel().await;
        let expected = if cfg!(feature = "h264-decode") {
            H264Offer::default()
        } else {
            H264Offer::Off
        };
        assert_eq!(channel.h264, expected);
    }

    #[test]
    fn preferred_video_codecs_follow_the_h264_offer() {
        // Default: MJPEG first, so spice-server keeps streaming MJPEG
        // (shakenfist/ryll#398), then H.264.
        assert_eq!(
            preferred_video_codecs(H264Offer::default()),
            &[SPICE_VIDEO_CODEC_TYPE_MJPEG, SPICE_VIDEO_CODEC_TYPE_H264]
        );
        // --experimental-h264: H.264 first, MJPEG as the fallback.
        assert_eq!(
            preferred_video_codecs(H264Offer::Preferred),
            &[SPICE_VIDEO_CODEC_TYPE_H264, SPICE_VIDEO_CODEC_TYPE_MJPEG]
        );
        // --no-h264: MJPEG only.
        assert_eq!(
            preferred_video_codecs(H264Offer::Off),
            &[SPICE_VIDEO_CODEC_TYPE_MJPEG]
        );
    }

    #[tokio::test]
    async fn stream_report_bytes_are_unchanged() {
        let (mut channel, mut peers) = test_display_channel().await;

        // A 51-byte STREAM_CREATE (the clip type ends it) for MJPEG
        // stream 7, so that there is a stream to report on.
        let mut create = vec![0u8; 51];
        create[4..8].copy_from_slice(&7u32.to_le_bytes());
        create[9] = SPICE_VIDEO_CODEC_TYPE_MJPEG;
        channel
            .handle_message(display_server::STREAM_CREATE, &create)
            .await
            .expect("stream_create");

        let stream = channel.streams.get_mut(&7).expect("stream 7 exists");
        stream.report_unique_id = 0xdead_beef;
        stream.report_start_frame_mm_time = 100;
        stream.report_end_frame_mm_time = 0;
        stream.report_num_frames = 5;
        stream.report_num_drops = 1;

        channel
            .send_stream_report(7)
            .await
            .expect("send STREAM_REPORT");
        let sent = peers.read_sent(6 + 32).await;
        assert_eq!(
            &sent[..30],
            &[
                102, 0, 32, 0, 0, 0, // mini header: STREAM_REPORT, 32 bytes
                7, 0, 0, 0, // stream_id
                0xef, 0xbe, 0xad, 0xde, // unique_id
                100, 0, 0, 0, // start_frame_mm_time
                0, 0, 0, 0, // end_frame_mm_time
                5, 0, 0, 0, // num_frames
                1, 0, 0, 0, // num_drops
            ]
        );
        // last_frame_delay is end_frame_mm_time minus the clock's now,
        // which has advanced by however long the test took.
        let delay = i32::from_le_bytes([sent[30], sent[31], sent[32], sent[33]]);
        assert!((-60_000..=0).contains(&delay), "delay {delay}");
        assert_eq!(&sent[34..], &[0xff, 0xff, 0xff, 0xff]); // audio_delay
    }
}
