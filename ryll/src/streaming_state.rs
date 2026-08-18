//! Derived state for the live streaming status-bar indicator
//! and the flap-notification heuristic.
//!
//! Reads the per-stream snapshot fields
//! (`streams_active`, `streams_recently_destroyed`) and
//! produces a single `StreamingState` value plus an optional
//! `NotificationToFire` for the caller to feed into
//! `push_notification`. The classifier is pure / synchronous;
//! all the heuristic constants live here so they can be tuned
//! in one place after field experience.
//!
//! The shape (`&DisplaySnapshot`, `now`, `session_start`,
//! `last_flap_notification` → `(StreamingState, Option<…>)`)
//! lets the caller own the cool-down timestamp on the
//! `RyllApp` and avoids any shared mutable state inside this
//! module.

use std::time::{Duration, Instant};

use shakenfist_spice_renderer::snapshots::{DisplaySnapshot, StreamSnapshot};

/// Window for "stream was just torn down": if the most recent
/// destroy is within this many seconds of `now`, the indicator
/// sits in `RecentlyDestroyed` (amber) rather than `Off` (grey).
/// Five seconds matches the operator-visible "did a stream just
/// die?" intuition without lingering long enough to mask a
/// genuine off-state.
pub const RECENTLY_DESTROYED_WINDOW: Duration = Duration::from_secs(5);

/// Window over which destroyed-stream counts are accumulated
/// when classifying as `Flapping`.
pub const FLAP_WINDOW: Duration = Duration::from_secs(30);

/// Minimum number of destroys-in-`FLAP_WINDOW` for the flap
/// heuristic to fire.
pub const FLAP_MIN_DESTROYS: usize = 3;

/// Mean lifetime threshold (over the destroys inside the
/// window) for the flap heuristic to fire. Streams whose mean
/// lifetime is at or above this value are not flapping in any
/// useful sense.
pub const FLAP_MEAN_LIFETIME_MAX: Duration = Duration::from_secs(3);

/// Cool-down between flap notifications. One notification per
/// minute is enough to alert the operator without spamming the
/// notifications panel when the flap pattern persists.
pub const FLAP_NOTIFY_COOLDOWN: Duration = Duration::from_secs(60);

/// Live classification of the display channel's video-stream
/// state, as seen by the status-bar indicator.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamingState {
    /// No active streams and no destroy in the last
    /// `RECENTLY_DESTROYED_WINDOW`. Grey indicator.
    Off,
    /// One or more streams currently open. Green indicator.
    Active,
    /// No active streams, but at least one was destroyed
    /// within the last `RECENTLY_DESTROYED_WINDOW`. Amber
    /// indicator. `secs_since` is the time in seconds between
    /// the most recent destroy and `now`.
    RecentlyDestroyed { secs_since: f64 },
    /// Flap heuristic fired: at least `FLAP_MIN_DESTROYS`
    /// destroys in the last `FLAP_WINDOW` with mean lifetime
    /// below `FLAP_MEAN_LIFETIME_MAX`. Red indicator.
    /// `destroys_in_window`, `window_secs`, and `mean_lifetime_secs`
    /// are surfaced for the tooltip so the operator can see
    /// the numbers that triggered the classification.
    Flapping {
        destroys_in_window: usize,
        window_secs: f64,
        mean_lifetime_secs: f64,
    },
}

/// Notification payload the caller should hand to
/// `push_notification(NotifySeverity::Warn,
/// NotificationSource::Internal, …)`. The classifier returns
/// `Some(NotificationToFire { … })` only when the cool-down
/// permits.
#[derive(Debug, Clone)]
pub struct NotificationToFire {
    pub message: String,
}

/// Convert a session-relative timestamp (seconds since
/// `session_start`) to an absolute `Instant`.
fn instant_from_secs(session_start: Instant, secs: f64) -> Instant {
    // f64 → Duration: saturating handles negative or NaN
    // payloads cleanly (treat them as "at session start").
    let dur = if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    };
    session_start + dur
}

/// Return the entries in `streams_recently_destroyed` whose
/// `destroyed_at_secs` falls within `window` of `now`. Each
/// returned entry carries its destroyed-at `Instant` so the
/// caller can subtract from `now` without redoing the
/// conversion.
fn destroys_in_window<'a>(
    streams_recently_destroyed: impl Iterator<Item = &'a StreamSnapshot>,
    now: Instant,
    session_start: Instant,
    window: Duration,
) -> Vec<(&'a StreamSnapshot, Instant)> {
    let cutoff = now.checked_sub(window).unwrap_or(now);
    streams_recently_destroyed
        .filter_map(|s| {
            let destroyed_at = s.destroyed_at_secs?;
            let inst = instant_from_secs(session_start, destroyed_at);
            // Strictly within the window: inst > cutoff and
            // inst <= now. A destroy at exactly `cutoff` is
            // outside (the "exactly 30 s ago" boundary case).
            if inst > cutoff && inst <= now {
                Some((s, inst))
            } else {
                None
            }
        })
        .collect()
}

