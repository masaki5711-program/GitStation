//! The statements of the job queue. Each is ONE statement in autocommit
//! (a short transaction that commits at once), bounded on the client by
//! [`Db::within`] and on the server by the session's statement and lock
//! timeouts. All run on the write pool (`forge_app`); `forge_ro` can read
//! the table but neither claim nor settle a job.
//!
//! A statement that makes a job claimable or schedules it (an enqueue, a
//! hand-back, the backoff of a failure or of an expired lease) sends
//! `NOTIFY forge_jobs, <kind>` itself (plan 18 S2, `crate::wake`): no extra
//! round trip, delivered at the commit, never for a rolled-back enqueue.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{
    ClaimedJob, Completed, Enqueued, Failed, JobFailure, JobId, JobKind, JobTimeout, Lease, NewJob,
    ReapedJob, Retry, WorkerId,
};
use crate::wake::Channel;
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
/// `ChangeTx::enqueue_job`: rolled back with it). A key held by a queued,
/// running or succeeded job of the kind dedups; a dead one does not, so the
/// work of a dead job can be enqueued again (index `jobs_dedup`, migration
/// `20261027120000`).
pub(crate) async fn enqueue_on(
    conn: &mut PgConnection,
    job: &NewJob,
) -> Result<Enqueued, ForgeError> {
    // The NOTIFY is part of the statement (no second round trip): only for
    // a row really inserted, delivered at the commit of the caller's
    // transaction, never when it rolls back. The first column is read.
    let id: Option<i64> = sqlx::query_scalar(
        "WITH added AS ( \
             INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, dedup_key, \
                    origin_request_id, origin_event_seq) \
             VALUES ($1, $2::jsonb, 'queued', $3, $4, $5, $6, $7) \
             ON CONFLICT (kind, dedup_key) WHERE dedup_key IS NOT NULL AND state <> 'dead' \
             DO NOTHING \
             RETURNING id, kind) \
         SELECT a.id, pg_catalog.pg_notify($8, a.kind) FROM added AS a",
    )
    .bind(job.kind.as_str())
    .bind(job.payload.to_string())
    .bind(i32::from(job.max_attempts.get()))
    .bind(job.timeout.millis())
    .bind(job.dedup_key.as_ref().map(|k| k.as_str()))
    .bind(job.origin_request_id.as_str())
    .bind(job.origin_event_seq)
    .bind(Channel::Jobs.name())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("jobs.enqueue", e))?;
    Ok(id.map_or(Enqueued::Deduped, |id| Enqueued::Queued(JobId::new(id))))
}

/// [`enqueue_on`] for a job ready only after `delay` (M3 I2: the next
/// attempt of a webhook delivery, after its backoff). The same dedup rule.
pub(crate) async fn enqueue_after_on(
    conn: &mut PgConnection,
    job: &NewJob,
    delay: Duration,
) -> Result<Enqueued, ForgeError> {
    // Notified at once although it is ready later: an idle runner reads
    // the next due time when it wakes, so the job starts on time.
    let id: Option<i64> = sqlx::query_scalar(
        "WITH added AS ( \
             INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, dedup_key, \
                    origin_request_id, origin_event_seq, run_after) \
             VALUES ($1, $2::jsonb, 'queued', $3, $4, $5, $6, $7, \
                    pg_catalog.now() + pg_catalog.make_interval(secs => $8)) \
             ON CONFLICT (kind, dedup_key) WHERE dedup_key IS NOT NULL AND state <> 'dead' \
             DO NOTHING \
             RETURNING id, kind) \
         SELECT a.id, pg_catalog.pg_notify($9, a.kind) FROM added AS a",
    )
    .bind(job.kind.as_str())
    .bind(job.payload.to_string())
    .bind(i32::from(job.max_attempts.get()))
    .bind(job.timeout.millis())
    .bind(job.dedup_key.as_ref().map(|k| k.as_str()))
    .bind(job.origin_request_id.as_str())
    .bind(job.origin_event_seq)
    .bind(delay.as_secs_f64())
    .bind(Channel::Jobs.name())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("jobs.enqueue_after", e))?;
    Ok(id.map_or(Enqueued::Deduped, |id| Enqueued::Queued(JobId::new(id))))
}

