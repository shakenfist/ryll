use std::time::{Duration, Instant};

/// Approximate height of the stats bar at the bottom of the window
pub(super) const STATS_BAR_HEIGHT: f32 = 20.0;

/// Debounce window for resolution-change notifications.
/// A burst of SurfaceCreated events within this window
/// (boot mode probes, drag-resize storms) collapses to a
/// single notification carrying the latest resolution.
pub(super) const RESOLUTION_NOTIFY_DEBOUNCE: Duration = Duration::from_millis(500);

/// Upper bound (logical pixels) for a primary surface
/// dimension that the auto-fit pipeline will honour. A
/// hostile or buggy SPICE server can announce
/// `SurfaceCreated { width: u32::MAX, height: u32::MAX }`;
/// without a bound we would forward that as
/// `ViewportCommand::InnerSize` to egui (platform-dependent
/// behaviour, possibly large internal allocations) and
/// emit a `"Display resolution: 4294967295x4294967295"`
/// notification. 16384 is `GL_MAX_TEXTURE_SIZE` on most
/// hardware and is comfortably above any realistic
/// display resolution.
pub(super) const MAX_AUTO_FIT_DIMENSION: u32 = 16384;

/// True when an inbound display-channel surface refers to the
/// primary surface — i.e. display channel 0, surface id 0. This
/// literal pair is used rather than tracking "the renderer's
/// current primary key" because the primary surface key is
/// fixed by the SPICE protocol; centralising the check here
/// keeps the trigger sites in sync if that ever changes.
pub(super) fn is_primary_surface(display_channel_id: u8, surface_id: u32) -> bool {
    display_channel_id == 0 && surface_id == 0
}

/// True when an announced surface size is small enough to
/// safely drive the auto-fit and resolution-notification
/// pipelines. See `MAX_AUTO_FIT_DIMENSION` for the
/// rationale; the trigger sites use this to refuse to
/// arm `pending_resize` / `pending_resolution_notify` for
/// nonsense-sized surfaces (which a hostile server can
/// announce trivially) without affecting the SPICE
/// renderer's own surface bookkeeping.
pub(super) fn auto_fit_size_acceptable(width: u32, height: u32) -> bool {
    width <= MAX_AUTO_FIT_DIMENSION && height <= MAX_AUTO_FIT_DIMENSION
}

/// Decide whether to issue a `ViewportCommand::InnerSize`
/// to fit the remote surface, and what size to ask for.
///
/// `pending` is the surface size pulled from
/// `pending_resize` (logical pixels, f32). `last_auto`
/// is the (8-aligned) size of the last auto-resize we
/// issued, or None if we have not auto-resized yet. `is_max`
/// is true when the viewport is maximised or fullscreen
/// and we should not change the inner size. `obey` is the
/// user-controlled toggle: when false the function always
/// returns None so the window is never auto-fitted.
///
/// Returns Some((width, height, aligned_w, aligned_h))
/// where `(width, height)` are the values to pass to
/// `ViewportCommand::InnerSize` (with `STATS_BAR_HEIGHT`
/// added to the height — see the call site) and
/// `(aligned_w, aligned_h)` are the values to store in
/// `last_auto_resize` and seed into `last_sent_resize`.
/// Returns None when no resize should fire.
pub(super) fn compute_auto_resize(
    pending: Option<(f32, f32)>,
    last_auto: Option<(u32, u32)>,
    is_max: bool,
    obey: bool,
) -> Option<(f32, f32, u32, u32)> {
    let (w, h) = pending?;
    if !obey || is_max {
        return None;
    }
    let aligned_w = ((w as u32).max(8) / 8) * 8;
    let aligned_h = ((h as u32).max(8) / 8) * 8;
    if last_auto == Some((aligned_w, aligned_h)) {
        return None;
    }
    Some((w, h, aligned_w, aligned_h))
}

