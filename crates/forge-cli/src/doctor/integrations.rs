//! The event feed checks of `forge doctor` (M3 I1c, ADR-0016 §11, runbook
//! integration-feed-lag):
//!
//! - `integrations.feed_lag` (`FG-INT-0009`, warn): every enabled
//!   integration with a binding read its feed within 24 hours, and its
//!   oldest unread event is younger than half the retention period.
//! - `subscribers.work-item-sync`: the feed's bound is `min(high_seq,
//!   work-item-sync)`; a missing or disabled subscriber stops the feed
//!   (warn), its lag is shown (a stuck one is `subscribers.lag`).
//! - `events.retention`: the policy and `retained_from`; warn when events
//!   older than 1.5 × the period are left (the worker's `events_retention`
//!   task is not running).
//! - `feed.sink_compare`: warn while `feed.require_sink_compare` is false
//!   (always in M3, plan 11 Q1: the seal sink comes with C9).

use forge_core::config::Config;
use forge_db::feed;
use forge_db::Db;

use super::{fail, ok, skip, warn, Check};

const FEED_LAG: &str = "integrations.feed_lag";
const WATERMARK: &str = "subscribers.work-item-sync";
const RETENTION: &str = "events.retention";
const SINK_COMPARE: &str = "feed.sink_compare";

/// The names, for the database-unavailable skips.
pub(super) const NAMES: [&str; 3] = [FEED_LAG, WATERMARK, RETENTION];

pub(super) async fn feed_checks(db: &Db) -> [Check; 3] {
    let lag = match feed::lag(db).await {
        Ok(Some(lag)) => match lag.check() {
            Ok(()) => ok(
                FEED_LAG,
                &format!(
                    "{} integration(s) with bindings read within {} h (retention {} days)",
                    lag.integrations,
                    feed::NOT_READING_AFTER_HOURS,
                    lag.retention_days
                ),
            ),
            Err(e) => fail(FEED_LAG, e),
        },
        Ok(None) => skip(FEED_LAG, "schema not migrated"),
        Err(e) => fail(FEED_LAG, e),
    };
    let watermark = match feed::watermark(db).await {
        Ok(Some(w)) => watermark_line(&w),
        Ok(None) => skip(WATERMARK, "no feed state yet (the sequencer creates it)"),
        Err(e) => fail(WATERMARK, e),
    };
    let retention = match feed::retention(db).await {
        Ok(Some(r)) => retention_line(&r),
        Ok(None) => skip(RETENTION, "no feed state yet"),
        Err(e) => fail(RETENTION, e),
    };
    [lag, watermark, retention]
}

fn watermark_line(w: &feed::WatermarkHealth) -> Check {
    match w.last_seq {
        None => warn(
            WATERMARK,
            "not registered: the integration feed returns no event until forge worker starts it",
        ),
        Some(_) if !w.enabled => warn(
            WATERMARK,
            "disabled: the integration feed stops at its last position (runbook subscriber-lag)",
        ),
        Some(last) => ok(
            WATERMARK,
            &format!(
                "last_seq {last} of high_seq {} (the feed's bound; {} behind)",
                w.high_seq,
                (w.high_seq - last).max(0)
            ),
        ),
    }
}

fn retention_line(r: &feed::RetentionHealth) -> Check {
    let oldest = r.oldest_age_seconds.map_or_else(
        || "no event".to_owned(),
        |s| format!("oldest {} days", s / 86_400),
    );
    let detail = format!(
        "{} days kept; retained_from {}; {oldest}",
        r.events_days, r.retained_from
    );
    if r.keeps_up() {
        ok(RETENTION, &detail)
    } else {
        warn(
            RETENTION,
            &format!("{detail}: older than 1.5 × the period (is the worker's events_retention task running?)"),
        )
    }
}

/// `feed.sink_compare` (configuration only).
pub(super) fn sink_compare_check(cfg: &Config) -> Check {
    if cfg.feed.require_sink_compare {
        ok(SINK_COMPARE, "the feed waits for the seal sink comparison")
    } else {
        warn(
            SINK_COMPARE,
            "feed.require_sink_compare=false: the integration feed answers without comparing the audit seal \
             with the seal sink (not built yet, C9); after a database restore run `forge admin events \
             new-epoch` (runbook integration-feed-lag)",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Outcome;

    #[test]
    fn a_missing_or_disabled_work_item_sync_warns() {
        let missing = watermark_line(&feed::WatermarkHealth {
            last_seq: None,
            enabled: false,
            high_seq: 9,
        });
        assert!(matches!(missing.outcome, Outcome::Warn(_)));
        let disabled = watermark_line(&feed::WatermarkHealth {
            last_seq: Some(3),
            enabled: false,
            high_seq: 9,
        });
        assert!(matches!(disabled.outcome, Outcome::Warn(_)));
        let running = watermark_line(&feed::WatermarkHealth {
            last_seq: Some(9),
            enabled: true,
            high_seq: 9,
        });
        assert!(matches!(running.outcome, Outcome::Ok(_)));
    }

    #[test]
    fn retention_that_does_not_keep_up_warns() {
        let behind = retention_line(&feed::RetentionHealth {
            events_days: 90,
            retained_from: 1,
            oldest_age_seconds: Some(200 * 86_400),
        });
        assert!(matches!(behind.outcome, Outcome::Warn(_)));
        let fine = retention_line(&feed::RetentionHealth {
            events_days: 90,
            retained_from: 1,
            oldest_age_seconds: Some(10 * 86_400),
        });
        assert!(matches!(fine.outcome, Outcome::Ok(_)));
    }
}
