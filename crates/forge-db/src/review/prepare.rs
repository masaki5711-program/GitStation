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
//!   leaves it, and it runs after the delay.
//! - **Retries.** Only a failed version step fails the job (the queue's
//!   backoff). A failed risk or spec step is handed over to its own job
//!   (`check.risk`, `check.spec_index`, the subscribers' dedup keys) and
//!   the prepare job goes on: a failed step retries only itself.

use std::time::Duration;

use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

use super::versions::{RefreshRequest, RefreshTrigger};
use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};

/// The kind of the prepare job.
pub const PREPARE_JOB: &str = "merge_request.prepare";
/// One run: the version (60 s), the mergeability check (90 s), the risk
/// evaluation (90 s) and the spec checks (120 s), each bounded on its own.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(360);
/// How long the mergeability job of a prepare job's request waits before
/// it may run by itself: past the check's own budget
/// (`MERGEABILITY_TIMEOUT`, 90 s), so it never races a check in progress
/// — it runs only for a prepare job that died or stopped mid-check.
pub const BACKSTOP_DELAY: Duration = Duration::from_secs(120);
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
