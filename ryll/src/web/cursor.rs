//! Renderer → browser cursor relay.
//!
//! Subscribes to the broadcast bus of [`ChannelEvent`]s emitted
//! by `run_connection` and forwards [`ChannelEvent::CursorShape`]
//! and [`ChannelEvent::CursorPosition`] to the browser over the
//! active bridge's control datachannel. The browser renders the
//! cursor as an `<img>` overlay above the `<video>`; the host
//! browser cursor is hidden over the video so the SPICE cursor
//! wins.
//!
//! Wire format (control DC, JSON envelopes):
//!
//! ```json
//! { "type": "cursor-shape", "png_b64": "...", "hot_x": 0, "hot_y": 0 }
//! { "type": "cursor-pos",   "x_norm": 0.5, "y_norm": 0.5 }
//! { "type": "cursor-hide" }
//! { "type": "cursor-show" }
//! ```
//!
//! Cursor shapes are encoded as PNG (using the existing `png`
//! crate already in ryll's dependency graph) and base64'd into
//! a JSON string. The shape payload is small in absolute terms
//! (a typical 32x32 RGBA cursor is ~4 KiB raw, well under 1 KiB
//! after PNG-encoding the typical sparse alpha channel). Cursor
//! position events are normalised against the primary surface
//! size so the browser can map them onto whatever
//! letterbox-corrected video area it currently has.
//!
//! `CursorPosition::visible == false` is dispatched as a
//! `cursor-hide`; `true` as a `cursor-show`. The renderer's
//! `ChannelEvent` enum doesn't carry separate hide/show
//! variants, just the per-position flag.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;
use shakenfist_spice_renderer::{ChannelEvent, CursorImage, SurfaceMirror};
use shakenfist_spice_webrtc::WebrtcBridge;
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{debug, warn};

/// Backoff schedule (in ms) for re-sending the cached cursor
/// state after a new bridge is installed. The data channel
/// isn't open until SCTP/DTLS finishes, which `/offer` does not
/// wait for — so the first attempt almost always fails. The
/// schedule covers a typical ICE/DTLS completion (~300-800 ms)
/// with headroom for slow paths, then gives up; the next real
/// cursor event will refresh state once the DC is up.
const REPLAY_BACKOFFS_MS: &[u64] = &[150, 250, 400, 600, 1000, 1500];

/// Wire-format server → browser cursor messages. The `type`
/// discriminator matches the JSON envelopes parsed by `app.js`.
#[derive(Serialize)]
#[serde(tag = "type")]
enum CursorMsg<'a> {
    #[serde(rename = "cursor-shape")]
    Shape {
        png_b64: &'a str,
        hot_x: u16,
        hot_y: u16,
    },
    #[serde(rename = "cursor-pos")]
    Pos { x_norm: f32, y_norm: f32 },
    #[serde(rename = "cursor-hide")]
    Hide,
    #[serde(rename = "cursor-show")]
    Show,
}

/// Cached state the relay replays to a freshly attached viewer.
/// Holds the most recent encoded `cursor-shape` payload (already
/// PNG'd + base64'd + JSON-wrapped, ready for the wire) and the
/// most recent `CursorPosition` (raw fields — denormalisation
/// happens at replay time against the current surface size in
/// case the guest has resized).
#[derive(Default, Clone)]
struct CursorCache {
    shape: Option<Vec<u8>>,
    position: Option<(u16, u16, bool)>,
}

/// Spawn-friendly relay. Loops until the broadcast channel
/// closes (i.e. the renderer's session orchestrator has exited).
/// `Lagged` from the broadcast receiver is logged and skipped —
/// the cursor stream is stateless deltas, so a missed shape
/// will be re-sent on the next pointer move and a missed
/// position is corrected by the next `CursorPosition`.
///
/// The relay caches the last `cursor-shape` and last
/// `CursorPosition` it sent. When `bridge_installed` fires
/// (signalled by `/offer` after a new bridge is installed),
/// the relay spawns a small task that replays the cached state
/// to the new viewer with a backoff — the data channel isn't
/// open the instant `/offer` returns, so the first send
/// attempts typically fail. Without this replay, viewers that
/// arrive after the cursor channel's initial `CURSOR_INIT`
/// (the shape carrier) would see no cursor sprite on static
/// screens like GDM where the guest never changes shape on
/// its own.
pub async fn run_cursor_relay(
    event_rx: broadcast::Receiver<ChannelEvent>,
    bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
    surface_mirror: Arc<Mutex<SurfaceMirror>>,
    bridge_installed: Arc<Notify>,
) {
    let cache: Arc<Mutex<CursorCache>> = Arc::new(Mutex::new(CursorCache::default()));
    run_cursor_relay_inner(
        event_rx,
        bridge_slot,
        surface_mirror,
        bridge_installed,
        cache,
    )
    .await
}

