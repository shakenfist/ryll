//! Server → browser control-channel messages.
//!
//! Everything the server pushes to `app.js` travels as JSON over the
//! one control datachannel the bridge owns — cursor shapes and
//! positions from [`super::cursor`], and the negotiated mouse mode
//! from [`super::inputs`]. This module holds the pieces both need:
//! the message envelopes, an outbound queue, and the task that drains
//! it onto whichever bridge is currently installed.
//!
//! # Why a queue rather than a direct write
//!
//! The producers are a cursor relay and an input relay, neither of
//! which should have to know how a bridge is stored, hold the bridge
//! lock on its hot path, or be impossible to test without a live
//! peer connection. [`ControlSink`] is a plain `mpsc::Sender`, so a
//! test can hold the other end and read exactly what the browser
//! would have been sent.
//!
//! Sends are best-effort in both directions. There is no bridge at all
//! between viewers, and `WebrtcBridge::send_control` writes straight
//! to the datachannel with no buffering and no open-state tracking —
//! anything written before SCTP has opened the channel is simply lost.
//! State that the browser must not miss is therefore *pulled* by the
//! browser (see `BrowserMsg::Hello`) rather than pushed at a moment
//! the server guesses is safe.

use std::sync::Arc;

use serde::Serialize;
use shakenfist_spice_webrtc::WebrtcBridge;
use tokio::sync::{mpsc, Mutex};
use tracing::debug;

/// Depth of the outbound queue.
///
/// Cursor motion is the only producer that can burst, and a browser
/// that is behind by this many messages is better served by the
/// newest state than by a backlog — so a full queue drops rather than
/// parks the producer. Deep enough that an ordinary burst does not
/// reach it.
const CONTROL_QUEUE_DEPTH: usize = 64;

/// Sending half of the outbound control queue.
pub(crate) type ControlSink = mpsc::Sender<Vec<u8>>;

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

/// Create the outbound queue. The receiver is handed to
/// [`run_control_writer`]; the sender is cloned to every producer.
pub(crate) fn control_queue() -> (ControlSink, mpsc::Receiver<Vec<u8>>) {
    mpsc::channel(CONTROL_QUEUE_DEPTH)
}

/// Queue a pre-encoded JSON payload for the browser.
///
/// Drops the payload if the queue is full or the writer has exited,
/// logging at debug. Both mean the browser is either gone or too far
/// behind for this message to still be worth delivering.
pub(crate) fn send_to_bridge(sink: &ControlSink, payload: Vec<u8>) {
    if let Err(e) = sink.try_send(payload) {
        debug!("web control: dropping outbound message: {}", e);
    }
}

/// Serialise `msg` and queue it.
pub(crate) fn send_msg(sink: &ControlSink, msg: &ControlMsg) {
    match serde_json::to_vec(msg) {
        Ok(payload) => send_to_bridge(sink, payload),
        Err(e) => debug!("web control: failed to encode message: {}", e),
    }
}

/// Drain the outbound queue onto whichever bridge is installed.
///
/// A long-lived task, like the cursor relay and the mouse-mode
/// tracker: the queue outlives any one bridge, and a message written
/// while no browser is connected is dropped here rather than at the
/// producer, which does not have to know either way.
pub(crate) async fn run_control_writer(
    mut rx: mpsc::Receiver<Vec<u8>>,
    bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
) {
    while let Some(payload) = rx.recv().await {
        let guard = bridge_slot.lock().await;
        if let Some(bridge) = guard.as_ref() {
            if let Err(e) = bridge.send_control(&payload).await {
                debug!("web control: send_control failed: {}", e);
            }
        }
    }
    debug!("web control: writer exiting (queue closed)");
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
    async fn a_queued_message_is_readable_by_the_writer() {
        let (sink, mut rx) = control_queue();
        send_msg(&sink, &ControlMsg::MouseMode { mode: 1 });
        let payload = rx.recv().await.expect("queued");
        let v: Value = serde_json::from_slice(&payload).expect("parse");
        assert_eq!(v["mode"], 1);
    }

    /// A producer must never be parked or fail loudly because the
    /// browser went away — the cursor relay runs on the renderer's
    /// event path.
    #[tokio::test]
    async fn sending_with_no_writer_is_a_no_op() {
        let (sink, rx) = control_queue();
        drop(rx);
        send_msg(&sink, &ControlMsg::MouseMode { mode: 1 });
    }

    /// A full queue drops the newest message rather than blocking.
    #[tokio::test]
    async fn a_full_queue_drops_rather_than_parking_the_producer() {
        let (sink, _rx) = control_queue();
        for _ in 0..(CONTROL_QUEUE_DEPTH + 10) {
            send_msg(&sink, &ControlMsg::MouseMode { mode: 1 });
        }
        assert_eq!(
            sink.capacity(),
            0,
            "the queue should be full, proving the extra sends were dropped"
        );
    }

    /// With no bridge installed the writer drains and discards,
    /// rather than parking with a message it can never deliver.
    #[tokio::test]
    async fn the_writer_drains_when_no_browser_is_connected() {
        let (sink, rx) = control_queue();
        let slot = Arc::new(Mutex::new(None));
        let handle = tokio::spawn(run_control_writer(rx, slot));

        send_msg(&sink, &ControlMsg::MouseMode { mode: 1 });
        // Dropping the last sender ends the writer; if it had parked
        // on the message above this would time out.
        drop(sink);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("writer did not exit")
            .expect("writer panicked");
    }
}
