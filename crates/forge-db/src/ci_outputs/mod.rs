//! The CI outputs (M4 C5, plan 14 §5 C5, ADR-0029 decisions 5 and 7;
//! migration 20261206200000): the job log's read, the failure fingerprints
//! and the diagnosis (plan 04 P-6 / P-7 / P-9, the deterministic part), and
//! the `ci/pipeline` check forge's worker writes.
//!
//! **No log text leaves this module undecided.** A log body
//! (`ci_log_chunks.body`) and a failure excerpt (`ci_job_failures.excerpt`)
//! are read on the write pool only (the read pool cannot select them), and
//! only by:
//! - [`job_log_decided`] / [`diagnosis_decided`]: after the caller's
//!   `decide` (forge-pipelines: `authorize_ci_output` on the pipeline's
//!   mark) returned `Ok` — the body is read after the decision, never
//!   before;
//! - [`fingerprint_failed_jobs`]: the system's own sweep, which hands the
//!   tail of a failed job's log to the caller's `digest` (forge-pipelines:
//!   mask, then fingerprint) and stores only what `digest` returns.
//!
//! The test `ci_log_text_is_read_only_after_a_decision` holds this.
//!
//! **`ci/pipeline`** ([`pipelines_due_for_check`],
//! [`record_pipeline_check`]): a pipeline whose status differs from
//! `check_status` is written as the check of its commit by the system
//! (`Actor::System`, label `forge-ci`, no reporter) through the same write
//! as every check run (`checks::write_run`: the commit's advisory lock, the
//! cap, `check.completed` only when the conclusion changed). Only the
//! newest pipeline of a (project, commit) writes; an older one is marked.
//! Lock order: the project row `FOR SHARE` (as a report), then the
//! pipeline row `FOR UPDATE SKIP LOCKED` (a completion or a claim holding
//! it is not waited for: the next pass takes it), then the check's locks.

use forge_errors::{ErrorCode, ForgeError};
use sqlx::PgConnection;

use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::checks::{write_run, NewCheckRun, Reported};
use crate::pipelines::{CopyMark, JobStatus, PipelineStatus, Source};
use crate::projects::lock_shared;
use crate::{query_error, read_query_error, Db};
use forge_core::{MergeRequestId, ObjectId, ProjectId};

/// The most bytes of log one page returns (whole chunks; the first chunk
/// always, so a page is never empty while chunks remain).
pub const LOG_PAGE_MAX_BYTES: i64 = 1024 * 1024;
/// The most chunks one page returns.
pub const LOG_PAGE_MAX_CHUNKS: i64 = 256;
/// The bytes of log after a page handed to the masking as context (never
/// returned): a secret split across the page's edge is still whole in
/// page + context.
pub const MASK_CONTEXT_BYTES: i64 = 512;
/// The bytes of log before a page (or a failure's tail) handed to the
/// masking as context (never returned): more than after it, so a private
/// key whose `BEGIN` line is before the page is still found.
pub const MASK_BEFORE_BYTES: i64 = 16 * 1024;
/// The most chunks read for the context before a page: the read is a
/// bounded `seq` range, never a scan of the whole log before it.
pub const MASK_BEFORE_CHUNKS: i32 = 256;
/// The most bytes of a failed job's log the fingerprint sweep reads (its
/// end: where a failure is reported).
pub const FAILURE_TAIL_BYTES: i64 = 256 * 1024;
/// The longest stored excerpt (the table's CHECK).
pub const MAX_EXCERPT_BYTES: usize = 8192;
/// The most error codes stored per failure (the table's CHECK).
pub const MAX_ERROR_CODES: usize = 10;

/// The job a log read is for, as read before the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTarget {
    pub pipeline_id: i64,
    pub job_id: i64,
    pub job_name: String,
    /// The job's current attempt.
    pub current_attempt: i32,
    pub status: JobStatus,
    /// The pipeline's mark, measured at its creation (ADR-0029 decision 5).
    pub copy_mark: CopyMark,
}

