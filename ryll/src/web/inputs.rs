//! Browser → renderer input relay.
//!
//! Drains the WebRTC bridge's control data channel
//! ([`shakenfist_spice_webrtc::WebrtcBridge::control_rx`]),
//! parses JSON input messages from `app.js`, and forwards them
//! into the renderer's existing `input_tx`
//! ([`shakenfist_spice_renderer::InputEvent`]) and `resize_tx`
//! (`(width, height)`) channels.
//!
//! Phase 5c uses raw 16-bit AT scancodes on the wire. The
//! browser ports the [`scancode_for_logical_key`] table from
//! `shakenfist_spice_renderer::channels::inputs` directly to a
//! `KeyboardEvent.code` → scancode lookup, so this side just
//! needs to wrap the integer in an [`InputEvent::KeyDown`] /
//! [`KeyUp`]. Pointer coordinates arrive as normalised `[0, 1]`
//! floats; we denormalise against the surface mirror's primary
//! surface dimensions to match what the renderer's
//! [`InputsChannel`] expects (absolute SPICE pixel coordinates
//! for the client mouse mode).
//!
//! [`scancode_for_logical_key`]: shakenfist_spice_renderer
//! [`InputEvent::KeyDown`]: shakenfist_spice_renderer::InputEvent
//! [`KeyUp`]: shakenfist_spice_renderer::InputEvent::KeyUp
//! [`InputsChannel`]: shakenfist_spice_renderer

use std::sync::Arc;

use serde::Deserialize;
use shakenfist_spice_renderer::{InputEvent, SurfaceMirror};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

/// Wire-format browser → server input messages. The `type`
/// discriminator matches the JSON envelopes built by `app.js`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BrowserMsg {
    /// Keyboard key event. `scancode` is a wire-format SPICE /
    /// AT scancode (extended keys carry the 0xE0 prefix in the
    /// low byte; see `make_scancode` in
    /// `shakenfist_spice_renderer::channels::inputs`).
    #[serde(rename = "key")]
    Key { scancode: u32, down: bool },

    /// Pointer-move with normalised `[0, 1]` coordinates over
    /// the rendered video area (already letterbox-corrected on
    /// the browser side).
    #[serde(rename = "pointer-move")]
    PointerMove { x_norm: f32, y_norm: f32 },

    /// Pointer-button press/release. `button` is the SPICE
    /// bitmask: 1=LEFT, 2=MIDDLE, 4=RIGHT (and 8/16 for
    /// scroll-up/down — those aren't sent from 5c's MVP but
    /// the field is wide enough).
    #[serde(rename = "pointer-button")]
    PointerButton {
        button: u32,
        down: bool,
        x_norm: f32,
        y_norm: f32,
    },

    /// Initial viewport message — browser tells us how big its
    /// `<video>` element is. We forward `(width, height)` to
    /// the renderer's `resize_tx`, which drives
    /// `VDAgentMonitorsConfig` in `MainChannel`.
    #[serde(rename = "viewport")]
    Viewport { width: u32, height: u32 },
}

