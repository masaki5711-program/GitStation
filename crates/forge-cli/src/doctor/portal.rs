//! `forge doctor` for the PFS portal (ADR-0036, runbook portal-widgets):
//!
//! - `portal.news` (database): skipped while news is off; a news item
//!   given up in the last 24 hours is `FG-INT-0023` (alert); one pending
//!   for more than an hour is `FG-INT-0022` (the lane is stopped or every
//!   attempt fails); otherwise the pending count.

use forge_core::config::Config;
use forge_db::portal::NewsHealth;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, skip, Check};

pub(super) const NEWS: &str = "portal.news";

/// A news item pending longer than this: every attempt fails, or no
/// worker sends (the last of 8 attempts runs about 21 minutes after the
/// first).
const PENDING_WARN_S: i64 = 3600;

/// `portal.news`.
pub(super) async fn news_check(db: &Db, cfg: &Config) -> Check {
    if !cfg.portal.news_on() {
        return skip(
            NEWS,
            "portal.enabled / portal.news_enabled is off: no news is written or sent",
        );
    }
    match forge_db::portal::health(db).await {
        Ok(h) => news_line(&h),
        Err(e) => fail(NEWS, e),
    }
}

fn news_line(h: &NewsHealth) -> Check {
    if h.failed_24h > 0 {
        return fail(
            NEWS,
            ForgeError::new(ErrorCode::PortalNewsUndeliverable)
                .with_context("reason", "failed_recently")
                .with_context("news_failed_24h", h.failed_24h),
        );
    }
    if h.oldest_pending_s.is_some_and(|s| s > PENDING_WARN_S) {
        return fail(
            NEWS,
            ForgeError::new(ErrorCode::PortalNewsAttemptFailed)
                .with_context("reason", "pending_too_long")
                .with_context("pending", h.pending)
                .with_context("oldest_pending_s", h.oldest_pending_s.unwrap_or_default()),
        );
    }
    ok(
        NEWS,
        &format!("{} news item(s) pending; none given up in 24 h", h.pending),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_news_line_reports_what_waits_and_what_failed() {
        let quiet = news_line(&NewsHealth::default());
        assert!(matches!(quiet.outcome, super::super::Outcome::Ok(_)));
        let failed = news_line(&NewsHealth {
            failed_24h: 2,
            ..NewsHealth::default()
        });
        let super::super::Outcome::Fail(e) = failed.outcome else {
            panic!("a failed news item is an alert");
        };
        assert_eq!(e.code(), ErrorCode::PortalNewsUndeliverable);
        let late = news_line(&NewsHealth {
            pending: 1,
            oldest_pending_s: Some(7200),
            failed_24h: 0,
        });
        let super::super::Outcome::Fail(e) = late.outcome else {
            panic!("a news item pending for hours is reported");
        };
        assert_eq!(e.code(), ErrorCode::PortalNewsAttemptFailed);
    }
}
