//! The failure policy of a periodic worker task: a failure is logged once
//! when a streak starts (and again only if the error code changes), the
//! next run waits twice as long each time up to a cap, and the first
//! success after a streak is logged and restores the normal period.

use std::time::Duration;

use forge_errors::ErrorCode;

/// What the task loop does after one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    /// Wait this long before the next run.
    pub delay: Duration,
    /// Log this failure (first of a streak, or a different code).
    pub log_failure: bool,
    /// A streak of this many failures just ended (log the recovery).
    pub recovered_after: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct Backoff {
    period: Duration,
    max: Duration,
    failures: u32,
    last_code: Option<ErrorCode>,
}

impl Backoff {
    pub(crate) fn new(period: Duration, max: Duration) -> Self {
        Self {
            period,
            max,
            failures: 0,
            last_code: None,
        }
    }

    pub(crate) fn success(&mut self) -> Decision {
        let recovered_after = (self.failures > 0).then_some(self.failures);
        self.failures = 0;
        self.last_code = None;
        Decision {
            delay: self.period,
            log_failure: false,
            recovered_after,
        }
    }

    pub(crate) fn failure(&mut self, code: ErrorCode) -> Decision {
        let log_failure = self.last_code != Some(code);
        self.failures = self.failures.saturating_add(1);
        self.last_code = Some(code);
        // period × 2^failures, capped (the shift is bounded before it overflows).
        let factor = 1u32 << self.failures.min(16);
        Decision {
            delay: self.period.saturating_mul(factor).min(self.max),
            log_failure,
            recovered_after: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_millis(200);
    const MAX: Duration = Duration::from_secs(5);

    #[test]
    fn a_failure_streak_is_logged_once_and_backs_off_up_to_the_cap() {
        let mut b = Backoff::new(PERIOD, MAX);
        let first = b.failure(ErrorCode::DbUnavailable);
        assert!(first.log_failure);
        assert_eq!(first.delay, Duration::from_millis(400));
        let second = b.failure(ErrorCode::DbUnavailable);
        assert!(!second.log_failure);
        assert_eq!(second.delay, Duration::from_millis(800));
        for _ in 0..40 {
            let d = b.failure(ErrorCode::DbUnavailable);
            assert!(!d.log_failure);
            assert!(d.delay <= MAX);
        }
        assert_eq!(b.failure(ErrorCode::DbUnavailable).delay, MAX);
        // A different failure is news.
        assert!(b.failure(ErrorCode::DbQueryFailed).log_failure);
    }

    #[test]
    fn the_first_success_ends_the_streak_and_restores_the_period() {
        let mut b = Backoff::new(PERIOD, MAX);
        assert_eq!(b.success().recovered_after, None);
        b.failure(ErrorCode::DbUnavailable);
        b.failure(ErrorCode::DbUnavailable);
        let ok = b.success();
        assert_eq!(ok.recovered_after, Some(2));
        assert_eq!(ok.delay, PERIOD);
        assert!(b.failure(ErrorCode::DbUnavailable).log_failure);
    }
}
