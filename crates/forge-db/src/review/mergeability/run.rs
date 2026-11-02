//! Running checks (see the module documentation of `mergeability`).

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{
    AuditDetails, CheckReason, DedupKey, DedupPart, EventType, MergeRequestChange,
    MergeRequestState, MergeabilityStatus, ObjectId, RefName,
};
use forge_errors::{ErrorCode, ForgeError};

use super::stored_invalid;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome};
use crate::{query_error, Db};

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

/// Start the run of request `seq` of `mr`.
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
    pub reason: Option<CheckReason>,
    pub head: Option<ObjectId>,
    pub target: Option<ObjectId>,
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
}

/// What [`record_check`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// A newer request exists, the merge request is no longer open, or its
    /// head moved since the run started: nothing written.
    Superseded,
    /// Stored; `flip`: the conflict event written with it.
    Stored { flip: Option<EventType> },
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

/// The conflict change of a decided `result` after `last` (only a
/// can ⇄ cannot difference flips).
fn flip_of(
    last: Option<MergeabilityStatus>,
    result: &CheckResult,
    subject: (MergeRequestId, i64, i64),
) -> Option<MergeRequestChange> {
    let (merge_request, iid, check_seq) = subject;
    let (Some(head), Some(target)) = (&result.head, &result.target) else {
        return None;
    };
    match (last?, result.status) {
        (MergeabilityStatus::CanBeMerged, MergeabilityStatus::CannotBeMerged) => {
            Some(MergeRequestChange::ConflictDetected {
                merge_request,
                iid,
                check_seq,
                head_sha: head.clone(),     // clone: the change owns its ids
                target_sha: target.clone(), // clone: as above
                conflict_files: result.conflicts.len(),
                files_truncated: result.conflicts_truncated,
            })
        }
        (MergeabilityStatus::CannotBeMerged, MergeabilityStatus::CanBeMerged) => {
            Some(MergeRequestChange::ConflictResolved {
                merge_request,
                iid,
                check_seq,
                head_sha: head.clone(),     // clone: the change owns its ids
                target_sha: target.clone(), // clone: as above
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

/// `(check_seq, last_decided, project_id, iid, state, head_sha)` under the
/// row lock.
type LockedRow = (i64, Option<String>, i64, i64, String, Option<String>);

/// Store `result` for request `seq` of `mr`, as `ctx`'s actor (the
/// worker: `system`): only while `seq` is the latest, the merge request is
/// open and its head is the one checked (`result.head`).
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
        let row: Option<LockedRow> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT g.check_seq, g.last_decided, m.project_id, m.iid, m.state, \
                     m.head_sha FROM forge.merge_request_mergeability g \
                     JOIN forge.merge_requests m ON m.id = g.merge_request_id \
                     WHERE g.merge_request_id = $1 FOR UPDATE OF g",
                )
                .bind(mr.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("review.mergeability_lock", e))
            })
            .await?;
        let Some((latest, last_decided, project, iid, state, head)) = row else {
            return Ok(crate::change::Settle::Discard(Recorded::Superseded));
        };
        let checked_head = result.head.as_ref().map(ObjectId::as_str);
        if latest != seq
            || state != MergeRequestState::Open.as_str()
            || head.as_deref() != checked_head
        {
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
            .bind(result.reason.map(CheckReason::as_str))
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
