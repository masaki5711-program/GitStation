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
/// caller commits). The new seq. A request that is not the sweep's resets
/// the sweep's attempt count.
pub(crate) async fn request_scan_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    project: ProjectId,
    origin: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<i64, ForgeError> {
    bump_and_enqueue(conn, mr, project, origin, origin_event_seq, false).await
}

async fn bump_and_enqueue(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    project: ProjectId,
    origin: RequestId,
    origin_event_seq: Option<i64>,
    by_sweep: bool,
) -> Result<i64, ForgeError> {
    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO forge.merge_request_conflict_scans (merge_request_id, project_id, \
         sweep_attempts) VALUES ($1, $2, CASE WHEN $3 THEN 1 ELSE 0 END) \
         ON CONFLICT (merge_request_id) DO UPDATE SET status = 'pending', \
         scan_seq = forge.merge_request_conflict_scans.scan_seq + 1, \
         sweep_attempts = CASE WHEN $3 \
             THEN LEAST(forge.merge_request_conflict_scans.sweep_attempts + 1, 100) ELSE 0 END, \
         updated_at = now() RETURNING scan_seq",
    )
    .bind(mr.get())
    .bind(project.get())
    .bind(by_sweep)
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
/// when run again (git or job trouble, the time budget, a peer that moved):
/// such a scan answers neither the version job nor the sweep.
const RETRYABLE: &str = "('git_timeout', 'git_busy', 'git_failed', 'job_failed', 'job_stale', \
     'pair_failed', 'budget_exhausted', 'peer_moved')";

/// What the version job's request did ([`request_scan_for_head`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanAsk {
    /// A scan was requested (its seq).
    Requested(i64),
    /// The stored scan answers this head and target (or one is pending).
    Answered,
    /// The merge request is gone or not open.
    NotOpen,
}

impl ScanAsk {
    /// The log value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested(_) => "requested",
            Self::Answered => "answered",
            Self::NotOpen => "not_open",
        }
    }

    /// The seq of a request.
    pub fn seq(self) -> Option<i64> {
        match self {
            Self::Requested(seq) => Some(seq),
            Self::Answered | Self::NotOpen => None,
        }
    }
}

/// The version job's request after a version (M3 X6): a scan of `mr`
/// unless its row already answers `head` on the merge request's current
/// target — a scan still `pending` (its job reads the head when it starts;
/// a `scanning` one may have read the old head, so it is superseded), or a
/// finished one of this head and target whose reason a rerun
/// would repeat. In one transaction under the row lock.
pub async fn request_scan_for_head(
    db: &Db,
    mr: MergeRequestId,
    head: &forge_core::ObjectId,
    origin: RequestId,
) -> Result<ScanAsk, ForgeError> {
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
            return Ok(ScanAsk::NotOpen);
        };
        if answered {
            tx.commit().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(ScanAsk::Answered);
        }
        let seq = request_scan_on(&mut tx, mr, ProjectId::new(project), origin, None).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(ScanAsk::Requested(seq))
    })
    .await
}

/// How often the sweep re-requests one row before it leaves it to the next
/// push, retarget or reopen.
pub const SCAN_SWEEP_ATTEMPTS: i32 = 3;
/// A finished row the sweep retries waits this long first (no hot loop on
/// a git failure that keeps coming back).
pub const SCAN_SWEEP_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

/// Why the sweep requested a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweptScan {
    pub merge_request: MergeRequestId,
    pub scan_seq: i64,
    /// `never_scanned` (open with a head and no row: opened before this
    /// build, or a version job's request that failed) or the retryable
    /// reason of the last scan.
    pub why: String,
    /// The sweep's attempts for this row, this one included.
    pub attempt: i32,
}

/// The sweep (worker, every 30 s; review HIGH): request a scan for at most
/// `limit` open merge requests with a head that have no scan row, or whose
/// last scan ended `failed` / `incomplete` with a retryable reason at least
/// [`SCAN_SWEEP_BACKOFF`] ago and fewer than [`SCAN_SWEEP_ATTEMPTS`] sweep
/// attempts; the longest waiting first, rows another sweep holds skipped.
/// Each request is logged at info.
pub async fn sweep_scans(
    db: &Db,
    limit: i64,
    origin: &RequestId,
) -> Result<Vec<SweptScan>, ForgeError> {
    const QUERY: &str = "review.conflict_scan_sweep";
    let backoff = i64::try_from(SCAN_SWEEP_BACKOFF.as_secs()).unwrap_or(i64::MAX);
    let swept = db
        .within(QUERY, async {
            let mut tx = db
                .write_pool()
                .begin()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            let missing: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT m.id, m.project_id FROM forge.merge_requests m \
                 WHERE m.state = 'open' AND m.head_sha IS NOT NULL AND NOT EXISTS \
                 (SELECT 1 FROM forge.merge_request_conflict_scans s \
                  WHERE s.merge_request_id = m.id) \
                 ORDER BY m.updated_at, m.id\n                 LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let left = limit - i64::try_from(missing.len()).unwrap_or(limit);
            let retry: Vec<(i64, i64, String)> = sqlx::query_as(&format!(
                "SELECT s.merge_request_id, s.project_id, s.reason \
                 FROM forge.merge_request_conflict_scans s \
                 JOIN forge.merge_requests m ON m.id = s.merge_request_id \
                 WHERE m.state = 'open' AND m.head_sha IS NOT NULL \
                 AND s.status IN ('failed', 'incomplete') AND s.reason IN {RETRYABLE} \
                 AND s.sweep_attempts < $2 \
                 AND s.updated_at < now() - make_interval(secs => $3::double precision) \
                 ORDER BY s.updated_at, s.merge_request_id LIMIT $1 \
                 FOR UPDATE OF s SKIP LOCKED"
            ))
            .bind(left.max(0))
            .bind(SCAN_SWEEP_ATTEMPTS)
            .bind(backoff)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let mut swept = Vec::with_capacity(missing.len() + retry.len());
            let rows = missing
                .into_iter()
                .map(|(mr, p)| (mr, p, "never_scanned".to_owned()))
                .chain(retry);
            for (mr, project, why) in rows {
                let mr = MergeRequestId::new(mr);
                // clone: each job keeps the sweep's request id
                let seq = bump_and_enqueue(
                    &mut tx,
                    mr,
                    ProjectId::new(project),
                    origin.clone(),
                    None,
                    true,
                )
                .await?;
                let attempt: i32 = sqlx::query_scalar(
                    "SELECT sweep_attempts FROM forge.merge_request_conflict_scans \
                     WHERE merge_request_id = $1",
                )
                .bind(mr.get())
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                swept.push(SweptScan {
                    merge_request: mr,
                    scan_seq: seq,
                    why,
                    attempt,
                });
            }
            tx.commit().await.map_err(|e| query_error(QUERY, e))?;
            Ok(swept)
        })
        .await?;
    for s in &swept {
        tracing::info!(
            target: "forge_db",
            event = "forge.review.conflict_scan_swept",
            request_id = %origin,
            merge_request_id = s.merge_request.get(),
            scan_seq = s.scan_seq,
            why = %s.why,
            attempt = s.attempt,
            max_attempts = SCAN_SWEEP_ATTEMPTS,
            "the sweep requested a conflict scan"
        );
    }
    Ok(swept)
}