/// What [`enqueue_superseding_on`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Superseding {
    pub(crate) enqueued: Enqueued,
    /// Queued jobs of older requests deleted.
    pub(crate) superseded: i64,
}

/// [`enqueue_on`] for the job of the newest request of `merge_request`
/// (merge load test 2026-11-17, F2): in the same statement, every QUEUED
/// job of the kind for that merge request with another dedup key is
/// deleted — it could only find its request superseded when it ran — and
/// the new job keeps the oldest one's place in line (`run_after`, never
/// later than now). A running job is left alone (its result is refused by
/// its seq). So a merge request has at most one queued job of the kind,
/// however often its target moves (index `jobs_queued_merge_request`,
/// migration 20261226100000).
pub(crate) async fn enqueue_superseding_on(
    conn: &mut PgConnection,
    job: &NewJob,
    merge_request: i64,
) -> Result<Superseding, ForgeError> {
    let (id, superseded): (Option<i64>, i64) = sqlx::query_as(
        "WITH gone AS ( \
             DELETE FROM ONLY forge.jobs WHERE kind = $1 AND state = 'queued' \
                AND payload ->> 'merge_request_id' = $8 AND dedup_key IS DISTINCT FROM $5 \
             RETURNING run_after), \
         added AS ( \
             INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, dedup_key, \
                    origin_request_id, origin_event_seq, run_after) \
             VALUES ($1, $2::jsonb, 'queued', $3, $4, $5, $6, $7, \
                    LEAST(pg_catalog.now(), (SELECT min(run_after) FROM gone))) \
             ON CONFLICT (kind, dedup_key) WHERE dedup_key IS NOT NULL AND state <> 'dead' \
             DO NOTHING \
             RETURNING id, kind) \
         SELECT (SELECT id FROM added), (SELECT count(*) FROM gone), \
                (SELECT pg_catalog.pg_notify($9, a.kind) FROM added AS a)",
    )
    .bind(job.kind.as_str())
    .bind(job.payload.to_string())
    .bind(i32::from(job.max_attempts.get()))
    .bind(job.timeout.millis())
    .bind(job.dedup_key.as_ref().map(|k| k.as_str()))
    .bind(job.origin_request_id.as_str())
    .bind(job.origin_event_seq)
    .bind(merge_request.to_string())
    .bind(Channel::Jobs.name())
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| query_error("jobs.enqueue_superseding", e))?;
    if superseded > 0 {
        tracing::debug!(
            target: "forge_db",
            event = "forge.jobs.superseded",
            request_id = %job.origin_request_id,
            kind = job.kind.as_str(),
            merge_request_id = merge_request,
            superseded,
            "queued jobs of older requests replaced by the newest"
        );
    }
    Ok(Superseding {
        enqueued: id.map_or(Enqueued::Deduped, |id| Enqueued::Queued(JobId::new(id))),
        superseded,
    })
}

/// Delete the QUEUED jobs of `kind` for `merge_requests` on `conn` (their
/// requests were superseded without a new job: past a fan-out cap, the
/// sweep asks again). The number deleted.
pub(crate) async fn supersede_queued_on(
    conn: &mut PgConnection,
    kind: &str,
    merge_requests: &[i64],
) -> Result<u64, ForgeError> {
    if merge_requests.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = merge_requests.iter().map(i64::to_string).collect();
    sqlx::query(
        "DELETE FROM ONLY forge.jobs WHERE kind = $1 AND state = 'queued' \
         AND payload ->> 'merge_request_id' = ANY($2::text[])",
    )
    .bind(kind)
    .bind(&ids)
    .execute(&mut *conn)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| query_error("jobs.supersede_queued", e))
}

type ClaimRow = (i64, String, String, i32, i32, i32, String, Option<i64>, i64);

