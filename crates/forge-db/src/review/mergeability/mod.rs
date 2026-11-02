//! Whether a merge request can be merged (M3 R2, ADR-0024 §3; "実装で決めた
//! こと（R2）"): the derived table `forge.merge_request_mergeability` and the
//! mergeability job's SQL (the git work is forge-review's
//! `mergeability::check`).
//!
//! - **Request** (`request.rs`): one statement bumps the row's `check_seq`
//!   and sets `unchecked` (creating the row; the conflict list is cleared,
//!   `checked_*` keep the last finished check), then the job
//!   `merge_request.mergeability` is enqueued with dedup key
//!   `mr_mergeability:<merge_request_id>:<check_seq>` — a fresh key per
//!   request, so a target going A → B → A is checked again even though the
//!   dedup index keeps succeeded jobs (20261027120000). Callers: the edit
//!   command on a retarget, the `merge-request-refresh` subscriber on a
//!   target push (the rest past the fan-out cap are marked without a job,
//!   [`request::defer_checks_on`]), the version job after a version
//!   ([`request_check`]), and the worker's sweep ([`sweep_unchecked`]: rows
//!   `unchecked` with no live job, oldest first — nothing past the cap is
//!   starved). Nothing enqueues on a read.
//! - **Run** (`run.rs`): [`begin_check`] locks the row; a job whose seq is
//!   no longer the latest is `Superseded`; otherwise it sets `checking` and
//!   hands over what to check. [`record_check`] writes the result under the
//!   row lock again, only while the seq is still the latest, the merge
//!   request still open and its head still the one checked.
//!   [`fail_check`] ends a run that failed for good (`job_failed`);
//!   [`reap_stale_checks`] ends rows left `checking` with no live job
//!   (`job_stale`). The sweep also verifies decided rows against the live
//!   tips (`confirm.rs`: [`decided_candidates`], [`confirm_live`]) and
//!   re-requests those whose pair moved.
//! - **Flips** (ADR-0024 §3, §6): `merge_request.conflict_detected` /
//!   `.conflict_resolved` only when a decision (can / cannot) differs from
//!   `last_decided` — never to or from an undecided status, never on the
//!   first decision. The flip commits with one audit row (actor `system`)
//!   and the event (dedup `<prefix>:<merge_request_id>:<check_seq>`); every
//!   other result is a derived-only commit (`Settle::CommitDerived`).
//! - **Staleness**: the status is about `checked_head_sha` /
//!   `checked_target_sha` only. A new version marks it `unchecked` in the
//!   version's own transaction ([`request::downgrade_on`]); readers compare
//!   the pair with the live head and target before trusting a decision.

use std::time::Duration;

use forge_core::ids::MergeRequestId;
use forge_core::{CheckReason, MergeabilityStatus, ObjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::{query_error, read_query_error, Db};

mod confirm;
mod request;
mod run;
mod stale;

pub use confirm::{confirm_live, decided_candidates, DecidedRow};
pub use request::DEFER_MAX;
pub(crate) use request::{defer_checks_on, downgrade_on, request_check_on};
pub use request::{request_check, sweep_unchecked, SWEEP_BATCH};
pub use run::{begin_check, record_check, CheckResult, CheckStart, CheckSubject, Recorded};
pub use stale::{
    fail_check, health, reap_stale_checks, MergeabilityHealth, STALE_AFTER, UNVERIFIED_AFTER,
};

/// The kind of the mergeability job.
pub const MERGEABILITY_JOB: &str = "merge_request.mergeability";
/// One run: a ref read, `merge-base`, `merge-tree` (30 s at most) and three
/// short transactions.
pub const MERGEABILITY_TIMEOUT: Duration = Duration::from_secs(90);

/// The stored status of one merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMergeability {
    pub status: MergeabilityStatus,
    pub reason: Option<CheckReason>,
    pub check_seq: i64,
    pub checked_head_sha: Option<ObjectId>,
    pub checked_target_sha: Option<ObjectId>,
    /// UTC, RFC 3339.
    pub checked_at: Option<String>,
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
}

type StatusRow = (
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<String>,
    bool,
);

const STATUS_COLUMNS: &str = "status, reason, check_seq, checked_head_sha, checked_target_sha, \
     to_char(checked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), conflicts, \
     conflicts_truncated";

pub(super) fn stored_invalid(reason: &'static str, mr: MergeRequestId) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_request_id", mr.get())
}

