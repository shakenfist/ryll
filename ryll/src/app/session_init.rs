use std::time::Duration;

/// Minimum interval between two accepted `SessionInitialized` events
/// while already connected.
///
/// The session id is server-chosen, so an id-equality guard alone is
/// defeated by alternating ids (1, 2, 1, 2, ...): every ~36-byte
/// `SPICE_MSG_MAIN_INIT` would then drive a full retire-and-respawn.
/// A genuine re-link without a visible disconnect is rare enough to
/// tolerate a few seconds of delay, so rate-limiting independently of
/// the id costs nothing legitimate and turns a per-message primitive
/// into a per-window one.
pub(super) const MIN_SESSION_RESPAWN_INTERVAL: Duration = Duration::from_secs(5);

/// What `process_events` should do with an incoming
/// `SessionInitialized`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionInitDecision {
    /// A genuinely new session: announce it and respawn per-session
    /// tasks.
    Accept,
    /// The session already in hand, re-announced.
    IgnoreDuplicate,
    /// A different session id, but too soon after the last accepted
    /// one to be a real re-link.
    IgnoreTooSoon,
}

/// Decide whether a `SessionInitialized(session_id)` should be acted
/// on.
///
/// Split out from the event arm so the two guards can be tested
/// without an eframe context: both exist to blunt a server-controlled
/// primitive, and "the second guard is unreachable because the first
/// already returned" is exactly the kind of mistake that is invisible
/// at the call site.
///
/// `since_last_accepted` is `None` before the first acceptance.
pub(super) fn classify_session_initialized(
    connected: bool,
    current_session_id: Option<u32>,
    session_id: u32,
    since_last_accepted: Option<Duration>,
) -> SessionInitDecision {
    if !connected {
        // Not connected: this is the event that establishes the
        // session, so neither guard applies.
        return SessionInitDecision::Accept;
    }
    if current_session_id == Some(session_id) {
        return SessionInitDecision::IgnoreDuplicate;
    }
    match since_last_accepted {
        Some(since) if since < MIN_SESSION_RESPAWN_INTERVAL => SessionInitDecision::IgnoreTooSoon,
        _ => SessionInitDecision::Accept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // SessionInitialized admission
    // -------------------------------------------------------------------------

    #[test]
    fn the_first_session_is_always_accepted() {
        assert_eq!(
            classify_session_initialized(false, None, 1, None),
            SessionInitDecision::Accept
        );
        // Also when a previous session left timestamps behind: the
        // rate limit is about respawns while connected, and a
        // reconnect after a visible disconnect must never be delayed.
        assert_eq!(
            classify_session_initialized(false, Some(9), 1, Some(Duration::from_millis(1))),
            SessionInitDecision::Accept
        );
    }

    #[test]
    fn re_announcing_the_held_session_is_a_duplicate() {
        assert_eq!(
            classify_session_initialized(true, Some(4), 4, Some(Duration::from_secs(3600))),
            SessionInitDecision::IgnoreDuplicate
        );
    }

    #[test]
    fn alternating_session_ids_are_rate_limited() {
        // The defect this pins: with only an id-equality guard, a
        // server alternating ids passes on every message and drives a
        // retire-and-respawn per ~36-byte MAIN_INIT. Each call below
        // names a different id from the one held, so the equality
        // guard does not fire and the rate limit is the only thing
        // standing between the server and a respawn.
        for (held, incoming) in [(1u32, 2u32), (2, 1), (1, 2)] {
            assert_eq!(
                classify_session_initialized(
                    true,
                    Some(held),
                    incoming,
                    Some(Duration::from_millis(1))
                ),
                SessionInitDecision::IgnoreTooSoon,
                "id {incoming} arriving 1 ms after the last accepted one must be refused"
            );
        }
    }

    #[test]
    fn a_genuine_relink_after_the_window_is_accepted() {
        // A real re-link without a visible disconnect is rare, but it
        // must still work: the rate limit delays it, it does not
        // block it.
        assert_eq!(
            classify_session_initialized(
                true,
                Some(1),
                2,
                Some(MIN_SESSION_RESPAWN_INTERVAL + Duration::from_millis(1))
            ),
            SessionInitDecision::Accept
        );
    }

    #[test]
    fn a_new_id_with_no_prior_acceptance_is_accepted() {
        // `last_session_initialized_at` is None until the first
        // acceptance; a missing timestamp must not be read as
        // "zero elapsed" and refuse the event.
        assert_eq!(
            classify_session_initialized(true, Some(1), 2, None),
            SessionInitDecision::Accept
        );
    }
}