/// Shared implementation behind [`run_cursor_relay`]. The cache
/// is taken as a parameter so unit tests can observe what was
/// captured after driving events through the broadcast channel.
async fn run_cursor_relay_inner(
    mut event_rx: broadcast::Receiver<ChannelEvent>,
    bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
    surface_mirror: Arc<Mutex<SurfaceMirror>>,
    bridge_installed: Arc<Notify>,
    cache: Arc<Mutex<CursorCache>>,
) {
    loop {
        tokio::select! {
            ev = event_rx.recv() => match ev {
                Ok(ChannelEvent::CursorShape(image)) => match encode_shape(&image) {
                    Ok(payload) => {
                        let _ = try_send_to_bridge(&bridge_slot, &payload).await;
                        cache.lock().await.shape = Some(payload);
                    }
                    Err(e) => warn!("web cursor: failed to encode shape: {}", e),
                },
                Ok(ChannelEvent::CursorPosition { x, y, visible }) => {
                    cache.lock().await.position = Some((x, y, visible));
                    relay_position(&bridge_slot, &surface_mirror, x, y, visible).await;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        "web cursor: lagged by {} events; the next \
                         cursor delta will resync",
                        n
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("web cursor: event channel closed; relay exiting");
                    return;
                }
            },
            _ = bridge_installed.notified() => {
                // Spawn the replay off-task so the backoff schedule
                // doesn't block live event processing. The cache is
                // Arc<Mutex<_>>, so the task always reads the latest
                // state on each retry — a fresh CursorShape arriving
                // mid-replay supersedes the cached payload, never
                // races behind it.
                let slot = bridge_slot.clone();
                let mirror = surface_mirror.clone();
                let cache = cache.clone();
                tokio::spawn(async move {
                    replay_to_new_bridge(slot, mirror, cache).await;
                });
            }
        }
    }
}

/// Send a `CursorPosition` to the active bridge: hide on
/// `visible=false`, otherwise position + show. Pulled out of
/// the main loop so the replay path can reuse it.
async fn relay_position(
    bridge_slot: &Arc<Mutex<Option<WebrtcBridge>>>,
    surface_mirror: &Arc<Mutex<SurfaceMirror>>,
    x: u16,
    y: u16,
    visible: bool,
) {
    if !visible {
        if let Ok(payload) = serde_json::to_vec(&CursorMsg::Hide) {
            let _ = try_send_to_bridge(bridge_slot, &payload).await;
        }
        return;
    }
    let dims = {
        let guard = surface_mirror.lock().await;
        guard.primary_surface().map(|s| s.size())
    };
    let Some((w, h)) = dims else {
        // No primary surface yet — drop silently.
        return;
    };
    let payload = match encode_position(x, y, w, h) {
        Ok(p) => p,
        Err(e) => {
            warn!("web cursor: failed to encode position: {}", e);
            return;
        }
    };
    let _ = try_send_to_bridge(bridge_slot, &payload).await;
    // After re-showing the cursor (visible=true) make
    // sure the overlay isn't hidden from a previous
    // hide.  The browser only reveals the overlay
    // again when it sees a new shape OR an explicit
    // show; sending a `cursor-show` after a real
    // position keeps the overlay visible without
    // flicker.
    if let Ok(p) = serde_json::to_vec(&CursorMsg::Show) {
        let _ = try_send_to_bridge(bridge_slot, &p).await;
    }
}