/// Classify the display channel's streaming state and decide
/// whether a flap notification should fire now.
///
/// `now` is the wall clock at the moment of classification
/// (typically `Instant::now()` in `RyllApp::update`).
///
/// `session_start` is the `Instant` to which the snapshot's
/// `*_at_secs: f64` fields are relative. In ryll this is the
/// per-connection `TrafficBuffers::start` value.
///
/// `last_flap_notification` is the most recent time the caller
/// fired a flap notification, or `None` if none has fired this
/// session. When the heuristic fires repeatedly, the caller
/// only sees a `NotificationToFire` once per
/// `FLAP_NOTIFY_COOLDOWN`.
pub fn classify(
    snapshot: &DisplaySnapshot,
    now: Instant,
    session_start: Instant,
    last_flap_notification: Option<Instant>,
) -> (StreamingState, Option<NotificationToFire>) {
    let recent = destroys_in_window(
        snapshot.streams_recently_destroyed.iter(),
        now,
        session_start,
        FLAP_WINDOW,
    );

    // Flap check fires whenever the heuristic is satisfied,
    // independent of whether any streams are currently active
    // (a churning stream could be in the brief "active again"
    // phase between destroys). The plan's wording prioritises
    // Flapping over the other states because it carries the
    // most actionable signal.
    let mean_lifetime = mean_lifetime_secs(&recent);
    let flapping = recent.len() >= FLAP_MIN_DESTROYS
        && mean_lifetime
            .map(|m| m < FLAP_MEAN_LIFETIME_MAX.as_secs_f64())
            .unwrap_or(false);

    if flapping {
        let notification =
            should_fire_notification(now, last_flap_notification).then(|| NotificationToFire {
                message: format!(
                    "Video stream flapping: {} destroys in {:.0} s, \
                     mean lifetime {:.1} s",
                    recent.len(),
                    FLAP_WINDOW.as_secs_f64(),
                    mean_lifetime.unwrap_or(0.0),
                ),
            });
        return (
            StreamingState::Flapping {
                destroys_in_window: recent.len(),
                window_secs: FLAP_WINDOW.as_secs_f64(),
                mean_lifetime_secs: mean_lifetime.unwrap_or(0.0),
            },
            notification,
        );
    }

    if !snapshot.streams_active.is_empty() {
        return (StreamingState::Active, None);
    }

    // No active streams: amber if a destroy is in the
    // RECENTLY_DESTROYED_WINDOW, grey otherwise.
    let recent_short = destroys_in_window(
        snapshot.streams_recently_destroyed.iter(),
        now,
        session_start,
        RECENTLY_DESTROYED_WINDOW,
    );
    if let Some((_, latest)) = recent_short.iter().max_by_key(|(_, inst)| *inst) {
        let secs_since = now.saturating_duration_since(*latest).as_secs_f64();
        return (StreamingState::RecentlyDestroyed { secs_since }, None);
    }

    (StreamingState::Off, None)
}

/// Compute the mean lifetime in seconds for the destroyed
/// streams in `recent`. Returns `None` for an empty slice or
/// when none of the entries carry both a `created_at_secs` and
/// a `destroyed_at_secs`. Entries with non-positive lifetimes
/// (clock skew, snapshot reordering) are clamped to zero so
/// they pull the mean down rather than inflating it.
fn mean_lifetime_secs(recent: &[(&StreamSnapshot, Instant)]) -> Option<f64> {
    if recent.is_empty() {
        return None;
    }
    let (sum, count) = recent
        .iter()
        .fold((0.0_f64, 0_usize), |(sum, count), (s, _)| {
            let destroyed = match s.destroyed_at_secs {
                Some(d) => d,
                None => return (sum, count),
            };
            let lifetime = (destroyed - s.created_at_secs).max(0.0);
            (sum + lifetime, count + 1)
        });
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
}

