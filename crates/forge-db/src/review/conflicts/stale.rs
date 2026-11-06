//! Ending scans that will not finish: a run that failed for good
//! ([`fail_scan`], `job_failed`) and rows left `pending` / `scanning` with
//! no live job of an open merge request ([`reap_stale_scans`],
//! `job_stale`). See the module documentation of `conflicts`.

use std::time::Duration;

use forge_core::ids::MergeRequestId;
use forge_errors::ForgeError;

use super::{prediction_incomplete, CONFLICT_SCAN_TIMEOUT, LIVE_JOB};
use crate::jobs::LEASE_SLACK;
use crate::{query_error, Db};

/// A row `pending` / `scanning` this long with no live job is stale: the
/// job's time limit and its lease slack have both passed.
pub const SCAN_STALE_AFTER: Duration =
    Duration::from_secs(CONFLICT_SCAN_TIMEOUT.as_secs() + LEASE_SLACK.as_secs() + 30);

/// End request `seq` of `mr` as `failed reason=job_failed` (its job failed
/// for good before it stored a result), if `seq` is still the latest and
/// unfinished. `true` when written.
pub async fn fail_scan(db: &Db, mr: MergeRequestId, seq: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "review.conflict_scan_fail";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "UPDATE forge.merge_request_conflict_scans SET status = 'failed', \
             reason = 'job_failed', updated_at = now() WHERE merge_request_id = $1 \
             AND scan_seq = $2 AND status IN ('pending', 'scanning')",
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

/// The sweep's reaper (worker, every 30 s): rows `pending` or `scanning`
/// for longer than `older_than` whose latest request has no queued or
/// running job become `failed reason=job_stale` (at most `limit`, open
/// merge requests only); each is logged once at warn with
/// `FG-REVIEW-0022 reason=job_stale`. The (merge request, seq) pairs ended.
pub async fn reap_stale_scans(
    db: &Db,
    older_than: Duration,
    limit: i64,
) -> Result<Vec<(MergeRequestId, i64)>, ForgeError> {
    const QUERY: &str = "review.conflict_scan_reap";
    let millis = i64::try_from(older_than.as_millis()).unwrap_or(i64::MAX);
    let rows: Vec<(i64, i64)> = db
        .within(QUERY, async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            sqlx::query_as(&format!(
                "UPDATE forge.merge_request_conflict_scans SET status = 'failed', \
                 reason = 'job_stale', updated_at = now() WHERE merge_request_id IN ( \
                 SELECT s.merge_request_id FROM forge.merge_request_conflict_scans s \
                 JOIN forge.merge_requests m ON m.id = s.merge_request_id \
                 WHERE s.status IN ('pending', 'scanning') AND m.state = 'open' \
                 AND s.updated_at < now() - make_interval(secs => $1::double precision / 1000) \
                 AND NOT {LIVE_JOB} ORDER BY s.merge_request_id LIMIT $2 \
                 FOR UPDATE OF s SKIP LOCKED) RETURNING merge_request_id, scan_seq"
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
        let e = prediction_incomplete("job_stale");
        tracing::warn!(
            target: "forge_db",
            event = "forge.review.conflict_scan_not_finished",
            error_code = e.code().id(),
            reason = "job_stale",
            merge_request_id = mr.get(),
            scan_seq = seq,
            "a conflict scan stayed unfinished with no live job; ended as failed"
        );
    }
    Ok(reaped)
}