/// One page of a job attempt's log (unmasked: the caller masks with
/// `before` and `after` as context and returns none of those two).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    pub target: LogTarget,
    pub attempt: i32,
    /// `(seq, body)` in order.
    pub chunks: Vec<(i32, String)>,
    /// Up to [`MASK_BEFORE_BYTES`] of the log before the page (from at
    /// most [`MASK_BEFORE_CHUNKS`] chunks).
    pub before: String,
    /// Up to [`MASK_CONTEXT_BYTES`] of the log after the page.
    pub after: String,
    /// The next stored chunk after this page (`None`: none stored yet).
    pub next_seq: Option<i32>,
}

fn unreadable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// The last `max` bytes of `text`, on a char boundary.
fn tail(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut at = text.len() - max;
    while !text.is_char_boundary(at) {
        at += 1;
    }
    text[at..].to_owned()
}

/// The first `max` bytes of `text`, on a char boundary.
fn head(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text.truncate(at);
    text
}

fn context_bytes() -> usize {
    usize::try_from(MASK_CONTEXT_BYTES).unwrap_or(512)
}

fn before_bytes() -> usize {
    usize::try_from(MASK_BEFORE_BYTES).unwrap_or(16 * 1024)
}

/// Up to [`MASK_BEFORE_BYTES`] of the log before chunk `from_seq`: whole
/// chunks of the `seq` range `[from_seq - MASK_BEFORE_CHUNKS, from_seq)`
/// (a range of the primary key, whatever the log's length), cut to the
/// bytes.
async fn context_before(
    c: &mut PgConnection,
    job: i64,
    attempt: i32,
    from_seq: i32,
) -> Result<String, ForgeError> {
    const QUERY: &str = "ci_outputs.context_before";
    let before: Option<String> = sqlx::query_scalar(
        "SELECT string_agg(body, '' ORDER BY seq) FROM ( \
           SELECT seq, body, sum(octet_length(body)) OVER (ORDER BY seq DESC) AS acc \
           FROM forge.ci_log_chunks WHERE job_id = $1 AND attempt = $2 \
             AND seq < $3 AND seq >= $3 - $5) t \
         WHERE acc - octet_length(body) < $4",
    )
    .bind(job)
    .bind(attempt)
    .bind(from_seq)
    .bind(MASK_BEFORE_BYTES)
    .bind(MASK_BEFORE_CHUNKS)
    .fetch_one(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    Ok(tail(before.unwrap_or_default(), before_bytes()))
}

/// Read one page of `job`'s log in `pipeline` of `project`, from chunk
/// `from_seq` of `attempt` (`None`: the job's current attempt), after
/// `decide` allowed it on the target (forge-pipelines: `authorize_ci_output`
/// with the pipeline's mark). `None` when the project has no such
/// pipeline, the pipeline no such job, or the job no such attempt —
/// before `decide`, so the same 404 for everyone who may read the project.
/// Errors: `decide`'s (nothing read), `FG-DB-*`.
pub async fn job_log_decided<P>(
    db: &Db,
    project: ProjectId,
    (pipeline, job): (i64, i64),
    attempt: Option<i32>,
    from_seq: i32,
    decide: impl FnOnce(&LogTarget) -> Result<P, ForgeError>,
) -> Result<Option<(LogPage, P)>, ForgeError> {
    const QUERY: &str = "ci_outputs.job_log";
    let row: Option<(i64, String, i32, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT j.id, j.name, j.attempt, j.status, p.copy_mark \
                 FROM forge.ci_jobs j JOIN forge.ci_pipelines p ON p.id = j.pipeline_id \
                 WHERE j.id = $1 AND j.pipeline_id = $2 AND p.project_id = $3",
            )
            .bind(job)
            .bind(pipeline)
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some((job_id, job_name, current_attempt, status, mark)) = row else {
        return Ok(None);
    };
    let target = LogTarget {
        pipeline_id: pipeline,
        job_id,
        job_name,
        current_attempt,
        status: JobStatus::parse(&status).ok_or_else(|| unreadable("stored_job_status_invalid"))?,
        copy_mark: CopyMark::parse(&mark).ok_or_else(|| unreadable("stored_mark_invalid"))?,
    };
    let attempt = attempt.unwrap_or(current_attempt);
    if !(1..=current_attempt).contains(&attempt) || from_seq < 0 {
        return Ok(None);
    }
    // The decision comes first: no body is read for a refused caller.
    let decided = decide(&target)?;
    let page = db
        .within(QUERY, async {
            let mut c = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            read_page(&mut c, job_id, attempt, from_seq).await
        })
        .await?;
    let (chunks, before, after, next_seq) = page;
    Ok(Some((
        LogPage {
            target,
            attempt,
            chunks,
            before,
            after,
            next_seq,
        },
        decided,
    )))
}

type Page = (Vec<(i32, String)>, String, String, Option<i32>);

async fn read_page(
    c: &mut PgConnection,
    job: i64,
    attempt: i32,
    from_seq: i32,
) -> Result<Page, ForgeError> {
    const QUERY: &str = "ci_outputs.job_log_page";
    let chunks: Vec<(i32, String)> = sqlx::query_as(
        "SELECT seq, body FROM ( \
           SELECT seq, body, sum(octet_length(body)) OVER (ORDER BY seq) AS acc \
           FROM forge.ci_log_chunks WHERE job_id = $1 AND attempt = $2 AND seq >= $3 \
           ORDER BY seq LIMIT $4) t \
         WHERE acc - octet_length(body) < $5 ORDER BY seq",
    )
    .bind(job)
    .bind(attempt)
    .bind(from_seq)
    .bind(LOG_PAGE_MAX_CHUNKS)
    .bind(LOG_PAGE_MAX_BYTES)
    .fetch_all(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let last = chunks.last().map_or(from_seq - 1, |(s, _)| *s);
    // Context on both sides, each a bounded `seq` range of whole chunks (a
    // chunk holds at least one byte: MASK_CONTEXT_BYTES chunks cover the
    // context after the page).
    let before = context_before(c, job, attempt, from_seq).await?;
    let after: Option<String> = sqlx::query_scalar(
        "SELECT string_agg(body, '' ORDER BY seq) FROM ( \
           SELECT seq, body, sum(octet_length(body)) OVER (ORDER BY seq) AS acc \
           FROM forge.ci_log_chunks WHERE job_id = $1 AND attempt = $2 \
             AND seq > $3 AND seq <= $3 + $4) t \
         WHERE acc - octet_length(body) < $4",
    )
    .bind(job)
    .bind(attempt)
    .bind(last)
    .bind(MASK_CONTEXT_BYTES)
    .fetch_one(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let next_seq: Option<i32> = sqlx::query_scalar(
        "SELECT min(seq) FROM forge.ci_log_chunks \
         WHERE job_id = $1 AND attempt = $2 AND seq > $3",
    )
    .bind(job)
    .bind(attempt)
    .bind(last)
    .fetch_one(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    Ok((
        chunks,
        before,
        head(after.unwrap_or_default(), context_bytes()),
        next_seq,
    ))
}

/// Whether `project` has `pipeline` (and, with `job`, that job in it):
/// metadata only, on the read pool — what a CI output route looks up
/// before it measures anything (a bogus id costs one indexed read).
/// Errors: `FG-DB-*`.
pub async fn output_exists(
    db: &Db,
    project: ProjectId,
    pipeline: i64,
    job: Option<i64>,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "ci_outputs.output_exists";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.ci_pipelines p \
               WHERE p.id = $1 AND p.project_id = $2 \
                 AND ($3::bigint IS NULL OR EXISTS (SELECT 1 FROM forge.ci_jobs j \
                   WHERE j.id = $3 AND j.pipeline_id = p.id)))",
        )
        .bind(pipeline)
        .bind(project.get())
        .bind(job)
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

// ── failure fingerprints (plan 04 P-6 / P-7) ──

/// What the fingerprint sweep hands its `digest`: the end of one failed
/// job attempt's log (unmasked: `digest` masks before it keeps anything).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureLog {
    pub job_name: String,
    /// The job's own error code (`FG-CI-0004` lease lost, a timeout…).
    pub job_error_code: Option<String>,
    /// Up to [`MASK_BEFORE_BYTES`] of the log before `tail` (masking
    /// context only, never kept).
    pub before: String,
    /// The last [`FAILURE_TAIL_BYTES`] of the attempt's log (whole chunks;
    /// empty: no log).
    pub tail: String,
    /// The 1-based line number of `tail`'s first line in the attempt's log.
    pub first_line: i64,
    /// Earlier chunks were not read.
    pub truncated: bool,
}

/// What `digest` returns for a [`FailureLog`]: all that is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDigest {
    /// `fp_` + 16 lowercase hex digits.
    pub fingerprint: String,
    /// At most [`MAX_ERROR_CODES`] `FG-…` codes.
    pub error_codes: Vec<String>,
    /// The masked failure lines, at most [`MAX_EXCERPT_BYTES`].
    pub excerpt: String,
    /// The excerpt's first and last line in the attempt's log.
    pub lines: Option<(i32, i32)>,
}

/// One failed job attempt the sweep fingerprinted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprinted {
    pub job_id: i64,
    pub attempt: i32,
    pub pipeline_id: i64,
    pub project_id: i64,
    pub fingerprint: String,
    /// The fingerprint's occurrences after this one (1: first seen).
    pub occurrences: i32,
}

