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
use shakenfist_spice_renderer::{EncoderControl, InputEvent, SurfaceMirror};
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

    /// Browser's smoothed bandwidth estimate, derived from
    /// `RTCPeerConnection.getStats().availableOutgoingBitrate`
    /// with an EMA filter applied on the browser side (see
    /// `sampleBandwidth` in `app.js`). We forward this as
    /// [`EncoderControl::SetBitrate`] so the encoder task can
    /// adapt its output bitrate to network conditions.
    #[serde(rename = "bandwidth")]
    Bandwidth { kbps: u32 },
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
    encoder_control: mpsc::Sender<EncoderControl>,
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
                let (snap_w, snap_h) = snap_viewport_to_standard_mode(width, height);
                if (snap_w, snap_h) == (width, height) {
                    debug!("web inputs: viewport {}x{}", width, height);
                } else {
                    debug!(
                        "web inputs: viewport {}x{} snapped to {}x{}",
                        width, height, snap_w, snap_h,
                    );
                }
                if resize_tx.send((snap_w, snap_h)).await.is_err() {
                    warn!("web inputs: resize_tx receiver dropped");
                }
            }

            BrowserMsg::Bandwidth { kbps } => {
                debug!("web inputs: bandwidth estimate {} kbps", kbps);
                if encoder_control
                    .send(EncoderControl::SetBitrate(kbps))
                    .await
                    .is_err()
                {
                    // The encoder task may have exited (e.g. the next
                    // /offer restart will create a fresh one). This is
                    // transient and non-fatal — the relay continues
                    // so subsequent input events still reach the renderer.
                    debug!("web inputs: encoder_control send failed; encoder may have exited");
                }
            }
        }
    }
    debug!("web inputs: control_rx closed; relay exiting");
}

/// Common display modes a Wayland GDM / mutter guest is likely
/// to accept via `VDAgentMonitorsConfig`.
///
/// Background: virtio-gpu (without virgl) advertises a fixed
/// canned mode list in its EDID. Modern mutter on Wayland only
/// honours resolution-change requests whose dimensions match
/// one of those modes; arbitrary client-window dimensions
/// silently no-op. The X11 path used to work around this via
/// `xrandr --newmode`+`--addmode`, but vdagent on Wayland
/// can't fabricate modes.
///
/// The list below is the intersection of (a) the canned EDID
/// modes QEMU's virtio-gpu commonly exposes and (b) the modes
/// well-known Wayland compositors enumerate by default. Ordered
/// by ascending pixel count for deterministic tie-breaking when
/// two modes are equidistant from the request.
const STANDARD_MODES: &[(u32, u32)] = &[
    (640, 480),
    (800, 600),
    (1024, 768),
    (1152, 864),
    (1280, 720),
    (1280, 800),
    (1280, 1024),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1600, 1200),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2048, 1152),
    (2560, 1440),
    (2560, 1600),
    (3840, 2160),
];

