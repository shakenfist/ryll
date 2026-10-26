use std::time::{Instant, SystemTime, UNIX_EPOCH};

use shakenfist_spice_protocol::NotifySeverity;

use crate::reconnect_policy::{next_step, GiveUp, ReconnectPolicy, RetryDecision, TicketBlock};

/// Discriminator for the disconnect-modal. The variant
/// determines title, body copy, and which buttons render. The
/// `OneShotConsumed` and `TicketExpired` variants are entered
/// when the .vv file's `delete-this-file` / `ticket-valid-until`
/// keys (see `kerbside-wt-docs/docs/spice/console-vv-extensions.md`)
/// indicate that any further reconnect is doomed; the
/// `Generic` variant covers everything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ModalVariant {
    /// Auto-reconnect budget exhausted on a reusable ticket.
    /// `latest_error` is the most recent attempt's failure
    /// string, shown in the modal body for context.
    Generic { latest_error: String },
    /// The .vv file's `delete-this-file=1` flag marked the
    /// ticket as single-use; the first link consumed it, and
    /// any reconnect would be rejected by the server. Reconnect
    /// button hidden.
    OneShotConsumed,
    /// `ticket-valid-until` has elapsed (wall-clock time). The
    /// server will reject any link from now on; auto-reconnect
    /// is suppressed and the modal explains why.
    TicketExpired { expired_at: SystemTime },
    /// A connection attempt that was not an auto-retry failed
    /// before it established a session: the dial, TLS, link or
    /// authentication failed or timed out. Nothing was lost, so
    /// this is not a disconnect and does not consume the
    /// auto-reconnect budget; whether a retry can succeed is the
    /// user's call, so the Reconnect button is offered whatever
    /// the ticket policy says.
    ConnectFailed { error: String },
}

/// Auto-reconnect state machine. Replaces the implicit
/// `show_disconnect_dialog: bool` + `disconnect_reason` pair so
/// every disconnect path either auto-recovers or surfaces a
/// well-typed modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReconnectState {
    /// Connected normally, or not yet attempted.
    Idle,
    /// Auto-reconnect is in progress. `attempt` ∈ 1..=MAX_ATTEMPTS;
    /// `next_at` is when the next `reconnect()` call should fire.
    Pending {
        attempt: u8,
        next_at: Instant,
        latest_error: String,
    },
    /// Budget exhausted (or ticket-related auto-suppression);
    /// the user takes over via the modal. Variant carries the
    /// reason and any context needed to render copy + buttons.
    Modal(ModalVariant),
}

impl ModalVariant {
    /// The modal for a ticket that forbids any further reconnect.
    pub(super) fn from_ticket_block(block: TicketBlock) -> Self {
        match block {
            TicketBlock::SingleUse => ModalVariant::OneShotConsumed,
            TicketBlock::Expired { expired_at } => ModalVariant::TicketExpired { expired_at },
        }
    }
}

/// If the ticket policy forbids any further reconnect, return the
/// appropriate `ModalVariant`. `None` means the normal retry path
/// is permitted.
pub(super) fn forbid_retry_modal(
    policy: &ReconnectPolicy,
    now_wall: SystemTime,
) -> Option<ModalVariant> {
    policy
        .forbid_retry(now_wall)
        .map(ModalVariant::from_ticket_block)
}