fn digest_invalid(field: &'static str) -> ForgeError {
    unreadable("failure_digest_invalid").with_context("field", field)
}

fn valid_digest(d: &FailureDigest) -> Result<(), ForgeError> {
    let fp = d.fingerprint.strip_prefix("fp_").unwrap_or_default();
    if fp.len() != 16 || !fp.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(digest_invalid("fingerprint"));
    }
    if d.error_codes.len() > MAX_ERROR_CODES {
        return Err(digest_invalid("error_codes"));
    }
    if d.excerpt.len() > MAX_EXCERPT_BYTES || d.excerpt.contains('\0') {
        return Err(digest_invalid("excerpt"));
    }
    if d.lines.is_some_and(|(a, b)| a < 1 || b < a) {
        return Err(digest_invalid("lines"));
    }
    Ok(())
}

/// Fingerprint at most `limit` failed job attempts not fingerprinted yet,
/// each in its own short transaction: the job row `FOR UPDATE SKIP LOCKED`
/// (a retry or a completion in flight is not waited for), the tail of its
/// log to `digest`, one `ci_job_failures` row (exactly once per attempt)
/// and the fingerprint folded into `failure_fingerprints`
/// (`occurrences + 1`). Returns what was recorded. Errors: `FG-DB-*`,
/// `FG-SYS-0001` for a digest out of shape (the job stays to retry).
pub async fn fingerprint_failed_jobs(
    db: &Db,
    limit: i64,
    digest: impl Fn(&FailureLog) -> FailureDigest,
) -> Result<Vec<Fingerprinted>, ForgeError> {
    let mut done = Vec::new();
    for _ in 0..limit.max(0) {
        match fingerprint_one(db, &digest).await? {
            Some(f) => done.push(f),
            None => break,
        }
    }
    Ok(done)
}