/// Claim the next ready job of one of `kinds` (none: nothing is claimed),
/// in [`priority_sql`] order.
pub async fn claim(
    db: &Db,
    worker: &WorkerId,
    kinds: &[JobKind],
) -> Result<Option<ClaimedJob>, ForgeError> {
    claim_excluding(db, worker, kinds, &[]).await
}

/// [`claim`], passing over the ready jobs whose [`ordering_key`] is in
/// `busy` (a job of the same kind for the same subject runs in this
/// process: `JobRunner`'s per-subject order).
pub async fn claim_excluding(
    db: &Db,
    worker: &WorkerId,
    kinds: &[JobKind],
    busy: &[String],
) -> Result<Option<ClaimedJob>, ForgeError> {
    Ok(claim_timed(db, worker, kinds, busy)
        .await?
        .map(|(job, _ready_for)| job))
}

/// [`claim_excluding`] with how long the job had been ready: from its
/// `run_after` (what the claim treats as ready — the enqueue, the end of a
/// backoff, a release, the place a superseded job held) to this claim, on
/// the database's clock. Plan 18 S0: the runner's `ready_to_claim_ms`.
pub(crate) async fn claim_timed(
    db: &Db,
    worker: &WorkerId,
    kinds: &[JobKind],
    busy: &[String],
) -> Result<Option<(ClaimedJob, Duration)>, ForgeError> {
    if kinds.is_empty() {
        return Ok(None);
    }
    db.within("jobs.claim", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("jobs.acquire", e))?;
        claim_on_excluding(&mut conn, worker, kinds, busy).await
    })
    .await
}

/// How long a ready job waits before its tier is raised to the default
/// one (no starvation of the bulk tiers under a steady load).
pub const PRIORITY_AGING_SECS: i64 = 300;

/// The claim order's tier of a job row `j` (lower first; then `run_after`,
/// `id`). Merge load test 2026-11-17 (F2): what a merge waits on runs
/// before the bulk recomputation a target push fans out.
///
/// - 0: the train pipeline of a merge queue (`ci.pipeline_create` with
///   `source = merge_queue`) and `repository.provision`;
/// - 1: `merge_request.refresh` (a pushed head, F7) and the mergeability
///   check of a merge request with a live queue entry;
/// - 2: everything else, and any job ready for longer than
///   [`PRIORITY_AGING_SECS`];
/// - 3: the other mergeability checks (a target push's fan-out);
/// - 4: `merge_request.conflict_scan` (advisory).
///
/// The payload's id is cast only when it is digits (a malformed payload
/// never fails the claim of every job).
pub(crate) fn priority_sql() -> String {
    use crate::pipelines::{Source, PIPELINE_CREATE_JOB};
    use crate::repositories::PROVISION_JOB;
    use crate::review::{CONFLICT_SCAN_JOB, MERGEABILITY_JOB, REFRESH_JOB};
    format!(
        "CASE \
           WHEN j.kind = '{PIPELINE_CREATE_JOB}' \
                AND j.payload ->> 'source' = '{queue}' THEN 0 \
           WHEN j.kind = '{PROVISION_JOB}' THEN 0 \
           WHEN j.kind = '{REFRESH_JOB}' THEN 1 \
           WHEN j.kind = '{MERGEABILITY_JOB}' AND EXISTS ( \
                SELECT 1 FROM forge.merge_queue_entries e \
                 WHERE e.state IN ('queued', 'testing') \
                   AND e.merge_request_id = CASE \
                       WHEN j.payload ->> 'merge_request_id' ~ '^[0-9]{{1,18}}$' \
                       THEN (j.payload ->> 'merge_request_id')::bigint END) THEN 1 \
           WHEN j.run_after < pg_catalog.now() \
                - pg_catalog.make_interval(secs => {PRIORITY_AGING_SECS}) THEN 2 \
           WHEN j.kind = '{MERGEABILITY_JOB}' THEN 3 \
           WHEN j.kind = '{CONFLICT_SCAN_JOB}' THEN 4 \
           ELSE 2 END",
        queue = Source::MergeQueue.as_str(),
    )
}