/// Spawn-friendly relay. Loops until `control_rx` closes (i.e.
/// the bridge dropped its sender, normally because the data
/// channel went away). Bad JSON is logged at debug and
/// otherwise ignored; we never panic on browser-supplied input.
pub async fn run_input_relay(
    mut control_rx: mpsc::Receiver<Vec<u8>>,
    input_tx: mpsc::Sender<InputEvent>,
    resize_tx: mpsc::Sender<(u32, u32)>,
    surface_mirror: Arc<Mutex<SurfaceMirror>>,
) {
    while let Some(payload) = control_rx.recv().await {
        let msg: BrowserMsg = match serde_json::from_slice(&payload) {
            Ok(m) => m,
            Err(e) => {
                debug!(
                    "web inputs: invalid JSON ({}): {:?}",
                    e,
                    std::str::from_utf8(&payload).ok()
                );
                continue;
            }
        };

        match msg {
            BrowserMsg::Key { scancode, down } => {
                let event = if down {
                    InputEvent::KeyDown(scancode)
                } else {
                    InputEvent::KeyUp(scancode)
                };
                if input_tx.send(event).await.is_err() {
                    warn!("web inputs: input_tx receiver dropped; relay exiting");
                    return;
                }
            }

            BrowserMsg::PointerMove { x_norm, y_norm } => {
                // Denormalise against the primary surface size
                // so the SPICE inputs channel sees absolute
                // pixel coordinates (client mouse mode). If
                // there's no primary yet (browser sent input
                // before SPICE finished session-init) drop
                // the event silently.
                let size = {
                    let guard = surface_mirror.lock().await;
                    guard.primary_surface().map(|s| s.size())
                };
                let Some((w, h)) = size else { continue };
                let (x, y) = denormalise(x_norm, y_norm, w, h);
                if input_tx.send(InputEvent::MouseMove { x, y }).await.is_err() {
                    warn!("web inputs: input_tx receiver dropped; relay exiting");
                    return;
                }
            }

            BrowserMsg::PointerButton {
                button,
                down,
                x_norm,
                y_norm,
            } => {
                let size = {
                    let guard = surface_mirror.lock().await;
                    guard.primary_surface().map(|s| s.size())
                };
                let Some((w, h)) = size else { continue };
                let (x, y) = denormalise(x_norm, y_norm, w, h);
                let event = if down {
                    InputEvent::MouseDown { button, x, y }
                } else {
                    InputEvent::MouseUp { button, x, y }
                };
                if input_tx.send(event).await.is_err() {
                    warn!("web inputs: input_tx receiver dropped; relay exiting");
                    return;
                }
            }

            BrowserMsg::Viewport { width, height } => {
                debug!("web inputs: viewport {}x{}", width, height);
                if resize_tx.send((width, height)).await.is_err() {
                    warn!("web inputs: resize_tx receiver dropped");
                }
            }
        }
    }
    debug!("web inputs: control_rx closed; relay exiting");
}

