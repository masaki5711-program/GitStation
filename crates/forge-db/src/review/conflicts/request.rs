//! Requesting scans (see the module documentation of `conflicts`).

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::RequestId;
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::conflict_scan_job;
use crate::jobs::{enqueue_on, Enqueued};
use crate::{query_error, Db};

/// Request a scan of `mr` on `conn`, the caller's transaction: the row is
/// `pending` with a new seq, and its job is enqueued (it exists only if the
/// caller commits). The new seq.
pub(crate) async fn request_scan_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    project: ProjectId,
    origin: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<i64, ForgeError> {
    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO forge.merge_request_conflict_scans (merge_request_id, project_id) \
         VALUES ($1, $2) ON CONFLICT (merge_request_id) DO UPDATE SET status = 'pending', \
         reason = NULL, scan_seq = forge.merge_request_conflict_scans.scan_seq + 1, \
         updated_at = now() RETURNING scan_seq",
    )
    .bind(mr.get())
    .bind(project.get())
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| query_error("review.conflict_scan_request", e))?;
    let job = conflict_scan_job(mr, seq, origin, origin_event_seq)?;
    if let Enqueued::Deduped = enqueue_on(conn, &job).await? {
        // A fresh seq is a fresh key: a duplicate means a job was enqueued
        // for a seq the row had not reached (never expected). The job that
        // exists runs for this seq, so nothing is lost; say so.
        tracing::warn!(
            target: "forge_db",
            event = "forge.review.conflict_scan_job_deduped",
            merge_request_id = mr.get(),
            scan_seq = seq,
            "a conflict scan job for this request already existed"
        );
    }
    Ok(seq)
}

/// Request a scan of `mr` in a transaction of its own (the version job,
/// after a new head). `Ok(None)`: the merge request is gone or not open.
pub async fn request_scan(
    db: &Db,
    mr: MergeRequestId,
    origin: RequestId,
) -> Result<Option<i64>, ForgeError> {
    const QUERY: &str = "review.conflict_scan_request";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let project: Option<i64> = sqlx::query_scalar(
            "SELECT project_id FROM forge.merge_requests WHERE id = $1 AND state = 'open'",
        )
        .bind(mr.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some(project) = project else {
            return Ok(None);
        };
        let seq = request_scan_on(&mut tx, mr, ProjectId::new(project), origin, None).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(seq))
    })
    .await
}

/// The reasons a scan of the same head and target may answer differently
/// when run again (git or job trouble, the time budget): such a scan does
/// not answer the version job.
const RETRYABLE: &str = "('git_timeout', 'git_busy', 'git_failed', 'job_failed', 'job_stale', \
     'pair_failed', 'budget_exhausted')";

/// The version job's request after a version (M3 X6): a scan of `mr`
/// unless its row already answers `head` on the merge request's current
/// target — a scan still `pending` (its job reads the head when it starts;
/// a `scanning` one may have read the old head, so it is superseded), or a
/// finished one of this head and target whose reason a rerun
/// would repeat. In one transaction under the row lock. The new seq when
/// one was requested; `Ok(None)` also when the merge request is not open.
pub async fn request_scan_for_head(
    db: &Db,
    mr: MergeRequestId,
    head: &forge_core::ObjectId,
    origin: RequestId,
) -> Result<Option<i64>, ForgeError> {
    const QUERY: &str = "review.conflict_scan_request_for_head";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let row: Option<(i64, bool)> = sqlx::query_as(&format!(
            "SELECT m.project_id, COALESCE(( \
               SELECT s.status = 'pending' \
                 OR (s.scanned_head_sha = $2 AND s.scanned_target_branch = m.target_branch \
                     AND COALESCE(s.reason, '') NOT IN {RETRYABLE}) \
               FROM forge.merge_request_conflict_scans s \
               WHERE s.merge_request_id = m.id FOR UPDATE OF s), false) \
             FROM forge.merge_requests m WHERE m.id = $1 AND m.state = 'open'"
        ))
        .bind(mr.get())
        .bind(head.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((project, answered)) = row else {
            return Ok(None);
        };
        if answered {
            tx.commit().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(None);
        }
        let seq = request_scan_on(&mut tx, mr, ProjectId::new(project), origin, None).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(seq))
    })
    .await
}