/// True when the cool-down permits firing a flap notification
/// now. `None` means "never fired before" — fire immediately.
fn should_fire_notification(now: Instant, last: Option<Instant>) -> bool {
    match last {
        None => true,
        Some(prev) => now.saturating_duration_since(prev) >= FLAP_NOTIFY_COOLDOWN,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn base_session() -> (Instant, Instant) {
        // Session started at `start`; "now" is 100 s later. The
        // 100 s offset gives plenty of room for tests to place
        // destroys at any time-since-session-start without
        // tripping `instant_from_secs`'s saturating branch.
        let start = Instant::now();
        let now = start + Duration::from_secs(100);
        (start, now)
    }

    fn stream(created_at: f64, destroyed_at: Option<f64>) -> StreamSnapshot {
        StreamSnapshot {
            stream_id: 1,
            surface_id: 0,
            codec_type: 1,
            stream_width: 800,
            stream_height: 600,
            dest_top: 0,
            dest_left: 0,
            dest_bottom: 600,
            dest_right: 800,
            created_at_secs: created_at,
            frames_received: 0,
            frames_decoded_ok: 0,
            frames_decode_failed: 0,
            last_frame_ts_secs: None,
            last_decode_ok_ts_secs: None,
            last_decode_duration_us: 0,
            destroyed_at_secs: destroyed_at,
            report_is_active: false,
            report_unique_id: 0,
            report_max_window_size: 0,
            report_timeout_ms: 0,
            report_send_count: 0,
            last_report_sent_ts_secs: None,
            last_report_num_frames: 0,
            last_report_num_drops: 0,
            last_report_last_frame_delay: 0,
            mjpeg_decoder_backend: String::new(),
            video_decoder_backend: String::new(),
        }
    }

    fn snapshot(active: Vec<StreamSnapshot>, destroyed: Vec<StreamSnapshot>) -> DisplaySnapshot {
        DisplaySnapshot {
            streams_active: active,
            streams_recently_destroyed: VecDeque::from(destroyed),
            ..Default::default()
        }
    }

    #[test]
    fn off_state_when_no_streams_and_no_recent_destroys() {
        let (start, now) = base_session();
        let snap = snapshot(vec![], vec![]);
        let (state, notif) = classify(&snap, now, start, None);
        assert_eq!(state, StreamingState::Off);
        assert!(notif.is_none());
    }

    #[test]
    fn off_state_when_destroy_is_outside_5s_window() {
        // Session is 100 s old; destroy was at +90 s, i.e. 10 s ago.
        let (start, now) = base_session();
        let snap = snapshot(vec![], vec![stream(85.0, Some(90.0))]);
        let (state, notif) = classify(&snap, now, start, None);
        assert_eq!(state, StreamingState::Off);
        assert!(notif.is_none());
    }

    #[test]
    fn active_state_when_streams_active_non_empty() {
        let (start, now) = base_session();
        let snap = snapshot(vec![stream(95.0, None)], vec![]);
        let (state, notif) = classify(&snap, now, start, None);
        assert_eq!(state, StreamingState::Active);
        assert!(notif.is_none());
    }

    #[test]
    fn recently_destroyed_state_within_5s() {
        // Destroy at +97 s; now is 100 s. secs_since = 3.0.
        let (start, now) = base_session();
        let snap = snapshot(vec![], vec![stream(90.0, Some(97.0))]);
        let (state, notif) = classify(&snap, now, start, None);
        match state {
            StreamingState::RecentlyDestroyed { secs_since } => {
                assert!((secs_since - 3.0).abs() < 1e-6, "secs_since={}", secs_since);
            }
            other => panic!("expected RecentlyDestroyed, got {:?}", other),
        }
        assert!(notif.is_none());
    }

    #[test]
    fn recently_destroyed_picks_most_recent_destroy() {
        let (start, now) = base_session();
        let snap = snapshot(
            vec![],
            vec![
                stream(80.0, Some(96.0)), // 4 s ago
                stream(85.0, Some(98.5)), // 1.5 s ago — should win
            ],
        );
        let (state, _) = classify(&snap, now, start, None);
        match state {
            StreamingState::RecentlyDestroyed { secs_since } => {
                assert!((secs_since - 1.5).abs() < 1e-6, "secs_since={}", secs_since);
            }
            other => panic!("expected RecentlyDestroyed, got {:?}", other),
        }
    }

    #[test]
    fn flapping_state_three_destroys_mean_below_3s() {
        // Three destroys at +90, +95, +100 — all inside the
        // 30 s window. Lifetimes 2.0, 2.0, 2.0 → mean 2.0 < 3.0.
        let (start, now) = base_session();
        let snap = snapshot(
            vec![],
            vec![
                stream(88.0, Some(90.0)),
                stream(93.0, Some(95.0)),
                stream(98.0, Some(100.0)),
            ],
        );
        let (state, notif) = classify(&snap, now, start, None);
        match state {
            StreamingState::Flapping {
                destroys_in_window,
                window_secs,
                mean_lifetime_secs,
            } => {
                assert_eq!(destroys_in_window, 3);
                assert!((window_secs - 30.0).abs() < 1e-6);
                assert!((mean_lifetime_secs - 2.0).abs() < 1e-6);
            }
            other => panic!("expected Flapping, got {:?}", other),
        }
        // First call with no prior notification → fires.
        assert!(notif.is_some());
    }

    #[test]
    fn flapping_boundary_exactly_3_destroys_mean_just_under_3s() {
        // Lifetimes 2.9, 2.9, 2.9 → mean 2.9 < 3.0 → flap.
        let (start, now) = base_session();
        let snap = snapshot(
            vec![],
            vec![
                stream(87.1, Some(90.0)),
                stream(92.1, Some(95.0)),
                stream(97.1, Some(100.0)),
            ],
        );
        let (state, _) = classify(&snap, now, start, None);
        assert!(matches!(state, StreamingState::Flapping { .. }));
    }

    #[test]
    fn not_flapping_when_mean_lifetime_at_threshold() {
        // Mean lifetime exactly 3.0 s — the heuristic is
        // strictly less than (<), so this should NOT flap.
        // With no other recent destroys it falls through to
        // RecentlyDestroyed (most recent destroy was 0 s ago).
        let (start, now) = base_session();
        let snap = snapshot(
            vec![],
            vec![
                stream(87.0, Some(90.0)),
                stream(92.0, Some(95.0)),
                stream(97.0, Some(100.0)),
            ],
        );
        let (state, notif) = classify(&snap, now, start, None);
        assert!(
            !matches!(state, StreamingState::Flapping { .. }),
            "got {:?}",
            state
        );
        assert!(notif.is_none());
    }

    #[test]
    fn not_flapping_when_only_two_destroys_in_window() {
        let (start, now) = base_session();
        let snap = snapshot(
            vec![],
            vec![stream(88.0, Some(90.0)), stream(98.0, Some(100.0))],
        );
        let (state, _) = classify(&snap, now, start, None);
        assert!(!matches!(state, StreamingState::Flapping { .. }));
    }

    #[test]
    fn flap_destroy_at_exact_30s_boundary_excluded() {
        // Destroy at session-relative 70.0 → 30.0 s before now.
        // The cutoff at `now - 30 s` is exclusive, so this entry
        // must NOT be counted. Two destroys remain inside → no flap.
        let (start, now) = base_session();
        let snap = snapshot(
            vec![],
            vec![
                stream(68.0, Some(70.0)),  // exactly 30 s ago — out
                stream(88.0, Some(90.0)),  // 10 s ago — in
                stream(98.0, Some(100.0)), // 0 s ago — in
            ],
        );
        let (state, _) = classify(&snap, now, start, None);
        assert!(
            !matches!(state, StreamingState::Flapping { .. }),
            "destroy at exactly 30 s ago must be excluded; got {:?}",
            state
        );
    }

    #[test]
    fn flap_cooldown_blocks_repeat_within_60s() {
        let (start, _) = base_session();
        // First call at session +100 s — fires.
        let now1 = start + Duration::from_secs(100);
        let snap1 = snapshot(
            vec![],
            vec![
                stream(88.0, Some(90.0)),
                stream(93.0, Some(95.0)),
                stream(98.0, Some(100.0)),
            ],
        );
        let (state1, notif1) = classify(&snap1, now1, start, None);
        assert!(matches!(state1, StreamingState::Flapping { .. }));
        assert!(notif1.is_some(), "first flap should fire");
        let fired_at = now1;

        // Second call 10 s later, with a still-fresh flap pattern.
        let now2 = start + Duration::from_secs(110);
        let snap2 = snapshot(
            vec![],
            vec![
                stream(98.0, Some(100.0)),
                stream(103.0, Some(105.0)),
                stream(108.0, Some(110.0)),
            ],
        );
        let (state2, notif2) = classify(&snap2, now2, start, Some(fired_at));
        assert!(matches!(state2, StreamingState::Flapping { .. }));
        assert!(
            notif2.is_none(),
            "second flap within 60 s must be suppressed"
        );
    }

    #[test]
    fn flap_fires_again_after_60s_cooldown() {
        let (start, _) = base_session();
        let now1 = start + Duration::from_secs(100);
        let fired_at = now1;
        // 70 s later — cool-down has elapsed.
        let now2 = start + Duration::from_secs(170);
        let snap = snapshot(
            vec![],
            vec![
                stream(158.0, Some(160.0)),
                stream(163.0, Some(165.0)),
                stream(168.0, Some(170.0)),
            ],
        );
        let (state, notif) = classify(&snap, now2, start, Some(fired_at));
        assert!(matches!(state, StreamingState::Flapping { .. }));
        assert!(
            notif.is_some(),
            "second flap after 60 s cool-down should fire"
        );
    }
}
