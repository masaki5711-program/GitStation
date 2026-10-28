//! What `forge doctor` reports about subscribers (ADR-0016 §11), read on
//! the read pool:
//! - `subscribers.lag` ([`states`], [`check_lag`]): an `ordered` subscriber
//!   at [`FAILURES_BEFORE_STOP`] failures is `FG-DB-0008 reason=stuck`; an
//!   enabled one whose next event was numbered more than [`BEHIND_AFTER`]
//!   ago is `reason=behind`.
//! - `subscribers.dead_letters` ([`dead_letters`], [`DeadLetters::check`]):
//!   events quarantined in the last 24 hours are `reason=quarantined`
//!   (older ones are listed, not alerted: they stay 30 days for inspection).

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use super::{Ordering, FAILURES_BEFORE_STOP};
use crate::{read_query_error, Db};

/// A next event older than this means the subscriber does not keep up.
pub const BEHIND_AFTER: Duration = Duration::from_secs(60);

/// One subscriber as doctor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriberState {
    pub name: String,
    pub ordering: Ordering,
    pub enabled: bool,
    pub last_seq: i64,
    /// `high_seq - last_seq` (0 when caught up).
    pub lag_events: i64,
    pub failures: i32,
    pub last_error_code: Option<String>,
    /// How long ago the next event (`last_seq + 1`) was numbered.
    pub oldest_pending_age_ms: Option<i64>,
    pub next_event_type: Option<String>,
}

impl SubscriberState {
    fn finding(&self, reason: &'static str) -> ForgeError {
        let or_none = |v: Option<String>| v.unwrap_or_else(|| "none".to_owned());
        ForgeError::new(ErrorCode::SubscriberLag)
            .with_context("reason", reason)
            .with_context("subscriber", &self.name)
            .with_context("seq", self.last_seq + 1)
            .with_context("event_type", or_none(self.next_event_type.clone()))
            .with_context("lag_events", self.lag_events)
            .with_context("failures", self.failures)
            .with_context("last_error_code", or_none(self.last_error_code.clone()))
            .with_context(
                "oldest_pending_age_ms",
                or_none(self.oldest_pending_age_ms.map(|a| a.to_string())),
            )
    }

    fn stuck(&self) -> bool {
        self.enabled && self.ordering == Ordering::Ordered && self.failures >= FAILURES_BEFORE_STOP
    }

    fn behind(&self) -> bool {
        let threshold = i64::try_from(BEHIND_AFTER.as_millis()).unwrap_or(i64::MAX);
        self.enabled
            && self.lag_events > 0
            && self
                .oldest_pending_age_ms
                .is_some_and(|age| age > threshold)
    }
}

/// `FG-DB-0008`: the first stuck subscriber, else the first one behind.
pub fn check_lag(states: &[SubscriberState]) -> Result<(), ForgeError> {
    if let Some(s) = states.iter().find(|s| s.stuck()) {
        return Err(s.finding("stuck"));
    }
    if let Some(s) = states.iter().find(|s| s.behind()) {
        return Err(s.finding("behind"));
    }
    Ok(())
}

type StateRow = (
    String,
    String,
    bool,
    i64,
    i64,
    i32,
    Option<String>,
    Option<i64>,
    Option<String>,
);

/// Every subscriber, or `None` before the schema migration.
pub async fn states(db: &Db) -> Result<Option<Vec<SubscriberState>>, ForgeError> {
    db.within("subscribers.states", async {
        let mut conn = db.acquire().await?;
        if !migrated(&mut conn).await? {
            return Ok(None);
        }
        let rows: Vec<StateRow> = sqlx::query_as(
            "SELECT s.name, s.ordering, s.enabled, s.last_seq, \
                    GREATEST(coalesce(f.high_seq, 0) - s.last_seq, 0), s.failures, \
                    s.last_error_code, \
                    (EXTRACT(epoch FROM pg_catalog.now() - e.sequenced_at) * 1000)::bigint, \
                    e.event_type \
             FROM forge.event_subscribers AS s \
             LEFT JOIN forge.event_feed_state AS f ON f.singleton \
             LEFT JOIN forge.events AS e ON e.seq = s.last_seq + 1 \
             ORDER BY s.name",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("subscribers.states", e))?;
        rows.into_iter()
            .map(|r| {
                let ordering = Ordering::parse(&r.1).ok_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("operation", "subscribers.states")
                        .with_context("reason", "unknown_ordering")
                        .with_context("subscriber", &r.0)
                })?;
                Ok(SubscriberState {
                    name: r.0,
                    ordering,
                    enabled: r.2,
                    last_seq: r.3,
                    lag_events: r.4,
                    failures: r.5,
                    last_error_code: r.6,
                    oldest_pending_age_ms: r.7,
                    next_event_type: r.8,
                })
            })
            .collect::<Result<Vec<_>, ForgeError>>()
            .map(Some)
    })
    .await
}

