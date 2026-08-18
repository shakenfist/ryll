//! Visual-digest polling task for the `digest-decode` feature.
//!
//! The task wakes every [`POLL_INTERVAL`], reads the primary
//! surface's RGBA framebuffer, runs the QR decoder, parses the
//! digest payload, and -- when the `frame_counter` differs from
//! the last observed value -- broadcasts a
//! [`ChannelEvent::DigestUpdated`].
//!
//! Off in production builds: gated by
//! `#[cfg(feature = "digest-decode")]` end to end so the
//! shakenfist-visual-digest dep, the rqrr QR backend, and the
//! `image` decode path do not land in a default ryll.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

use crate::channels::ChannelEvent;
use crate::surface_mirror::SurfaceMirror;

/// How often the polling task wakes to check the primary surface
/// for a new digest.  100 ms is a deliberate compromise: short
/// enough that a Sextant phase transition is observed inside
/// half a frame, long enough that the task does not dominate CPU
/// time when no digest is present.  Tunable later behind a CLI
/// flag if that is ever needed.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Run the polling task until `cancel` flips to true.
///
/// `surface_mirror` is the broadcast bus consumer's mirror of the
/// SPICE display channel's primary surface.  `event_tx` is the
/// broadcast sender the control server's event translator
/// subscribes to.
pub async fn run_digest_poller(
    surface_mirror: Arc<Mutex<SurfaceMirror>>,
    event_tx: broadcast::Sender<ChannelEvent>,
    cancel: Arc<AtomicBool>,
) {
    info!(
        "digest: polling task started (interval = {:?})",
        POLL_INTERVAL
    );
    let mut last_frame_counter: Option<u32> = None;
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let Some((width, height, rgba)) = ({
            let mirror = surface_mirror.lock().await;
            mirror.primary_surface().map(|s| {
                // Clone the pixel buffer so we can release the
                // mirror lock before doing the expensive QR
                // decode.  100 ms cadence + a single guest of
                // bounded resolution makes the copy cost
                // negligible compared to the rqrr scan.
                (s.width, s.height, s.pixels().to_vec())
            })
        }) else {
            continue;
        };

        let Some(payload) = shakenfist_visual_digest::decode_qr_rgba(&rgba, width, height) else {
            // No QR detected.  Common during boot / scene
            // transitions; debug, not warn.
            debug!(
                "digest: no QR detected this tick (surface {}x{})",
                width, height
            );
            continue;
        };

        let digest = match shakenfist_visual_digest::decode(&payload) {
            Ok(d) => d,
            Err(e) => {
                warn!("digest: payload decode failed: {}", e);
                continue;
            }
        };

        if Some(digest.frame_counter) == last_frame_counter {
            continue;
        }
        last_frame_counter = Some(digest.frame_counter);

        // Serialise the raw_records list into a generic JSON
        // value so the wire shape does not have to leak the
        // digest crate's `Event` type out of this module.  The
        // digest crate's `serde` feature derives Serialize on
        // `Event` with `rename_all = "snake_case"`.
        let events_json = match serde_json::to_value(&digest.raw_records) {
            Ok(v) => v,
            Err(e) => {
                warn!("digest: events serialisation failed: {}", e);
                serde_json::Value::Array(Vec::new())
            }
        };

        debug!(
            "digest: new payload (frame_counter={}, framebuffer_hash={:08x}, {} records)",
            digest.frame_counter,
            digest.framebuffer_hash,
            digest.raw_records.len(),
        );

        // send() fails only when no receivers exist.  At
        // session-tear-down time that is expected; debug-log
        // and move on rather than treating it as an error.
        if event_tx
            .send(ChannelEvent::DigestUpdated {
                frame_counter: digest.frame_counter,
                framebuffer_hash: digest.framebuffer_hash,
                events: events_json,
            })
            .is_err()
        {
            debug!("digest: broadcast send failed (no subscribers); stopping");
            break;
        }
    }

    info!("digest: polling task stopped");
}
