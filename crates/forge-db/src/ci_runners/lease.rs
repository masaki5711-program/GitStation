//! The runner side of `forge.ci_jobs` (M4 C3, plan 14 §5 C3, ADR-0029):
//! claim, heartbeat and lease, log append, completion, the lease reaper and
//! the health read of doctor `ci.runners`.
//!
//! **A lease** is `ci_jobs.runner_id` + `lease_until` (the database's
//! clock). A runner may act on a job only while it holds an unexpired
//! lease of the same attempt; every write re-checks that in its own
//! statement (`WHERE runner_id = $runner AND attempt = $attempt AND
//! lease_until > now()`), so a runner that lost the lease — or whose job
//! was retried and claimed again — changes nothing (`FG-CI-0004`).
//!
//! **Claim** ([`claim`]): one statement picks the oldest `pending` job whose
//! tags the runner's cover, of a live pipeline and a project not pending
//! deletion, `FOR UPDATE OF job, pipeline SKIP LOCKED` — two runners never
//! get the same job, and a claim never waits on a cancel or a completion
//! that holds the pipeline. In the same transaction the job becomes
//! `running` with a lease, the pipeline `running`, and one job token is
//! issued (expiring after the job's timeout + [`TOKEN_GRACE_S`]).
//!
//! The same pick holds the project row and its namespace row `FOR SHARE
//! SKIP LOCKED` until the token is in (ADR-0029 obligation 1): a deletion
//! locks the project row `FOR UPDATE` and a move (a group's too) locks the
//! subtree's namespace rows `FOR NO KEY UPDATE` before they revoke the
//! project's tokens, so a claim either skips a project being deleted or
//! moved, or commits its token before their revocation runs (which then
//! waits for it, sees the token and revokes it). The claim waits on neither
//! (`SKIP LOCKED`), so no lock cycle with them exists.
//!
//! **Lock order** (no deadlock with cancel / retry, which lock the pipeline
//! first): completion and the reaper lock the pipeline row, then the job.
//!
//! **The reaper** ([`reap`]): a held job whose lease ended goes back to the
//! queue exactly once (`pending`, a new attempt, `lease_losses + 1`), or
//! fails with `FG-CI-0004` at the third lost lease; a cancelled job a dead
//! runner still held is released. Its token is revoked in the same
//! transaction.
//!
//! **Secrets** ([`Claimed::secrets_allowed`]): decided by
//! `StoredPipeline::secrets_allowed()` alone (ADR-0029 obligation 5). No
//! secret value exists before C6; the claim hands out the names only when
//! allowed, never otherwise.

use forge_core::ci_token::{CiToken, CiTokenKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::PgConnection;

use super::{revoke_job_tokens, token_failed, Revocation};
use crate::pipelines::{pipeline_of, PipelineRow, StoredPipeline, PIPELINE_COLUMNS};
use crate::{query_error, read_query_error, Db};

/// How long a claim or a heartbeat holds the lease.
pub const LEASE_S: i64 = 120;
/// A job token lives for the job's timeout plus this.
pub const TOKEN_GRACE_S: i64 = 600;
/// The third lost lease fails the job (the column's CHECK bound).
pub const LEASE_LOSS_LIMIT: i32 = 3;
/// The largest log chunk, in bytes (the column's CHECK).
pub const MAX_LOG_CHUNK: usize = 64 * 1024;
/// The most chunks per job attempt (`seq` 0..=4095).
pub const MAX_LOG_CHUNKS: i64 = 4096;
/// The most log bytes per job attempt.
pub const MAX_LOG_BYTES: i64 = 16 * 1024 * 1024;
/// The most held jobs one reaper pass looks at.
pub const REAP_BATCH: i64 = 100;
/// A pending job older than this, unclaimed, is `FG-CI-0005`.
pub const NO_RUNNER_AFTER_S: i64 = 600;
/// A runner silent for longer than this is not live.
pub const RUNNER_SILENT_AFTER_S: i64 = 300;

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"";

/// A job a runner claimed: what it needs to run it, and its token (shown
/// once).
#[derive(Debug)]
pub struct Claimed {
    pub job_id: i64,
    pub attempt: i32,
    pub name: String,
    pub stage: String,
    pub image: String,
    pub timeout_s: i32,
    pub tags: Vec<String>,
    pub pipeline: StoredPipeline,
    /// `pipeline.secrets_allowed()` (ADR-0029 obligation 5): the only
    /// answer to "may this job be given secrets".
    pub secrets_allowed: bool,
    /// The secret names the job asks for, when allowed; empty otherwise.
    pub secret_names: Vec<String>,
    pub lease_until: String,
    pub token_expires_at: String,
    pub token: CiToken,
}

fn lease_lost(job: i64, runner: i64, operation: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::JobLeaseLost)
        .with_context("job_id", job)
        .with_context("runner_id", runner)
        .with_context("operation", operation)
}