/// The quarantined events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetters {
    pub total: i64,
    pub last_24h: i64,
    /// The newest one: subscriber, seq, error code.
    pub newest: Option<(String, i64, String)>,
}

impl DeadLetters {
    /// `FG-DB-0008 reason=quarantined` when events were quarantined in the
    /// last 24 hours.
    pub fn check(&self) -> Result<(), ForgeError> {
        let Some((subscriber, seq, code)) = self.newest.as_ref().filter(|_| self.last_24h > 0)
        else {
            return Ok(());
        };
        Err(ForgeError::new(ErrorCode::SubscriberLag)
            .with_context("reason", "quarantined")
            .with_context("subscriber", subscriber)
            .with_context("newest_seq", seq)
            .with_context("last_error_code", code)
            .with_context("count_24h", self.last_24h))
    }
}

/// The dead letters, or `None` before the schema migration.
pub async fn dead_letters(db: &Db) -> Result<Option<DeadLetters>, ForgeError> {
    db.within("subscribers.dead_letters", async {
        let mut conn = db.acquire().await?;
        if !migrated(&mut conn).await? {
            return Ok(None);
        }
        let (total, last_24h): (i64, i64) = sqlx::query_as(
            "SELECT pg_catalog.count(*), \
                    pg_catalog.count(*) FILTER (WHERE quarantined_at > \
                        pg_catalog.now() - interval '24 hours') \
             FROM forge.subscriber_dead_letters",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("subscribers.dead_letters", e))?;
        let newest: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT subscriber, seq, error_code FROM forge.subscriber_dead_letters \
             ORDER BY quarantined_at DESC, seq DESC LIMIT 1",
        )
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| read_query_error("subscribers.dead_letters", e))?;
        Ok(Some(DeadLetters {
            total,
            last_24h,
            newest,
        }))
    })
    .await
}

async fn migrated(conn: &mut sqlx::PgConnection) -> Result<bool, ForgeError> {
    sqlx::query_scalar("SELECT pg_catalog.to_regclass('forge.event_subscribers') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("subscribers.migrated", e))
}

#[cfg(test)]
mod unit {
    use super::*;

    fn state(ordering: Ordering, failures: i32, lag: i64, age: Option<i64>) -> SubscriberState {
        SubscriberState {
            name: "s".to_owned(),
            ordering,
            enabled: true,
            last_seq: 4,
            lag_events: lag,
            failures,
            last_error_code: None,
            oldest_pending_age_ms: age,
            next_event_type: None,
        }
    }

    #[test]
    fn stuck_wins_over_behind_and_idle_is_fine() {
        assert!(check_lag(&[state(Ordering::Ordered, 0, 0, None)]).is_ok());
        assert!(check_lag(&[state(Ordering::Independent, 9, 3, Some(1000))]).is_ok());
        assert!(check_lag(&[state(Ordering::Ordered, 3, 3, Some(60_000))]).is_ok());
        let behind = check_lag(&[state(Ordering::Ordered, 3, 3, Some(60_001))])
            .expect_err("behind")
            .to_string();
        assert!(behind.contains("reason=behind"), "{behind}");
        let stuck = check_lag(&[
            state(Ordering::Independent, 0, 3, Some(90_000)),
            state(Ordering::Ordered, 5, 1, Some(10)),
        ])
        .expect_err("stuck")
        .to_string();
        assert!(stuck.contains("reason=stuck"), "{stuck}");
        assert!(stuck.contains("seq=5"), "{stuck}");
    }
}
