//! The e-mail checks of `forge doctor` (M3 N1, ADR-0016 Amendment N1,
//! runbook notification-email). Read-only: nothing is sent.
//!
//! - `notify.queue` (`FG-INT-0015`, warn) — from `forge.notifications`: the
//!   oldest notification still waiting is older than
//!   [`BACKLOG_AFTER`] (`reason=backlog`: no worker runs the lane, or the
//!   relay is down and the tries have not run out yet), or any failed
//!   (`reason=failed_recently`) or ended with an unknown outcome
//!   (`reason=outcome_unknown`) in the last 24 hours. With
//!   `notify.enabled = false` the line says so and nothing is judged: no
//!   notification is recorded then.
//! - `notify.relay` (`FG-INT-0015`, warn) — a TCP connection to
//!   `notify.smtp_host:notify.smtp_port` within `notify.connect_timeout_ms`
//!   (the connection is closed at once; no SMTP command is sent). A relay
//!   the worker's host cannot reach is the first thing to know.

use std::time::Duration;

use forge_core::config::NotifyConfig;
use forge_db::notify::{self, Backlog};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use tokio::net::TcpStream;

use super::{fail, ok, skip, Check};

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
pub(super) fn queue_line(backlog: &Backlog) -> Check {
    let oldest = backlog.oldest_waiting_secs.unwrap_or(0);
    let detail = format!(
        "{} waiting (oldest {} s), {} failed and {} unknown in the last 24 h",
        backlog.waiting, oldest, backlog.failed_24h, backlog.unknown_24h
    );
    let limit = i64::try_from(BACKLOG_AFTER.as_secs()).unwrap_or(i64::MAX);
    if oldest > limit {
        return fail(
            QUEUE,
            problem("backlog")
                .with_context("waiting", backlog.waiting)
                .with_context("oldest_waiting_secs", oldest),
        );
    }
    if backlog.failed_24h > 0 {
        return fail(
            QUEUE,
            problem("failed_recently").with_context("failed_24h", backlog.failed_24h),
        );
    }
    if backlog.unknown_24h > 0 {
        return fail(
            QUEUE,
            problem("outcome_unknown").with_context("unknown_24h", backlog.unknown_24h),
        );
    }
    ok(QUEUE, &detail)
}

/// `notify.queue` and `notify.relay`.
pub(super) async fn notify_checks(db: &Db, cfg: &NotifyConfig) -> [Check; 2] {
    let Some(smtp) = cfg.smtp.as_ref().filter(|_| cfg.enabled) else {
        let off = "notify.enabled=false: merge request events record no notifications and no mail is sent";
        return [skip(QUEUE, off), skip(RELAY, off)];
    };
    let queue = match notify::backlog(db).await {
        Ok(backlog) => queue_line(&backlog),
        Err(e) => fail(QUEUE, e),
    };
    let relay = relay_line(&smtp.host, smtp.port, cfg.connect_timeout).await;
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
            unknown_24h: unknown,
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
            queue_line(&backlog(0, None, 0, 0)).outcome,
            Outcome::Ok(_)
        ));
        // Waiting briefly is normal.
        assert!(matches!(
            queue_line(&backlog(3, Some(30), 0, 0)).outcome,
            Outcome::Ok(_)
        ));
        for (b, why) in [
            (backlog(2, Some(601), 0, 0), "backlog"),
            (backlog(0, None, 1, 0), "failed_recently"),
            (backlog(0, None, 0, 1), "outcome_unknown"),
            // A backlog is named first.
            (backlog(5, Some(9_999), 4, 4), "backlog"),
        ] {
            let line = queue_line(&b);
            assert_eq!(reason(&line).as_deref(), Some(why), "{b:?}");
            let Outcome::Fail(e) = &line.outcome else {
                panic!("expected a failure");
            };
            assert_eq!(e.code(), ErrorCode::SmtpUnavailable);
        }
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
