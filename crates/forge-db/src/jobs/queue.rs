//! The statements of the job queue. Each is ONE statement in autocommit
//! (a short transaction that commits at once), bounded on the client by
//! [`Db::within`] and on the server by the session's statement and lock
//! timeouts. All run on the write pool (`forge_app`); `forge_ro` can read
//! the table but neither claim nor settle a job.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{
    ClaimedJob, Completed, Enqueued, Failed, JobFailure, JobId, JobKind, JobTimeout, Lease, NewJob,
    ReapedJob, Retry, WorkerId,
};
use crate::{query_error, Db};

/// At most this many expired leases per [`reap`].
pub const REAP_BATCH: i64 = 1000;

/// `last_error` is kept well below the column's 8 KiB CHECK.
const LAST_ERROR_MAX: usize = 4096;

/// The backoff after attempt `j.attempts`, in seconds (see `super::backoff`).
macro_rules! backoff_secs {
    () => {
        "LEAST(3600, 10 * (2 ^ LEAST(GREATEST(j.attempts - 1, 0), 20)))"
    };
}

/// Insert a queued job on the write pool.
pub async fn enqueue(db: &Db, job: &NewJob) -> Result<Enqueued, ForgeError> {
    db.within("jobs.enqueue", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("jobs.acquire", e))?;
        enqueue_on(&mut conn, job).await
    })
    .await
}

/// Insert a queued job on `conn` (inside the caller's transaction, e.g.
/// `ChangeTx::enqueue_job`: rolled back with it).
pub(crate) async fn enqueue_on(
    conn: &mut PgConnection,
    job: &NewJob,
) -> Result<Enqueued, ForgeError> {
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, dedup_key, \
         origin_request_id, origin_event_seq) \
         VALUES ($1, $2::jsonb, 'queued', $3, $4, $5, $6, $7) \
         ON CONFLICT (kind, dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING \
         RETURNING id",
    )
    .bind(job.kind.as_str())
    .bind(job.payload.to_string())
    .bind(i32::from(job.max_attempts.get()))
    .bind(job.timeout.millis())
    .bind(job.dedup_key.as_ref().map(|k| k.as_str()))
    .bind(job.origin_request_id.as_str())
    .bind(job.origin_event_seq)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("jobs.enqueue", e))?;
    Ok(id.map_or(Enqueued::Deduped, |id| Enqueued::Queued(JobId::new(id))))
}

type ClaimRow = (i64, String, String, i32, i32, i32, String, Option<i64>);

/// Claim the oldest ready job of one of `kinds` (none: nothing is claimed).
pub async fn claim(
    db: &Db,
    worker: &WorkerId,
    kinds: &[JobKind],
) -> Result<Option<ClaimedJob>, ForgeError> {
    if kinds.is_empty() {
        return Ok(None);
    }
    db.within("jobs.claim", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("jobs.acquire", e))?;
        claim_on(&mut conn, worker, kinds).await
    })
    .await
}