/// Replay the cached cursor shape + position to the bridge in
/// `bridge_slot`. The data channel typically isn't open the
/// instant `/offer` returns, so we walk through
/// [`REPLAY_BACKOFFS_MS`] sleeping between attempts. On each
/// retry the cache is re-read so a CursorShape arriving during
/// the replay supersedes the stale payload. Gives up silently
/// after the schedule is exhausted — the next real cursor event
/// will refresh state once the DC is up.
async fn replay_to_new_bridge(
    bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
    surface_mirror: Arc<Mutex<SurfaceMirror>>,
    cache: Arc<Mutex<CursorCache>>,
) {
    for delay_ms in REPLAY_BACKOFFS_MS {
        tokio::time::sleep(Duration::from_millis(*delay_ms)).await;

        let snapshot = cache.lock().await.clone();
        if snapshot.shape.is_none() && snapshot.position.is_none() {
            // Nothing to replay yet.
            return;
        }

        let mut shape_ok = true;
        if let Some(ref payload) = snapshot.shape {
            shape_ok = try_send_to_bridge(&bridge_slot, payload).await.is_ok();
        }

        let mut position_ok = true;
        if let Some((x, y, visible)) = snapshot.position {
            if !visible {
                if let Ok(payload) = serde_json::to_vec(&CursorMsg::Hide) {
                    position_ok = try_send_to_bridge(&bridge_slot, &payload).await.is_ok();
                }
            } else {
                let dims = {
                    let guard = surface_mirror.lock().await;
                    guard.primary_surface().map(|s| s.size())
                };
                if let Some((w, h)) = dims {
                    if let Ok(payload) = encode_position(x, y, w, h) {
                        position_ok = try_send_to_bridge(&bridge_slot, &payload).await.is_ok();
                    }
                    if position_ok {
                        if let Ok(p) = serde_json::to_vec(&CursorMsg::Show) {
                            position_ok = try_send_to_bridge(&bridge_slot, &p).await.is_ok();
                        }
                    }
                } else {
                    // No primary surface yet — try again next round.
                    position_ok = false;
                }
            }
        }

        if shape_ok && position_ok {
            debug!("web cursor: replayed cached state to new viewer");
            return;
        }
    }
    debug!("web cursor: replay budget exhausted; next real event will resync");
}

/// PNG-encode a [`CursorImage`] and wrap it in a `cursor-shape`
/// JSON envelope ready for the control DC.
fn encode_shape(image: &CursorImage) -> anyhow::Result<Vec<u8>> {
    let png_bytes = encode_png(image)?;
    let png_b64 = STANDARD.encode(&png_bytes);
    let msg = CursorMsg::Shape {
        png_b64: &png_b64,
        hot_x: image.hot_spot_x,
        hot_y: image.hot_spot_y,
    };
    Ok(serde_json::to_vec(&msg)?)
}

/// Encode an RGBA8 [`CursorImage`] as a PNG byte stream using
/// the `png` crate already in ryll's dep tree (also used by the
/// bug-report screenshotter).
fn encode_png(image: &CursorImage) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&image.pixels)?;
    }
    Ok(out)
}

/// Encode a `(x, y)` cursor position normalised against the
/// primary surface size into a `cursor-pos` JSON envelope.
fn encode_position(x: u16, y: u16, width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let w = width.max(1) as f32;
    let h = height.max(1) as f32;
    let msg = CursorMsg::Pos {
        x_norm: (x as f32) / w,
        y_norm: (y as f32) / h,
    };
    Ok(serde_json::to_vec(&msg)?)
}