async fn fingerprint_one(
    db: &Db,
    digest: &impl Fn(&FailureLog) -> FailureDigest,
) -> Result<Option<Fingerprinted>, ForgeError> {
    const QUERY: &str = "ci_outputs.fingerprint";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        type Candidate = (i64, i32, i64, i64, String, Option<String>);
        let candidate: Option<Candidate> = sqlx::query_as(
            "SELECT id, attempt, pipeline_id, project_id, name, error_code FROM forge.ci_jobs \
             WHERE status = 'failed' AND diagnosed_attempt IS DISTINCT FROM attempt \
             ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((job, attempt, pipeline, project, job_name, job_error_code)) = candidate else {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(None);
        };
        let log = failure_log(&mut tx, job, attempt, job_name, job_error_code).await?;
        let d = digest(&log);
        if let Err(e) = valid_digest(&d) {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Err(e.with_context("job_id", job).with_context("attempt", attempt));
        }
        let seen: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.ci_job_failures WHERE job_id = $1 AND attempt = $2)",
        )
        .bind(job)
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let occurrences = if seen {
            // Recorded already (a crash between the two writes cannot
            // happen: one transaction); only the mark was missing.
            sqlx::query_scalar(
                "SELECT f.occurrences FROM forge.failure_fingerprints f \
                 JOIN forge.ci_job_failures j ON j.fingerprint_id = f.id \
                 WHERE j.job_id = $1 AND j.attempt = $2",
            )
            .bind(job)
            .bind(attempt)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?
        } else {
            let (fingerprint_id, occurrences): (i64, i32) = sqlx::query_as(
                "INSERT INTO forge.failure_fingerprints (project_id, fingerprint, first_job_id, \
                   last_job_id) VALUES ($1, $2, $3, $3) \
                 ON CONFLICT (project_id, fingerprint) DO UPDATE SET \
                   occurrences = forge.failure_fingerprints.occurrences + 1, \
                   last_job_id = EXCLUDED.last_job_id, last_seen_at = now() \
                 RETURNING id, occurrences",
            )
            .bind(project)
            .bind(&d.fingerprint)
            .bind(job)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            sqlx::query(
                "INSERT INTO forge.ci_job_failures (job_id, attempt, project_id, pipeline_id, \
                   fingerprint_id, error_codes, excerpt, first_line, last_line) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(job)
            .bind(attempt)
            .bind(project)
            .bind(pipeline)
            .bind(fingerprint_id)
            .bind(&d.error_codes)
            .bind(&d.excerpt)
            .bind(d.lines.map(|l| l.0))
            .bind(d.lines.map(|l| l.1))
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            occurrences
        };
        sqlx::query("UPDATE forge.ci_jobs SET diagnosed_attempt = attempt WHERE id = $1")
            .bind(job)
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(Fingerprinted {
            job_id: job,
            attempt,
            pipeline_id: pipeline,
            project_id: project,
            fingerprint: d.fingerprint,
            occurrences,
        }))
    })
    .await
}

