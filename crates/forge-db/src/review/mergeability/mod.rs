//! Whether a merge request can be merged (M3 R2, ADR-0024 §3; "実装で決めた
//! こと（R2）"): the derived table `forge.merge_request_mergeability` and the
//! mergeability job's SQL (the git work is forge-review's
//! `mergeability::check`).
//!
//! - **Request** ([`request_check_on`], in the caller's transaction): one
//!   statement bumps the row's `check_seq` and sets `unchecked` (creating
//!   the row), then the job `merge_request.mergeability` is enqueued with
//!   dedup key `mr_mergeability:<merge_request_id>:<check_seq>` — a fresh
//!   key per request, so a target going A → B → A is checked again even
//!   though the dedup index keeps succeeded jobs (20261027120000). Callers:
//!   the edit command on a retarget, the `merge-request-refresh` subscriber
//!   on a target push (deferred ones past the fan-out cap get the bump
//!   without a job, [`defer_checks_on`]), and the version job after a
//!   version ([`request_check`], its own transaction). Nothing enqueues on
//!   a read.
//! - **Run**: [`begin_check`] locks the row; a job whose seq is no longer
//!   the latest is `Superseded` (nothing written, the newer job decides);
//!   otherwise it sets `checking` and hands over what to check (the merge
//!   request's recorded head and target branch). [`record_check`] writes
//!   the result under the row lock again, only while the seq is still the
//!   latest.
//! - **Flips** (ADR-0024 §3, §6): `merge_request.conflict_detected` /
//!   `.conflict_resolved` only when a decision (can / cannot) differs from
//!   `last_decided`, the previous decision — never to or from an undecided
//!   status, never on the first decision. The flip commits with one audit
//!   row (actor `system`) and the event (dedup
//!   `<prefix>:<merge_request_id>:<check_seq>`); every other result is a
//!   derived-only commit (`Settle::CommitDerived`).

use std::time::Duration;

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{
    AuditDetails, DedupKey, DedupPart, MergeRequestChange, MergeRequestState, MergeabilityStatus,
    ObjectId, RefName, RequestId,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome};