impl ReconnectState {
    /// Pure transition for a disconnect event. Returns the new
    /// state, or `None` if the event should be ignored (e.g. a
    /// duplicate channel-storm event while we're already in
    /// `Pending` or `Modal`).
    ///
    /// `awaiting_outcome` is `true` when the disconnect is the
    /// failure of an in-flight reconnect attempt (we previously
    /// called `reconnect()` from a `Pending` tick and are now
    /// hearing back). `false` for the initial disconnect or for
    /// duplicate storm events.
    ///
    /// `policy` derived from the .vv file's ticket-related
    /// keys. When it forbids retries (single-use ticket
    /// consumed, or `ticket-valid-until` elapsed) we skip the
    /// auto-retry path entirely and land in the matching
    /// `Modal` variant on the first disconnect.
    pub(super) fn on_disconnect(
        &self,
        awaiting_outcome: bool,
        last_modal_at: Option<Instant>,
        now: Instant,
        now_wall: SystemTime,
        policy: ReconnectPolicy,
        latest_error: String,
    ) -> Option<Self> {
        // Ticket-bound deployments: any further reconnect would
        // be rejected, so trip the modal on the first disconnect
        // event of any kind — no point in burning the budget.
        if let Some(variant) = forbid_retry_modal(&policy, now_wall) {
            // Already in the matching Modal? Ignore the storm.
            if matches!(self, ReconnectState::Modal(v) if v == &variant) {
                return None;
            }
            return Some(ReconnectState::Modal(variant));
        }

        let failed_attempts = if awaiting_outcome {
            match self {
                ReconnectState::Pending { attempt, .. } => *attempt,
                // `awaiting_outcome` should imply we were in
                // Pending; defensively land in Generic Modal so
                // we don't silently re-arm a retry from a stale
                // state.
                _ => {
                    return Some(ReconnectState::Modal(ModalVariant::Generic {
                        latest_error,
                    }))
                }
            }
        } else {
            match self {
                ReconnectState::Idle => 0,
                // Already disconnected — ignore the duplicate.
                ReconnectState::Pending { .. } | ReconnectState::Modal(_) => return None,
            }
        };

        match next_step(&policy, now, now_wall, last_modal_at, failed_attempts) {
            RetryDecision::RetryAfter(backoff) => Some(ReconnectState::Pending {
                attempt: failed_attempts + 1,
                next_at: now + backoff,
                latest_error,
            }),
            RetryDecision::GiveUp(GiveUp::Ticket(block)) => Some(ReconnectState::Modal(
                ModalVariant::from_ticket_block(block),
            )),
            RetryDecision::GiveUp(GiveUp::Exhausted) => {
                Some(ReconnectState::Modal(ModalVariant::Generic {
                    latest_error,
                }))
            }
        }
    }
}

/// Format a `SystemTime` as a `HH:MM:SS` clock string in UTC
/// for the `TicketExpired` modal body. UTC is unambiguous for a
/// modal that explains "the ticket expired at …" — the user is
/// usually in the same TZ as their issuing system anyway, and
/// pulling in a TZ-aware crate just for one line of modal copy
/// would be disproportionate. Falls back to the raw unix
/// timestamp if the time is before the epoch (impossible in
/// practice but cheap to handle).
pub(super) fn format_expiry_local(t: SystemTime) -> String {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs() % 86400;
            let h = secs / 3600;
            let m = (secs % 3600) / 60;
            let s = secs % 60;
            format!("{:02}:{:02}:{:02} UTC", h, m, s)
        }
        Err(_) => "before 1970-01-01".to_string(),
    }
}

