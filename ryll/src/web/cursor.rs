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

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;
use shakenfist_spice_renderer::{ChannelEvent, CursorImage, SurfaceMirror};
use shakenfist_spice_webrtc::WebrtcBridge;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, warn};

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

/// Spawn-friendly relay. Loops until the broadcast channel
/// closes (i.e. the renderer's session orchestrator has exited).
/// `Lagged` from the broadcast receiver is logged and skipped —
/// the cursor stream is stateless deltas, so a missed shape
/// will be re-sent on the next pointer move and a missed
/// position is corrected by the next `CursorPosition`.
pub async fn run_cursor_relay(
    mut event_rx: broadcast::Receiver<ChannelEvent>,
    bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
    surface_mirror: Arc<Mutex<SurfaceMirror>>,
) {
    loop {
        match event_rx.recv().await {
            Ok(ChannelEvent::CursorShape(image)) => match encode_shape(&image) {
                Ok(payload) => send_to_bridge(&bridge_slot, &payload).await,
                Err(e) => warn!("web cursor: failed to encode shape: {}", e),
            },
            Ok(ChannelEvent::CursorPosition { x, y, visible }) => {
                if !visible {
                    if let Ok(payload) = serde_json::to_vec(&CursorMsg::Hide) {
                        send_to_bridge(&bridge_slot, &payload).await;
                    }
                    continue;
                }
                let dims = {
                    let guard = surface_mirror.lock().await;
                    guard.primary_surface().map(|s| s.size())
                };
                let Some((w, h)) = dims else {
                    // No primary surface yet — drop silently.
                    continue;
                };
                let payload = match encode_position(x, y, w, h) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("web cursor: failed to encode position: {}", e);
                        continue;
                    }
                };
                send_to_bridge(&bridge_slot, &payload).await;
                // After re-showing the cursor (visible=true) make
                // sure the overlay isn't hidden from a previous
                // hide.  The browser only reveals the overlay
                // again when it sees a new shape OR an explicit
                // show; sending a `cursor-show` after a real
                // position keeps the overlay visible without
                // flicker.
                if let Ok(p) = serde_json::to_vec(&CursorMsg::Show) {
                    send_to_bridge(&bridge_slot, &p).await;
                }
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
        }
    }
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
/// DC. If there's no bridge (no viewer connected) drop the
/// message silently — the next `CursorShape` / `CursorPosition`
/// will re-deliver state once a viewer attaches.
async fn send_to_bridge(slot: &Arc<Mutex<Option<WebrtcBridge>>>, payload: &[u8]) {
    let guard = slot.lock().await;
    if let Some(bridge) = guard.as_ref() {
        if let Err(e) = bridge.send_control(payload).await {
            debug!("web cursor: send_control failed: {}", e);
        }
    }
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
}
