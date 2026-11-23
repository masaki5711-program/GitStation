//! The `merge_request.prepare` job (plan 18 S6, ADR-0037 (f)): one job per
//! merge request version instead of four. `forge worker` runs it as a
//! composite handler (forge-cli `worker/prepare.rs`): the version
//! (`merge_request.refresh`'s body), the mergeability check, `forge/risk`
//! and the spec checks (`forge/spec-lint`, `forge/spec-trace`,
//! `forge/doc-lint`, `forge/ci-lint`) one after the other, in one claim.
//!
//! - **Enqueued** where the version job was: the `merge-request-refresh`
//!   subscriber for a push that moved a merge request's source branch,
//!   [`super::open_mr_decided`] and [`super::reopen_mr_decided`]. The
//!   payload is the version job's (`merge_request_id`, `pushed_by`); the
//!   dedup key `mr_prepare:<id>:<trigger>` is the version job's with
//!   another prefix (`opened`, `reopened:<revision>`, `push:<seq>`). Not
//!   `<id>:<head>` as the plan sketched: the head is read by the job, not
//!   known when it is enqueued, and a key held by a succeeded job would
//!   keep a source pushed back to an earlier head (A → B → A) from being
//!   recorded again.
//! - **Version events.** A version recorded by the job says so in its
//!   `merge_request.pushed` payload (`"prepared": true`, [`PREPARED`]): the
//!   `risk` and `spec-index` subscribers ask for no job for it — the
//!   prepare job runs those checks itself. A version event without it (a
//!   version job enqueued before S6, or written by any other caller) asks
//!   as before.
//! - **The mergeability step** keeps the queue's bookkeeping: the request
//!   bumps `check_seq` and enqueues its job as always, but ready only after
//!   [`BACKSTOP_DELAY`] ([`super::request_check_backstop`]); the prepare job
//!   then checks that seq itself. Done → the queued job is deleted
//!   ([`super::settle_backstop`] `Drop`); failed → it is made ready now
//!   (`Wake`) and retries, and ends the request, as the mergeability job
//!   always did. While the check runs, the queued job is the request's
//!   live job (the sweeps and the reaper see it); a prepare job that dies
//!   leaves it, and it runs after the delay. A settle that fails is
//!   logged and the job goes on: the queued job then runs after the delay
//!   (a second check of the same seq, or the retry, a little later).
//! - **Retries.** Only a failed version step fails the job (the queue's
//!   backoff). A failed risk or spec step is handed over to its own job
//!   (`check.risk`, `check.spec_index`, the subscribers' dedup keys) and
//!   the prepare job goes on: a failed step retries only itself. When the
//!   hand-over cannot queue a job (its key is held by a finished job, or
//!   the database refused), the step's runs are answered `error` at once.
//! - **A dead prepare job** after its version was recorded: the worker's
//!   `prepare_sweep` answers `error` for the checks the head still lacks
//!   ([`unanswered_dead_prepares`]), so no check waits forever.

use std::time::Duration;

use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

use super::versions::{RefreshRequest, RefreshTrigger};
use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};

/// The kind of the prepare job.
pub const PREPARE_JOB: &str = "merge_request.prepare";
/// One run: the version (60 s), the mergeability check (90 s), the risk
/// evaluation (90 s) and the spec checks (120 s), each bounded on its own
/// (360 s), plus slack for the steps' own bookkeeping (review of S6: the
/// sum alone left no room for the hand-overs and the backstop's update).
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(420);
/// How long the mergeability job of a prepare job's request waits before
/// it may run by itself: the check's own budget (`MERGEABILITY_TIMEOUT`,
/// 90 s) plus 30 s, counted from the request (the check starts right
/// after it). A check that takes longer than that — or a worker that
/// stalls between the request and the check — can race the job; the race
/// only costs a second run of the same `check_seq` (the result is stored
/// once per seq, the merge result once per pair). It runs for real only
/// for a prepare job that died or stopped mid-check.
pub const BACKSTOP_DELAY: Duration =
    Duration::from_secs(super::mergeability::MERGEABILITY_TIMEOUT.as_secs() + 30);
/// The `merge_request.pushed` payload key of a version recorded by the
/// prepare job (module docs).
pub const PREPARED: &str = "prepared";

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", PREPARE_JOB)
}

