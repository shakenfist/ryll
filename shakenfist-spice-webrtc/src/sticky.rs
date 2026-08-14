//! A one-shot, level-triggered signal: a [`tokio::sync::Notify`]
//! paired with a sticky [`AtomicBool`].
//!
//! `Notify` alone is edge-triggered — `notify_waiters()` wakes only
//! the waiters registered at that instant, and a waiter that
//! subscribes afterwards blocks forever. The bridge needs
//! level-triggered semantics in two places (the dead signal and the
//! ICE-gathering-complete signal): once the event has happened, every
//! waiter must return, no matter when it arrives. The sticky flag
//! provides the level; the `Notify` provides the wakeup.
//!
//! This type exists because the wait side is subtly easy to get
//! wrong, and getting it wrong cost us a real production bug (the
//! `wait_for_dead` lost wakeup, phase-01 step 1f' of
//! `docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md`). The
//! correct ordering is: register interest first via
//! [`Notified::enable`], *then* check the flag, then await. Checking
//! the flag before registering leaves a window where [`raise`] fires
//! `notify_waiters()` with nobody registered and the await blocks
//! forever. Centralising the pattern means the argument is made — and
//! tested — once, instead of being copy-pasted into every wait site.
//!
//! The producer half is equally constrained: [`raise`] must use
//! `notify_waiters()`, never `notify_one()`. `notify_one()` stores a
//! permit when no waiter is registered, and a waiter whose `Notified`
//! is dropped on the flag fast-path would consume nothing, leaving
//! the permit to spuriously wake some later, unrelated waiter.
//!
//! [`raise`]: StickySignal::raise
//! [`Notified::enable`]: tokio::sync::futures::Notified::enable

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A signal that can be raised exactly once and waited on any number
/// of times, before or after the raise. See the module docs for why
/// this exists and why the implementation looks the way it does.
///
/// Raised is a terminal state: there is deliberately no way to reset
/// the flag. A reset would reintroduce the window where an event
/// from before the reset is mistaken for one after it — callers that
/// need a fresh signal make a fresh `StickySignal`.
#[derive(Debug, Default)]
pub struct StickySignal {
    notify: Notify,
    flag: AtomicBool,
}

impl StickySignal {
    /// A new, unraised signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raise the signal, waking every current waiter and making every
    /// future [`wait`] return immediately.
    ///
    /// Returns `true` only for the call that actually raised the
    /// signal, so a producer that must act exactly once (log a
    /// transition, tear something down) can hang that work off the
    /// return value without a separate guard.
    ///
    /// [`wait`]: Self::wait
    pub fn raise(&self) -> bool {
        if self.flag.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.notify.notify_waiters();
        true
    }

    /// Whether the signal has been raised.
    pub fn is_raised(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Wait until the signal is raised, returning immediately if it
    /// already has been.
    ///
    /// The `enable()` before the flag check is load-bearing.
    /// `Notified` does not register interest until it is first
    /// polled, so the naive "check the flag, then await" ordering has
    /// a window: the signal can be raised between the load and the
    /// first poll, firing `notify_waiters()` with nobody registered,
    /// and the await then blocks forever. `enable()` registers up
    /// front, so a notification landing in that window is still
    /// delivered.
    ///
    /// Cancellation-safe: dropping the returned future — including on
    /// the fast path, where the enabled `Notified` is dropped without
    /// being consumed — leaks no state inside the `Notify`, because
    /// `notify_waiters()` never stores a permit.
    pub async fn wait(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if self.flag.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    /// A waiter that arrives after the raise must return immediately
    /// via the flag fast-path — `Notify` alone would hang it forever.
    #[tokio::test]
    async fn wait_after_raise_returns_immediately() {
        let signal = StickySignal::new();
        assert!(signal.raise());
        assert!(signal.is_raised());

        tokio::time::timeout(Duration::from_secs(1), signal.wait())
            .await
            .expect("a late waiter must take the sticky fast-path");
    }

    /// A waiter registered before the raise is woken by it.
    ///
    /// Note what this does *not* pin: the lost-wakeup window itself
    /// (a raise landing between the flag check and the `Notified`'s
    /// first poll). On this single-threaded runtime the waiter runs
    /// to its await point before the raise, so even the buggy
    /// check-then-await ordering would pass here. The window is
    /// covered probabilistically by
    /// `concurrent_raise_and_wait_never_lose_the_wakeup` below;
    /// this test pins the ordinary notification path
    /// deterministically.
    ///
    /// Single-threaded runtime on purpose: `yield_now` then
    /// deterministically runs the spawned waiter up to its await
    /// before the raise happens.
    #[tokio::test]
    async fn raise_after_wait_registers_wakes_the_waiter() {
        let signal = Arc::new(StickySignal::new());

        let waiter = tokio::spawn({
            let signal = Arc::clone(&signal);
            async move { signal.wait().await }
        });

        // Let the waiter run to its await point, so the raise below
        // exercises the notification path rather than the flag
        // fast-path.
        tokio::task::yield_now().await;
        assert!(!signal.is_raised());

        assert!(signal.raise());
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("registered waiter must be woken by raise")
            .expect("waiter task must not panic");
    }

    /// All concurrent waiters wake on one raise, and a waiter that
    /// comes back afterwards returns again — the signal is a level,
    /// not an edge.
    #[tokio::test]
    async fn multiple_and_repeat_waiters_all_return() {
        let signal = Arc::new(StickySignal::new());

        let waiters: Vec<_> = (0..3)
            .map(|_| {
                tokio::spawn({
                    let signal = Arc::clone(&signal);
                    async move { signal.wait().await }
                })
            })
            .collect();
        tokio::task::yield_now().await;

        assert!(signal.raise());
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("every concurrent waiter must wake")
                .expect("waiter task must not panic");
        }

        // Sequential re-wait after the raise.
        tokio::time::timeout(Duration::from_secs(1), signal.wait())
            .await
            .expect("a repeat waiter must return immediately");
    }

    /// Hammer the raise/wait race on a genuinely parallel runtime.
    ///
    /// Each iteration spawns a fresh waiter and raiser with no
    /// synchronisation between them, so across many iterations the
    /// raise lands at every point of the wait sequence — including
    /// inside the window between the flag check and the first poll,
    /// which the deterministic tests above cannot reach. Against the
    /// buggy check-then-await ordering (no `enable()`), iterations
    /// where the raise lands in that window hang the waiter and trip
    /// the per-iteration timeout. Probabilistic rather than
    /// exhaustive, but it is the only test in the suite with a
    /// non-zero chance of observing the schedule that caused the
    /// original reaper bug.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_raise_and_wait_never_lose_the_wakeup() {
        for i in 0..5_000 {
            let signal = Arc::new(StickySignal::new());

            let waiter = tokio::spawn({
                let signal = Arc::clone(&signal);
                async move { signal.wait().await }
            });
            let raiser = tokio::spawn({
                let signal = Arc::clone(&signal);
                async move {
                    signal.raise();
                }
            });

            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "iteration {i}: waiter hung — a raise landing between \
                         the flag check and the first poll was lost"
                    )
                })
                .expect("waiter task must not panic");
            raiser.await.expect("raiser task must not panic");
        }
    }

    /// Only the first raise reports having done the raising;
    /// subsequent raises are no-ops. Producers hang their
    /// exactly-once work off this return value.
    #[tokio::test]
    async fn raise_is_exactly_once() {
        let signal = StickySignal::new();
        assert!(!signal.is_raised());
        assert!(signal.raise());
        assert!(!signal.raise());
        assert!(!signal.raise());
        assert!(signal.is_raised());
    }
}
