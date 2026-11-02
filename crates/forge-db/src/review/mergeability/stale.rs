//! Ending checks that will not finish (review HIGH): a run that failed for
//! good ([`fail_check`], `job_failed`), rows left `checking` with no live
//! job of an open merge request ([`reap_stale_checks`], `job_stale`), and
//! doctor's counts (also decided rows whose pair is stale or unverified)
//! ([`health`]). See the module documentation of `mergeability`.

use std::time::Duration;

use forge_core::ids::MergeRequestId;
use forge_errors::ForgeError;

use super::{not_checked, ANSWERING, LIVE_JOB, MERGEABILITY_TIMEOUT};
use crate::jobs::LEASE_SLACK;
use crate::{query_error, Db};

/// A row `checking` this long with no live job is stale: the job's time
/// limit and its lease slack have both passed.
pub const STALE_AFTER: Duration =
    Duration::from_secs(MERGEABILITY_TIMEOUT.as_secs() + LEASE_SLACK.as_secs() + 30);

/// End request `seq` of `mr` as `check_failed reason=job_failed` (its job
/// failed for good before it stored a result), if `seq` is still the
/// latest and nothing was decided for it. `true` when written.
pub async fn fail_check(db: &Db, mr: MergeRequestId, seq: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "review.mergeability_fail";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "UPDATE forge.merge_request_mergeability SET status = 'check_failed', \
             reason = 'job_failed', conflicts = '{}', conflicts_truncated = false, \
             updated_at = now() WHERE merge_request_id = $1 AND check_seq = $2 \
             AND status IN ('unchecked', 'checking')",
        )
        .bind(mr.get())
        .bind(seq)
        .execute(&mut *conn)
        .await
        .map(|r| r.rows_affected() > 0)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// The sweep's reaper (worker, every 30 s): rows `checking` for longer than
/// `older_than` whose latest request has no queued or running job become
/// `check_failed reason=job_stale` (at most `limit`); each is logged once
/// at warn with `FG-REVIEW-0003 reason=job_stale`. The (merge request,
/// seq) pairs ended.
pub async fn reap_stale_checks(
    db: &Db,
    older_than: Duration,
    limit: i64,
) -> Result<Vec<(MergeRequestId, i64)>, ForgeError> {
    const QUERY: &str = "review.mergeability_reap";
    let millis = i64::try_from(older_than.as_millis()).unwrap_or(i64::MAX);
    let rows: Vec<(i64, i64)> = db
        .within(QUERY, async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            sqlx::query_as(&format!(
                "UPDATE forge.merge_request_mergeability SET status = 'check_failed', \
                 reason = 'job_stale', conflicts = '{{}}', conflicts_truncated = false, \
                 updated_at = now() WHERE merge_request_id IN ( \
                 SELECT g.merge_request_id FROM forge.merge_request_mergeability g \
                 JOIN forge.merge_requests m ON m.id = g.merge_request_id \
                 WHERE g.status = 'checking' AND m.state = 'open' \
                 AND g.updated_at < now() - make_interval(secs => $1::double precision / 1000) \
                 AND NOT {LIVE_JOB} ORDER BY g.merge_request_id LIMIT $2 \
                 FOR UPDATE OF g SKIP LOCKED) RETURNING merge_request_id, check_seq"
            ))
            .bind(millis)
            .bind(limit)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let reaped: Vec<(MergeRequestId, i64)> = rows
        .into_iter()
        .map(|(mr, seq)| (MergeRequestId::new(mr), seq))
        .collect();
    for (mr, seq) in &reaped {
        let e = not_checked("job_stale");
        tracing::warn!(
            target: "forge_db",
            event = "forge.review.mergeability_not_checked",
            error_code = e.code().id(),
            reason = "job_stale",
            merge_request_id = mr.get(),
            check_seq = seq,
            "a mergeability check stayed `checking` with no live job; ended as check_failed"
        );
    }
    Ok(reaped)
}

/// What doctor's `review.mergeability` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeabilityHealth {
    /// Rows `checking` past [`STALE_AFTER`] with no live job (the sweep
    /// ends them; seen here when no worker runs it).
    pub checking_stale: i64,
    /// Open merge requests `unchecked` for longer than [`STALE_AFTER`] with
    /// no live job (the sweep re-requests them, 50 per run).
    pub unchecked_unqueued: i64,
    /// Decided rows of open merge requests whose checked head is not the
    /// merge request's head, for longer than [`STALE_AFTER`] with no live job.
    pub decided_stale_head: i64,
    /// Decided rows of open merge requests whose pair nobody verified
    /// against the live tips for longer than [`UNVERIFIED_AFTER`] (the
    /// sweep verifies 50 per run; a count means it does not keep up or
    /// does not run).
    pub decided_unverified: i64,
}

