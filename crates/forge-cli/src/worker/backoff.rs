//! The failure policy of a periodic worker task. A failure is logged when a
//! streak starts, when the failure changes (another code, or another
//! `operation` / `query` / `reason` in its context), and again every
//! [`RELOG_EVERY`] while the same failure persists, so a long outage stays
//! visible in the log without flooding it. The next run waits twice as long
//! each time up to a cap; the first success after a streak is logged and
//! restores the normal period.
//!
//! A successful run may ask for the next one at once ([`Next::Immediately`],
//! e.g. the sequencer after a full batch); at most [`MAX_IMMEDIATE`] runs in
//! a row skip the period, then one period is waited, so a task that always
//! has more work cannot spin.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::time::Instant;

/// A persisting failure is logged again after this long.
pub(crate) const RELOG_EVERY: Duration = Duration::from_secs(60);

/// Consecutive runs that may start without waiting the period.
pub(crate) const MAX_IMMEDIATE: u32 = 50;

/// When the next run starts after a successful one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// After the task's period (the normal case).
    AfterPeriod,
    /// At once: the run left work behind (bounded by [`MAX_IMMEDIATE`]).
    Immediately,
}

/// What makes two failures "the same" for logging: the code and the context
/// that names where and why it failed.
///
/// Only `operation` / `query` / `reason` take part, and those keys carry a
/// fixed vocabulary (query names and reason slugs chosen in code, never ids,
/// counts or user input): otherwise every failure would look new and the
/// re-log limit would not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signature {
    code: ErrorCode,
    detail: Vec<(&'static str, String)>,
}

impl Signature {
    const KEYS: [&'static str; 3] = ["operation", "query", "reason"];

    pub(crate) fn of(e: &ForgeError) -> Self {
        Self {
            code: e.code(),
            detail: e
                .context()
                .iter()
                .filter(|(k, _)| Self::KEYS.contains(k))
                .cloned()
                .collect(),
        }
    }
}

/// What the task loop does after one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    /// Wait this long before the next run.
    pub delay: Duration,
    /// Log this failure (new streak, a changed failure, or due again).
    pub log_failure: bool,
    /// Failures in the current streak, this one included (0 after a success).
    pub failures: u32,
    /// A streak of this many failures just ended (log the recovery).
    pub recovered_after: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct Backoff {
    period: Duration,
    max: Duration,
    failures: u32,
    last: Option<Signature>,
    logged_at: Option<Instant>,
    /// Runs in a row that started without waiting the period.
    immediate: u32,
}

impl Backoff {
    pub(crate) fn new(period: Duration, max: Duration) -> Self {
        Self {
            period,
            max,
            failures: 0,
            last: None,
            logged_at: None,
            immediate: 0,
        }
    }

    pub(crate) fn success(&mut self, next: Next) -> Decision {
        let recovered_after = (self.failures > 0).then_some(self.failures);
        self.failures = 0;
        self.last = None;
        self.logged_at = None;
        let delay = match next {
            Next::Immediately if self.immediate < MAX_IMMEDIATE => {
                self.immediate += 1;
                Duration::ZERO
            }
            Next::Immediately | Next::AfterPeriod => {
                self.immediate = 0;
                self.period
            }
        };
        Decision {
            delay,
            log_failure: false,
            failures: 0,
            recovered_after,
        }
    }