/// The lease as the route's loader reads it (the read pool): which runner
/// holds the job and whether it has ended; `None` when there is no job.
pub async fn lease_of(db: &Db, job: i64) -> Result<Option<(Option<i64>, bool)>, ForgeError> {
    const QUERY: &str = "ci_runners.lease_of";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT runner_id, status IN ('succeeded', 'failed', 'canceled') \
             FROM forge.ci_jobs WHERE id = $1",
        )
        .bind(job)
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// What a job's fetch of its pinned commit is decided on (M4 C4): the job's
/// pipeline, the pipeline's project and SHA, and that project's repository
/// (`None` without such a job). Read in one statement on the read pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCopy {
    pub job_id: i64,
    pub pipeline_id: i64,
    pub project: forge_core::ProjectId,
    pub sha: forge_core::ObjectId,
    pub repository: forge_core::ProjectRepository,
}

/// The facts of [`JobCopy`] for `job`; a stored SHA or repository state that
/// does not parse is `FG-SYS-0001` (never a pass).
pub async fn job_copy(db: &Db, job: i64) -> Result<Option<JobCopy>, ForgeError> {
    const QUERY: &str = "ci_runners.job_copy";
    let row: Option<(i64, i64, String, i64, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.id, p.project_id, p.sha, r.id, r.state \
                 FROM forge.ci_jobs j \
                 JOIN forge.ci_pipelines p ON p.id = j.pipeline_id \
                 JOIN forge.repositories r ON r.project_id = p.project_id \
                 WHERE j.id = $1",
            )
            .bind(job)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some((pipeline_id, project, sha, repo, state)) = row else {
        return Ok(None);
    };
    let corrupt = |reason: &'static str| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", reason)
            .with_context("job_id", job)
    };
    Ok(Some(JobCopy {
        job_id: job,
        pipeline_id,
        project: forge_core::ProjectId::new(project),
        sha: forge_core::ObjectId::parse(&sha).map_err(|_| corrupt("pipeline_sha_invalid"))?,
        repository: forge_core::ProjectRepository {
            id: forge_core::RepoId::new(repo),
            state: forge_core::RepositoryState::parse(&state)
                .ok_or_else(|| corrupt("unknown_repository_state"))?,
        },
    }))
}

type ClaimRow = (
    i64,
    i32,
    String,
    String,
    String,
    i32,
    Vec<String>,
    Vec<String>,
    String,
);