/// The prepare job for `request`, enqueued because of `trigger`.
pub fn prepare_job(
    request: RefreshRequest,
    trigger: RefreshTrigger,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let id = request.merge_request.get();
    let key = match trigger {
        RefreshTrigger::Opened => format!("mr_prepare:{id}:opened"),
        RefreshTrigger::Reopened { revision } => format!("mr_prepare:{id}:reopened:{revision}"),
        RefreshTrigger::Pushed { seq } => format!("mr_prepare:{id}:push:{seq}"),
    };
    Ok(NewJob {
        kind: JobKind::parse(PREPARE_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "merge_request_id": id,
            "pushed_by": request.pushed_by.map(forge_core::UserId::get),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(PREPARE_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// Whether a `merge_request.pushed` payload was written by the prepare
/// job ([`PREPARED`]; absent or anything but `true`: no).
pub fn prepared_version(payload: &serde_json::Value) -> bool {
    payload.get(PREPARED) == Some(&serde_json::Value::Bool(true))
}

/// A dead prepare job whose merge request's current head has no answer
/// for some of the checks the job owed (review of S6, HIGH): the
/// `prepare_sweep` of `forge worker` answers them `error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredPrepare {
    pub job_id: i64,
    pub merge_request: forge_core::MergeRequestId,
    pub project: forge_core::ProjectId,
    pub iid: i64,
    /// The merge request's current head (the version the job recorded,
    /// or a later one whose own prepare job has not answered yet).
    pub head: forge_core::ObjectId,
    pub origin_request_id: String,
    pub last_error_code: String,
    /// No `forge/risk` run on the head.
    pub risk_missing: bool,
    /// No `forge/spec-lint` run on the head (the spec step writes its
    /// four runs together).
    pub spec_missing: bool,
}

/// At most `limit` [`UnansweredPrepare`]s with a job id above
/// `after_job_id`, by id: dead within
/// `checks::risk::UNANSWERED_WINDOW_DAYS`, of an open merge request with a
/// head in a project that is not soft-deleted, whose head lacks forge's
/// `forge/risk` or `forge/spec-lint` run — and no live prepare job of the
/// merge request (a newer one answers the head itself).
pub async fn unanswered_dead_prepares(
    db: &crate::Db,
    after_job_id: i64,
    limit: i64,
) -> Result<Vec<UnansweredPrepare>, ForgeError> {
    const QUERY: &str = "review.prepare_unanswered";
    type Row = (
        i64,
        i64,
        i64,
        i64,
        String,
        String,
        Option<String>,
        bool,
        bool,
    );
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT j.id, m.id, m.project_id, m.iid, m.head_sha, j.origin_request_id, \
                 j.last_error_code, \
                 NOT EXISTS (SELECT 1 FROM forge.check_runs r WHERE r.project_id = m.project_id \
                   AND r.head_sha = m.head_sha AND r.name = $3), \
                 NOT EXISTS (SELECT 1 FROM forge.check_runs r WHERE r.project_id = m.project_id \
                   AND r.head_sha = m.head_sha AND r.name = $4) \
                 FROM forge.jobs j JOIN forge.merge_requests m ON m.id = CASE \
                 WHEN j.payload->>'merge_request_id' ~ '^[0-9]{1,18}$' \
                 THEN (j.payload->>'merge_request_id')::bigint END \
                 JOIN forge.projects p ON p.id = m.project_id \
                 WHERE j.kind = $1 AND j.state = 'dead' AND j.id > $5 \
                 AND j.finished_at > now() - make_interval(days => $2) \
                 AND p.deleted_at IS NULL AND m.state = 'open' AND m.head_sha IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM forge.jobs l WHERE l.kind = $1 \
                   AND l.state IN ('queued', 'running') \
                   AND l.payload->>'merge_request_id' = j.payload->>'merge_request_id') \
                 AND (NOT EXISTS (SELECT 1 FROM forge.check_runs r \
                   WHERE r.project_id = m.project_id AND r.head_sha = m.head_sha AND r.name = $3) \
                   OR NOT EXISTS (SELECT 1 FROM forge.check_runs r \
                   WHERE r.project_id = m.project_id AND r.head_sha = m.head_sha AND r.name = $4)) \
                 ORDER BY j.id LIMIT $6",
            )
            .bind(PREPARE_JOB)
            .bind(crate::checks::risk::UNANSWERED_WINDOW_DAYS)
            .bind(forge_core::checks::RISK_CHECK)
            .bind(forge_core::checks::SPEC_LINT_CHECK)
            .bind(after_job_id)
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| crate::read_query_error(QUERY, e))
        })
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (job_id, mr, project, iid, head, origin, code, risk_missing, spec_missing) in rows {
        // The table's CHECK keeps it an object id; one this build cannot
        // parse is data that should not exist: passed over, never silently.
        let Ok(head) = forge_core::ObjectId::parse(&head) else {
            tracing::warn!(
                target: "forge_db",
                event = "forge.review.prepare_unanswered_unreadable",
                error_code = ErrorCode::Internal.id(),
                job_id,
                merge_request_id = mr,
                "a dead prepare job's merge request head cannot be read; passed over"
            );
            continue;
        };
        out.push(UnansweredPrepare {
            job_id,
            merge_request: forge_core::MergeRequestId::new(mr),
            project: forge_core::ProjectId::new(project),
            iid,
            head,
            origin_request_id: origin,
            // `jobs_dead_has_error`: a dead job always has one.
            last_error_code: code.unwrap_or_else(|| ErrorCode::JobDeadLettered.id().to_owned()),
            risk_missing,
            spec_missing,
        });
    }
    Ok(out)
}