/// Snap a browser-driven viewport request to the nearest mode
/// the guest is likely to accept. See [`STANDARD_MODES`] for
/// the candidate list and the rationale.
///
/// "Nearest" is Euclidean distance in pixel space. If the
/// request exactly matches a standard mode (the common case
/// when the operator drags the window to a familiar size) the
/// pass-through is free. The crude metric biases toward modes
/// of similar overall size rather than similar aspect, which
/// in practice gives sensible results — a 2108x1267 request
/// (close to 5:3) lands on 2048x1152 (16:9), which mutter
/// accepts and the browser then scales/letterboxes to fill
/// the actual window.
pub(crate) fn snap_viewport_to_standard_mode(width: u32, height: u32) -> (u32, u32) {
    let mut best = STANDARD_MODES[0];
    let mut best_dist = u64::MAX;
    for &(w, h) in STANDARD_MODES {
        if w == width && h == height {
            return (w, h);
        }
        let dw = (w as i64 - width as i64).unsigned_abs();
        let dh = (h as i64 - height as i64).unsigned_abs();
        let dist = dw.saturating_mul(dw).saturating_add(dh.saturating_mul(dh));
        if dist < best_dist {
            best_dist = dist;
            best = (w, h);
        }
    }
    best
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
    /// the test asserts against, the encoder-control receiver,
    /// and the relay's join handle.
    ///
    /// Pass `Some(tx)` to supply a custom encoder-control sender
    /// (e.g. to test send-failure by pre-dropping the receiver);
    /// pass `None` to get an automatically-created pair.
    #[allow(clippy::type_complexity)]
    fn spawn_relay(
        mirror: Arc<Mutex<SurfaceMirror>>,
        encoder_control_tx: Option<mpsc::Sender<EncoderControl>>,
    ) -> (
        mpsc::Sender<Vec<u8>>,
        mpsc::Receiver<InputEvent>,
        mpsc::Receiver<(u32, u32)>,
        mpsc::Receiver<EncoderControl>,
        tokio::task::JoinHandle<()>,
    ) {
        let (control_tx, control_rx) = mpsc::channel::<Vec<u8>>(16);
        let (input_tx, input_rx) = mpsc::channel::<InputEvent>(16);
        let (resize_tx, resize_rx) = mpsc::channel::<(u32, u32)>(4);
        let (enc_tx, enc_rx) = mpsc::channel::<EncoderControl>(8);
        let enc_tx = encoder_control_tx.unwrap_or(enc_tx);
        let handle = tokio::spawn(run_input_relay(
            control_rx, input_tx, resize_tx, mirror, enc_tx,
        ));
        (control_tx, input_rx, resize_rx, enc_rx, handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn key_down_message_dispatches_keydown_event() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, mut input_rx, _resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

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
        let (tx, mut input_rx, _resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

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
        let (tx, mut input_rx, _resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

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
        let (tx, mut input_rx, _resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

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
        let (tx, _input_rx, mut resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

        let payload = br#"{"type":"viewport","width":1920,"height":1080}"#.to_vec();
        tx.send(payload).await.expect("send");

        let dims = tokio::time::timeout(Duration::from_secs(1), resize_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        assert_eq!(dims, (1920, 1080));
    }

    /// A viewport message at a non-standard size must arrive at
    /// `resize_tx` snapped to the nearest standard mode. Without
    /// the snap the guest's Wayland compositor silently drops the
    /// request because the dims don't match the EDID mode list.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn viewport_message_snaps_non_standard_dims() {
        let mirror = primary_mirror(640, 480).await;
        let (tx, _input_rx, mut resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

        // 2108x1267 is one of the actual dims observed in
        // test-session-008f. Closest standard mode by
        // Euclidean distance is 2048x1152.
        let payload = br#"{"type":"viewport","width":2108,"height":1267}"#.to_vec();
        tx.send(payload).await.expect("send");

        let dims = tokio::time::timeout(Duration::from_secs(1), resize_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should send");
        assert_eq!(dims, (2048, 1152));
    }

    #[test]
    fn snap_passes_through_exact_match() {
        assert_eq!(snap_viewport_to_standard_mode(1920, 1080), (1920, 1080));
        assert_eq!(snap_viewport_to_standard_mode(1280, 800), (1280, 800));
    }

    /// The very-small case must not panic and must land on the
    /// smallest standard mode (640x480). A future regression that
    /// underflowed the distance calc would surface here.
    #[test]
    fn snap_handles_below_smallest_mode() {
        assert_eq!(snap_viewport_to_standard_mode(100, 100), (640, 480));
    }

    /// The above-largest case lands on the largest standard mode
    /// (3840x2160). A future regression that overflowed the
    /// squared-distance arithmetic would surface here.
    #[test]
    fn snap_handles_above_largest_mode() {
        assert_eq!(snap_viewport_to_standard_mode(7680, 4320), (3840, 2160));
    }

    /// Real dims observed in test-session-008f. Each one mutter
    /// silently rejected; the snap should produce a mode mutter
    /// will accept.
    #[test]
    fn snap_matches_008f_observed_dims() {
        assert_eq!(snap_viewport_to_standard_mode(2108, 1267), (2048, 1152));
        assert_eq!(snap_viewport_to_standard_mode(2150, 511), (1920, 1080));
        assert_eq!(snap_viewport_to_standard_mode(1742, 1208), (1600, 1200));
        assert_eq!(snap_viewport_to_standard_mode(1544, 1325), (1600, 1200));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_json_is_skipped_then_relay_keeps_running() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, mut input_rx, _resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

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
        let (tx, mut input_rx, _resize_rx, _enc_rx, _h) = spawn_relay(mirror, None);

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
        let (tx, _input_rx, _resize_rx, _enc_rx, handle) = spawn_relay(mirror, None);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("relay did not exit within 1 s")
            .expect("relay task panicked");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bandwidth_message_dispatches_to_encoder_control() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, _input_rx, _resize_rx, mut enc_rx, _h) = spawn_relay(mirror, None);

        let payload = br#"{"type":"bandwidth","kbps":7500}"#.to_vec();
        tx.send(payload).await.expect("send");

        let ctrl = tokio::time::timeout(Duration::from_secs(1), enc_rx.recv())
            .await
            .expect("timeout")
            .expect("encoder_control should receive");
        match ctrl {
            EncoderControl::SetBitrate(kbps) => assert_eq!(kbps, 7500),
            other => panic!("expected SetBitrate(7500), got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bandwidth_message_with_missing_kbps_is_ignored() {
        let mirror = primary_mirror(1920, 1080).await;
        let (tx, mut input_rx, _resize_rx, mut enc_rx, _h) = spawn_relay(mirror, None);

        // Missing required field: should parse-fail and be skipped.
        tx.send(br#"{"type":"bandwidth"}"#.to_vec())
            .await
            .expect("send");

        // Nothing should arrive on encoder_control.
        let r = tokio::time::timeout(Duration::from_millis(200), enc_rx.recv()).await;
        assert!(r.is_err(), "bandwidth with missing kbps should be dropped");

        // Relay must still be running: a subsequent valid key message
        // should still dispatch normally.
        tx.send(br#"{"type":"key","scancode":30,"down":true}"#.to_vec())
            .await
            .expect("send");
        let event = tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should continue after bad bandwidth msg");
        match event {
            InputEvent::KeyDown(sc) => assert_eq!(sc, 0x1E),
            other => panic!("expected KeyDown(0x1E), got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bandwidth_message_encoder_control_send_failure_does_not_kill_relay() {
        let mirror = primary_mirror(1920, 1080).await;

        // Create an encoder_control channel and immediately drop the
        // receiver so any send will fail.
        let (enc_tx, enc_rx) = mpsc::channel::<EncoderControl>(8);
        drop(enc_rx);

        let (tx, mut input_rx, _resize_rx, _enc_rx, _h) = spawn_relay(mirror, Some(enc_tx));

        // Send a bandwidth message — encoder_control.send() will fail
        // because the receiver is dropped, but the relay must not exit.
        tx.send(br#"{"type":"bandwidth","kbps":5000}"#.to_vec())
            .await
            .expect("send");

        // Send a key message immediately after; it must still arrive.
        tx.send(br#"{"type":"key","scancode":1,"down":true}"#.to_vec())
            .await
            .expect("send");

        let event = tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
            .await
            .expect("timeout")
            .expect("relay should still run after encoder_control send failure");
        match event {
            InputEvent::KeyDown(sc) => assert_eq!(sc, 1),
            other => panic!("expected KeyDown(1), got {:?}", other),
        }
    }
}