/// Claim the oldest job `runner` can run (module docs); `None`: nothing
/// waits for it (or it was disabled meanwhile).
pub async fn claim(db: &Db, runner: i64) -> Result<Option<Claimed>, ForgeError> {
    const QUERY: &str = "ci_runners.claim";
    let token = CiToken::generate(CiTokenKind::Job).map_err(|_| token_failed())?;
    let hash = token.secret_sha256();
    let public_id = token.public_id().as_str().to_owned();
    let claimed = db
        .within(QUERY, async {
            let mut tx = db
                .write_pool()
                .begin()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            let tags: Option<Vec<String>> = sqlx::query_scalar(
                "SELECT tags FROM forge.ci_runners WHERE id = $1 AND enabled FOR SHARE",
            )
            .bind(runner)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let Some(tags) = tags else {
                tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
                return Ok(None);
            };
            let picked: Option<(i64, i64)> = sqlx::query_as(
                "SELECT j.id, j.pipeline_id FROM forge.ci_jobs j \
                 JOIN forge.ci_pipelines p ON p.id = j.pipeline_id \
                 JOIN forge.projects pr ON pr.id = j.project_id \
                 JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = pr.id \
                 WHERE j.status = 'pending' AND j.tags <@ $1::text[] \
                   AND p.status IN ('pending', 'running') AND pr.deleted_at IS NULL \
                 ORDER BY j.id LIMIT 1 \
                 FOR UPDATE OF j, p SKIP LOCKED FOR SHARE OF pr, n SKIP LOCKED",
            )
            .bind(&tags)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let Some((job, pipeline_id)) = picked else {
                tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
                return Ok(None);
            };
            let row: ClaimRow = sqlx::query_as(&format!(
                "UPDATE forge.ci_jobs SET status = 'running', runner_id = $2, \
                   lease_until = now() + make_interval(secs => $3), started_at = now(), \
                   updated_at = now() WHERE id = $1 \
                 RETURNING id, attempt, name, stage, image, timeout_s, tags, secrets, \
                   to_char(lease_until AT TIME ZONE 'UTC', '{TS}')"
            ))
            .bind(job)
            .bind(runner)
            .bind(LEASE_S as f64)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            sqlx::query(
                "UPDATE forge.ci_pipelines SET status = 'running', updated_at = now() \
                 WHERE id = $1 AND status = 'pending'",
            )
            .bind(pipeline_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let pipeline: PipelineRow = sqlx::query_as(&format!(
                "SELECT {PIPELINE_COLUMNS} FROM forge.ci_pipelines WHERE id = $1"
            ))
            .bind(pipeline_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let pipeline = pipeline_of(pipeline)?;
            let expires: String = sqlx::query_scalar(&format!(
                "INSERT INTO forge.ci_job_tokens (public_id, secret_sha256, job_id, attempt, \
                   runner_id, project_id, sha, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(secs => $8)) \
                 RETURNING to_char(expires_at AT TIME ZONE 'UTC', '{TS}')"
            ))
            .bind(&public_id)
            .bind(hash.as_slice())
            .bind(job)
            .bind(row.1)
            .bind(runner)
            .bind(pipeline.project.get())
            .bind(&pipeline.sha)
            .bind((i64::from(row.5) + TOKEN_GRACE_S) as f64)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            tx.commit().await.map_err(|e| query_error(QUERY, e))?;
            Ok(Some((row, pipeline, expires)))
        })
        .await?;
    let Some((row, pipeline, token_expires_at)) = claimed else {
        return Ok(None);
    };
    let (job_id, attempt, name, stage, image, timeout_s, tags, secrets, lease_until) = row;
    let secrets_allowed = pipeline.secrets_allowed();
    Ok(Some(Claimed {
        job_id,
        attempt,
        name,
        stage,
        image,
        timeout_s,
        tags,
        secret_names: if secrets_allowed { secrets } else { Vec::new() },
        secrets_allowed,
        pipeline,
        lease_until,
        token_expires_at,
        token,
    }))
}

/// What a heartbeat found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heartbeat {
    /// Still running: the lease now ends at this time (UTC).
    Running { lease_until: String },
    /// The job was cancelled: stop it and complete (acknowledge).
    Canceled,
}

/// Extend `runner`'s lease on `job` (attempt `attempt`). Errors:
/// `FG-CI-0004` when the runner no longer holds an unexpired lease of that
/// attempt (expired, requeued, retried and claimed again).
pub async fn heartbeat(
    db: &Db,
    runner: i64,
    job: i64,
    attempt: i32,
) -> Result<Heartbeat, ForgeError> {
    const QUERY: &str = "ci_runners.heartbeat";
    let extended: Option<String> = db
        .within(QUERY, async {
            sqlx::query_scalar(&format!(
                "UPDATE forge.ci_jobs SET lease_until = now() + make_interval(secs => $4), \
                   updated_at = now() \
                 WHERE id = $1 AND runner_id = $2 AND attempt = $3 AND status = 'running' \
                   AND lease_until > now() \
                 RETURNING to_char(lease_until AT TIME ZONE 'UTC', '{TS}')"
            ))
            .bind(job)
            .bind(runner)
            .bind(attempt)
            .bind(LEASE_S as f64)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    if let Some(lease_until) = extended {
        return Ok(Heartbeat::Running { lease_until });
    }
    if held_canceled(db, runner, job, attempt).await? {
        return Ok(Heartbeat::Canceled);
    }
    Err(lease_lost(job, runner, "heartbeat").with_context("attempt", attempt))
}

/// Is `job` cancelled while `runner` still holds it (attempt `attempt`)?
async fn held_canceled(db: &Db, runner: i64, job: i64, attempt: i32) -> Result<bool, ForgeError> {
    const QUERY: &str = "ci_runners.held_canceled";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.ci_jobs WHERE id = $1 AND runner_id = $2 \
               AND attempt = $3 AND status = 'canceled')",
        )
        .bind(job)
        .bind(runner)
        .bind(attempt)
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// What a log append did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// Stored.
    Stored,
    /// A chunk with this `seq` was stored already (a resend): nothing done.
    Duplicate,
}

/// Append `body` as chunk `seq` of `job`'s attempt `attempt` (append only).
/// Errors: `FG-CI-0004` (the lease, as [`heartbeat`]), `FG-CI-0006` past
/// [`MAX_LOG_CHUNKS`] / [`MAX_LOG_BYTES`], `FG-WEB-0006` for an empty or
/// oversized chunk, one with a NUL, or a `seq` out of range (the route
/// checks first).
pub async fn append_log(
    db: &Db,
    runner: i64,
    job: i64,
    attempt: i32,
    seq: i64,
    body: &str,
) -> Result<Appended, ForgeError> {
    const QUERY: &str = "ci_runners.append_log";
    // PostgreSQL `text` cannot hold U+0000: refused here, not as a
    // database error.
    if body.is_empty()
        || body.len() > MAX_LOG_CHUNK
        || body.contains('\0')
        || !(0..MAX_LOG_CHUNKS).contains(&seq)
    {
        return Err(ForgeError::new(ErrorCode::RequestInvalid)
            .with_context("reason", "log_chunk_invalid")
            .with_context("job_id", job)
            .with_context("seq", seq));
    }
    let len = i64::try_from(body.len()).unwrap_or(i64::MAX);
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        // One append per job at a time (the totals below stay exact).
        let project: Option<i64> = sqlx::query_scalar(
            "SELECT project_id FROM forge.ci_jobs WHERE id = $1 AND runner_id = $2 \
               AND attempt = $3 AND status = 'running' AND lease_until > now() FOR UPDATE",
        )
        .bind(job)
        .bind(runner)
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some(project) = project else {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Err(lease_lost(job, runner, "log").with_context("attempt", attempt));
        };
        let (chunks, bytes, seen): (i64, i64, bool) = sqlx::query_as(
            "SELECT count(*), coalesce(sum(octet_length(body)), 0)::bigint, \
               coalesce(bool_or(seq = $3), false) \
             FROM forge.ci_log_chunks WHERE job_id = $1 AND attempt = $2",
        )
        .bind(job)
        .bind(attempt)
        .bind(seq)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        if seen {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(Appended::Duplicate);
        }
        if chunks >= MAX_LOG_CHUNKS || bytes + len > MAX_LOG_BYTES {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Err(ForgeError::new(ErrorCode::JobLogTooLarge)
                .with_context("job_id", job)
                .with_context("runner_id", runner)
                .with_context("attempt", attempt)
                .with_context("seq", seq)
                .with_context("chunks", chunks)
                .with_context("stored_bytes", bytes)
                .with_context("limit_bytes", MAX_LOG_BYTES));
        }
        sqlx::query(
            "INSERT INTO forge.ci_log_chunks (job_id, attempt, seq, project_id, body) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(job)
        .bind(attempt)
        .bind(i32::try_from(seq).unwrap_or(i32::MAX))
        .bind(project)
        .bind(body)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Appended::Stored)
    })
    .await
}