/// Decide what `(width, height)` to send to the guest as a
/// `VDAgentMonitorsConfig` from a given viewport size.
///
/// `viewport` is the live inner-rect size in logical
/// pixels (typically `egui::ViewportInputState::inner_rect`).
/// `is_max` is true when the viewport is maximised or
/// fullscreen — in that case we do not subtract
/// `STATS_BAR_HEIGHT` from the height because the stats
/// bar overlays inside the maximised area rather than
/// adding to it.
///
/// The result is 8-pixel aligned (rounded down, matching
/// what the SPICE display-channel mode-set machinery
/// expects) and clamped to a minimum of 8 on each axis so
/// we never send a degenerate `(0, 0)` resize during a
/// pathological viewport report.
pub(super) fn compute_outgoing_resize(viewport: (f32, f32), is_max: bool) -> (u32, u32) {
    let bar_height = if is_max { 0.0 } else { STATS_BAR_HEIGHT };
    let w_raw = viewport.0.max(0.0) as u32;
    let h_raw = (viewport.1 - bar_height).max(0.0) as u32;
    let aligned_w = (w_raw.max(8) / 8) * 8;
    let aligned_h = (h_raw.max(8) / 8) * 8;
    (aligned_w, aligned_h)
}

/// Decide whether the pending resolution-change
/// notification has been quiet long enough to emit, and
/// what value to emit.
///
/// `pending` pairs the latest queued (w, h) with its
/// observation timestamp; the two are always set and
/// cleared together at every call site.
///
/// Returns Some((w, h)) when:
/// * a value is pending,
/// * at least `debounce` has elapsed since the value
///   was queued, and
/// * the value differs from `last_notified` (so we do
///   not re-emit a confirmation of the existing mode).
///
/// Returns None to leave the pending state in place
/// (still inside the debounce window) or to drop it
/// silently (matches last_notified — the caller should
/// also clear the pending field in that case; see the
/// call site).
///
/// Pure for unit-testability — `now` is injected.
pub(super) fn resolution_notification_due(
    pending: Option<((u32, u32), Instant)>,
    last_notified: Option<(u32, u32)>,
    now: Instant,
    debounce: Duration,
) -> Option<(u32, u32)> {
    let (target, queued_at) = pending?;
    if now.saturating_duration_since(queued_at) < debounce {
        return None;
    }
    if last_notified == Some(target) {
        return None;
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_auto_resize_decisions() {
        // No pending event => no resize.
        assert_eq!(compute_auto_resize(None, None, false, true), None);

        // Pending size, never resized before, not maximised =>
        // resize to the aligned target.
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), None, false, true),
            Some((1024.0, 768.0, 1024, 768)),
        );

        // Same target as last_auto => skip (dedup).
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), Some((1024, 768)), false, true),
            None,
        );

        // Maximised => skip even when target differs.
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), Some((640, 480)), true, true),
            None,
        );

        // Non-aligned size gets aligned to 8 px boundary.
        assert_eq!(
            compute_auto_resize(Some((1366.0, 770.0)), None, false, true),
            Some((1366.0, 770.0, 1360, 768)),
        );

        // Differs after alignment from last_auto => resize.
        assert_eq!(
            compute_auto_resize(Some((1280.0, 800.0)), Some((1024, 768)), false, true),
            Some((1280.0, 800.0, 1280, 800)),
        );

        // obey = false short-circuits even when the target
        // differs and the window is not maximised.
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), None, false, false,),
            None,
        );

        // obey = false short-circuits even when the target equals
        // last_auto (would dedup anyway, but we want the obey
        // gate to be the reason).
        assert_eq!(
            compute_auto_resize(Some((1024.0, 768.0)), Some((1024, 768)), false, false,),
            None,
        );
    }

    #[test]
    fn compute_outgoing_resize_decisions() {
        // Even-aligned input passes through unchanged
        // (height has STATS_BAR_HEIGHT subtracted first).
        assert_eq!(
            compute_outgoing_resize((1024.0, 768.0 + STATS_BAR_HEIGHT), false),
            (1024, 768),
        );

        // Non-aligned widths round DOWN to the 8 px grid
        // (matches the historical `w -= w % 8` form).
        assert_eq!(
            compute_outgoing_resize((1366.0, 768.0 + STATS_BAR_HEIGHT), false),
            (1360, 768),
        );

        // Non-aligned heights round DOWN too.
        assert_eq!(
            compute_outgoing_resize((1024.0, 770.0 + STATS_BAR_HEIGHT), false),
            (1024, 768),
        );

        // Sub-8 viewport dims clamp to the 8 px floor on each
        // axis. Use 4×4 (after bar subtraction) — both axes
        // hit the clamp.
        assert_eq!(
            compute_outgoing_resize((4.0, 4.0 + STATS_BAR_HEIGHT), false),
            (8, 8),
        );

        // Negative viewport dims (f32 < 0) clamp to zero
        // before the 8 px floor — must not panic on the
        // `as u32` conversion. f32 -> u32 is saturating in
        // Rust, but we still rely on the .max(0.0) up front.
        assert_eq!(compute_outgoing_resize((-100.0, -50.0), false), (8, 8),);

        // is_max = true skips the STATS_BAR_HEIGHT
        // subtraction. Same viewport, different is_max ->
        // different height.
        assert_eq!(compute_outgoing_resize((1024.0, 768.0), true), (1024, 768),);
        // is_max = false subtracts STATS_BAR_HEIGHT (20) then rounds down to
        // the 8-px grid: (768 - 20) = 748, rounded down to 744.
        assert_eq!(compute_outgoing_resize((1024.0, 768.0), false), (1024, 744),);
    }

    /// After auto-fitting to a fresh guest surface, the next
    /// frame's outgoing-resize computation must produce the
    /// same (aligned_w, aligned_h) so `last_sent_resize`
    /// dedupes and we do not echo our own resize back to the
    /// guest as a fresh VDAgentMonitorsConfig.
    #[test]
    fn round_trip_no_echo() {
        // Guest sends SurfaceCreated 1024x768. compute_auto_resize
        // returns the (w, h, aligned_w, aligned_h) we will fit
        // to and seed into last_sent_resize.
        let auto = compute_auto_resize(Some((1024.0, 768.0)), None, false, true)
            .expect("auto-fit should fire on first surface");
        let (fit_w, fit_h, aligned_w, aligned_h) = auto;
        assert_eq!((fit_w as u32, fit_h as u32), (1024, 768));
        assert_eq!((aligned_w, aligned_h), (1024, 768));

        // egui then reports the new viewport inner-rect: the
        // surface size plus STATS_BAR_HEIGHT (we asked for
        // total_h = h + STATS_BAR_HEIGHT in the resize block).
        let viewport = (fit_w, fit_h + STATS_BAR_HEIGHT);
        let outgoing = compute_outgoing_resize(viewport, false);

        // The outgoing computation must match the seeded
        // last_sent_resize, so maybe_send_monitors_resize
        // dedupes and does NOT fire.
        assert_eq!(outgoing, (aligned_w, aligned_h));
    }

    /// If the guest answers a ryll-driven resize request with
    /// a *different* size (e.g. ryll asked for 1280x800, guest
    /// can only do 1024x768), auto-fit re-seeds last_sent_resize
    /// to the guest's choice. The next outgoing computation
    /// against the new viewport must dedupe so ryll does not
    /// then ask for 1024x768 again as if it were a user-driven
    /// resize.
    #[test]
    fn round_trip_guest_overrides_request() {
        // State at the start of the test: last_sent_resize
        // was (1280, 800) because the user dragged the window
        // to that size and we sent a VDAgentMonitorsConfig
        // accordingly. last_auto_resize is None because no
        // auto-fit has fired yet this session.
        let last_sent = Some((1280u32, 800u32));
        let last_auto: Option<(u32, u32)> = None;

        // Guest replies with a 1024x768 SurfaceCreated.
        let auto = compute_auto_resize(Some((1024.0, 768.0)), last_auto, false, true)
            .expect("auto-fit should fire — surface differs from last_auto");
        let (_, _, aligned_w, aligned_h) = auto;
        assert_eq!((aligned_w, aligned_h), (1024, 768));

        // Caller seeds both last_sent_resize and
        // last_auto_resize from the auto-fit result.
        let new_last_sent = Some((aligned_w, aligned_h));
        assert_ne!(
            new_last_sent, last_sent,
            "last_sent must update — we did not request 1024x768"
        );

        // egui reports the new viewport size. Outgoing
        // computation against it must match new_last_sent so
        // we do NOT then fire a fresh VDAgentMonitorsConfig
        // asking the guest for a size it just gave us.
        let viewport = (aligned_w as f32, aligned_h as f32 + STATS_BAR_HEIGHT);
        let outgoing = compute_outgoing_resize(viewport, false);
        assert_eq!(Some(outgoing), new_last_sent);
    }

    #[test]
    fn auto_fit_size_acceptable_bounds() {
        // Anchors the cap so a typo (`>` vs `>=`, `||` vs
        // `&&`) cannot silently let an attacker-controlled
        // dimension through. The cap matches
        // GL_MAX_TEXTURE_SIZE on common hardware and is
        // comfortably above any realistic display.
        assert!(auto_fit_size_acceptable(0, 0));
        assert!(auto_fit_size_acceptable(1024, 768));
        assert!(auto_fit_size_acceptable(
            MAX_AUTO_FIT_DIMENSION,
            MAX_AUTO_FIT_DIMENSION
        ));
        assert!(!auto_fit_size_acceptable(MAX_AUTO_FIT_DIMENSION + 1, 768));
        assert!(!auto_fit_size_acceptable(1024, MAX_AUTO_FIT_DIMENSION + 1));
        assert!(!auto_fit_size_acceptable(u32::MAX, u32::MAX));
    }

    #[test]
    fn is_primary_surface_only_zero_zero() {
        // Anchors the gating predicate so a typo
        // (`||` for `&&`, or a non-zero default) cannot
        // silently widen the set of surfaces that drive
        // auto-fit and resolution notifications.
        assert!(is_primary_surface(0, 0));
        assert!(!is_primary_surface(0, 1));
        assert!(!is_primary_surface(1, 0));
        assert!(!is_primary_surface(1, 1));
    }

    #[test]
    fn resolution_notification_due_nothing_pending() {
        let now = Instant::now();
        assert_eq!(
            resolution_notification_due(None, None, now, Duration::from_millis(500),),
            None,
        );
    }

    #[test]
    fn resolution_notification_due_inside_debounce() {
        let now = Instant::now();
        let queued_at = now - Duration::from_millis(100);
        assert_eq!(
            resolution_notification_due(
                Some(((1024, 768), queued_at)),
                None,
                now,
                Duration::from_millis(500),
            ),
            None,
            "100 ms < 500 ms debounce — must not fire",
        );
    }

    #[test]
    fn resolution_notification_due_past_window_emits() {
        let now = Instant::now();
        let queued_at = now - Duration::from_millis(600);
        assert_eq!(
            resolution_notification_due(
                Some(((1024, 768), queued_at)),
                None,
                now,
                Duration::from_millis(500),
            ),
            Some((1024, 768)),
        );
    }

    #[test]
    fn resolution_notification_due_past_window_dedupes() {
        // Pending value matches last_notified — caller
        // should suppress so we do not announce the same
        // resolution twice in a row.
        let now = Instant::now();
        let queued_at = now - Duration::from_millis(600);
        assert_eq!(
            resolution_notification_due(
                Some(((1024, 768), queued_at)),
                Some((1024, 768)),
                now,
                Duration::from_millis(500),
            ),
            None,
        );
    }
}