/// The claim statement on `conn` (tests run it on other roles).
pub(crate) async fn claim_on(
    conn: &mut PgConnection,
    worker: &WorkerId,
    kinds: &[JobKind],
) -> Result<Option<ClaimedJob>, ForgeError> {
    let kinds: Vec<&str> = kinds.iter().map(JobKind::as_str).collect();
    let row: Option<ClaimRow> = sqlx::query_as(
        "WITH next AS ( \
             SELECT id FROM ONLY forge.jobs \
              WHERE state = 'queued' AND run_after <= pg_catalog.now() \
                AND attempts < max_attempts AND kind = ANY($2::text[]) \
              ORDER BY run_after, id LIMIT 1 \
              FOR UPDATE SKIP LOCKED) \
         UPDATE ONLY forge.jobs AS j \
            SET state = 'running', attempts = j.attempts + 1, worker_id = $1, \
                lease_until = pg_catalog.now() \
                    + pg_catalog.make_interval(secs => (j.timeout_ms + $3) / 1000.0) \
           FROM next WHERE j.id = next.id \
         RETURNING j.id, j.kind, j.payload::text, j.attempts, j.max_attempts, j.timeout_ms, \
                   j.origin_request_id, j.origin_event_seq",
    )
    .bind(worker.as_str())
    .bind(&kinds)
    .bind(i32::try_from(super::LEASE_SLACK.as_millis()).unwrap_or(30_000))
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("jobs.claim", e))?;
    let Some((id, kind, payload, attempt, max_attempts, timeout_ms, origin, seq)) = row else {
        return Ok(None);
    };
    let corrupt = |what: &'static str| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "jobs.claim")
            .with_context("reason", "invalid_row")
            .with_context("column", what)
            .with_context("job_id", id)
    };
    // The job is claimed now: an invalid row is reported (its lease expires
    // and reap retries it, then deads it — never silently skipped).
    let kind = JobKind::parse(&kind).ok_or_else(|| corrupt("kind"))?;
    let payload = serde_json::from_str(&payload).map_err(|_| corrupt("payload"))?;
    let timeout = u64::try_from(timeout_ms)
        .ok()
        .and_then(|ms| JobTimeout::new(Duration::from_millis(ms)))
        .ok_or_else(|| corrupt("timeout_ms"))?;
    Ok(Some(ClaimedJob {
        lease: Lease {
            job_id: JobId::new(id),
            worker_id: worker.clone(), // clone: the lease owns its fence key
            attempt,
        },
        kind,
        payload,
        max_attempts,
        timeout,
        origin_request_id: origin,
        origin_event_seq: seq,
    }))
}

/// `FG-DB-0007 reason=fenced`: the lease is no longer this worker's.
fn fenced(lease: &Lease, kind: &JobKind, operation: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::JobLeaseExpired)
        .with_context("operation", operation)
        .with_context("reason", "fenced")
        .with_context("job_id", lease.job_id.get())
        .with_context("kind", kind.as_str())
        .with_context("worker_id", lease.worker_id.as_str())
        .with_context("attempt", lease.attempt)
}

/// Mark a claimed job succeeded — only while this worker still holds the
/// lease of this attempt.
pub async fn complete(db: &Db, job: &ClaimedJob) -> Result<Completed, ForgeError> {
    let lease = &job.lease;
    let done = db
        .within("jobs.complete", async {
            sqlx::query(
                "UPDATE ONLY forge.jobs SET state = 'succeeded', finished_at = pg_catalog.now(), \
                        lease_until = NULL \
                  WHERE id = $1 AND state = 'running' AND worker_id = $2 AND attempts = $3",
            )
            .bind(lease.job_id.get())
            .bind(lease.worker_id.as_str())
            .bind(lease.attempt)
            .execute(db.write_pool())
            .await
            .map_err(|e| query_error("jobs.complete", e))
        })
        .await?;
    Ok(if done.rows_affected() == 1 {
        Completed::Applied
    } else {
        Completed::Fenced(fenced(lease, &job.kind, "jobs.complete"))
    })
}

/// The failure as stored in `last_error` (code and context, ≤ 4 KiB).
fn last_error(error: &ForgeError) -> String {
    let context: serde_json::Map<String, serde_json::Value> = error
        .context()
        .iter()
        .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String(v.clone())))
        .collect();
    let full = serde_json::json!({ "code": error.code().id(), "context": context }).to_string();
    if full.len() <= LAST_ERROR_MAX {
        return full;
    }
    serde_json::json!({ "code": error.code().id(), "truncated": true }).to_string()
}

