//! The e-mail checks of `forge doctor` (M3 N1, ADR-0016 Amendment N1,
//! runbook notification-email). Read-only: nothing is sent.
//!
//! - `notify.queue` (`FG-INT-0015`, warn) — from `forge.notifications`, read
//!   **whatever `notify.enabled` says** (what was left behind when mail was
//!   switched off is still there): the oldest notification still waiting is
//!   older than [`BACKLOG_AFTER`] (`reason=backlog`), or any failed in the
//!   last 24 hours (`reason=failed_recently`), or any ended with an unknown
//!   outcome in the last 30 days (`reason=outcome_unknown`, with the age of
//!   the oldest: there is no acknowledgement, so after 30 days it is
//!   history). A plain warning (no code) when notifications were suppressed in
//!   the last 24 hours because an address is missing or unusable — data to
//!   fix, apart from the people's own choices. With mail off and nothing to
//!   report the line is information.
//! - `notify.relay` (`FG-INT-0015`, warn) — a TCP connection to
//!   `notify.smtp_host:notify.smtp_port` within `notify.connect_timeout_ms`
//!   (the connection is closed at once; no SMTP command is sent). Only when
//!   mail is on.

use std::time::Duration;

use forge_core::config::NotifyConfig;
use forge_db::notify::{self, Backlog};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use tokio::net::TcpStream;

use super::{fail, ok, skip, warn, Check};

pub(super) const QUEUE: &str = "notify.queue";
pub(super) const RELAY: &str = "notify.relay";
/// The names, for the database-unavailable skips.
pub(super) const NAMES: [&str; 2] = [QUEUE, RELAY];

/// A notification waiting longer than this is a backlog (a send takes
/// seconds; a retried one waits at most the queue's backoff, minutes).
pub(super) const BACKLOG_AFTER: Duration = Duration::from_secs(600);

fn problem(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SmtpUnavailable).with_context("reason", reason)
}

/// The `notify.queue` line of `backlog` (pure: the tests feed it directly).
/// `enabled` only changes how a quiet queue is shown.
pub(super) fn queue_line(enabled: bool, backlog: &Backlog) -> Check {
    let oldest = backlog.oldest_waiting_secs.unwrap_or(0);
    let detail = format!(
        "{} waiting (oldest {} s), {} failed in the last 24 h, {} unknown in the last 30 days (oldest {} s), {} suppressed for an unusable address in the last 24 h",
        backlog.waiting,
        oldest,
        backlog.failed_24h,
        backlog.unknown_30d,
        backlog.oldest_unknown_secs.unwrap_or(0),
        backlog.address_suppressed_24h
    );
    let limit = i64::try_from(BACKLOG_AFTER.as_secs()).unwrap_or(i64::MAX);
    if oldest > limit {
        return fail(
            QUEUE,
            problem("backlog")
                .with_context("waiting", backlog.waiting)
                .with_context("oldest_waiting_secs", oldest)
                .with_context("mail_enabled", enabled),
        );
    }
    if backlog.failed_24h > 0 {
        return fail(
            QUEUE,
            problem("failed_recently").with_context("failed_24h", backlog.failed_24h),
        );
    }
    if backlog.unknown_30d > 0 {
        return fail(
            QUEUE,
            problem("outcome_unknown")
                .with_context("unknown_30d", backlog.unknown_30d)
                .with_context(
                    "oldest_unknown_secs",
                    backlog.oldest_unknown_secs.unwrap_or(0),
                ),
        );
    }
    if backlog.address_suppressed_24h > 0 {
        return warn(QUEUE, &format!("unusable addresses: {detail}"));
    }
    if enabled {
        ok(QUEUE, &detail)
    } else {
        skip(
            QUEUE,
            "notify.enabled=false: nothing waiting, nothing recorded, no mail sent",
        )
    }
}

/// `notify.queue` and `notify.relay`.
pub(super) async fn notify_checks(db: &Db, cfg: &NotifyConfig) -> [Check; 2] {
    let queue = match notify::backlog(db).await {
        Ok(backlog) => queue_line(cfg.enabled, &backlog),
        Err(e) => fail(QUEUE, e),
    };
    let relay = match cfg.smtp.as_ref().filter(|_| cfg.enabled) {
        Some(smtp) => relay_line(&smtp.host, smtp.port, cfg.connect_timeout).await,
        None => skip(RELAY, "notify.enabled=false: no relay is used"),
    };
    [queue, relay]
}