/// Map normalised `[0, 1]` coordinates to absolute pixel
/// coordinates within a `(w, h)` surface. Out-of-range inputs
/// are clamped — the browser side already clamps for letterbox,
/// but we re-clamp defensively in case the JS table evolves.
fn denormalise(x_norm: f32, y_norm: f32, w: u32, h: u32) -> (u32, u32) {
    let x = (x_norm.clamp(0.0, 1.0) * (w as f32)).round() as i64;
    let y = (y_norm.clamp(0.0, 1.0) * (h as f32)).round() as i64;
    // The cast to u32 below is bounded by the clamp above —
    // the result is in `[0, w]` (inclusive). Saturate the
    // upper edge to `w - 1` / `h - 1` so a `1.0` input doesn't
    // land off the end of the surface.
    let x = x.clamp(0, w.saturating_sub(1) as i64) as u32;
    let y = y.clamp(0, h.saturating_sub(1) as i64) as u32;
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakenfist_spice_renderer::ChannelEvent;
    use std::time::Duration;

    async fn primary_mirror(width: u32, height: u32) -> Arc<Mutex<SurfaceMirror>> {
        let mirror = Arc::new(Mutex::new(SurfaceMirror::new()));
        {
            let mut guard = mirror.lock().await;
            guard.apply_event(&ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width,
                height,
            });
        }
        mirror
    }

    /// Helper: run the relay against a fresh set of channels.
    /// Returns the senders the test can drive plus the receivers
    /// the test asserts against and the relay's join handle.
    #[allow(clippy::type_complexity)]
    fn spawn_relay(
        mirror: Arc<Mutex<SurfaceMirror>>,
    ) -> (
        mpsc::Sender<Vec<u8>>,
        mpsc::Receiver<InputEvent>,
        mpsc::Receiver<(u32, u32)>,
        tokio::task::JoinHandle<()>,
    ) {
        let (control_tx, control_rx) = mpsc::channel::<Vec<u8>>(16);
        let (input_tx, input_rx) = mpsc::channel::<InputEvent>(16);
        let (resize_tx, resize_rx) = mpsc::channel::<(u32, u32)>(4);
        let handle = tokio::spawn(run_input_relay(control_rx, input_tx, resize_tx, mirror));
        (control_tx, input_rx, resize_rx, handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn key_down_message_dispatches_keydown_event() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay(mirror);

        // 0xE048 is Up arrow in wire-format (E0-prefixed).
        let payload = br#"{"type":"key","scancode":57416,"down":true}"#.to_vec();
        tx.send(payload).await.expect("send");

        let event = tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        match event {
            InputEvent::KeyDown(sc) => assert_eq!(sc, 0xE048),
            other => panic!("expected KeyDown(0xE048), got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn key_up_message_dispatches_keyup_event() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay(mirror);

        // 0x1E is the 'A' base scancode.
        let payload = br#"{"type":"key","scancode":30,"down":false}"#.to_vec();
        tx.send(payload).await.expect("send");

        let event = tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        match event {
            InputEvent::KeyUp(sc) => assert_eq!(sc, 0x1E),
            other => panic!("expected KeyUp(0x1E), got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pointer_move_denormalises_to_centre() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay(mirror);

        let payload = br#"{"type":"pointer-move","x_norm":0.5,"y_norm":0.5}"#.to_vec();
        tx.send(payload).await.expect("send");

        let event = tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        match event {
            InputEvent::MouseMove { x, y } => {
                assert_eq!(x, 960);
                assert_eq!(y, 540);
            }
            other => panic!("expected MouseMove, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pointer_button_down_dispatches_mousedown() {
        let mirror = primary_mirror(1000, 800).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay(mirror);

        // SPICE bitmask: 1=LEFT.
        let payload =
            br#"{"type":"pointer-button","button":1,"down":true,"x_norm":0.25,"y_norm":0.5}"#
                .to_vec();
        tx.send(payload).await.expect("send");

        let event = tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        match event {
            InputEvent::MouseDown { button, x, y } => {
                assert_eq!(button, 1);
                assert_eq!(x, 250);
                assert_eq!(y, 400);
            }
            other => panic!("expected MouseDown, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn viewport_message_dispatches_to_resize_tx() {
        let mirror = primary_mirror(640, 480).await;
        let (tx, _input_rx, mut resize_rx, _h) = spawn_relay(mirror);

        let payload = br#"{"type":"viewport","width":1920,"height":1080}"#.to_vec();
        tx.send(payload).await.expect("send");

        let dims = tokio::time::timeout(Duration::from_secs(1), resize_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        assert_eq!(dims, (1920, 1080));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_json_is_skipped_then_relay_keeps_running() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay(mirror);

        tx.send(b"not json".to_vec()).await.expect("send");
        // A well-formed message after the bad one should still
        // be delivered.
        tx.send(br#"{"type":"key","scancode":1,"down":true}"#.to_vec())
            .await
            .expect("send");

        let event = tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        match event {
            InputEvent::KeyDown(sc) => assert_eq!(sc, 1),
            other => panic!("expected KeyDown(1), got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pointer_move_without_primary_is_dropped() {
        // Empty mirror: no primary surface yet.
        let mirror = Arc::new(Mutex::new(SurfaceMirror::new()));
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay(mirror);

        tx.send(br#"{"type":"pointer-move","x_norm":0.5,"y_norm":0.5}"#.to_vec())
            .await
            .expect("send");
        // Expect timeout: nothing should arrive.
        let r = tokio::time::timeout(Duration::from_millis(200), input_rx.recv()).await;
        assert!(r.is_err(), "pointer-move should be dropped when no primary");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_exits_when_control_tx_dropped() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, _input_rx, _resize_rx, handle) = spawn_relay(mirror);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("relay did not exit within 1 s")
            .expect("relay task panicked");
    }
}