/// Record a failed run: queued again after the backoff, or dead when the
/// attempts are used up or the failure is [`Retry::Never`] — only while
/// this worker still holds the lease of this attempt.
pub async fn fail(db: &Db, job: &ClaimedJob, failure: &JobFailure) -> Result<Failed, ForgeError> {
    let lease = &job.lease;
    let never = match failure.retry {
        Retry::Backoff => false,
        Retry::Never => true,
    };
    let row: Option<(bool, i64)> = db
        .within("jobs.fail", async {
            sqlx::query_as(concat!(
                "UPDATE ONLY forge.jobs AS j SET \
                    state = CASE WHEN $4 OR j.attempts >= j.max_attempts \
                                 THEN 'dead' ELSE 'queued' END, \
                    run_after = CASE WHEN $4 OR j.attempts >= j.max_attempts THEN j.run_after \
                                ELSE pg_catalog.now() + pg_catalog.make_interval(secs => ",
                backoff_secs!(),
                ") END, \
                    finished_at = CASE WHEN $4 OR j.attempts >= j.max_attempts \
                                       THEN pg_catalog.now() END, \
                    lease_until = NULL, last_error_code = $5, last_error = $6::jsonb \
                  WHERE j.id = $1 AND j.state = 'running' AND j.worker_id = $2 \
                    AND j.attempts = $3 \
                 RETURNING j.state = 'dead', \
                    (EXTRACT(epoch FROM j.run_after - pg_catalog.now()) * 1000)::bigint"
            ))
            .bind(lease.job_id.get())
            .bind(lease.worker_id.as_str())
            .bind(lease.attempt)
            .bind(never)
            .bind(failure.error.code().id())
            .bind(last_error(&failure.error))
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error("jobs.fail", e))
        })
        .await?;
    Ok(match row {
        None => Failed::Fenced(fenced(lease, &job.kind, "jobs.fail")),
        Some((true, _)) => Failed::Dead,
        Some((false, ms)) => Failed::Retrying {
            retry_in: Duration::from_millis(u64::try_from(ms).unwrap_or(0)),
        },
    })
}

type ReapRow = (i64, String, Option<String>, i32, i32, String, bool);

/// End expired leases in ONE statement: dead when the attempts are used up
/// (`last_error_code = FG-DB-0007`), otherwise queued after the backoff.
/// Rows a worker is settling right now (locked) are left for the next run.
pub async fn reap(db: &Db) -> Result<Vec<ReapedJob>, ForgeError> {
    let rows: Vec<ReapRow> = db
        .within("jobs.reap", async {
            sqlx::query_as(concat!(
                "UPDATE ONLY forge.jobs AS j SET \
                    state = CASE WHEN j.attempts >= j.max_attempts THEN 'dead' ELSE 'queued' END, \
                    run_after = CASE WHEN j.attempts >= j.max_attempts THEN j.run_after \
                                ELSE pg_catalog.now() + pg_catalog.make_interval(secs => ",
                backoff_secs!(),
                ") END, \
                    finished_at = CASE WHEN j.attempts >= j.max_attempts \
                                       THEN pg_catalog.now() END, \
                    lease_until = NULL, last_error_code = 'FG-DB-0007', \
                    last_error = pg_catalog.jsonb_build_object('code', 'FG-DB-0007', \
                        'context', pg_catalog.jsonb_build_object('reason', 'lease_expired', \
                            'worker_id', j.worker_id, 'attempt', j.attempts::text)) \
                   FROM (SELECT id FROM ONLY forge.jobs \
                          WHERE state = 'running' AND lease_until < pg_catalog.now() \
                          ORDER BY lease_until LIMIT $1 \
                          FOR UPDATE SKIP LOCKED) AS expired \
                  WHERE j.id = expired.id \
                 RETURNING j.id, j.kind, j.worker_id, j.attempts, j.max_attempts, \
                           j.origin_request_id, j.state = 'dead'"
            ))
            .bind(REAP_BATCH)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error("jobs.reap", e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, kind, worker_id, attempts, max_attempts, origin_request_id, dead)| ReapedJob {
                job_id: JobId::new(id),
                kind,
                worker_id,
                attempts,
                max_attempts,
                origin_request_id,
                dead,
            },
        )
        .collect())
}