/// A connection to the relay and nothing else.
async fn relay_line(host: &str, port: u16, within: Duration) -> Check {
    match tokio::time::timeout(within, TcpStream::connect((host, port))).await {
        Ok(Ok(_closed_at_once)) => ok(RELAY, &format!("{host}:{port} accepts connections")),
        Ok(Err(_)) | Err(_) => fail(
            RELAY,
            problem("connect_failed")
                .with_context("stage", "connect")
                .with_context("host", host)
                .with_context("port", port),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)] // tests may fail loudly
mod tests {
    use super::*;
    use crate::doctor::Outcome;
    use forge_notify::test_support::{FakeSmtp, Script};

    fn backlog(waiting: i64, oldest: Option<i64>, failed: i64, unknown: i64) -> Backlog {
        Backlog {
            waiting,
            oldest_waiting_secs: oldest,
            failed_24h: failed,
            unknown_30d: unknown,
            oldest_unknown_secs: (unknown > 0).then_some(3 * 86_400),
            address_suppressed_24h: 0,
        }
    }

    fn reason(check: &Check) -> Option<String> {
        match &check.outcome {
            Outcome::Fail(e) => e
                .context()
                .iter()
                .find(|(k, _)| *k == "reason")
                .map(|(_, v)| v.to_string()),
            Outcome::Ok(_) | Outcome::Warn(_) | Outcome::Skip(_) => None,
        }
    }

    #[test]
    fn a_quiet_queue_is_ok_and_each_problem_has_its_reason() {
        assert!(matches!(
            queue_line(true, &backlog(0, None, 0, 0)).outcome,
            Outcome::Ok(_)
        ));
        // Waiting briefly is normal.
        assert!(matches!(
            queue_line(true, &backlog(3, Some(30), 0, 0)).outcome,
            Outcome::Ok(_)
        ));
        for (b, why) in [
            (backlog(2, Some(601), 0, 0), "backlog"),
            (backlog(0, None, 1, 0), "failed_recently"),
            (backlog(0, None, 0, 1), "outcome_unknown"),
            // A backlog is named first.
            (backlog(5, Some(9_999), 4, 4), "backlog"),
        ] {
            let line = queue_line(true, &b);
            assert_eq!(reason(&line).as_deref(), Some(why), "{b:?}");
            let Outcome::Fail(e) = &line.outcome else {
                panic!("expected a failure");
            };
            assert_eq!(e.code(), ErrorCode::SmtpUnavailable);
        }
    }

    #[test]
    fn an_old_unknown_unusable_addresses_and_a_disabled_lane_are_reported() {
        // An unknown outcome three days old is still reported (not only 24 h).
        let line = queue_line(true, &backlog(0, None, 0, 1));
        assert_eq!(reason(&line).as_deref(), Some("outcome_unknown"));
        // Unusable addresses are a warning of their own, apart from choices.
        let mut b = backlog(0, None, 0, 0);
        b.address_suppressed_24h = 2;
        assert!(matches!(queue_line(true, &b).outcome, Outcome::Warn(_)));
        // With mail off a quiet queue is information, a leftover is reported.
        assert!(matches!(
            queue_line(false, &backlog(0, None, 0, 0)).outcome,
            Outcome::Skip(_)
        ));
        let leftover = queue_line(false, &backlog(4, Some(5_000), 0, 0));
        assert_eq!(reason(&leftover).as_deref(), Some("backlog"));
        let unknown = queue_line(false, &backlog(0, None, 0, 2));
        assert_eq!(reason(&unknown).as_deref(), Some("outcome_unknown"));
    }

    #[tokio::test]
    async fn the_relay_line_connects_and_closes_without_sending_anything() {
        let relay = FakeSmtp::start(Script::default()).await;
        let addr = relay.addr();
        let line = relay_line(&addr.ip().to_string(), addr.port(), Duration::from_secs(2)).await;
        assert!(matches!(line.outcome, Outcome::Ok(_)));
        // Give the fake a moment to see the connection end.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(relay.messages().is_empty());
        assert!(
            !relay.commands().iter().any(|c| c.starts_with("MAIL")),
            "{:?}",
            relay.commands()
        );
        // A port nobody listens on.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let line = relay_line("127.0.0.1", closed, Duration::from_secs(2)).await;
        assert_eq!(reason(&line).as_deref(), Some("connect_failed"));
    }

    #[tokio::test]
    async fn a_disabled_lane_is_information_and_never_touches_the_database() {
        // No database is needed: the lane is off.
        let t = forge_db::test_support::FreshDb::migrated().await;
        let cfg = NotifyConfig::default();
        let [queue, relay] = notify_checks(&t.db, &cfg).await;
        // The backlog is read even when mail is off (nothing waits here).
        for line in [&queue, &relay] {
            assert!(matches!(line.outcome, Outcome::Skip(_)),);
        }
        // Enabled with a relay: both are judged.
        let fake = FakeSmtp::start(Script::default()).await;
        let [queue, relay] = notify_checks(&t.db, &fake.config()).await;
        assert!(matches!(queue.outcome, Outcome::Ok(_)),);
        assert!(matches!(relay.outcome, Outcome::Ok(_)),);
        t.drop_now().await;
    }
}