use crate::jobs::{self, enqueue_on, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::{query_error, read_query_error, Db};

/// The kind of the mergeability job.
pub const MERGEABILITY_JOB: &str = "merge_request.mergeability";
/// One run: a ref read, `merge-base`, `merge-tree` (30 s at most) and three
/// short transactions.
pub const MERGEABILITY_TIMEOUT: Duration = Duration::from_secs(90);

/// The stored status of one merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMergeability {
    pub status: MergeabilityStatus,
    pub reason: Option<String>,
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

fn stored_invalid(reason: &'static str, mr: MergeRequestId) -> ForgeError {
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
        reason,
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

/// The job that checks request `check_seq` of `mr`.
pub fn mergeability_job(
    mr: MergeRequestId,
    check_seq: i64,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let id = mr.get();
    let key = format!("mr_mergeability:{id}:{check_seq}");
    Ok(NewJob {
        kind: JobKind::parse(MERGEABILITY_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({ "merge_request_id": id, "check_seq": check_seq }),
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

/// Bump `mr`'s check (status `unchecked`, `check_seq + 1`) on `conn`, the
/// caller's transaction; the new seq.
async fn bump_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    project: ProjectId,
) -> Result<i64, ForgeError> {
    sqlx::query_scalar(
        "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id) \
         VALUES ($1, $2) ON CONFLICT (merge_request_id) DO UPDATE SET status = 'unchecked', \
         reason = NULL, check_seq = forge.merge_request_mergeability.check_seq + 1, \
         updated_at = now() RETURNING check_seq",
    )
    .bind(mr.get())
    .bind(project.get())
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("review.mergeability_request", e))
}

/// Request a check of `mr` (module docs) on `conn`, the caller's
/// transaction: the row is `unchecked` with a new seq, and its job is
/// enqueued (it exists only if the caller commits). The new seq.
pub(crate) async fn request_check_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    project: ProjectId,
    origin: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<i64, ForgeError> {
    let seq = bump_on(conn, mr, project).await?;
    let job = mergeability_job(mr, seq, origin, origin_event_seq)?;
    // A fresh seq is a fresh key: never deduplicated.
    let _enqueued = enqueue_on(conn, &job).await?;
    Ok(seq)
}

/// Mark the open merge requests of `project` targeting one of `targets`
/// and not in `enqueued` `unchecked` (a new seq, no job: past the fan-out
/// cap, ADR-0024 §3), on `conn`; how many.
pub(crate) async fn defer_checks_on(
    conn: &mut PgConnection,
    project: ProjectId,
    targets: &[String],
    enqueued: &[i64],
) -> Result<u64, ForgeError> {
    sqlx::query(
        "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id) \
         SELECT id, project_id FROM forge.merge_requests WHERE project_id = $1 \
         AND state = 'open' AND target_branch = ANY($2) AND id <> ALL($3) \
         ON CONFLICT (merge_request_id) DO UPDATE SET status = 'unchecked', reason = NULL, \
         check_seq = forge.merge_request_mergeability.check_seq + 1, updated_at = now()",
    )
    .bind(project.get())
    .bind(targets)
    .bind(enqueued)
    .execute(conn)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| query_error("review.mergeability_defer", e))
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

/// What a check works on ([`begin_check`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSubject {
    pub project: ProjectId,
    pub iid: i64,
    /// The merge request's recorded head (`None`: no version yet).
    pub head: Option<ObjectId>,
    pub target_branch: RefName,
}

/// How a run starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStart {
    /// A newer request exists (or the row is gone): its job decides.
    Superseded { latest: Option<i64> },
    /// The merge request is not open (or gone): not checked.
    NotOpen(Option<MergeRequestState>),
    /// Status `checking`; check this.
    Check(CheckSubject),
}

/// Start the run of request `seq` of `mr` (module docs).
pub async fn begin_check(db: &Db, mr: MergeRequestId, seq: i64) -> Result<CheckStart, ForgeError> {
    const QUERY: &str = "review.mergeability_begin";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let latest: Option<i64> = sqlx::query_scalar(
            "SELECT check_seq FROM forge.merge_request_mergeability WHERE merge_request_id = $1 \
             FOR UPDATE",
        )
        .bind(mr.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        if latest != Some(seq) {
            return Ok(CheckStart::Superseded { latest });
        }
        let row: Option<(i64, i64, String, Option<String>, String)> = sqlx::query_as(
            "SELECT project_id, iid, state, head_sha, target_branch FROM forge.merge_requests \
             WHERE id = $1",
        )
        .bind(mr.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((project, iid, state, head, target)) = row else {
            return Ok(CheckStart::NotOpen(None));
        };
        let state = MergeRequestState::parse(&state)
            .ok_or_else(|| stored_invalid("stored_state_invalid", mr))?;
        if state != MergeRequestState::Open {
            return Ok(CheckStart::NotOpen(Some(state)));
        }
        let head = head
            .map(|h| ObjectId::parse(&h).map_err(|_| stored_invalid("stored_head_sha_invalid", mr)))
            .transpose()?;
        let target_branch =
            RefName::parse(&target).map_err(|_| stored_invalid("stored_branch_invalid", mr))?;
        sqlx::query(
            "UPDATE forge.merge_request_mergeability SET status = 'checking', reason = NULL, \
             updated_at = now() WHERE merge_request_id = $1",
        )
        .bind(mr.get())
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(CheckStart::Check(CheckSubject {
            project: ProjectId::new(project),
            iid,
            head,
            target_branch,
        }))
    })
    .await
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Never `unchecked` / `checking`.
    pub status: MergeabilityStatus,
    pub reason: Option<&'static str>,
    pub head: Option<ObjectId>,
    pub target: Option<ObjectId>,
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
}

/// What [`record_check`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// A newer request exists: nothing written.
    Superseded,
    /// Stored; `flip`: the conflict event written with it.
    Stored { flip: Option<forge_core::EventType> },
}

fn audit_entry(project: ProjectId, details: &AuditDetails) -> AuditEntry {
    AuditEntry {
        action: details.action(),
        outcome: Outcome::Succeeded,
        resource_type: "project",
        resource_id: Some(project.get()),
        project_id: Some(project),
        error_code: None,
        details: details.to_value(),
    }
}

/// The conflict change of a decided `result` after `last` (module docs:
/// only a can ⇄ cannot difference flips).
fn flip_of(
    last: Option<MergeabilityStatus>,
    result: &CheckResult,
    subject: (MergeRequestId, i64, i64),
) -> Option<MergeRequestChange> {
    let (merge_request, iid, check_seq) = subject;
    let (Some(head), Some(target)) = (result.head.clone(), result.target.clone()) else {
        return None;
    };
    match (last?, result.status) {
        (MergeabilityStatus::CanBeMerged, MergeabilityStatus::CannotBeMerged) => {
            Some(MergeRequestChange::ConflictDetected {
                merge_request,
                iid,
                check_seq,
                head_sha: head,
                target_sha: target,
                conflict_files: result.conflicts.len(),
                files_truncated: result.conflicts_truncated,
            })
        }
        (MergeabilityStatus::CannotBeMerged, MergeabilityStatus::CanBeMerged) => {
            Some(MergeRequestChange::ConflictResolved {
                merge_request,
                iid,
                check_seq,
                head_sha: head,
                target_sha: target,
            })
        }
        (
            MergeabilityStatus::Unchecked
            | MergeabilityStatus::Checking
            | MergeabilityStatus::CanBeMerged
            | MergeabilityStatus::CannotBeMerged
            | MergeabilityStatus::CheckFailed,
            _,
        ) => None,
    }
}

/// Store `result` for request `seq` of `mr` (module docs), as `ctx`'s
/// actor (the worker: `system`).
pub async fn record_check(
    db: &Db,
    ctx: &ChangeContext,
    mr: MergeRequestId,
    seq: i64,
    result: &CheckResult,
) -> Result<Recorded, ForgeError> {
    const ACTION: &str = "merge_request.mergeability_checked";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<(i64, Option<String>, i64, i64)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT g.check_seq, g.last_decided, m.project_id, m.iid \
                     FROM forge.merge_request_mergeability g \
                     JOIN forge.merge_requests m ON m.id = g.merge_request_id \
                     WHERE g.merge_request_id = $1 FOR UPDATE OF g",
                )
                .bind(mr.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("review.mergeability_lock", e))
            })
            .await?;
        let Some((latest, last_decided, project, iid)) = row else {
            return Ok(crate::change::Settle::Discard(Recorded::Superseded));
        };
        if latest != seq {
            return Ok(crate::change::Settle::Discard(Recorded::Superseded));
        }
        let last = last_decided
            .as_deref()
            .map(|l| {
                MergeabilityStatus::parse(l)
                    .ok_or_else(|| stored_invalid("stored_last_decided_invalid", mr))
            })
            .transpose()?;
        let decided = result.status.is_decided().then_some(result.status);
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_request_mergeability SET status = $2, reason = $3, \
                 checked_head_sha = $4, checked_target_sha = $5, checked_at = now(), \
                 conflicts = $6, conflicts_truncated = $7, \
                 last_decided = COALESCE($8, last_decided), updated_at = now() \
                 WHERE merge_request_id = $1",
            )
            .bind(mr.get())
            .bind(result.status.as_str())
            .bind(result.reason)
            .bind(result.head.as_ref().map(ObjectId::as_str))
            .bind(result.target.as_ref().map(ObjectId::as_str))
            .bind(&result.conflicts)
            .bind(result.conflicts_truncated)
            .bind(decided.map(MergeabilityStatus::as_str))
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("review.mergeability_store", e))
        })
        .await?;
        let project = ProjectId::new(project);
        let Some(change) = flip_of(last, result, (mr, iid, seq)) else {
            return Ok(crate::change::Settle::CommitDerived(Recorded::Stored {
                flip: None,
            }));
        };
        let dedup = DedupKey::new(
            change.dedup_prefix(),
            &[DedupPart::Id(mr.get()), DedupPart::Id(seq)],
        )?;
        let details = AuditDetails::MergeRequest(change);
        let event_type = details.event_type().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
        })?;
        let record = ChangeRecord {
            audit: audit_entry(project, &details),
            affected: Vec::new(),
            events: vec![DomainEvent {
                event_type,
                schema_version: 1,
                aggregate_type: "merge_request",
                aggregate_id: mr.get(),
                project_id: Some(project),
                payload: details.to_value(),
                dedup_key: Some(dedup),
            }],
        };
        Ok(crate::change::Settle::Commit(
            record,
            Recorded::Stored {
                flip: Some(event_type),
            },
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
