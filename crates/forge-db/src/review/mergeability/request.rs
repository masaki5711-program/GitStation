//! Requesting checks (see the module documentation of `mergeability`).

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{ObjectId, RequestId};
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::{mergeability_job, LIVE_JOB};
use crate::jobs::{enqueue_on, Enqueued};
use crate::{query_error, Db};

/// The most merge requests one target push marks `unchecked` past the
/// fan-out cap (the rest keep their status — the readers' staleness rule
/// and the sweep cover them — and `FG-REVIEW-0003 reason=fanout_deferred`
/// is logged).
pub const DEFER_MAX: i64 = 5_000;
/// Rows one sweep run re-requests at most.
pub const SWEEP_BATCH: i64 = 50;

/// Bump `mr`'s check (status `unchecked`, `check_seq + 1`, the conflict
/// list cleared) on `conn`, the caller's transaction; the new seq.
async fn bump_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    project: ProjectId,
) -> Result<i64, ForgeError> {
    sqlx::query_scalar(
        "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id) \
         VALUES ($1, $2) ON CONFLICT (merge_request_id) DO UPDATE SET status = 'unchecked', \
         reason = NULL, conflicts = '{}', conflicts_truncated = false, \
         check_seq = forge.merge_request_mergeability.check_seq + 1, \
         updated_at = now() RETURNING check_seq",
    )
    .bind(mr.get())
    .bind(project.get())
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("review.mergeability_request", e))
}

/// Request a check of `mr` on `conn`, the caller's transaction: the row is
/// `unchecked` with a new seq, and its job is enqueued (it exists only if
/// the caller commits). The new seq.
pub(crate) async fn request_check_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    project: ProjectId,
    origin: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<i64, ForgeError> {
    let seq = bump_on(conn, mr, project).await?;
    let job = mergeability_job(mr, seq, origin, origin_event_seq)?;
    if let Enqueued::Deduped = enqueue_on(conn, &job).await? {
        // A fresh seq is a fresh key: a duplicate means a job was enqueued
        // for a seq the row had not reached (never expected). The job that
        // exists runs for this seq, so nothing is lost; say so.
        tracing::warn!(
            target: "forge_db",
            event = "forge.review.mergeability_job_deduped",
            merge_request_id = mr.get(),
            check_seq = seq,
            "a mergeability job for this request already existed"
        );
    }
    Ok(seq)
}

/// Mark open merge requests of `project` targeting one of `targets` and not
/// in `enqueued` `unchecked` (a new seq, no job: past the fan-out cap,
/// ADR-0024 §3), on `conn`, at most [`DEFER_MAX`] in id order (a stable
/// lock order); `(marked, left)` — `left` open merge requests beyond the
/// bound kept their status.
pub(crate) async fn defer_checks_on(
    conn: &mut PgConnection,
    project: ProjectId,
    targets: &[String],
    enqueued: &[i64],
) -> Result<(u64, i64), ForgeError> {
    let marked = sqlx::query(
        "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id) \
         SELECT id, project_id FROM forge.merge_requests WHERE project_id = $1 \
         AND state = 'open' AND target_branch = ANY($2) AND id <> ALL($3) \
         ORDER BY id LIMIT $4 \
         ON CONFLICT (merge_request_id) DO UPDATE SET status = 'unchecked', reason = NULL, \
         conflicts = '{}', conflicts_truncated = false, \
         check_seq = forge.merge_request_mergeability.check_seq + 1, updated_at = now()",
    )
    .bind(project.get())
    .bind(targets)
    .bind(enqueued)
    .bind(DEFER_MAX)
    .execute(&mut *conn)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| query_error("review.mergeability_defer", e))?;
    if marked < u64::try_from(DEFER_MAX).unwrap_or(u64::MAX) {
        return Ok((marked, 0));
    }
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.merge_requests WHERE project_id = $1 AND state = 'open' \
         AND target_branch = ANY($2) AND id <> ALL($3)",
    )
    .bind(project.get())
    .bind(targets)
    .bind(enqueued)
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("review.mergeability_defer_count", e))?;
    Ok((marked, (total - DEFER_MAX).max(0)))
}

/// Mark `mr` `unchecked` on `conn` (the version's own transaction): its
/// stored decision is about another head. A run in progress keeps its seq
/// (its result is refused: the head changed), and the version job requests
/// the new check.
pub(crate) async fn downgrade_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    head: &ObjectId,
) -> Result<(), ForgeError> {
    sqlx::query(
        "UPDATE forge.merge_request_mergeability SET status = 'unchecked', reason = NULL, \
         conflicts = '{}', conflicts_truncated = false, updated_at = now() \
         WHERE merge_request_id = $1 AND status <> 'checking' \
         AND checked_head_sha IS DISTINCT FROM $2",
    )
    .bind(mr.get())
    .bind(head.as_str())
    .execute(conn)
    .await
    .map(drop)
    .map_err(|e| query_error("review.mergeability_downgrade", e))
}