/// Map an auto-reconnect `ModalVariant` to the `(severity,
/// message)` pair that surfaces in the notification pane when the
/// state machine lands in Modal. Pure so it can be unit-tested
/// apart from the connection-event push sites that depend on it.
pub(super) fn modal_variant_notification(variant: &ModalVariant) -> (NotifySeverity, String) {
    match variant {
        ModalVariant::Generic { .. } => (
            NotifySeverity::Error,
            "Auto-reconnect failed after 3 attempts".to_string(),
        ),
        ModalVariant::OneShotConsumed => (
            NotifySeverity::Error,
            "Connection ended — single-use ticket consumed".to_string(),
        ),
        ModalVariant::TicketExpired { .. } => (
            NotifySeverity::Error,
            "Connection ended — ticket expired".to_string(),
        ),
        ModalVariant::ConnectFailed { .. } => (
            NotifySeverity::Error,
            "Connection attempt failed".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconnect_policy::{
        MAX_RECONNECT_ATTEMPTS, RECONNECT_BACKOFF_SECS, RECONNECT_CLUSTER_RESET,
    };
    use std::time::Duration;

    // ── ReconnectState transitions ──────────────────────────

    fn err(s: &str) -> String {
        s.to_string()
    }

    fn no_policy() -> ReconnectPolicy {
        ReconnectPolicy::default()
    }

    fn epoch() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    #[test]
    fn reconnect_idle_to_pending_on_first_disconnect() {
        let now = Instant::now();
        let next =
            ReconnectState::Idle.on_disconnect(false, None, now, epoch(), no_policy(), err("eof"));
        match next {
            Some(ReconnectState::Pending {
                attempt,
                next_at,
                latest_error,
            }) => {
                assert_eq!(attempt, 1);
                assert_eq!(
                    next_at - now,
                    Duration::from_secs(RECONNECT_BACKOFF_SECS[0])
                );
                assert_eq!(latest_error, "eof");
            }
            other => panic!("expected Pending(1), got {:?}", other),
        }
    }

    #[test]
    fn reconnect_full_failure_cluster_lands_in_modal() {
        let now = Instant::now();
        // Initial disconnect: Idle → Pending(1).
        let s1 = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), no_policy(), err("e1"))
            .unwrap();
        // Attempt 1 failure: Pending(1) → Pending(2).
        let s2 = s1
            .on_disconnect(
                true,
                None,
                now + Duration::from_secs(1),
                epoch(),
                no_policy(),
                err("e2"),
            )
            .unwrap();
        assert!(matches!(s2, ReconnectState::Pending { attempt: 2, .. }));
        // Attempt 2 failure: Pending(2) → Pending(3).
        let s3 = s2
            .on_disconnect(
                true,
                None,
                now + Duration::from_secs(5),
                epoch(),
                no_policy(),
                err("e3"),
            )
            .unwrap();
        assert!(matches!(s3, ReconnectState::Pending { attempt: 3, .. }));
        // Attempt 3 failure: Pending(3) → Modal(Generic) with latest error.
        let s4 = s3
            .on_disconnect(
                true,
                None,
                now + Duration::from_secs(20),
                epoch(),
                no_policy(),
                err("final"),
            )
            .unwrap();
        match s4 {
            ReconnectState::Modal(ModalVariant::Generic { latest_error }) => {
                assert_eq!(latest_error, "final");
            }
            other => panic!("expected Modal(Generic), got {:?}", other),
        }
    }

    #[test]
    fn reconnect_storm_event_during_pending_is_ignored() {
        let now = Instant::now();
        let pending = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), no_policy(), err("first"))
            .unwrap();
        // A non-awaiting second event (channel storm) must not
        // advance the attempt counter — that would burn budget
        // for a single underlying failure.
        let next = pending.on_disconnect(
            false,
            None,
            now + Duration::from_millis(10),
            epoch(),
            no_policy(),
            err("dup"),
        );
        assert!(next.is_none());
    }

    #[test]
    fn reconnect_cluster_reset_window_blocks_retry() {
        let now = Instant::now();
        // Recent modal: a fresh disconnect must skip Pending and
        // land directly in Modal. Otherwise a flapping server
        // would have us banging away forever.
        let modal_at = now - Duration::from_secs(60);
        let next = ReconnectState::Idle.on_disconnect(
            false,
            Some(modal_at),
            now,
            epoch(),
            no_policy(),
            err("flap"),
        );
        assert!(matches!(
            next,
            Some(ReconnectState::Modal(ModalVariant::Generic { .. }))
        ));
    }

    #[test]
    fn reconnect_cluster_reset_window_expires() {
        let now = Instant::now();
        // Beyond the 5-min reset window, a fresh budget unlocks.
        let modal_at = now - RECONNECT_CLUSTER_RESET - Duration::from_secs(1);
        let next = ReconnectState::Idle
            .on_disconnect(
                false,
                Some(modal_at),
                now,
                epoch(),
                no_policy(),
                err("later"),
            )
            .unwrap();
        assert!(matches!(next, ReconnectState::Pending { attempt: 1, .. }));
    }

    #[test]
    fn reconnect_modal_ignores_extra_storm_events() {
        // Once we're in Modal, additional non-awaiting events
        // must not change state — the user is in control.
        let now = Instant::now();
        let modal = ReconnectState::Modal(ModalVariant::Generic {
            latest_error: "x".into(),
        });
        let next = modal.on_disconnect(false, None, now, epoch(), no_policy(), err("y"));
        assert!(next.is_none());
    }

    #[test]
    fn reconnect_awaiting_outcome_from_idle_lands_in_modal_defensively() {
        // Defensive arm of on_disconnect: awaiting_outcome=true
        // implies we were in Pending (we just called
        // reconnect() from the GUI tick). If a stale event
        // somehow arrives while state is Idle, the state
        // machine doesn't silently re-arm a retry — it lands
        // in Modal(Generic) so the user takes over. Pin the
        // safety-net behaviour so a future refactor can't
        // strip it without the test catching the change.
        let now = Instant::now();
        let next = ReconnectState::Idle
            .on_disconnect(
                true, // awaiting_outcome from Idle: shouldn't happen, defensive
                None,
                now,
                epoch(),
                no_policy(),
                err("stale"),
            )
            .unwrap();
        assert!(
            matches!(
                &next,
                ReconnectState::Modal(ModalVariant::Generic { latest_error }) if latest_error == "stale"
            ),
            "awaiting_outcome from non-Pending must land in Modal(Generic), got {:?}",
            next
        );
    }

    #[test]
    fn reconnect_awaiting_outcome_from_modal_lands_in_modal_defensively() {
        // Same defensive arm, entering from Modal. Replaces
        // the existing Modal with a fresh Generic carrying
        // the new error — no silent re-arm, no panic.
        let now = Instant::now();
        let modal = ReconnectState::Modal(ModalVariant::OneShotConsumed);
        let next = modal
            .on_disconnect(true, None, now, epoch(), no_policy(), err("stale"))
            .unwrap();
        assert!(
            matches!(
                &next,
                ReconnectState::Modal(ModalVariant::Generic { latest_error }) if latest_error == "stale"
            ),
            "awaiting_outcome from Modal must produce Modal(Generic), got {:?}",
            next
        );
    }

    // ── Ticket-policy paths ─────────────────────────────────

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
    fn ticket_single_use_skips_pending_and_lands_in_oneshot_modal() {
        // delete-this-file=1: a fresh disconnect must short-circuit
        // straight to OneShotConsumed without ever entering
        // Pending. Auto-retry would only produce server-side
        // ticket-validation failures.
        let now = Instant::now();
        let next = ReconnectState::Idle
            .on_disconnect(
                false,
                None,
                now,
                epoch(),
                one_shot_policy(),
                err("anything"),
            )
            .unwrap();
        assert!(matches!(
            next,
            ReconnectState::Modal(ModalVariant::OneShotConsumed)
        ));
    }

    #[test]
    fn ticket_single_use_storm_event_is_noop_in_modal() {
        // After landing in OneShotConsumed, a duplicate disconnect
        // (channel storm) must not refire the modal transition.
        let now = Instant::now();
        let modal = ReconnectState::Modal(ModalVariant::OneShotConsumed);
        let next = modal.on_disconnect(false, None, now, epoch(), one_shot_policy(), err("storm"));
        assert!(next.is_none());
    }

    #[test]
    fn ticket_expired_in_past_lands_in_ticket_expired_modal() {
        let now = Instant::now();
        let expiry = epoch() - Duration::from_secs(60);
        let policy = expiring_at(expiry);
        // Disconnect arrives after the ticket has already expired
        // (per our wall clock). Skip Pending entirely.
        let next = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), policy, err("dead"))
            .unwrap();
        match next {
            ReconnectState::Modal(ModalVariant::TicketExpired { expired_at }) => {
                assert_eq!(expired_at, expiry);
            }
            other => panic!("expected Modal(TicketExpired), got {:?}", other),
        }
    }

    #[test]
    fn ticket_valid_in_future_takes_normal_pending_path() {
        let now = Instant::now();
        // Ticket good for another hour: a disconnect should enter
        // Pending(1) as usual.
        let expiry = epoch() + Duration::from_secs(3600);
        let next = ReconnectState::Idle
            .on_disconnect(false, None, now, epoch(), expiring_at(expiry), err("blip"))
            .unwrap();
        assert!(matches!(next, ReconnectState::Pending { attempt: 1, .. }));
    }

    // ── Connection-event message formats ───────────────────

    #[test]
    fn connection_event_message_format_attempt_fire() {
        // The attempt-fire notification template embeds the
        // attempt number so the 30 s dedup window doesn't
        // collapse successive attempts of one cluster. Catches
        // a typo in the format string and pins the embedded
        // MAX_RECONNECT_ATTEMPTS reference.
        let expected = format!("Reconnect attempt {}/{}…", 2, MAX_RECONNECT_ATTEMPTS);
        assert_eq!(expected, "Reconnect attempt 2/3…");
    }

    #[test]
    fn connection_event_message_format_modal_variants() {
        // Pin the (severity, message) pair each ModalVariant
        // maps to. Catches drift between modal copy and
        // notification copy.
        let (sev, msg) = modal_variant_notification(&ModalVariant::Generic {
            latest_error: "ignored".into(),
        });
        assert_eq!(sev, NotifySeverity::Error);
        assert_eq!(msg, "Auto-reconnect failed after 3 attempts");

        let (sev, msg) = modal_variant_notification(&ModalVariant::OneShotConsumed);
        assert_eq!(sev, NotifySeverity::Error);
        assert_eq!(msg, "Connection ended — single-use ticket consumed");

        let (sev, msg) = modal_variant_notification(&ModalVariant::TicketExpired {
            expired_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        });
        assert_eq!(sev, NotifySeverity::Error);
        assert_eq!(msg, "Connection ended — ticket expired");
    }
}
