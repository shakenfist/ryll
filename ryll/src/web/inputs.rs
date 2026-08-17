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
//! surface dimensions to get the SPICE pixel coordinates the
//! renderer's [`InputsChannel`] expects.
//!
//! Which pointer message we then send depends on the negotiated
//! mouse mode, tracked by [`run_mouse_mode_tracker`]: absolute
//! positions in client mode, relative deltas in server mode. A
//! SPICE server discards the form it did not negotiate without
//! saying anything, so sending the wrong one presents as a dead
//! pointer rather than as an error.
//!
//! [`scancode_for_logical_key`]: shakenfist_spice_renderer
//! [`InputEvent::KeyDown`]: shakenfist_spice_renderer::InputEvent
//! [`KeyUp`]: shakenfist_spice_renderer::InputEvent::KeyUp
//! [`InputsChannel`]: shakenfist_spice_renderer

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use shakenfist_spice_protocol::MOUSE_MODE_SERVER;
use shakenfist_spice_renderer::{even_dimensions, ChannelEvent, InputEvent, SurfaceMirror};
use shakenfist_spice_webrtc::WebrtcBridge;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, info, warn};

use super::control::{send_msg, ControlMsg};

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

/// Track the SPICE session's mouse mode into a shared cell for
/// [`run_input_relay`] to read.
///
/// This is a long-lived task spawned once by `run_web`, not per
/// bridge, because the mode is session state: the server announces
/// it at session-init, seconds before any browser connects, and a
/// `broadcast::Receiver` created later would never see that
/// message. A per-offer subscription would therefore always start
/// out not knowing the mode.
pub async fn run_mouse_mode_tracker(
    mut event_rx: broadcast::Receiver<ChannelEvent>,
    mouse_mode: Arc<AtomicU32>,
    bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
) {
    loop {
        match event_rx.recv().await {
            Ok(ChannelEvent::MouseMode(mode)) => {
                mouse_mode.store(mode, Ordering::Relaxed);
                info!("web inputs: mouse mode is now {}", mode);
                // The browser draws the cursor differently per
                // mode, so it needs to know too. A browser that
                // connects after this point is caught up by
                // `post_offer`, which sends the current mode once
                // the bridge is installed.
                send_msg(&bridge_slot, &ControlMsg::MouseMode { mode }).await;
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // A dropped MouseMode would leave us sending the
                // wrong message type indefinitely, so say so.
                warn!(
                    "web inputs: mouse mode tracker lagged by {} events; \
                     mouse mode may be stale",
                    n
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("web inputs: mouse mode tracker exiting (broadcast closed)");
                return;
            }
        }
    }
}

/// Spawn-friendly relay. Loops until `control_rx` closes (i.e.
/// the bridge dropped its sender, normally because the data
/// channel went away). Bad JSON is logged at debug and
/// otherwise ignored; we never panic on browser-supplied input.
///
/// `mouse_mode` selects how pointer movement is delivered, and
/// getting it wrong is silent: see the `PointerMove` arm.
pub async fn run_input_relay(
    mut control_rx: mpsc::Receiver<Vec<u8>>,
    input_tx: mpsc::Sender<InputEvent>,
    resize_tx: mpsc::Sender<(u32, u32)>,
    surface_mirror: Arc<Mutex<SurfaceMirror>>,
    mouse_mode: Arc<AtomicU32>,
) {
    // Last pointer position in surface pixels, for deriving
    // relative deltas in server mouse mode.
    let mut last_pos: Option<(u32, u32)> = None;

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
                // Denormalise against the primary surface size so
                // the SPICE inputs channel sees pixel coordinates.
                // If there's no primary yet (browser sent input
                // before SPICE finished session-init) drop
                // the event silently.
                let size = {
                    let guard = surface_mirror.lock().await;
                    guard.primary_surface().map(|s| s.size())
                };
                let Some((w, h)) = size else { continue };
                let (x, y) = denormalise(x_norm, y_norm, w, h);

                // Which message the server will actually act on
                // depends on the mouse mode, and it ignores the
                // wrong one without complaint — see
                // `ryll/src/app.rs`, which makes the same choice
                // for the GUI.
                //
                // Client mode means the guest has a vdagent and
                // therefore an absolute pointing device, so
                // `MOUSE_POSITION` lands. Server mode means it
                // does not, and only relative `MOUSE_MOTION` is
                // consumed: sending absolute positions to a
                // server-mode session moves nothing at all, which
                // is what made this worth fixing rather than
                // documenting.
                let event = if mouse_mode.load(Ordering::Relaxed) == MOUSE_MODE_SERVER {
                    // First move after connect has no reference
                    // point; treat it as a zero delta rather than
                    // as a jump from the origin.
                    let (prev_x, prev_y) = last_pos.unwrap_or((x, y));
                    let dx = x as i32 - prev_x as i32;
                    let dy = y as i32 - prev_y as i32;
                    if dx == 0 && dy == 0 {
                        // Nothing to tell the guest, and sending it
                        // anyway is not free: every MOUSE_MOTION
                        // takes a slot in the ack window, which only
                        // drains on MOUSE_MOTION_ACK. The browser
                        // reports sub-pixel movement that denormalises
                        // to the same pixel, and the very first move
                        // is a deliberate zero, so this is a steady
                        // trickle rather than a rarity.
                        last_pos = Some((x, y));
                        continue;
                    }
                    InputEvent::MouseMotion { dx, dy }
                } else {
                    InputEvent::MouseMove { x, y }
                };
                last_pos = Some((x, y));

                if input_tx.send(event).await.is_err() {
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
                // Buttons carry coordinates too, so record them:
                // a click that arrives without a preceding move
                // would otherwise leave the next delta measured
                // from a stale position.
                last_pos = Some((x, y));
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
                // Round to even before asking the guest for the
                // mode. The browser reports `Math.round()` of a CSS
                // size, so odd values are ordinary, and nothing
                // between here and vdagent rounds — the guest would
                // grant exactly what it was asked for and the H.264
                // encoder cannot code an odd surface. Asking for a
                // size the encoder can use is cheaper than teaching
                // every downstream stage to cope with one it cannot.
                let (width, height) = even_dimensions(width, height);
                debug!("web inputs: viewport {}x{}", width, height);
                if width == 0 || height == 0 {
                    debug!("web inputs: ignoring degenerate viewport");
                    continue;
                }
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
    use shakenfist_spice_protocol::MOUSE_MODE_CLIENT;
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
        spawn_relay_in_mode(mirror, MOUSE_MODE_CLIENT)
    }

    #[allow(clippy::type_complexity)]
    fn spawn_relay_in_mode(
        mirror: Arc<Mutex<SurfaceMirror>>,
        mode: u32,
    ) -> (
        mpsc::Sender<Vec<u8>>,
        mpsc::Receiver<InputEvent>,
        mpsc::Receiver<(u32, u32)>,
        tokio::task::JoinHandle<()>,
    ) {
        let (control_tx, control_rx) = mpsc::channel::<Vec<u8>>(16);
        let (input_tx, input_rx) = mpsc::channel::<InputEvent>(16);
        let (resize_tx, resize_rx) = mpsc::channel::<(u32, u32)>(4);
        let handle = tokio::spawn(run_input_relay(
            control_rx,
            input_tx,
            resize_tx,
            mirror,
            Arc::new(AtomicU32::new(mode)),
        ));
        (control_tx, input_rx, resize_rx, handle)
    }

    async fn next_event(rx: &mut mpsc::Receiver<InputEvent>) -> InputEvent {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send")
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
    async fn pointer_move_in_server_mode_sends_relative_motion() {
        // A guest with no vdagent leaves the session in server
        // mouse mode, where the SPICE server consumes only
        // relative MOUSE_MOTION and silently ignores absolute
        // MOUSE_POSITION. Sending the absolute form there moves
        // the guest pointer not at all.
        let mirror = primary_mirror(1000, 800).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay_in_mode(mirror, MOUSE_MODE_SERVER);

        // First move establishes the reference point and sends
        // nothing: it has no previous position to measure against, so
        // its delta is zero, and a zero delta would burn an
        // ack-window slot to tell the guest to stay put.
        tx.send(br#"{"type":"pointer-move","x_norm":0.5,"y_norm":0.5}"#.to_vec())
            .await
            .expect("send");

        // Second move is a delta from the first: 0.5 -> 0.6 of
        // 1000 is +100, 0.5 -> 0.25 of 800 is -200.
        tx.send(br#"{"type":"pointer-move","x_norm":0.6,"y_norm":0.25}"#.to_vec())
            .await
            .expect("send");
        match next_event(&mut input_rx).await {
            InputEvent::MouseMotion { dx, dy } => {
                assert_eq!((dx, dy), (100, -200));
            }
            other => panic!("expected MouseMotion, got {:?}", other),
        }
    }

    /// A move that denormalises to the pixel the pointer is already
    /// on tells the guest nothing, and every `MouseMotion` costs a
    /// slot in an ack window that only drains on `MOUSE_MOTION_ACK`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_zero_delta_move_is_not_sent_in_server_mode() {
        let mirror = primary_mirror(1000, 800).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay_in_mode(mirror, MOUSE_MODE_SERVER);

        // Reference point, then two sub-pixel moves that land on it.
        for payload in [
            br#"{"type":"pointer-move","x_norm":0.5,"y_norm":0.5}"#.to_vec(),
            br#"{"type":"pointer-move","x_norm":0.5001,"y_norm":0.5001}"#.to_vec(),
            br#"{"type":"pointer-move","x_norm":0.4999,"y_norm":0.4999}"#.to_vec(),
        ] {
            tx.send(payload).await.expect("send");
        }
        // A real move behind them: it must be the *first* thing that
        // arrives, which is only true if the three above sent nothing.
        tx.send(br#"{"type":"pointer-move","x_norm":0.6,"y_norm":0.5}"#.to_vec())
            .await
            .expect("send");

        match next_event(&mut input_rx).await {
            InputEvent::MouseMotion { dx, dy } => assert_eq!(
                (dx, dy),
                (100, 0),
                "a zero-delta move was forwarded ahead of the real one"
            ),
            other => panic!("expected MouseMotion, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_button_position_becomes_the_next_delta_reference() {
        // A click carries coordinates. If it did not update the
        // reference point, a move after a click-without-move
        // would be measured from wherever the pointer last was.
        let mirror = primary_mirror(1000, 800).await;
        let (tx, mut input_rx, _resize_rx, _h) = spawn_relay_in_mode(mirror, MOUSE_MODE_SERVER);

        tx.send(
            br#"{"type":"pointer-button","button":1,"down":true,"x_norm":0.5,"y_norm":0.5}"#
                .to_vec(),
        )
        .await
        .expect("send");
        assert!(matches!(
            next_event(&mut input_rx).await,
            InputEvent::MouseDown { .. }
        ));

        tx.send(br#"{"type":"pointer-move","x_norm":0.6,"y_norm":0.5}"#.to_vec())
            .await
            .expect("send");
        match next_event(&mut input_rx).await {
            InputEvent::MouseMotion { dx, dy } => assert_eq!((dx, dy), (100, 0)),
            other => panic!("expected MouseMotion, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mouse_mode_tracker_records_the_latest_mode() {
        let (event_tx, event_rx) = broadcast::channel::<ChannelEvent>(8);
        let mode = Arc::new(AtomicU32::new(MOUSE_MODE_CLIENT));
        // No bridge: the tracker's send to the browser is a no-op,
        // which is the normal state before anyone connects.
        let slot = Arc::new(Mutex::new(None));
        let handle = tokio::spawn(run_mouse_mode_tracker(event_rx, mode.clone(), slot));

        event_tx
            .send(ChannelEvent::MouseMode(MOUSE_MODE_SERVER))
            .expect("send");

        // Poll briefly: the tracker is a separate task.
        for _ in 0..100 {
            if mode.load(Ordering::Relaxed) == MOUSE_MODE_SERVER {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mode.load(Ordering::Relaxed), MOUSE_MODE_SERVER);

        drop(event_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
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

    /// The browser reports `Math.round()` of a CSS size, so an odd
    /// viewport is ordinary rather than exotic. Nothing downstream
    /// rounds — vdagent asks the guest for exactly this, and the
    /// H.264 encoder cannot code an odd surface — so it has to
    /// happen here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_odd_viewport_is_rounded_before_the_guest_sees_it() {
        let mirror = primary_mirror(640, 480).await;
        let (tx, _input_rx, mut resize_rx, _h) = spawn_relay(mirror);

        let payload = br#"{"type":"viewport","width":1367,"height":769}"#.to_vec();
        tx.send(payload).await.expect("send");

        let dims = tokio::time::timeout(Duration::from_secs(1), resize_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        assert_eq!(dims, (1366, 768));
    }

    /// A viewport that rounds to zero would ask the guest for a mode
    /// it cannot set and the encoder for a size it rejects.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_degenerate_viewport_is_dropped() {
        let mirror = primary_mirror(640, 480).await;
        let (tx, _input_rx, mut resize_rx, _h) = spawn_relay(mirror);

        tx.send(br#"{"type":"viewport","width":1,"height":768}"#.to_vec())
            .await
            .expect("send");
        // A good message behind it proves the relay kept running.
        tx.send(br#"{"type":"viewport","width":800,"height":600}"#.to_vec())
            .await
            .expect("send");

        let dims = tokio::time::timeout(Duration::from_secs(1), resize_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        assert_eq!(dims, (800, 600), "the degenerate viewport was forwarded");
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