/// Request a check of `mr` in a transaction of its own (the version job,
/// after a version). `Ok(None)`: the merge request is gone or not open.
pub async fn request_check(
    db: &Db,
    mr: MergeRequestId,
    origin: RequestId,
) -> Result<Option<i64>, ForgeError> {
    const QUERY: &str = "review.mergeability_request";
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
        let seq = request_check_on(&mut tx, mr, ProjectId::new(project), origin, None).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(seq))
    })
    .await
}

/// The sweep (worker, every 30 s): request a check for at most `limit`
/// open merge requests whose row is `unchecked` with no live job (past a
/// fan-out cap, or a request whose job died), the ones waiting longest
/// first; rows another sweep holds are skipped. The (merge request, seq)
/// pairs requested.
pub async fn sweep_unchecked(
    db: &Db,
    limit: i64,
    origin: &RequestId,
) -> Result<Vec<(MergeRequestId, i64)>, ForgeError> {
    const QUERY: &str = "review.mergeability_sweep";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let rows: Vec<(i64, i64)> = sqlx::query_as(&format!(
            "SELECT g.merge_request_id, g.project_id FROM forge.merge_request_mergeability g \
             JOIN forge.merge_requests m ON m.id = g.merge_request_id \
             WHERE g.status = 'unchecked' AND m.state = 'open' AND NOT {LIVE_JOB} \
             ORDER BY g.checked_at NULLS FIRST, g.updated_at, g.merge_request_id \
             LIMIT $1 FOR UPDATE OF g SKIP LOCKED"
        ))
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let mut requested = Vec::with_capacity(rows.len());
        for (mr, project) in rows {
            let mr = MergeRequestId::new(mr);
            // clone: each job keeps the sweep's request id
            let seq = request_check_on(&mut tx, mr, ProjectId::new(project), origin.clone(), None)
                .await?;
            requested.push((mr, seq));
        }
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(requested)
    })
    .await
}

/// How long a clean decision may stand without its merge result before
/// [`sweep_missing_merge_results`] asks again: the job writes the result
/// right after the decision (a live job is never swept), so only a row
/// stored before M4 C7 or a run whose merge-result step failed for good is
/// older; the bound keeps a failing repository from being re-requested
/// every run.
pub const MISSING_RESULT_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// The rollout's backfill (review MEDIUM, M4 C7; worker, every 30 s):
/// request a check for at most `limit` open merge requests whose stored
/// `can_be_merged` (for the merge request's current head) has no
/// `merge_result_refs` row for its checked (head, target) pair, decided
/// longer than `older_than` ago, with no live job — rows stored before the
/// merge results existed (they would never get a merge request pipeline,
/// so never `ci/pipeline`), or a run whose merge-result step failed for
/// good. Idempotent: the check writes the row, after which the merge
/// request is no longer a candidate. The (merge request, seq) pairs
/// requested.
pub async fn sweep_missing_merge_results(
    db: &Db,
    older_than: std::time::Duration,
    limit: i64,
    origin: &RequestId,
) -> Result<Vec<(MergeRequestId, i64)>, ForgeError> {
    const QUERY: &str = "review.merge_result_backfill";
    let millis = i64::try_from(older_than.as_millis()).unwrap_or(i64::MAX);
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let rows: Vec<(i64, i64)> = sqlx::query_as(&format!(
            "SELECT g.merge_request_id, g.project_id FROM forge.merge_request_mergeability g \
             JOIN forge.merge_requests m ON m.id = g.merge_request_id \
             WHERE g.status = 'can_be_merged' AND m.state = 'open' \
             AND g.checked_head_sha = m.head_sha AND g.checked_target_sha IS NOT NULL \
             AND g.updated_at < now() - make_interval(secs => $1::double precision / 1000) \
             AND NOT EXISTS (SELECT 1 FROM forge.merge_result_refs r \
             WHERE r.merge_request_id = g.merge_request_id \
             AND r.head_sha = g.checked_head_sha AND r.target_sha = g.checked_target_sha) \
             AND NOT {LIVE_JOB} \
             ORDER BY g.updated_at, g.merge_request_id LIMIT $2 FOR UPDATE OF g SKIP LOCKED"
        ))
        .bind(millis)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let mut requested = Vec::with_capacity(rows.len());
        for (mr, project) in rows {
            let mr = MergeRequestId::new(mr);
            // clone: each job keeps the sweep's request id
            let seq = request_check_on(&mut tx, mr, ProjectId::new(project), origin.clone(), None)
                .await?;
            requested.push((mr, seq));
        }
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(requested)
    })
    .await
}
