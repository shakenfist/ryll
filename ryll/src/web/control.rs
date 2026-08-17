//! Server → browser control-channel messages.
//!
//! Everything the server pushes to `app.js` travels as JSON over
//! the one control datachannel the bridge owns — cursor shapes and
//! positions from [`super::cursor`], and the negotiated mouse mode
//! from [`super::inputs`]. This module holds the pieces both need:
//! the send helper, and the mouse-mode envelope.
//!
//! Sends are best-effort. There is no bridge between viewers, and a
//! browser that misses a message gets the next one — the state
//! these messages carry is re-sent on the next change, and on every
//! new bridge.

use std::sync::Arc;

use serde::Serialize;
use shakenfist_spice_webrtc::WebrtcBridge;
use tokio::sync::Mutex;
use tracing::debug;

/// Tell the browser which mouse mode the SPICE session negotiated.
///
/// The browser needs this for the same reason the GUI does: in
/// client mode the viewer owns the pointer position and should draw
/// the cursor where the user's pointer actually is, while in server
/// mode the guest is authoritative and the only truthful position
/// is the one the cursor channel reports. See the equivalent
/// branch in `ryll/src/app.rs`.
#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum ControlMsg {
    #[serde(rename = "mouse-mode")]
    MouseMode { mode: u32 },
}

/// Send a pre-encoded JSON payload to whichever bridge is currently
/// in the slot, if any.
///
/// Failures are logged at debug and dropped: with no browser
/// connected there is no bridge, which is the normal state rather
/// than an error.
pub(crate) async fn send_to_bridge(slot: &Arc<Mutex<Option<WebrtcBridge>>>, payload: &[u8]) {
    let guard = slot.lock().await;
    if let Some(bridge) = guard.as_ref() {
        if let Err(e) = bridge.send_control(payload).await {
            debug!("web control: send_control failed: {}", e);
        }
    }
}

/// Serialise `msg` and send it to the active bridge.
pub(crate) async fn send_msg(slot: &Arc<Mutex<Option<WebrtcBridge>>>, msg: &ControlMsg) {
    match serde_json::to_vec(msg) {
        Ok(payload) => send_to_bridge(slot, &payload).await,
        Err(e) => debug!("web control: failed to encode message: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn mouse_mode_serialises_to_the_shape_app_js_parses() {
        let payload = serde_json::to_vec(&ControlMsg::MouseMode { mode: 2 }).expect("encode");
        let v: Value = serde_json::from_slice(&payload).expect("parse");
        assert_eq!(v["type"], "mouse-mode");
        assert_eq!(v["mode"], 2);
    }

    #[tokio::test]
    async fn sending_with_no_bridge_is_a_no_op() {
        let slot = Arc::new(Mutex::new(None));
        // Must not panic: no browser connected is the normal state.
        send_msg(&slot, &ControlMsg::MouseMode { mode: 1 }).await;
    }
}