/// A runner's result for a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobResult {
    Succeeded,
    Failed,
}

impl JobResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// What a completion recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    pub job_id: i64,
    pub pipeline_id: i64,
    pub project_id: i64,
    /// The job's final status: the result, or `canceled` (an acknowledged
    /// cancel: the result is ignored).
    pub status: &'static str,
    /// The pipeline's status after the change.
    pub pipeline_status: String,
}

/// Record `runner`'s `result` for `job` (attempt `attempt`): the job ends,
/// its lease is released and its token revoked in the same transaction;
/// jobs waiting on it become `pending` (or `canceled` after a failure);
/// the pipeline's status follows its jobs. A cancelled job held by the
/// runner is only released (`status: canceled`). Errors: `FG-CI-0004`.
pub async fn complete(
    db: &Db,
    runner: i64,
    job: i64,
    attempt: i32,
    result: JobResult,
) -> Result<Completed, ForgeError> {
    const QUERY: &str = "ci_runners.complete";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let Some((pipeline, project)) = pipeline_of_job(&mut tx, job).await? else {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Err(lease_lost(job, runner, "complete").with_context("attempt", attempt));
        };
        lock_pipeline(&mut tx, pipeline).await?;
        let state: Option<(String, bool)> = sqlx::query_as(
            "SELECT status, lease_until > now() FROM forge.ci_jobs \
             WHERE id = $1 AND runner_id = $2 AND attempt = $3 FOR UPDATE",
        )
        .bind(job)
        .bind(runner)
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let status = match state.as_ref().map(|(s, live)| (s.as_str(), *live)) {
            Some(("canceled", _)) => "canceled",
            Some(("running", true)) => result.as_str(),
            _ => {
                tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
                return Err(lease_lost(job, runner, "complete").with_context("attempt", attempt));
            }
        };
        sqlx::query(
            "UPDATE forge.ci_jobs SET status = $2, runner_id = NULL, lease_until = NULL, \
               finished_at = coalesce(finished_at, now()), updated_at = now() WHERE id = $1",
        )
        .bind(job)
        .bind(status)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let why = if status == "canceled" {
            Revocation::JobCanceled
        } else {
            Revocation::JobFinished
        };
        revoke_job_tokens(&mut tx, &[job], why).await?;
        let pipeline_status = settle_pipeline(&mut tx, pipeline).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Completed {
            job_id: job,
            pipeline_id: pipeline,
            project_id: project,
            status,
            pipeline_status,
        })
    })
    .await
}