/// Send a payload over the currently active bridge's control
/// DC. Returns `Err` if there is no bridge in the slot or if
/// the underlying `send_control` failed (the data channel is
/// closed, or — most commonly during the replay path — not yet
/// open because SCTP/DTLS is still handshaking). Both failure
/// modes are swallowed at the caller in the steady-state path;
/// the replay path uses the result to drive its backoff.
async fn try_send_to_bridge(
    slot: &Arc<Mutex<Option<WebrtcBridge>>>,
    payload: &[u8],
) -> anyhow::Result<()> {
    let guard = slot.lock().await;
    let Some(bridge) = guard.as_ref() else {
        return Err(anyhow::anyhow!("no active bridge"));
    };
    if let Err(e) = bridge.send_control(payload).await {
        debug!("web cursor: send_control failed: {}", e);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn dummy_cursor(width: u16, height: u16) -> CursorImage {
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..(width as usize * height as usize) {
            pixels.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
        }
        CursorImage {
            width,
            height,
            hot_spot_x: 3,
            hot_spot_y: 4,
            pixels,
        }
    }

    #[test]
    fn encode_shape_produces_png_envelope() {
        let img = dummy_cursor(16, 16);
        let payload = encode_shape(&img).expect("encode_shape");
        let v: Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(v["type"], "cursor-shape");
        assert_eq!(v["hot_x"], 3);
        assert_eq!(v["hot_y"], 4);
        let b64 = v["png_b64"].as_str().expect("png_b64 string");
        assert!(!b64.is_empty(), "png_b64 should be non-empty");
        let png_bytes = STANDARD.decode(b64).expect("base64 decode");
        // PNG signature: 89 50 4E 47 0D 0A 1A 0A.
        assert_eq!(
            &png_bytes[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "decoded base64 should start with the PNG signature"
        );
    }

    #[test]
    fn encode_shape_handles_32x32_synthetic() {
        // The plan suggests testing with a 32x32 cursor.
        let img = dummy_cursor(32, 32);
        let payload = encode_shape(&img).expect("encode_shape");
        let v: Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(v["type"], "cursor-shape");
        let b64 = v["png_b64"].as_str().expect("png_b64");
        let png_bytes = STANDARD.decode(b64).expect("base64");
        assert!(png_bytes.len() > 8);
    }

    #[test]
    fn encode_position_normalises_against_surface() {
        // 1920x1080 surface, cursor at (960, 540) should land
        // at (0.5, 0.5).
        let payload = encode_position(960, 540, 1920, 1080).expect("encode_position");
        let v: Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(v["type"], "cursor-pos");
        let x = v["x_norm"].as_f64().expect("x_norm number");
        let y = v["y_norm"].as_f64().expect("y_norm number");
        assert!((x - 0.5).abs() < 1e-3, "x_norm={}", x);
        assert!((y - 0.5).abs() < 1e-3, "y_norm={}", y);
    }

    #[test]
    fn encode_position_top_left_is_zero() {
        let payload = encode_position(0, 0, 1920, 1080).expect("encode_position");
        let v: Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(v["x_norm"], 0.0);
        assert_eq!(v["y_norm"], 0.0);
    }

    #[test]
    fn cursor_hide_envelope_round_trips() {
        let payload = serde_json::to_vec(&CursorMsg::Hide).expect("serialize");
        let v: Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(v["type"], "cursor-hide");
    }

    #[test]
    fn cursor_show_envelope_round_trips() {
        let payload = serde_json::to_vec(&CursorMsg::Show).expect("serialize");
        let v: Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(v["type"], "cursor-show");
    }

    #[tokio::test]
    async fn try_send_to_bridge_errs_when_slot_empty() {
        let slot: Arc<Mutex<Option<WebrtcBridge>>> = Arc::new(Mutex::new(None));
        let err = try_send_to_bridge(&slot, b"{}")
            .await
            .expect_err("no bridge should fail");
        assert!(err.to_string().contains("no active bridge"));
    }

    /// `replay_to_new_bridge` with an empty cache must walk the
    /// schedule and return without panicking. It must NOT block
    /// for the full schedule — an empty cache exits on the first
    /// iteration after the first sleep, so the whole call
    /// completes within the first backoff window.
    #[tokio::test]
    async fn replay_with_empty_cache_returns_quickly() {
        let slot: Arc<Mutex<Option<WebrtcBridge>>> = Arc::new(Mutex::new(None));
        let mirror = Arc::new(Mutex::new(SurfaceMirror::new()));
        let cache = Arc::new(Mutex::new(CursorCache::default()));

        let start = std::time::Instant::now();
        replay_to_new_bridge(slot, mirror, cache).await;
        let elapsed = start.elapsed();

        // First backoff is 150 ms; the empty-cache check fires
        // right after it, so the whole call must finish well
        // before the full schedule (~3.85 s) would.
        assert!(
            elapsed < Duration::from_millis(500),
            "replay with empty cache took {:?}, expected <500 ms",
            elapsed,
        );
    }

    /// Drive a CursorShape and a CursorPosition through the
    /// relay's broadcast channel and verify the cache reflects
    /// them. No real bridge is involved — `try_send_to_bridge`
    /// silently fails with "no active bridge", which is fine;
    /// what we're asserting is that the cache update side-effect
    /// fired regardless.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_caches_shape_and_position() {
        use shakenfist_spice_renderer::ChannelEvent;

        let (event_tx, event_rx) = broadcast::channel::<ChannelEvent>(16);
        let slot: Arc<Mutex<Option<WebrtcBridge>>> = Arc::new(Mutex::new(None));
        let mirror = Arc::new(Mutex::new(SurfaceMirror::new()));
        let installed = Arc::new(Notify::new());
        let cache = Arc::new(Mutex::new(CursorCache::default()));

        let cache_for_relay = cache.clone();
        let relay = tokio::spawn(run_cursor_relay_inner(
            event_rx,
            slot,
            mirror,
            installed,
            cache_for_relay,
        ));

        event_tx
            .send(ChannelEvent::CursorShape(dummy_cursor(16, 16)))
            .expect("broadcast send");
        event_tx
            .send(ChannelEvent::CursorPosition {
                x: 100,
                y: 200,
                visible: true,
            })
            .expect("broadcast send");

        // Yield long enough for the relay to drain both events.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let snapshot = cache.lock().await.clone();
        let shape = snapshot.shape.expect("shape should be cached");
        let v: Value = serde_json::from_slice(&shape).expect("shape json");
        assert_eq!(v["type"], "cursor-shape");
        assert_eq!(snapshot.position, Some((100, 200, true)));

        relay.abort();
        let _ = tokio::time::timeout(Duration::from_millis(200), relay).await;
    }
}