/// How long a decided row may go without the sweep verifying its pair.
pub const UNVERIFIED_AFTER: Duration = Duration::from_secs(3_600);

impl MergeabilityHealth {
    /// `FG-REVIEW-0003 reason=undecided` when any count is not 0.
    pub fn check(&self) -> Result<(), ForgeError> {
        if self.checking_stale == 0
            && self.unchecked_unqueued == 0
            && self.decided_stale_head == 0
            && self.decided_unverified == 0
        {
            return Ok(());
        }
        Err(not_checked("undecided")
            .with_context("checking_stale", self.checking_stale)
            .with_context("unchecked_unqueued", self.unchecked_unqueued)
            .with_context("decided_stale_head", self.decided_stale_head)
            .with_context("decided_unverified", self.decided_unverified))
    }
}

/// The counts behind doctor's `review.mergeability` (read pool, bounded;
/// `None` before the table exists).
pub async fn health(db: &Db) -> Result<Option<MergeabilityHealth>, ForgeError> {
    const QUERY: &str = "review.mergeability_health";
    db.within(QUERY, async {
        let exists: bool = sqlx::query_scalar(
            "SELECT to_regclass('forge.merge_request_mergeability') IS NOT NULL",
        )
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| crate::read_query_error(QUERY, e))?;
        if !exists {
            return Ok(None);
        }
        let (checking_stale, unchecked_unqueued): (i64, i64) = sqlx::query_as(&format!(
            "SELECT count(*) FILTER (WHERE g.status = 'checking'), \
             count(*) FILTER (WHERE g.status = 'unchecked') \
             FROM forge.merge_request_mergeability g \
             JOIN forge.merge_requests m ON m.id = g.merge_request_id \
             WHERE g.status IN ('checking', 'unchecked') AND m.state = 'open' \
             AND g.updated_at < now() - make_interval(secs => $1) AND NOT {LIVE_JOB}"
        ))
        .bind(STALE_AFTER.as_secs_f64())
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| crate::read_query_error(QUERY, e))?;
        let (decided_stale_head, decided_unverified): (i64, i64) = sqlx::query_as(&format!(
            "SELECT count(*) FILTER (WHERE g.checked_head_sha IS DISTINCT FROM m.head_sha \
             AND g.updated_at < now() - make_interval(secs => $1) AND NOT {LIVE_JOB}), \
             count(*) FILTER (WHERE COALESCE(g.confirmed_at, g.checked_at) \
             < now() - make_interval(secs => $2)) \
             FROM forge.merge_request_mergeability g \
             JOIN forge.merge_requests m ON m.id = g.merge_request_id \
             WHERE m.state = 'open' AND {ANSWERING}"
        ))
        .bind(STALE_AFTER.as_secs_f64())
        .bind(UNVERIFIED_AFTER.as_secs_f64())
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| crate::read_query_error(QUERY, e))?;
        Ok(Some(MergeabilityHealth {
            checking_stale,
            unchecked_unqueued,
            decided_stale_head,
            decided_unverified,
        }))
    })
    .await
}
