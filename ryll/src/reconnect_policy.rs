//! Retry rules for a lost SPICE connection, independent of any
//! front end. The GUI maps the outcome onto its disconnect modal;
//! a headless caller (`--rdp`) just sleeps and retries, or stops.
//!
//! The rules: a short fixed backoff, no retry at all when the .vv
//! file's ticket is single-use or past its valid-until time, and
//! no fresh budget for a disconnect shortly after a previous
//! give-up.

#![cfg_attr(not(feature = "gui"), allow(dead_code))]

use std::time::{Duration, Instant, SystemTime};

/// Auto-reconnect retry budget per disconnect cluster.
pub const MAX_RECONNECT_ATTEMPTS: u8 = 3;

/// Backoff (seconds) before each reconnect attempt within a
/// cluster. Index 0 is the wait before attempt 1, index 1 before
/// attempt 2, etc. Shape matches spice-gtk's reconnect policy:
/// short first attempt for blip recovery, longer windows for
/// server restarts. Total worst-case wait ~21 s before giving up.
pub const RECONNECT_BACKOFF_SECS: [u64; MAX_RECONNECT_ATTEMPTS as usize] = [1, 4, 16];

/// After the auto-reconnect budget is exhausted, further
/// disconnects within this window give up again without
/// re-trying -- a flapping server cannot make us bang away
/// forever. A fresh budget unlocks after this elapses.
pub const RECONNECT_CLUSTER_RESET: Duration = Duration::from_secs(5 * 60);

/// Why the ticket forbids any further reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketBlock {
    /// The .vv file's `delete-this-file=1` flag marked the ticket
    /// as single-use; the first link consumed it.
    SingleUse,
    /// `ticket-valid-until` has elapsed (wall-clock time).
    Expired { expired_at: SystemTime },
}

/// Why no further retry will be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// The retry budget is spent, or a recent give-up means this
    /// cluster gets no budget at all.
    Exhausted,
    /// The ticket cannot be used again.
    Ticket(TicketBlock),
}

/// What to do after a disconnect or a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Make the next attempt after waiting this long.
    RetryAfter(Duration),
    /// Stop retrying.
    GiveUp(GiveUp),
}

/// Policy bits derived from the .vv file's ticket-related keys.
/// Bundled so callers can take a single argument rather than
/// threading two unrelated values.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReconnectPolicy {
    /// `delete-this-file=1` was set: the previous link consumed
    /// the ticket, so the server will reject any reconnect.
    pub ticket_is_single_use: bool,
    /// `ticket-valid-until=<unix-ts>` was set: when this wall
    /// time has passed, any reconnect would be doomed.
    pub ticket_valid_until: Option<SystemTime>,
}

impl ReconnectPolicy {
    /// If the ticket policy forbids any further reconnect, say
    /// why. `None` means the normal retry path is permitted.
    pub fn forbid_retry(&self, now_wall: SystemTime) -> Option<TicketBlock> {
        if self.ticket_is_single_use {
            return Some(TicketBlock::SingleUse);
        }
        if let Some(expiry) = self.ticket_valid_until {
            if now_wall >= expiry {
                return Some(TicketBlock::Expired { expired_at: expiry });
            }
        }
        None
    }
}

