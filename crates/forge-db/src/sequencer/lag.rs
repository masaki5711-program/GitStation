//! The sequencer's backlog and lag (ADR-0016 §7, `FG-DB-0005`): committed
//! rows wait for a number and the heartbeat is older than [`LAG_AFTER`].
//! Audit rows count only while audit sequencing is enabled (while it is
//! intentionally off they are expected to wait).

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use super::LOCK_KEY;
use crate::{read_query_error, Db};

/// A backlog with a heartbeat older than this is lag.
pub const LAG_AFTER: Duration = Duration::from_secs(30);

/// What waits for a sequence number, read in one statement on the read pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlog {
    pub unsequenced_events: i64,
    /// `None` while audit sequencing is off (the rows are not counted).
    pub unsequenced_audit: Option<i64>,
    /// `None`: no sequencer has run in this database yet.
    pub heartbeat_age_ms: Option<i64>,
    /// The session holding the sequencer's advisory lock, if any.
    pub lock_holder_pid: Option<i32>,
}

impl Backlog {
    /// The rows that count for lag.
    pub fn unsequenced(&self) -> i64 {
        self.unsequenced_events
            .saturating_add(self.unsequenced_audit.unwrap_or(0))
    }

    /// `FG-DB-0005` when rows wait and the heartbeat is stale or missing.
    pub fn check_lag(&self) -> Result<(), ForgeError> {
        let threshold = i64::try_from(LAG_AFTER.as_millis()).unwrap_or(i64::MAX);
        let stale = self.heartbeat_age_ms.is_none_or(|age| age > threshold);
        if self.unsequenced() == 0 || !stale {
            return Ok(());
        }
        let or_none = |v: Option<String>| v.unwrap_or_else(|| "none".to_owned());
        Err(ForgeError::new(ErrorCode::OutboxSequencerLag)
            .with_context("unsequenced_count", self.unsequenced())
            .with_context("unsequenced_events", self.unsequenced_events)
            .with_context(
                "unsequenced_audit",
                self.unsequenced_audit
                    .map_or_else(|| "off".to_owned(), |n| n.to_string()),
            )
            .with_context(
                "heartbeat_age_ms",
                or_none(self.heartbeat_age_ms.map(|a| a.to_string())),
            )
            .with_context(
                "lock_holder_pid",
                or_none(self.lock_holder_pid.map(|p| p.to_string())),
            ))
    }
}

/// The backlog, or `None` before the first schema migration (no tables).
pub async fn backlog(db: &Db) -> Result<Option<Backlog>, ForgeError> {
    db.within("sequencer.backlog", async {
        let mut conn = db.acquire().await?;
        let migrated: bool = sqlx::query_scalar(
            "SELECT pg_catalog.to_regclass('forge.event_feed_state') IS NOT NULL",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("sequencer.backlog", e))?;
        if !migrated {
            return Ok(None);
        }
        let row: (i64, Option<i64>, Option<i64>, Option<i32>) = sqlx::query_as(
            "SELECT (SELECT pg_catalog.count(*) FROM ONLY forge.events WHERE seq IS NULL), \
                    CASE WHEN s.audit_sequencing_enabled THEN \
                        (SELECT pg_catalog.count(*) FROM ONLY forge.audit_log WHERE seq IS NULL) \
                    END, \
                    (EXTRACT(epoch FROM pg_catalog.now() - s.sequencer_heartbeat) \
                        * 1000)::bigint, \
                    (SELECT l.pid FROM pg_catalog.pg_locks l \
                      WHERE l.locktype = 'advisory' AND l.granted \
                        AND l.classid = $1::bigint::oid AND l.objid = $2::bigint::oid \
                        AND l.objsubid = 2 \
                        AND l.database = (SELECT d.oid FROM pg_catalog.pg_database d \
                                          WHERE d.datname = pg_catalog.current_database()) \
                      LIMIT 1) \
             FROM (SELECT 1) AS one \
             LEFT JOIN ONLY forge.event_feed_state AS s ON s.singleton",
        )
        .bind(LOCK_KEY.0)
        .bind(LOCK_KEY.1)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("sequencer.backlog", e))?;
        Ok(Some(Backlog {
            unsequenced_events: row.0,
            unsequenced_audit: row.1,
            heartbeat_age_ms: row.2,
            lock_holder_pid: row.3,
        }))
    })
    .await
}

#[cfg(test)]
mod unit {
    use super::*;

    fn backlog(events: i64, audit: Option<i64>, age: Option<i64>) -> Backlog {
        Backlog {
            unsequenced_events: events,
            unsequenced_audit: audit,
            heartbeat_age_ms: age,
            lock_holder_pid: None,
        }
    }

    #[test]
    fn lag_needs_both_a_backlog_and_a_stale_heartbeat() {
        assert!(backlog(0, None, None).check_lag().is_ok());
        assert!(backlog(0, Some(0), Some(90_000)).check_lag().is_ok());
        assert!(backlog(3, None, Some(30_000)).check_lag().is_ok());
        assert!(backlog(3, None, Some(30_001)).check_lag().is_err());
        assert!(backlog(3, None, None).check_lag().is_err());
        assert!(backlog(0, Some(1), Some(31_000)).check_lag().is_err());
        let text = backlog(0, Some(2), None)
            .check_lag()
            .expect_err("lag")
            .to_string();
        assert!(text.contains("heartbeat_age_ms=none"), "{text}");
        assert!(text.contains("lock_holder_pid=none"), "{text}");
    }
}