/// The end of a job attempt's log: whole chunks from the end until
/// [`FAILURE_TAIL_BYTES`], and the line number its first line has.
async fn failure_log(
    c: &mut PgConnection,
    job: i64,
    attempt: i32,
    job_name: String,
    job_error_code: Option<String>,
) -> Result<FailureLog, ForgeError> {
    const QUERY: &str = "ci_outputs.failure_log";
    let (tail_text, first_seq): (Option<String>, Option<i32>) = sqlx::query_as(
        "SELECT string_agg(body, '' ORDER BY seq), min(seq) FROM ( \
           SELECT seq, body, sum(octet_length(body)) OVER (ORDER BY seq DESC) AS acc \
           FROM forge.ci_log_chunks WHERE job_id = $1 AND attempt = $2) t \
         WHERE acc - octet_length(body) < $3",
    )
    .bind(job)
    .bind(attempt)
    .bind(FAILURE_TAIL_BYTES)
    .fetch_one(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let first_seq = first_seq.unwrap_or(0);
    let (lines_before, chunks_before): (i64, i64) = sqlx::query_as(
        "SELECT coalesce(sum(octet_length(body) - octet_length(replace(body, E'\\n', ''))), 0)::bigint, \
           count(*) FROM forge.ci_log_chunks WHERE job_id = $1 AND attempt = $2 AND seq < $3",
    )
    .bind(job)
    .bind(attempt)
    .bind(first_seq)
    .fetch_one(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let before = context_before(c, job, attempt, first_seq).await?;
    Ok(FailureLog {
        job_name,
        job_error_code,
        before,
        tail: tail_text.unwrap_or_default(),
        first_line: lines_before + 1,
        truncated: chunks_before > 0,
    })
}

// ── the diagnosis (plan 04 P-9, deterministic) ──

/// A known fix of a fingerprint (plan 04 P-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownFix {
    pub merge_request_id: i64,
    pub iid: i64,
    pub title: String,
    pub fixed_at: String,
}

/// One failed job of a pipeline as the diagnosis reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFailure {
    pub job_id: i64,
    pub job_name: String,
    pub stage: String,
    pub attempt: i32,
    /// The job's own error code.
    pub job_error_code: Option<String>,
    /// `None` while the sweep has not fingerprinted it yet.
    pub found: Option<FoundFailure>,
}