fn parse_row(row: StatusRow, mr: MergeRequestId) -> Result<StoredMergeability, ForgeError> {
    let (status, reason, check_seq, head, target, checked_at, conflicts, truncated) = row;
    let oid = |raw: Option<String>| {
        raw.map(|r| {
            ObjectId::parse(&r).map_err(|_| stored_invalid("stored_checked_sha_invalid", mr))
        })
        .transpose()
    };
    Ok(StoredMergeability {
        status: MergeabilityStatus::parse(&status)
            .ok_or_else(|| stored_invalid("stored_mergeability_invalid", mr))?,
        reason: reason
            .map(|r| {
                CheckReason::parse(&r).ok_or_else(|| stored_invalid("stored_reason_invalid", mr))
            })
            .transpose()?,
        check_seq,
        checked_head_sha: oid(head)?,
        checked_target_sha: oid(target)?,
        checked_at,
        conflicts,
        conflicts_truncated: truncated,
    })
}

/// The stored status of `mr` (`None`: no check was ever requested — the
/// API shows `unchecked`). One bounded statement on the read pool.
pub async fn mergeability(
    db: &Db,
    mr: MergeRequestId,
) -> Result<Option<StoredMergeability>, ForgeError> {
    const QUERY: &str = "review.mergeability";
    let row: Option<StatusRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {STATUS_COLUMNS} FROM forge.merge_request_mergeability \
                 WHERE merge_request_id = $1"
            ))
            .bind(mr.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(|r| parse_row(r, mr)).transpose()
}

/// The same on the write pool (the version job compares what it just
/// read; the tests read what was just committed).
pub async fn mergeability_written(
    db: &Db,
    mr: MergeRequestId,
) -> Result<Option<StoredMergeability>, ForgeError> {
    const QUERY: &str = "review.mergeability_written";
    let row: Option<StatusRow> = db
        .within(QUERY, async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            sqlx::query_as(&format!(
                "SELECT {STATUS_COLUMNS} FROM forge.merge_request_mergeability \
                 WHERE merge_request_id = $1"
            ))
            .bind(mr.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(|r| parse_row(r, mr)).transpose()
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", MERGEABILITY_JOB)
}

/// The dedup key of request `check_seq` of `mr` (the live-job predicate of
/// the sweep and the reaper builds the same string in SQL).
fn dedup_key(mr: MergeRequestId, check_seq: i64) -> String {
    format!("mr_mergeability:{}:{check_seq}", mr.get())
}

/// The job that checks request `check_seq` of `mr`.
pub fn mergeability_job(
    mr: MergeRequestId,
    check_seq: i64,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = dedup_key(mr, check_seq);
    Ok(NewJob {
        kind: JobKind::parse(MERGEABILITY_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({ "merge_request_id": mr.get(), "check_seq": check_seq }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(MERGEABILITY_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// What a claimed mergeability job checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeabilityRequest {
    pub merge_request: MergeRequestId,
    pub check_seq: i64,
}

/// The payload of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn mergeability_payload(
    payload: &serde_json::Value,
) -> Result<MergeabilityRequest, ForgeError> {
    let positive = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .filter(|v| *v > 0)
            .ok_or_else(|| invalid_job("invalid_job_payload"))
    };
    Ok(MergeabilityRequest {
        merge_request: MergeRequestId::new(positive("merge_request_id")?),
        check_seq: positive("check_seq")?,
    })
}

/// `FG-REVIEW-0003 reason=<reason>`: a merge request whose mergeability is
/// not being decided (job_failed, job_stale, fanout_deferred).
pub fn not_checked(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::MergeabilityNotChecked).with_context("reason", reason)
}

/// The SQL of "a queued or running job holds this row's latest request"
/// (`g` is the mergeability row).
const LIVE_JOB: &str = "EXISTS (SELECT 1 FROM forge.jobs j \
     WHERE j.kind = 'merge_request.mergeability' \
     AND j.dedup_key = 'mr_mergeability:' || g.merge_request_id || ':' || g.check_seq \
     AND j.state IN ('queued', 'running'))";

/// The deterministic `check_failed` reasons (`CheckReason::is_deterministic`
/// besides `conflict`, which is `cannot_be_merged`).
pub(super) const ANSWERING: &str = "(g.status IN ('can_be_merged', 'cannot_be_merged') \
     OR (g.status = 'check_failed' AND g.reason IN \
     ('target_missing', 'unrelated_histories', 'no_version')))";

#[cfg(test)]
mod tests;