/// Decide what follows a disconnect.
///
/// `failed_attempts` is the number of reconnect attempts already
/// made and failed in this cluster: 0 for the first disconnect of
/// a session, `n` when attempt `n` has just failed. `last_give_up`
/// is when the previous cluster was given up on, if one was; it
/// only matters for the first disconnect of a cluster.
pub fn next_step(
    policy: &ReconnectPolicy,
    now: Instant,
    now_wall: SystemTime,
    last_give_up: Option<Instant>,
    failed_attempts: u8,
) -> RetryDecision {
    if let Some(block) = policy.forbid_retry(now_wall) {
        return RetryDecision::GiveUp(GiveUp::Ticket(block));
    }

    if failed_attempts == 0 {
        if let Some(t) = last_give_up {
            if now.duration_since(t) < RECONNECT_CLUSTER_RESET {
                return RetryDecision::GiveUp(GiveUp::Exhausted);
            }
        }
    }

    match RECONNECT_BACKOFF_SECS.get(failed_attempts as usize) {
        Some(secs) => RetryDecision::RetryAfter(Duration::from_secs(*secs)),
        None => RetryDecision::GiveUp(GiveUp::Exhausted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn epoch() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    fn no_policy() -> ReconnectPolicy {
        ReconnectPolicy::default()
    }

    fn one_shot_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            ticket_is_single_use: true,
            ticket_valid_until: None,
        }
    }

    fn expiring_at(t: SystemTime) -> ReconnectPolicy {
        ReconnectPolicy {
            ticket_is_single_use: false,
            ticket_valid_until: Some(t),
        }
    }

    #[test]
    fn reconnect_backoff_progression_matches_spec() {
        // The backoffs published in plan §A.1 are 1s/4s/16s.
        // Lock them in with a direct check so a typo in the
        // constant array is caught by the test suite.
        assert_eq!(RECONNECT_BACKOFF_SECS, [1, 4, 16]);
        assert_eq!(MAX_RECONNECT_ATTEMPTS, 3);
    }

    #[test]
    fn next_step_walks_the_backoff_then_gives_up() {
        let now = Instant::now();
        for (failed, secs) in [(0u8, 1u64), (1, 4), (2, 16)] {
            assert_eq!(
                next_step(&no_policy(), now, epoch(), None, failed),
                RetryDecision::RetryAfter(Duration::from_secs(secs)),
                "after {} failed attempts",
                failed
            );
        }
        assert_eq!(
            next_step(&no_policy(), now, epoch(), None, 3),
            RetryDecision::GiveUp(GiveUp::Exhausted)
        );
    }

    #[test]
    fn next_step_single_use_ticket_never_retries() {
        let now = Instant::now();
        for failed in 0..=3 {
            assert_eq!(
                next_step(&one_shot_policy(), now, epoch(), None, failed),
                RetryDecision::GiveUp(GiveUp::Ticket(TicketBlock::SingleUse))
            );
        }
    }

    #[test]
    fn next_step_expired_ticket_never_retries() {
        let now = Instant::now();
        let expiry = epoch() - Duration::from_secs(60);
        assert_eq!(
            next_step(&expiring_at(expiry), now, epoch(), None, 1),
            RetryDecision::GiveUp(GiveUp::Ticket(TicketBlock::Expired { expired_at: expiry }))
        );
    }

    #[test]
    fn next_step_ticket_expiring_exactly_now_is_expired() {
        let now = Instant::now();
        assert_eq!(
            next_step(&expiring_at(epoch()), now, epoch(), None, 0),
            RetryDecision::GiveUp(GiveUp::Ticket(TicketBlock::Expired {
                expired_at: epoch()
            }))
        );
    }

    #[test]
    fn next_step_ticket_valid_in_future_retries() {
        let now = Instant::now();
        let expiry = epoch() + Duration::from_secs(3600);
        assert_eq!(
            next_step(&expiring_at(expiry), now, epoch(), None, 0),
            RetryDecision::RetryAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn next_step_recent_give_up_skips_the_budget() {
        let now = Instant::now();
        let gave_up = now - Duration::from_secs(60);
        assert_eq!(
            next_step(&no_policy(), now, epoch(), Some(gave_up), 0),
            RetryDecision::GiveUp(GiveUp::Exhausted)
        );
    }

    #[test]
    fn next_step_old_give_up_unlocks_a_fresh_budget() {
        let now = Instant::now();
        let gave_up = now - RECONNECT_CLUSTER_RESET - Duration::from_secs(1);
        assert_eq!(
            next_step(&no_policy(), now, epoch(), Some(gave_up), 0),
            RetryDecision::RetryAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn next_step_recent_give_up_does_not_cut_a_cluster_short() {
        // The flap rule applies to the first disconnect of a
        // cluster only; a cluster already under way keeps going.
        let now = Instant::now();
        let gave_up = now - Duration::from_secs(60);
        assert_eq!(
            next_step(&no_policy(), now, epoch(), Some(gave_up), 1),
            RetryDecision::RetryAfter(Duration::from_secs(4))
        );
    }
}
