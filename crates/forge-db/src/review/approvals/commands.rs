//! Approve, unapprove and the revocation of approvals (M3 R5, ADR-0024 §6,
//! §8.1, §8.2). Every command locks the project row, the merge request row
//! and the caller's roles and account (`commands::facts`, the review lock
//! order) and hands them to `decide` before anything is written.

use forge_core::ids::{ApprovalId, ProjectId, UserId};
use forge_core::{
    AuditDetails, DedupKey, DedupPart, MergeRequestChange, ObjectId, UnapproveReason,
    UncountedReason,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{
    approval_columns, approval_facts_on, live_approvals_on, ApprovalFacts, ApprovalRow,
    StoredApproval, MAX_APPROVALS,
};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::query_error;
use crate::review::commands::{absent, facts, MergeRequestFacts};
use crate::review::StoredMergeRequest;
use crate::Db;

/// What an approval did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveOutcome {
    /// Written: one `merge_request.approved` audit row and event (plus one
    /// `merge_request.unapproved` for a stale live approval it replaced).
    Approved {
        counted: bool,
        uncounted_reason: Option<UncountedReason>,
    },
    /// Unapprove: the caller's live approval ended (`reason=unapproved`).
    Withdrawn,
    /// The caller's live approval is already for this head and target, or
    /// (unapprove) there is none: nothing written.
    Unchanged,
}

/// How an approval is counted right after it was written (forge-review's
/// evaluation of the facts as locked): `(counted, uncounted_reason)`.
pub trait Classify:
    FnOnce(&MergeRequestFacts, &ApprovalFacts, ApprovalId) -> (bool, Option<UncountedReason>)
{
}
impl<F> Classify for F where
    F: FnOnce(&MergeRequestFacts, &ApprovalFacts, ApprovalId) -> (bool, Option<UncountedReason>)
{
}

/// The audit entry and event of one approval change.
/// `approval` is the row the change is about (the dedup key).
fn entry(
    project: ProjectId,
    approval: ApprovalId,
    change: MergeRequestChange,
) -> Result<(AuditEntry, DomainEvent), ForgeError> {
    let dedup = DedupKey::new(change.dedup_prefix(), &[DedupPart::Id(approval.get())])?;
    let aggregate = change.merge_request().get();
    let details = AuditDetails::MergeRequest(change);
    let event_type = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    let value = details.to_value();
    Ok((
        AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the event payload
        },
        DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: aggregate,
            project_id: Some(project),
            payload: value,
            dedup_key: Some(dedup),
        },
    ))
}

/// End the live approvals of `mr` (all, or `only`'s) with `reason`, on
/// `conn`; the ended rows.
async fn end_on(
    conn: &mut PgConnection,
    mr: &StoredMergeRequest,
    reason: UnapproveReason,
    only: Option<UserId>,
) -> Result<Vec<StoredApproval>, ForgeError> {
    let rows: Vec<ApprovalRow> = sqlx::query_as(&format!(
        "UPDATE forge.merge_request_approvals SET revoked_at = now(), revoked_reason = $2 \
         WHERE merge_request_id = $1 AND revoked_at IS NULL \
         AND ($3::bigint IS NULL OR approver_id = $3) RETURNING {}",
        approval_columns()
    ))
    .bind(mr.id.get())
    .bind(reason.as_str())
    .bind(only.map(UserId::get))
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.approvals_end", e))?;
    let mut ended: Vec<StoredApproval> = rows
        .into_iter()
        .map(ApprovalRow::parse)
        .collect::<Result<_, _>>()?;
    ended.sort_by_key(|a| a.id.get());
    Ok(ended)
}

/// The audit rows and events of ended approvals.
fn ended_records(
    mr: &StoredMergeRequest,
    ended: Vec<StoredApproval>,
    reason: UnapproveReason,
) -> Result<(Vec<AuditEntry>, Vec<DomainEvent>), ForgeError> {
    let mut rows = Vec::with_capacity(ended.len());
    let mut events = Vec::with_capacity(ended.len());
    for a in ended {
        let (row, event) = entry(
            mr.project_id,
            a.id,
            MergeRequestChange::Unapproved {
                merge_request: mr.id,
                iid: mr.iid,
                approval: a.id,
                approver: a.approver,
                head_sha: a.head_sha,
                reason,
            },
        )?;
        rows.push(row);
        events.push(event);
    }
    Ok((rows, events))
}

/// Revoke every live approval of `mr` for `reason` inside `tx` (a new head
/// in the version job, a new target in the edit command — ADR-0024 §8.1,
/// §8.2): the audit rows (to add to the change as `affected`) and events.
pub(crate) async fn revoke_on(
    tx: &mut ChangeTx,
    mr: &StoredMergeRequest,
    reason: UnapproveReason,
) -> Result<(Vec<AuditEntry>, Vec<DomainEvent>), ForgeError> {
    let ended = tx
        .step(|c| async move { end_on(c, mr, reason, None).await })
        .await?;
    ended_records(mr, ended, reason)
}