async fn pipeline_of_job(c: &mut PgConnection, job: i64) -> Result<Option<(i64, i64)>, ForgeError> {
    sqlx::query_as("SELECT pipeline_id, project_id FROM forge.ci_jobs WHERE id = $1")
        .bind(job)
        .fetch_optional(c)
        .await
        .map_err(|e| query_error("ci_runners.pipeline_of_job", e))
}

async fn lock_pipeline(c: &mut PgConnection, pipeline: i64) -> Result<(), ForgeError> {
    sqlx::query("SELECT 1 FROM forge.ci_pipelines WHERE id = $1 FOR UPDATE")
        .bind(pipeline)
        .execute(c)
        .await
        .map(drop)
        .map_err(|e| query_error("ci_runners.lock_pipeline", e))
}

/// After a job ended: jobs whose `needs` failed or were cancelled are
/// cancelled (repeated down the chain, at most one pass per wave), jobs
/// whose `needs` all succeeded become `pending`, and a `pending` /
/// `running` pipeline takes its jobs' state. Returns the pipeline status.
pub(crate) async fn settle_pipeline(
    c: &mut PgConnection,
    pipeline: i64,
) -> Result<String, ForgeError> {
    const QUERY: &str = "ci_runners.settle_pipeline";
    // A chain is at most 49 waves deep (ci_jobs_wave).
    for _ in 0..50 {
        let cancelled = sqlx::query(
            "UPDATE forge.ci_jobs j SET status = 'canceled', finished_at = now(), \
               updated_at = now() \
             WHERE j.pipeline_id = $1 AND j.status = 'created' AND EXISTS ( \
               SELECT 1 FROM forge.ci_jobs n WHERE n.pipeline_id = $1 \
                 AND n.name = ANY (j.needs) AND n.status IN ('failed', 'canceled'))",
        )
        .bind(pipeline)
        .execute(&mut *c)
        .await
        .map_err(|e| query_error(QUERY, e))?
        .rows_affected();
        if cancelled == 0 {
            break;
        }
    }
    sqlx::query(
        "UPDATE forge.ci_jobs j SET status = 'pending', updated_at = now() \
         WHERE j.pipeline_id = $1 AND j.status = 'created' AND NOT EXISTS ( \
           SELECT 1 FROM forge.ci_jobs n WHERE n.pipeline_id = $1 \
             AND n.name = ANY (j.needs) AND n.status <> 'succeeded')",
    )
    .bind(pipeline)
    .execute(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    sqlx::query(
        "UPDATE forge.ci_pipelines p SET status = s.next, updated_at = now(), \
           finished_at = CASE WHEN s.next = 'running' THEN NULL ELSE now() END \
         FROM (SELECT CASE \
                 WHEN bool_or(status IN ('created', 'pending', 'running')) THEN 'running' \
                 WHEN bool_and(status = 'succeeded') THEN 'succeeded' \
                 WHEN bool_or(status = 'failed') THEN 'failed' \
                 ELSE 'canceled' END AS next \
               FROM forge.ci_jobs WHERE pipeline_id = $1) s \
         WHERE p.id = $1 AND p.status IN ('pending', 'running') AND p.status <> s.next",
    )
    .bind(pipeline)
    .execute(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    sqlx::query_scalar("SELECT status FROM forge.ci_pipelines WHERE id = $1")
        .bind(pipeline)
        .fetch_one(&mut *c)
        .await
        .map_err(|e| query_error(QUERY, e))
}

/// What the reaper did with one held job whose lease ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaped {
    pub job_id: i64,
    pub pipeline_id: i64,
    pub project_id: i64,
    pub runner_id: i64,
    /// `requeued` (pending, a new attempt), `failed` (`FG-CI-0004`, the
    /// third lost lease) or `released` (a cancelled job).
    pub outcome: &'static str,
    pub lease_losses: i32,
}

/// What one reaper pass did: the jobs taken back, and the jobs it could not
/// take back (each with its error; the caller logs them). One failing job
/// never stops the pass.
#[derive(Debug, Default)]
pub struct ReapPass {
    pub reaped: Vec<Reaped>,
    pub failed: Vec<(i64, ForgeError)>,
}

/// One reaper pass (module docs): at most [`REAP_BATCH`] held jobs whose
/// lease ended, one transaction each (pipeline, then job: the lock order of
/// cancel and completion). A job another pass or a completion holds is
/// skipped (`SKIP LOCKED`), and the statement re-checks the lease, so a
/// job is returned to the queue exactly once. A job whose transaction
/// fails is rolled back and listed in [`ReapPass::failed`]; the pass goes
/// on with the next. Errors: only the scan itself.
pub async fn reap(db: &Db) -> Result<ReapPass, ForgeError> {
    const QUERY: &str = "ci_runners.reap";
    let due: Vec<(i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT id, pipeline_id FROM forge.ci_jobs \
                 WHERE runner_id IS NOT NULL AND lease_until < now() \
                 ORDER BY lease_until LIMIT $1",
            )
            .bind(REAP_BATCH)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let mut pass = ReapPass {
        reaped: Vec::with_capacity(due.len()),
        failed: Vec::new(),
    };
    for (job, pipeline) in due {
        match db.within(QUERY, reap_one(db, job, pipeline)).await {
            Ok(Some(reaped)) => pass.reaped.push(reaped),
            Ok(None) => {}
            Err(e) => pass.failed.push((job, e.with_context("job_id", job))),
        }
    }
    Ok(pass)
}

type ReapRow = (i64, i64, String, i32, i32);

async fn reap_one(db: &Db, job: i64, pipeline: i64) -> Result<Option<Reaped>, ForgeError> {
    const QUERY: &str = "ci_runners.reap_one";
    let mut tx = db
        .write_pool()
        .begin()
        .await
        .map_err(|e| query_error(QUERY, e))?;
    lock_pipeline(&mut tx, pipeline).await?;
    let held: Option<ReapRow> = sqlx::query_as(
        "SELECT runner_id, project_id, status, lease_losses, attempt FROM forge.ci_jobs \
         WHERE id = $1 AND runner_id IS NOT NULL AND lease_until < now() \
         FOR UPDATE SKIP LOCKED",
    )
    .bind(job)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let Some((runner, project, status, losses, attempt)) = held else {
        tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
        return Ok(None);
    };
    let (outcome, losses) = if status != "running" {
        ("released", losses)
    } else if losses + 1 >= LEASE_LOSS_LIMIT || attempt >= 100 {
        ("failed", (losses + 1).min(LEASE_LOSS_LIMIT))
    } else {
        ("requeued", losses + 1)
    };
    let sql = match outcome {
        "requeued" => {
            "UPDATE forge.ci_jobs SET status = 'pending', runner_id = NULL, lease_until = NULL, \
               attempt = attempt + 1, lease_losses = $2, started_at = NULL, updated_at = now() \
             WHERE id = $1"
        }
        "failed" => {
            "UPDATE forge.ci_jobs SET status = 'failed', runner_id = NULL, lease_until = NULL, \
               lease_losses = $2, error_code = 'FG-CI-0004', finished_at = now(), \
               updated_at = now() WHERE id = $1"
        }
        _ => {
            "UPDATE forge.ci_jobs SET runner_id = NULL, lease_until = NULL, lease_losses = $2, \
               updated_at = now() WHERE id = $1"
        }
    };
    sqlx::query(sql)
        .bind(job)
        .bind(losses)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
    revoke_job_tokens(&mut tx, &[job], Revocation::LeaseLost).await?;
    if outcome == "failed" {
        settle_pipeline(&mut tx, pipeline).await?;
    }
    tx.commit().await.map_err(|e| query_error(QUERY, e))?;
    Ok(Some(Reaped {
        job_id: job,
        pipeline_id: pipeline,
        project_id: project,
        runner_id: runner,
        outcome,
        lease_losses: losses,
    }))
}

/// What doctor `ci.runners` and the worker's `FG-CI-0005` check read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerHealth {
    /// Pending jobs older than [`NO_RUNNER_AFTER_S`].
    pub waiting: i64,
    pub oldest_job_id: Option<i64>,
    pub oldest_wait_s: Option<i64>,
    pub enabled_runners: i64,
    /// Enabled runners heard from within [`RUNNER_SILENT_AFTER_S`].
    pub live_runners: i64,
}