/// The SQL of a job row `j`'s ordering key (NULL: none) — what
/// [`ordering_key`] computes in Rust: the merge request
/// (`merge_request_id`), else the merge request by number
/// (`project_id` / `iid`), else the head (`project_id` / `head_sha`).
pub(crate) const ORDERING_KEY_SQL: &str = "COALESCE( \
     'mr:' || (j.payload ->> 'merge_request_id'), \
     'iid:' || (j.payload ->> 'project_id') || '/' || (j.payload ->> 'iid'), \
     'head:' || (j.payload ->> 'project_id') || '/' || (j.payload ->> 'head_sha'))";

/// The ordering key of a job of `kind` with `payload` ([`ORDERING_KEY_SQL`]
/// in Rust, prefixed with the kind): two jobs with the same key never run
/// at once in one `JobRunner`.
pub fn ordering_key(kind: &JobKind, payload: &serde_json::Value) -> Option<String> {
    // `->>`: a string as is, JSON null or a missing field NULL, anything
    // else its JSON text.
    let text = |field: &str| match payload.get(field) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()), // clone: the key owns it
        Some(other) => Some(other.to_string()),
    };
    let subject = text("merge_request_id")
        .map(|mr| format!("mr:{mr}"))
        .or_else(|| Some(format!("iid:{}/{}", text("project_id")?, text("iid")?)))
        .or_else(|| {
            Some(format!(
                "head:{}/{}",
                text("project_id")?,
                text("head_sha")?
            ))
        })?;
    Some(format!("{}:{subject}", kind.as_str()))
}

/// The claim statement on `conn` (tests run it on other roles).
#[cfg(test)]
pub(crate) async fn claim_on(
    conn: &mut PgConnection,
    worker: &WorkerId,
    kinds: &[JobKind],
) -> Result<Option<ClaimedJob>, ForgeError> {
    Ok(claim_on_excluding(conn, worker, kinds, &[])
        .await?
        .map(|(job, _ready_for)| job))
}

/// [`claim_on`] passing over the ordering keys in `busy`; with how long
/// the job had been ready ([`claim_timed`]).
pub(crate) async fn claim_on_excluding(
    conn: &mut PgConnection,
    worker: &WorkerId,
    kinds: &[JobKind],
    busy: &[String],
) -> Result<Option<(ClaimedJob, Duration)>, ForgeError> {
    let kinds: Vec<&str> = kinds.iter().map(JobKind::as_str).collect();
    let sql = format!(
        "WITH next AS ( \
             SELECT j.id FROM ONLY forge.jobs j \
              WHERE j.state = 'queued' AND j.run_after <= pg_catalog.now() \
                AND j.attempts < j.max_attempts AND j.kind = ANY($2::text[]) \
                AND (j.kind || ':' || COALESCE({ORDERING_KEY_SQL}, '')) <> ALL($4::text[]) \
              ORDER BY {priority}, j.run_after, j.id LIMIT 1 \
              FOR UPDATE OF j SKIP LOCKED) \
         UPDATE ONLY forge.jobs AS j \
            SET state = 'running', attempts = j.attempts + 1, worker_id = $1, \
                lease_until = pg_catalog.now() \
                    + pg_catalog.make_interval(secs => (j.timeout_ms + $3) / 1000.0) \
           FROM next WHERE j.id = next.id \
         RETURNING j.id, j.kind, j.payload::text, j.attempts, j.max_attempts, j.timeout_ms, \
                   j.origin_request_id, j.origin_event_seq, \
                   (EXTRACT(epoch FROM pg_catalog.now() - j.run_after) * 1000)::bigint",
        priority = priority_sql(),
    );
    let row: Option<ClaimRow> = sqlx::query_as(&sql)
        .bind(worker.as_str())
        .bind(&kinds)
        .bind(super::LEASE_SLACK_MS)
        .bind(busy)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error("jobs.claim", e))?;
    let Some((id, kind, payload, attempt, max_attempts, timeout_ms, origin, seq, ready_ms)) = row
    else {
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
    // The claim takes `run_after <= now()`: never negative.
    let ready_for = Duration::from_millis(u64::try_from(ready_ms).unwrap_or(0));
    let job = ClaimedJob {
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
    };
    Ok(Some((job, ready_for)))
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
                "WITH failed AS ( \
                 UPDATE ONLY forge.jobs AS j SET \
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
                 RETURNING j.kind, j.state = 'dead' AS dead, \
                    (EXTRACT(epoch FROM j.run_after - pg_catalog.now()) * 1000)::bigint \
                        AS retry_ms) \
                 SELECT f.dead, f.retry_ms, \
                        (SELECT pg_catalog.pg_notify($7, f.kind) WHERE NOT f.dead) \
                   FROM failed AS f"
            ))
            .bind(lease.job_id.get())
            .bind(lease.worker_id.as_str())
            .bind(lease.attempt)
            .bind(never)
            .bind(failure.error.code().id())
            .bind(last_error(&failure.error))
            // A retry is scheduled: idle runners read the due time again.
            .bind(Channel::Jobs.name())
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