/// `FG-REVIEW-0015` ApprovalLimitReached.
fn full(mr: &StoredMergeRequest) -> ForgeError {
    ForgeError::new(ErrorCode::ApprovalLimitReached)
        .with_context("reason", "approvals_per_merge_request")
        .with_context("limit", MAX_APPROVALS)
        .with_context("project_id", mr.project_id.get())
        .with_context("iid", mr.iid)
        .with_context("merge_request_id", mr.id.get())
}

/// Approve merge request `iid` of `project` as `viewer` for its current
/// head and target (ADR-0024 §8.1): after `decide` (forge-review: the
/// reconfirmed `authorize()` and the requested `sha` against the head as
/// locked, `FG-REVIEW-0008 stale_view`), a live approval of the caller for
/// the same pair is [`ApproveOutcome::Unchanged`]; one for another pair is
/// ended (`new_commits` / `target_changed`) and replaced. More than
/// [`MAX_APPROVALS`] live approvals is `FG-REVIEW-0015`. The new row is
/// classified by `classify` on the approval facts read after the insert
/// (`merge_request.approved` carries `counted` / `uncounted_reason`).
pub async fn approve_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
    classify: impl Classify,
) -> Result<(ApproveOutcome, StoredApproval, P), ForgeError> {
    const ACTION: &str = "merge_request.approved";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let mr = facts.merge_request.clone().ok_or_else(absent)?; // clone: the facts stay for classify
        let head = mr
            .head_sha
            .as_deref()
            .map(ObjectId::parse)
            .transpose()?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::HeadChanged)
                    .with_context("reason", "stale_view")
                    .with_context("detail", "no_version")
                    .with_context("merge_request_id", mr.id.get())
            })?;
        let mr_id = mr.id;
        let live = tx
            .step(|c| async move { live_approvals_on(c, mr_id).await })
            .await?;
        let mut affected = Vec::new();
        let mut events = Vec::new();
        if let Some(own) = live.iter().find(|a| a.approver == viewer) {
            if own.head_sha == head && own.target_branch == mr.target_branch {
                return Ok(Settle::Discard((
                    ApproveOutcome::Unchanged,
                    own.clone(),
                    decided,
                ))); // clone: the answer
            }
            let reason = if own.target_branch == mr.target_branch {
                UnapproveReason::NewCommits
            } else {
                UnapproveReason::TargetChanged
            };
            let mr_ref = &mr;
            let ended = tx
                .step(|c| async move { end_on(c, mr_ref, reason, Some(viewer)).await })
                .await?;
            let (rows, evs) = ended_records(&mr, ended, reason)?;
            affected.extend(rows);
            events.extend(evs);
        } else if live.len() as i64 >= MAX_APPROVALS {
            return Err(full(&mr));
        }
        let (head_ref, target) = (&head, &mr.target_branch);
        let row: ApprovalRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.merge_request_approvals (merge_request_id, project_id, \
                     approver_id, head_sha, target_branch) VALUES ($1, $2, $3, $4, $5) \
                     RETURNING {}",
                    approval_columns()
                ))
                .bind(mr_id.get())
                .bind(project.get())
                .bind(viewer.get())
                .bind(head_ref.as_str())
                .bind(target.as_str())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("review.approval_insert", e))
            })
            .await?;
        let stored = row.parse()?;
        let approvals = tx
            .step(|c| async move { approval_facts_on(c, project, mr_id).await })
            .await?;
        let (counted, uncounted_reason) = classify(&facts, &approvals, stored.id);
        let (audit, event) = entry(
            project,
            stored.id,
            MergeRequestChange::Approved {
                merge_request: mr.id,
                iid: mr.iid,
                approval: stored.id,
                approver: viewer,
                head_sha: head.clone(), // clone: the change owns its values
                target_branch: mr.target_branch.clone(),
                counted,
                uncounted_reason,
            },
        )?;
        events.insert(0, event);
        let record = ChangeRecord {
            audit,
            affected,
            events,
        };
        Ok(Settle::Commit(
            record,
            (
                ApproveOutcome::Approved {
                    counted,
                    uncounted_reason,
                },
                stored,
                decided,
            ),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Withdraw `viewer`'s live approval of merge request `iid` of `project`
/// (`reason=unapproved`), after `decide`. None: [`ApproveOutcome::Unchanged`]
/// and `None`.
pub async fn unapprove_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(ApproveOutcome, Option<StoredApproval>, P), ForgeError> {
    const ACTION: &str = "merge_request.unapproved";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let mr = facts.merge_request.ok_or_else(absent)?;
        let reason = UnapproveReason::Unapproved;
        let mr_ref = &mr;
        let mut ended = tx
            .step(|c| async move { end_on(c, mr_ref, reason, Some(viewer)).await })
            .await?;
        // clone: the answer keeps the row
        let Some(first) = ended.first().cloned() else {
            return Ok(Settle::Discard((ApproveOutcome::Unchanged, None, decided)));
        };
        let (mut rows, events) = ended_records(&mr, std::mem::take(&mut ended), reason)?;
        let audit = rows.remove(0);
        Ok(Settle::Commit(
            ChangeRecord {
                audit,
                affected: rows,
                events,
            },
            (ApproveOutcome::Withdrawn, Some(first), decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