impl RunnerHealth {
    /// Jobs wait past the limit: `FG-CI-0005`.
    pub fn no_runner_available(&self) -> bool {
        self.waiting > 0
    }
}

/// [`RunnerHealth`] on the read pool.
pub async fn health(db: &Db) -> Result<RunnerHealth, ForgeError> {
    const QUERY: &str = "ci_runners.health";
    let row: (i64, Option<i64>, Option<i64>, i64, i64) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT \
                   (SELECT count(*) FROM forge.ci_jobs WHERE status = 'pending' \
                     AND updated_at < now() - make_interval(secs => $1)), \
                   (SELECT id FROM forge.ci_jobs WHERE status = 'pending' \
                     AND updated_at < now() - make_interval(secs => $1) ORDER BY updated_at, id \
                     LIMIT 1), \
                   (SELECT floor(extract(epoch FROM now() - min(updated_at)))::bigint \
                     FROM forge.ci_jobs WHERE status = 'pending' \
                     AND updated_at < now() - make_interval(secs => $1)), \
                   (SELECT count(*) FROM forge.ci_runners WHERE enabled), \
                   (SELECT count(*) FROM forge.ci_runners WHERE enabled \
                     AND last_contact_at > now() - make_interval(secs => $2))",
            )
            .bind(NO_RUNNER_AFTER_S as f64)
            .bind(RUNNER_SILENT_AFTER_S as f64)
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(RunnerHealth {
        waiting: row.0,
        oldest_job_id: row.1,
        oldest_wait_s: row.2,
        enabled_runners: row.3,
        live_runners: row.4,
    })
}