/// What the fingerprint sweep recorded for one failed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundFailure {
    pub fingerprint: String,
    pub error_codes: Vec<String>,
    /// Masked when stored.
    pub excerpt: String,
    pub lines: Option<(i32, i32)>,
    pub occurrences: i32,
    pub first_seen_at: String,
    pub known_fix: Option<KnownFix>,
}

/// The facts the diagnosis is decided on, read before the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosisTarget {
    pub pipeline_id: i64,
    pub status: PipelineStatus,
    pub copy_mark: CopyMark,
    pub error_code: Option<String>,
}

/// The failed jobs (at their current attempt) of `pipeline` in `project`
/// with what the sweep found, after `decide` allowed the target. `None`
/// when the project has no such pipeline (before `decide`). Errors:
/// `decide`'s (nothing read), `FG-DB-*`.
pub async fn diagnosis_decided<P>(
    db: &Db,
    project: ProjectId,
    pipeline: i64,
    decide: impl FnOnce(&DiagnosisTarget) -> Result<P, ForgeError>,
) -> Result<Option<(DiagnosisTarget, Vec<StoredFailure>, P)>, ForgeError> {
    const QUERY: &str = "ci_outputs.diagnosis";
    let row: Option<(String, String, Option<String>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT status, copy_mark, error_code FROM forge.ci_pipelines \
                 WHERE id = $1 AND project_id = $2",
            )
            .bind(pipeline)
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some((status, mark, error_code)) = row else {
        return Ok(None);
    };
    let target = DiagnosisTarget {
        pipeline_id: pipeline,
        status: PipelineStatus::parse(&status)
            .ok_or_else(|| unreadable("stored_status_invalid"))?,
        copy_mark: CopyMark::parse(&mark).ok_or_else(|| unreadable("stored_mark_invalid"))?,
        error_code,
    };
    // The decision comes first: no excerpt is read for a refused caller.
    let decided = decide(&target)?;
    type Row = (
        i64,
        String,
        String,
        i32,
        Option<String>,
        Option<String>,
        Option<Vec<String>>,
        Option<String>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT j.id, j.name, j.stage, j.attempt, j.error_code, f.fingerprint, \
                   jf.error_codes, jf.excerpt, jf.first_line, jf.last_line, f.occurrences, \
                   to_char(f.first_seen_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                   m.id, m.iid, m.title, \
                   to_char(f.fixed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.ci_jobs j \
                 LEFT JOIN forge.ci_job_failures jf ON jf.job_id = j.id AND jf.attempt = j.attempt \
                 LEFT JOIN forge.failure_fingerprints f ON f.id = jf.fingerprint_id \
                 LEFT JOIN forge.merge_requests m ON m.id = f.fixed_by_merge_request_id \
                 WHERE j.pipeline_id = $1 AND j.project_id = $2 AND j.status = 'failed' \
                 ORDER BY j.wave, j.stage, j.name",
            )
            .bind(pipeline)
            .bind(project.get())
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let failures = rows
        .into_iter()
        .map(|r| {
            let found = match (r.5, r.6, r.7, r.10, r.11) {
                (Some(fingerprint), Some(error_codes), Some(excerpt), Some(occ), Some(first)) => {
                    Some(FoundFailure {
                        fingerprint,
                        error_codes,
                        excerpt,
                        lines: r.8.zip(r.9),
                        occurrences: occ,
                        first_seen_at: first,
                        known_fix: match (r.12, r.13, r.14, r.15) {
                            (Some(id), Some(iid), Some(title), Some(at)) => Some(KnownFix {
                                merge_request_id: id,
                                iid,
                                title,
                                fixed_at: at,
                            }),
                            _ => None,
                        },
                    })
                }
                _ => None,
            };
            StoredFailure {
                job_id: r.0,
                job_name: r.1,
                stage: r.2,
                attempt: r.3,
                job_error_code: r.4,
                found,
            }
        })
        .collect();
    Ok(Some((target, failures, decided)))
}

// ── ci/pipeline (ADR-0029 decision 7) ──

/// The pipelines whose status is not yet their check's, oldest first (at
/// most `limit`).
pub async fn pipelines_due_for_check(db: &Db, limit: i64) -> Result<Vec<i64>, ForgeError> {
    const QUERY: &str = "ci_outputs.checks_due";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT id FROM forge.ci_pipelines WHERE check_status IS DISTINCT FROM status \
             ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// What `build` sees to make the `ci/pipeline` run of a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineCheckFacts {
    pub pipeline_id: i64,
    pub project: ProjectId,
    pub status: PipelineStatus,
    pub error_code: Option<String>,
    /// Jobs by status: `(succeeded, failed, canceled, total)`.
    pub jobs: (i64, i64, i64, i64),
}

/// What [`record_pipeline_check`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineCheck {
    /// The pipeline is gone, held by another transaction, or already
    /// reported at its status: nothing done (the next pass looks again).
    Skipped,
    /// A newer pipeline of the same commit owns the check: only marked.
    Superseded,
    /// The check was written; `fixed`: fingerprints this merge request's
    /// success recorded as fixed (plan 04 P-7).
    Written { reported: Box<Reported>, fixed: u64 },
}

/// Write the `ci/pipeline` run of `pipeline` (module docs) as `ctx`'s
/// actor (the system), with the run `build` makes from the locked facts.
/// Errors (nothing written): `build`'s, `FG-SYS-0001` for a run of another
/// name, `FG-REVIEW-0021` (cannot happen: the name is outside the cap),
/// `FG-DB-*`.
pub async fn record_pipeline_check(
    db: &Db,
    ctx: &ChangeContext,
    pipeline: i64,
    build: impl FnOnce(&PipelineCheckFacts) -> Result<NewCheckRun, ForgeError>,
) -> Result<PipelineCheck, ForgeError> {
    const QUERY: &str = "ci_outputs.record_check";
    let project: Option<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar("SELECT project_id FROM forge.ci_pipelines WHERE id = $1")
                .bind(pipeline)
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some(project) = project.map(ProjectId::new) else {
        return Ok(PipelineCheck::Skipped);
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        match lock_shared(&mut tx, project).await {
            Ok(_) => {}
            // Purged meanwhile: its pipelines are gone too.
            Err(e) if e.code() == ErrorCode::NotFound => {
                return Ok(Settle::Discard(PipelineCheck::Skipped))
            }
            Err(e) => return Err(e),
        }
        type Locked = (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            bool,
        );
        let locked: Option<Locked> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT p.status, p.sha, p.check_status, p.error_code, \
                       CASE WHEN p.source = $3 THEN p.merge_request_id END, \
                       NOT EXISTS (SELECT 1 FROM forge.ci_pipelines q WHERE \
                         q.project_id = p.project_id AND q.sha = p.sha AND q.id > p.id) \
                     FROM forge.ci_pipelines p WHERE p.id = $1 AND p.project_id = $2 \
                     FOR UPDATE OF p SKIP LOCKED",
                )
                .bind(pipeline)
                .bind(project.get())
                .bind(Source::MergeRequest.as_str())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some((status, sha, check_status, error_code, merge_request, newest)) = locked else {
            return Ok(Settle::Discard(PipelineCheck::Skipped));
        };
        if check_status.as_deref() == Some(status.as_str()) {
            return Ok(Settle::Discard(PipelineCheck::Skipped));
        }
        let parsed =
            PipelineStatus::parse(&status).ok_or_else(|| unreadable("stored_status_invalid"))?;
        let s = status.clone(); // clone: the step owns its bind
        tx.step(|c| async move {
            sqlx::query("UPDATE forge.ci_pipelines SET check_status = $2 WHERE id = $1")
                .bind(pipeline)
                .bind(s)
                .execute(c)
                .await
                .map(drop)
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
        if !newest {
            return Ok(Settle::CommitDerived(PipelineCheck::Superseded));
        }
        let jobs: (i64, i64, i64, i64) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT count(*) FILTER (WHERE status = 'succeeded'), \
                       count(*) FILTER (WHERE status = 'failed'), \
                       count(*) FILTER (WHERE status = 'canceled'), count(*) \
                     FROM forge.ci_jobs WHERE pipeline_id = $1",
                )
                .bind(pipeline)
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let head = ObjectId::parse(&sha).map_err(|_| unreadable("stored_sha_invalid"))?;
        let new = build(&PipelineCheckFacts {
            pipeline_id: pipeline,
            project,
            status: parsed,
            error_code,
            jobs,
        })?;
        if !forge_core::checks::reserved_from_http(&new.name) {
            return Err(unreadable("not_ci_pipeline_check")
                .with_context("name", new.name.as_str().to_owned()));
        }
        let (reported, record) = write_run(&mut tx, ctx, (project, &head), None, &new).await?;
        let fixed = match (parsed, merge_request) {
            (PipelineStatus::Succeeded, Some(mr)) => {
                mark_fixed(&mut tx, project, MergeRequestId::new(mr), pipeline).await?
            }
            _ => 0,
        };
        Ok(Settle::Commit(
            record,
            PipelineCheck::Written {
                reported: Box::new(reported),
                fixed,
            },
        ))
    }
    .await;
    tx.settle("check_run.reported", outcome).await
}

/// Plan 04 P-7: the fingerprints that failed in an earlier pipeline of
/// merge request `mr` and have no fix yet are fixed by `mr` (its pipeline
/// `pipeline` succeeded).
async fn mark_fixed(
    tx: &mut ChangeTx,
    project: ProjectId,
    mr: MergeRequestId,
    pipeline: i64,
) -> Result<u64, ForgeError> {
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.failure_fingerprints f SET fixed_by_merge_request_id = $2, \
               fixed_at = now() \
             WHERE f.project_id = $1 AND f.fixed_by_merge_request_id IS NULL AND f.id IN ( \
               SELECT jf.fingerprint_id FROM forge.ci_job_failures jf \
               JOIN forge.ci_pipelines p ON p.id = jf.pipeline_id \
               WHERE p.project_id = $1 AND p.merge_request_id = $2 AND p.id < $3)",
        )
        .bind(project.get())
        .bind(mr.get())
        .bind(pipeline)
        .execute(c)
        .await
        .map(|r| r.rows_affected())
        .map_err(|e| query_error("ci_outputs.mark_fixed", e))
    })
    .await
}

#[cfg(test)]
mod tests;
