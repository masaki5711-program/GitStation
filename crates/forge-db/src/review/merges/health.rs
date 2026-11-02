//! What `forge doctor` reads about merges (review round 1): whether the
//! reconciliation in `forge serve` is keeping up. A live attempt older than
//! the reconciliation's bound plus a margin means no reconciliation ran (or
//! every run failed) since it became due; a `failed` attempt waits for a
//! person (runbook merge-execution).

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// The counts of [`health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergesHealth {
    /// `queued` / `prepared` / `executing` past their bound + the margin.
    pub overdue: i64,
    /// `failed` attempts of the last 7 days (each needs a person).
    pub failed_recent: i64,
    /// Seconds since the oldest overdue attempt became due (0: none).
    pub oldest_overdue_secs: i64,
}

impl MergesHealth {
    /// `FG-REVIEW-0009 reason=reconcile_overdue` while something is overdue,
    /// `reason=ref_state_unknown` while a recent attempt is `failed`.
    pub fn check(&self) -> Result<(), ForgeError> {
        if self.overdue > 0 {
            return Err(ForgeError::new(ErrorCode::MergeExecutionFailed)
                .with_context("reason", "reconcile_overdue")
                .with_context("overdue", self.overdue)
                .with_context("oldest_overdue_secs", self.oldest_overdue_secs));
        }
        if self.failed_recent > 0 {
            return Err(ForgeError::new(ErrorCode::MergeExecutionFailed)
                .with_context("reason", "ref_state_unknown")
                .with_context("failed_recent", self.failed_recent));
        }
        Ok(())
    }
}

/// The counts (`None`: the table is not migrated yet). `idle_after` and
/// `executing_after` are the reconciliation's bounds; `margin` how much
/// later than due an attempt counts as overdue. One statement, read pool.
pub async fn health(
    db: &Db,
    idle_after: Duration,
    executing_after: Duration,
    margin: Duration,
) -> Result<Option<MergesHealth>, ForgeError> {
    const QUERY: &str = "review.merges_health";
    db.within(QUERY, async {
        let exists: bool =
            sqlx::query_scalar("SELECT to_regclass('forge.merge_request_merges') IS NOT NULL")
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
        if !exists {
            return Ok(None);
        }
        let (overdue, failed_recent, oldest): (i64, i64, Option<f64>) = sqlx::query_as(
            "WITH due AS ( \
               SELECT CASE WHEN state = 'executing' \
                 THEN executing_at + make_interval(secs => $2) \
                 ELSE updated_at + make_interval(secs => $1) END AS at \
               FROM forge.merge_request_merges \
               WHERE state IN ('queued', 'prepared', 'executing')) \
             SELECT (SELECT count(*) FROM due WHERE at + make_interval(secs => $3) < now()), \
               (SELECT count(*) FROM forge.merge_request_merges WHERE state = 'failed' \
                 AND finished_at > now() - interval '7 days'), \
               (SELECT EXTRACT(EPOCH FROM now() - min(at))::float8 FROM due \
                 WHERE at + make_interval(secs => $3) < now())",
        )
        .bind(idle_after.as_secs_f64())
        .bind(executing_after.as_secs_f64())
        .bind(margin.as_secs_f64())
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Some(MergesHealth {
            overdue,
            failed_recent,
            // Whole seconds are enough for a report.
            #[expect(clippy::cast_possible_truncation, reason = "seconds of an age")]
            oldest_overdue_secs: oldest.map_or(0, |s| s.max(0.0) as i64),
        }))
    })
    .await
}
