//! The seal backlog (ADR-0018 §4, `FG-DB-0015`): an audit row older than
//! [`BACKLOG_AFTER`] that no seal covers — numbered after the last seal, or
//! still waiting for its number (audit sequencing off or stalled). Read on
//! the read pool in one statement, counts capped like the sequencer's.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use crate::sequencer::{shown, COUNT_CAP};
use crate::{read_query_error, Db};

/// An unsealed row older than this is a backlog.
pub const BACKLOG_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealBacklog {
    /// `None`: nothing is sealed yet.
    pub last_sealed_seq: Option<i64>,
    /// `None`: no sequencer has run yet.
    pub audit_high_seq: Option<i64>,
    pub audit_sequencing: bool,
    /// Numbered rows after the last seal (at most [`COUNT_CAP`] + 1).
    pub unsealed_numbered: i64,
    /// Rows without a number (at most [`COUNT_CAP`] + 1).
    pub unsequenced: i64,
    /// Age of the oldest unsealed row; `None` when every row is sealed.
    pub oldest_age_ms: Option<i64>,
}

impl SealBacklog {
    /// `FG-DB-0015` when the oldest unsealed row is older than
    /// [`BACKLOG_AFTER`].
    pub fn check(&self) -> Result<(), ForgeError> {
        let threshold = i64::try_from(BACKLOG_AFTER.as_millis()).unwrap_or(i64::MAX);
        let Some(age) = self.oldest_age_ms.filter(|age| *age > threshold) else {
            return Ok(());
        };
        let or_none = |v: Option<i64>| v.map_or_else(|| "none".to_owned(), |n| n.to_string());
        Err(ForgeError::new(ErrorCode::AuditSealBacklog)
            .with_context("oldest_age_s", age / 1000)
            .with_context("last_sealed_seq", or_none(self.last_sealed_seq))
            .with_context("audit_high_seq", or_none(self.audit_high_seq))
            .with_context("unsealed_numbered", shown(self.unsealed_numbered))
            .with_context("unsequenced", shown(self.unsequenced))
            .with_context(
                "audit_sequencing",
                if self.audit_sequencing { "on" } else { "off" },
            ))
    }
}

/// The backlog, or `None` before the seal migration (no forge.audit_seal).
pub async fn backlog(db: &Db) -> Result<Option<SealBacklog>, ForgeError> {
    db.within("seal.backlog", async {
        let mut conn = db.acquire().await?;
        let migrated: bool =
            sqlx::query_scalar("SELECT pg_catalog.to_regclass('forge.audit_seal') IS NOT NULL")
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| read_query_error("seal.backlog", e))?;
        if !migrated {
            return Ok(None);
        }
        let row: (Option<i64>, Option<i64>, bool, i64, i64, Option<i64>) = sqlx::query_as(
            "SELECT p.last_seq, f.audit_high_seq, coalesce(f.audit_sequencing_enabled, false), \
                    (SELECT pg_catalog.count(*) FROM (SELECT 1 FROM ONLY forge.audit_log AS a \
                     WHERE a.seq > coalesce(p.last_seq, 0) LIMIT $1) AS w), \
                    (SELECT pg_catalog.count(*) FROM (SELECT 1 FROM ONLY forge.audit_log AS a \
                     WHERE a.seq IS NULL LIMIT $1) AS w), \
                    (EXTRACT(epoch FROM pg_catalog.now() - LEAST( \
                        (SELECT a.occurred_at FROM ONLY forge.audit_log AS a \
                          WHERE a.seq > coalesce(p.last_seq, 0) ORDER BY a.seq LIMIT 1), \
                        (SELECT a.occurred_at FROM ONLY forge.audit_log AS a \
                          WHERE a.seq IS NULL ORDER BY a.id LIMIT 1))) * 1000)::bigint \
             FROM (SELECT 1) AS one \
             LEFT JOIN LATERAL (SELECT s.last_seq FROM ONLY forge.audit_seal AS s \
                                ORDER BY s.last_seq DESC LIMIT 1) AS p ON true \
             LEFT JOIN ONLY forge.event_feed_state AS f ON f.singleton",
        )
        .bind(COUNT_CAP + 1)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("seal.backlog", e))?;
        Ok(Some(SealBacklog {
            last_sealed_seq: row.0,
            audit_high_seq: row.1,
            audit_sequencing: row.2,
            unsealed_numbered: row.3,
            unsequenced: row.4,
            oldest_age_ms: row.5,
        }))
    })
    .await
}

#[cfg(test)]
mod unit {
    use super::*;

    fn backlog(age: Option<i64>) -> SealBacklog {
        SealBacklog {
            last_sealed_seq: None,
            audit_high_seq: Some(4),
            audit_sequencing: false,
            unsealed_numbered: 4,
            unsequenced: 2,
            oldest_age_ms: age,
        }
    }

    #[test]
    fn only_a_row_older_than_five_minutes_is_a_backlog() {
        assert!(backlog(None).check().is_ok());
        assert!(backlog(Some(300_000)).check().is_ok());
        let err = backlog(Some(300_001)).check().expect_err("backlog");
        assert_eq!(err.code(), ErrorCode::AuditSealBacklog);
        let text = err.to_string();
        for part in [
            "oldest_age_s=300",
            "last_sealed_seq=none",
            "audit_high_seq=4",
            "audit_sequencing=off",
        ] {
            assert!(text.contains(part), "{text}");
        }
    }
}