    pub(crate) fn failure(&mut self, signature: Signature, now: Instant) -> Decision {
        let changed = self.last.as_ref() != Some(&signature);
        let due = self
            .logged_at
            .is_none_or(|at| now.saturating_duration_since(at) >= RELOG_EVERY);
        let log_failure = changed || due;
        if log_failure {
            self.logged_at = Some(now);
        }
        self.failures = self.failures.saturating_add(1);
        self.last = Some(signature);
        self.immediate = 0;
        // period × 2^failures, capped (the shift is bounded before it overflows).
        let factor = 1u32 << self.failures.min(16);
        Decision {
            delay: self.period.saturating_mul(factor).min(self.max),
            log_failure,
            failures: self.failures,
            recovered_after: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_millis(200);
    const MAX: Duration = Duration::from_secs(5);

    fn sig(code: ErrorCode, reason: &str) -> Signature {
        Signature::of(&ForgeError::new(code).with_context("reason", reason))
    }

    #[test]
    fn a_failure_streak_is_logged_once_and_backs_off_up_to_the_cap() {
        let now = Instant::now();
        let mut b = Backoff::new(PERIOD, MAX);
        let unavailable = sig(ErrorCode::DbUnavailable, "connect_refused");
        let first = b.failure(unavailable.clone(), now);
        assert!(first.log_failure);
        assert_eq!(first.failures, 1);
        assert_eq!(first.delay, Duration::from_millis(400));
        let second = b.failure(unavailable.clone(), now);
        assert!(!second.log_failure);
        assert_eq!(second.delay, Duration::from_millis(800));
        for _ in 0..40 {
            let d = b.failure(unavailable.clone(), now);
            assert!(!d.log_failure);
            assert!(d.delay <= MAX);
        }
        let capped = b.failure(unavailable, now);
        assert_eq!(capped.delay, MAX);
        assert_eq!(capped.failures, 43);
        // A different failure is news.
        assert!(
            b.failure(sig(ErrorCode::DbQueryFailed, "connect_refused"), now)
                .log_failure
        );
    }

    #[test]
    fn a_changed_reason_or_query_is_logged_again() {
        let now = Instant::now();
        let mut b = Backoff::new(PERIOD, MAX);
        assert!(
            b.failure(sig(ErrorCode::DbQueryFailed, "lock_timeout"), now)
                .log_failure
        );
        assert!(
            !b.failure(sig(ErrorCode::DbQueryFailed, "lock_timeout"), now)
                .log_failure
        );
        assert!(
            b.failure(sig(ErrorCode::DbQueryFailed, "permission_denied"), now)
                .log_failure
        );
        let query = |q: &str| {
            Signature::of(
                &ForgeError::new(ErrorCode::DbQueryFailed)
                    .with_context("reason", "permission_denied")
                    .with_context("query", q),
            )
        };
        assert!(b.failure(query("sequencer.assign_seq"), now).log_failure);
        assert!(!b.failure(query("sequencer.assign_seq"), now).log_failure);
        assert!(b.failure(query("sequencer.notify"), now).log_failure);
        // Context outside the signature (e.g. a retry delay) is not news.
        let with_retry = Signature::of(
            &ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("reason", "permission_denied")
                .with_context("query", "sequencer.notify")
                .with_context("retry_in_ms", 800),
        );
        assert!(!b.failure(with_retry, now).log_failure);
    }

    #[test]
    fn a_persisting_failure_is_logged_again_every_minute() {
        let start = Instant::now();
        let mut b = Backoff::new(PERIOD, MAX);
        let s = sig(ErrorCode::DbUnavailable, "connect_refused");
        assert!(b.failure(s.clone(), start).log_failure);
        assert!(
            !b.failure(s.clone(), start + Duration::from_secs(59))
                .log_failure
        );
        let again = b.failure(s.clone(), start + RELOG_EVERY);
        assert!(again.log_failure);
        assert_eq!(again.failures, 3);
        // The next re-log is a minute after the previous log.
        assert!(
            !b.failure(s.clone(), start + RELOG_EVERY + Duration::from_secs(30))
                .log_failure
        );
        assert!(b.failure(s, start + RELOG_EVERY * 2).log_failure);
    }

    #[test]
    fn the_first_success_ends_the_streak_and_restores_the_period() {
        let now = Instant::now();
        let mut b = Backoff::new(PERIOD, MAX);
        assert_eq!(b.success(Next::AfterPeriod).recovered_after, None);
        let s = sig(ErrorCode::DbUnavailable, "connect_refused");
        b.failure(s.clone(), now);
        b.failure(s.clone(), now);
        let ok = b.success(Next::AfterPeriod);
        assert_eq!(ok.recovered_after, Some(2));
        assert_eq!(ok.delay, PERIOD);
        assert_eq!(ok.failures, 0);
        assert!(b.failure(s, now).log_failure);
    }

    #[test]
    fn immediate_runs_skip_the_period_a_bounded_number_of_times_in_a_row() {
        let now = Instant::now();
        let mut b = Backoff::new(PERIOD, MAX);
        for _ in 0..MAX_IMMEDIATE {
            assert_eq!(b.success(Next::Immediately).delay, Duration::ZERO);
        }
        // The bound: one period is waited, then the count starts again.
        assert_eq!(b.success(Next::Immediately).delay, PERIOD);
        assert_eq!(b.success(Next::Immediately).delay, Duration::ZERO);
        // A normal run resets the count.
        assert_eq!(b.success(Next::AfterPeriod).delay, PERIOD);
        for _ in 0..MAX_IMMEDIATE {
            assert_eq!(b.success(Next::Immediately).delay, Duration::ZERO);
        }
        // So does a failure, whose backoff is never skipped.
        let s = sig(ErrorCode::DbUnavailable, "connect_refused");
        assert_eq!(b.failure(s, now).delay, Duration::from_millis(400));
        assert_eq!(b.success(Next::Immediately).delay, Duration::ZERO);
    }
}