/// Hand a claimed job back at shutdown, as if it had not been claimed:
/// `queued`, ready now, `attempts - 1` (a shutdown is not the job's fault),
/// no lease — only while this worker still holds the lease of this attempt
/// (`Ok(false)`: fenced, nothing changed).
///
/// `attempts - 1` makes the attempt number reusable: the next claim gets
/// the same attempt again. That is safe only because the caller releases
/// after the handler's task has ended — no run of the old attempt can still
/// write with that number. Never release a job whose handler may run.
pub async fn release(db: &Db, job: &ClaimedJob) -> Result<bool, ForgeError> {
    let lease = &job.lease;
    let done = db
        .within("jobs.release", async {
            // Ready now: the runners of the other processes are told (one
            // row per job handed back, so `rows_affected` still counts it).
            sqlx::query(
                "WITH freed AS ( \
                     UPDATE ONLY forge.jobs SET state = 'queued', lease_until = NULL, \
                            worker_id = NULL, run_after = pg_catalog.now(), \
                            attempts = attempts - 1 \
                      WHERE id = $1 AND state = 'running' AND worker_id = $2 AND attempts = $3 \
                     RETURNING kind) \
                 SELECT pg_catalog.pg_notify($4, f.kind) FROM freed AS f",
            )
            .bind(lease.job_id.get())
            .bind(lease.worker_id.as_str())
            .bind(lease.attempt)
            .bind(Channel::Jobs.name())
            .execute(db.write_pool())
            .await
            .map_err(|e| query_error("jobs.release", e))
        })
        .await?;
    Ok(done.rows_affected() == 1)
}

/// End expired leases in ONE statement: dead when the attempts are used up
/// (`last_error_code = FG-DB-0007`), otherwise queued after the backoff.
/// Rows a worker is settling right now (locked) are left for the next run.
/// At most [`REAP_BATCH`] per call.
pub async fn reap(db: &Db) -> Result<Vec<ReapedJob>, ForgeError> {
    reap_at_most(db, REAP_BATCH).await
}

/// [`reap`] with another limit (tests).
pub(crate) async fn reap_at_most(db: &Db, limit: i64) -> Result<Vec<ReapedJob>, ForgeError> {
    let rows: Vec<ReapRow> = db
        .within("jobs.reap", async {
            sqlx::query_as(concat!(
                "WITH reaped AS ( \
                 UPDATE ONLY forge.jobs AS j SET \
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
                           j.origin_request_id, j.state = 'dead' AS dead) \
                 SELECT r.id, r.kind, r.worker_id, r.attempts, r.max_attempts, \
                        r.origin_request_id, r.dead, \
                        (SELECT pg_catalog.pg_notify($2, r.kind) WHERE NOT r.dead) \
                   FROM reaped AS r"
            ))
            .bind(limit)
            // Queued again after a backoff: idle runners read the due time.
            .bind(Channel::Jobs.name())
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
