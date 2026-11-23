//! Approve, unapprove and the revocation of approvals (M3 R5, ADR-0024 §6,
//! §8.1, §8.2). Every command locks the project row, the merge request row
//! and the caller's roles and account (`commands::facts`, the review lock
//! order) and hands them to `decide` before anything is written.

use forge_core::approver::PersonOrg;
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
use crate::act_as::{consume_on, via_on, ActAsUse};
use crate::change::{
    Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
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

/// Whom an approval counts for and their organisation at approval time
/// (ADR-0038 §3.1, §3.5): the approver themself, or the delegator named by
/// `on_behalf_of` (checked active under the transaction's lock,
/// `FG-REVIEW-0033` otherwise). `org` is the organisation of the person it
/// counts for, read by the caller just before (recorded with the row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveAs {
    pub on_behalf_of: Option<UserId>,
    pub org: PersonOrg,
}

impl ApproveAs {
    /// For oneself, the organisation not read (`not_read`): it matches no
    /// selector (tests, and callers without a selector to decide).
    pub fn own_unknown() -> Self {
        Self {
            on_behalf_of: None,
            org: PersonOrg::Unknown("not_read".to_owned()),
        }
    }
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

/// End the live approvals of `mr` (all, or `only`'s; never those in
/// `except`) with `reason`, on `conn`; the ended rows.
async fn end_on(
    conn: &mut PgConnection,
    mr: &StoredMergeRequest,
    reason: UnapproveReason,
    only: Option<UserId>,
    except: &[i64],
) -> Result<Vec<StoredApproval>, ForgeError> {
    let rows: Vec<ApprovalRow> = sqlx::query_as(&format!(
        "UPDATE forge.merge_request_approvals SET revoked_at = now(), revoked_reason = $2 \
         WHERE merge_request_id = $1 AND revoked_at IS NULL \
         AND ($3::bigint IS NULL OR approver_id = $3) AND NOT (id = ANY ($4)) RETURNING {}",
        approval_columns()
    ))
    .bind(mr.id.get())
    .bind(reason.as_str())
    .bind(only.map(UserId::get))
    .bind(except)
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
        .step(|c| async move { end_on(c, mr, reason, None, &[]).await })
        .await?;
    ended_records(mr, ended, reason)
}

/// Carry every live approval of `mr` to `head` inside `tx` (M3 R6,
/// ADR-0024 §8.2: the project keeps approvals on push and forge-review
/// found that the change may carry them): each ends with
/// `revoked_reason = carried` and a new row for `head` and the current
/// target names it in `carried_from`. One `merge_request.approved` per
/// carried approval (`carried_from`, `uncounted_reason = carried`: the job
/// does not evaluate), never an unapprove as well — a change stays within
/// its 100 events (at most 50 live approvals plus the push's own event).
async fn carry_on(
    tx: &mut ChangeTx,
    mr: &StoredMergeRequest,
    head: &ObjectId,
) -> Result<(Vec<AuditEntry>, Vec<DomainEvent>), ForgeError> {
    // M3 S2 (ADR-0025 §4): an approval given acting for a person proves
    // that person saw the head it was given on — never another. It ends
    // with `new_commits` whatever the project keeps on push.
    let act_as = tx
        .step(|c| async move { end_act_as_on(c, mr).await })
        .await?;
    let (mut rows, mut events) = ended_records(mr, act_as, UnapproveReason::NewCommits)?;
    let lapsed = tx
        .step(|c| async move { end_lapsed_delegated_on(c, mr).await })
        .await?;
    let (mut lapsed_rows, mut lapsed_events) =
        ended_records(mr, lapsed, UnapproveReason::NewCommits)?;
    rows.append(&mut lapsed_rows);
    events.append(&mut lapsed_events);
    let reason = UnapproveReason::Carried;
    let ended = tx
        .step(|c| async move { end_on(c, mr, reason, None, &[]).await })
        .await?;
    rows.reserve(ended.len());
    events.reserve(ended.len());
    for old in ended {
        let from = old.id;
        let approver = old.approver;
        let row: ApprovalRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.merge_request_approvals (merge_request_id, project_id, \
                     approver_id, head_sha, target_branch, carried_from, credential_origin, \
                     on_behalf_of, approver_org) \
                     SELECT $1, $2, $3, $4, $5, $6, o.credential_origin, o.on_behalf_of, \
                     o.approver_org FROM forge.merge_request_approvals o WHERE o.id = $6 \
                     RETURNING {}",
                    approval_columns()
                ))
                .bind(mr.id.get())
                .bind(mr.project_id.get())
                .bind(approver.get())
                .bind(head.as_str())
                .bind(mr.target_branch.as_str())
                .bind(from.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("review.approval_carry", e))
            })
            .await?;
        let carried = row.parse()?;
        let (audit, event) = entry(
            mr.project_id,
            carried.id,
            MergeRequestChange::Approved {
                merge_request: mr.id,
                iid: mr.iid,
                approval: carried.id,
                approver,
                head_sha: head.clone(), // clone: the change owns its values
                target_branch: mr.target_branch.clone(),
                counted: false,
                uncounted_reason: Some(UncountedReason::Carried),
                carried_from: Some(from),
            },
        )?;
        rows.push(audit);
        events.push(event);
    }
    Ok((rows, events))
}

/// Carry the live approvals of `mr` whose ids are in `keep` to `head` and
/// end the others with `new_commits`, inside `tx` (M3 R6 review round 2:
/// the version job allowed the change from each kept approval's approved
/// head, [`super::approved_heads_on`]). Each live approval yields exactly
/// one event, either way.
pub(crate) async fn carry_some_on(
    tx: &mut ChangeTx,
    mr: &StoredMergeRequest,
    head: &ObjectId,
    keep: &[i64],
) -> Result<(Vec<AuditEntry>, Vec<DomainEvent>), ForgeError> {
    let reason = UnapproveReason::NewCommits;
    let ended = tx
        .step(|c| async move { end_on(c, mr, reason, None, keep).await })
        .await?;
    let (mut rows, mut events) = ended_records(mr, ended, reason)?;
    // Only the kept ones are live now.
    let (mut carried_rows, mut carried_events) = carry_on(tx, mr, head).await?;
    rows.append(&mut carried_rows);
    events.append(&mut carried_events);
    Ok((rows, events))
}

/// End the live approvals of `mr` that were given acting for a person
/// (`credential_origin = act_as_assertion`) with `new_commits`, on `conn`.
async fn end_act_as_on(
    conn: &mut PgConnection,
    mr: &StoredMergeRequest,
) -> Result<Vec<StoredApproval>, ForgeError> {
    let rows: Vec<ApprovalRow> = sqlx::query_as(&format!(
        "UPDATE forge.merge_request_approvals SET revoked_at = now(), revoked_reason = $2 \
         WHERE merge_request_id = $1 AND revoked_at IS NULL \
         AND credential_origin = 'act_as_assertion' RETURNING {}",
        approval_columns()
    ))
    .bind(mr.id.get())
    .bind(UnapproveReason::NewCommits.as_str())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.approvals_end_act_as", e))?;
    let mut ended: Vec<StoredApproval> = rows
        .into_iter()
        .map(ApprovalRow::parse)
        .collect::<Result<_, _>>()?;
    ended.sort_by_key(|a| a.id.get());
    Ok(ended)
}

/// End the live approvals of `mr` given on someone's behalf whose
/// delegation to the approver no longer holds now (ended, removed,
/// replaced, or the delegator left) with `new_commits`, on `conn` (review
/// of ADR-0038 step C: a carry is a new approval of the new head, so the
/// delegation must hold for it).
async fn end_lapsed_delegated_on(
    conn: &mut PgConnection,
    mr: &StoredMergeRequest,
) -> Result<Vec<StoredApproval>, ForgeError> {
    let rows: Vec<ApprovalRow> = sqlx::query_as(&format!(
        "UPDATE forge.merge_request_approvals SET revoked_at = now(), revoked_reason = $2 \
         WHERE merge_request_id = $1 AND revoked_at IS NULL AND on_behalf_of IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM forge.approval_delegations d \
           JOIN forge.users u ON u.id = d.delegator_id \
           WHERE d.delegator_id = merge_request_approvals.on_behalf_of \
             AND d.delegate_id = merge_request_approvals.approver_id \
             AND d.starts_at <= now() AND now() < d.ends_at \
             AND u.state = 'active' AND u.kind = 'human') \
         RETURNING {}",
        approval_columns()
    ))
    .bind(mr.id.get())
    .bind(UnapproveReason::NewCommits.as_str())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.approvals_end_lapsed_delegated", e))?;
    let mut ended: Vec<StoredApproval> = rows
        .into_iter()
        .map(ApprovalRow::parse)
        .collect::<Result<_, _>>()?;
    ended.sort_by_key(|a| a.id.get());
    Ok(ended)
}

/// An approval ended with a delegation: (approval, merge request, iid,
/// project, approver, head).
type EndedRow = (i64, i64, i64, i64, i64, String);

/// At most this many live approvals end with one removed delegation (one
/// event each, within a change's events).
const MAX_DELEGATED_ENDED: usize = crate::change::MAX_EVENTS - 1;

/// End the live approvals given on `delegator`'s behalf (all of them, or
/// those of another delegate than `keep`) with `delegation_ended`, inside
/// `tx` (review of ADR-0038 step C: removing or replacing a delegation
/// withdraws what the delegate approved for the person). The audit rows
/// (one per approval, on its project: `affected`) and events. More than
/// [`MAX_DELEGATED_ENDED`] is `FG-SYS-0001 reason=delegated_approvals_over_cap`
/// (nothing written).
pub(crate) async fn end_delegated_on(
    tx: &mut ChangeTx,
    delegator: UserId,
    keep: Option<UserId>,
) -> Result<(Vec<AuditEntry>, Vec<DomainEvent>), ForgeError> {
    let rows: Vec<EndedRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "UPDATE forge.merge_request_approvals a SET revoked_at = now(), \
                   revoked_reason = $3 \
                 FROM forge.merge_requests m \
                 WHERE m.id = a.merge_request_id AND a.on_behalf_of = $1 \
                   AND a.revoked_at IS NULL AND ($2::bigint IS NULL OR a.approver_id <> $2) \
                 RETURNING a.id, a.merge_request_id, m.iid, a.project_id, a.approver_id, \
                   a.head_sha",
            )
            .bind(delegator.get())
            .bind(keep.map(UserId::get))
            .bind(UnapproveReason::DelegationEnded.as_str())
            .fetch_all(c)
            .await
            .map_err(|e| query_error("review.approvals_end_delegated", e))
        })
        .await?;
    if rows.len() > MAX_DELEGATED_ENDED {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "delegated_approvals_over_cap")
            .with_context("user_id", delegator.get())
            .with_context("approvals", rows.len()));
    }
    let mut audits = Vec::with_capacity(rows.len());
    let mut events = Vec::with_capacity(rows.len());
    let mut rows = rows;
    rows.sort_by_key(|r| r.0);
    for (id, mr, iid, project, approver, head) in rows {
        let head_sha = ObjectId::parse(&head).map_err(|_| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "stored_approval_head_invalid")
        })?;
        let (audit, event) = entry(
            ProjectId::new(project),
            ApprovalId::new(id),
            MergeRequestChange::Unapproved {
                merge_request: forge_core::ids::MergeRequestId::new(mr),
                iid,
                approval: ApprovalId::new(id),
                approver: UserId::new(approver),
                head_sha,
                reason: UnapproveReason::DelegationEnded,
            },
        )?;
        audits.push(audit);
        events.push(event);
    }
    Ok((audits, events))
}

/// Where an approval written by `ctx` came from (ADR-0017 §4.2): acting
/// for a person, a token, or a session.
fn credential_origin(ctx: &ChangeContext) -> &'static str {
    match (&ctx.actor, ctx.credential_id) {
        (Actor::UserViaServiceAccount { .. }, _) => "act_as_assertion",
        (_, Some(_)) => "token",
        (_, None) => "session",
    }
}

/// The act-as facts of an act-as change inside `tx` (`None` otherwise):
/// the service account's role and binding, locked.
async fn act_as_facts(
    tx: &mut ChangeTx,
    act_as: Option<&ActAsUse>,
    project: ProjectId,
) -> Result<Option<crate::act_as::ViaFacts>, ForgeError> {
    let Some(using) = act_as else {
        return Ok(None);
    };
    let (integration, via) = (using.integration, using.via);
    tx.step(|c| async move { via_on(c, integration, via, project).await })
        .await
        .map(Some)
}

/// The change that only consumes the assertion (an act-as approve that
/// found its approval already there, or an unapprove with none to end).
fn consumed_only(entry: AuditEntry) -> ChangeRecord {
    ChangeRecord {
        audit: entry,
        affected: Vec::new(),
        events: Vec::new(),
    }
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
///
/// M3 S2 (ADR-0025 §5.3): an act-as approve (`act_as`) also reads the
/// service account's facts under the lock (`MergeRequestFacts::via`, for
/// `decide`), records `credential_origin = act_as_assertion`, and consumes
/// the assertion in this transaction (`act_as_assertion.consumed`; a
/// concurrent second use is `FG-AUTH-0020 consumed` and nothing is
/// written) — also when the approval was already there.
///
/// ADR-0038 §3 (step C): `as_` names whom the approval counts for and their
/// organisation at approval time (both recorded with the row and part of
/// its identity); a live approval of the caller for the same pair but for
/// someone else is ended (`unapproved`) and replaced.
#[allow(clippy::too_many_arguments)] // the R5 signature plus the assertion
pub async fn approve_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    act_as: Option<&ActAsUse>,
    as_: &ApproveAs,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
    classify: impl Classify,
) -> Result<(ApproveOutcome, StoredApproval, P), ForgeError> {
    const ACTION: &str = "merge_request.approved";
    let origin = credential_origin(ctx);
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let mut facts = facts(&mut tx, project, Some(iid), viewer).await?;
        facts.via = act_as_facts(&mut tx, act_as, project).await?;
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
        if let Some(delegator) = as_.on_behalf_of {
            let active = tx
                .step(|c| async move { super::delegations::active_on(c, delegator, viewer).await })
                .await?;
            if !active {
                return Err(super::delegations::inactive(
                    "no_delegation",
                    delegator,
                    viewer,
                ));
            }
        }
        let live = tx
            .step(|c| async move { live_approvals_on(c, mr_id).await })
            .await?;
        let mut affected = Vec::new();
        let mut events = Vec::new();
        if let Some(own) = live.iter().find(|a| a.approver == viewer) {
            if own.head_sha == head
                && own.target_branch == mr.target_branch
                && own.on_behalf_of == as_.on_behalf_of
            {
                let answer = (ApproveOutcome::Unchanged, own.clone(), decided); // clone: the answer
                return match act_as {
                    None => Ok(Settle::Discard(answer)),
                    Some(using) => {
                        let entry = consume_on(&mut tx, using, &request).await?;
                        Ok(Settle::Commit(consumed_only(entry), answer))
                    }
                };
            }
            let reason = if own.target_branch != mr.target_branch {
                UnapproveReason::TargetChanged
            } else if own.head_sha == head {
                // ADR-0038 §3.5: the same head, for someone else now.
                UnapproveReason::Unapproved
            } else {
                UnapproveReason::NewCommits
            };
            let mr_ref = &mr;
            let ended = tx
                .step(|c| async move { end_on(c, mr_ref, reason, Some(viewer), &[]).await })
                .await?;
            let (rows, evs) = ended_records(&mr, ended, reason)?;
            affected.extend(rows);
            events.extend(evs);
        } else if live.len() as i64 >= MAX_APPROVALS {
            return Err(full(&mr));
        }
        // Review of ADR-0038 step C: one live approval per person it counts
        // for (the partial unique index is the backstop). The person
        // approving themself replaces their delegate's approval for them
        // (`unapproved`); a delegate approving for someone who already
        // holds one is `FG-REVIEW-0034`.
        let person = as_.on_behalf_of.unwrap_or(viewer);
        if let Some(other) = live
            .iter()
            .find(|a| a.approver != viewer && a.on_behalf_of.unwrap_or(a.approver) == person)
        {
            if as_.on_behalf_of.is_some() {
                return Err(ForgeError::new(ErrorCode::ApprovalPersonAlreadyApproved)
                    .with_context("on_behalf_of", person.get())
                    .with_context("user_id", viewer.get())
                    .with_context("merge_request_id", mr.id.get()));
            }
            let (mr_ref, delegate) = (&mr, other.approver);
            let reason = UnapproveReason::Unapproved;
            let ended = tx
                .step(|c| async move { end_on(c, mr_ref, reason, Some(delegate), &[]).await })
                .await?;
            let (rows, evs) = ended_records(&mr, ended, reason)?;
            affected.extend(rows);
            events.extend(evs);
        }
        let (head_ref, target) = (&head, &mr.target_branch);
        let org = as_.org.to_value().to_string();
        let on_behalf_of = as_.on_behalf_of.map(UserId::get);
        let row: ApprovalRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.merge_request_approvals (merge_request_id, project_id, \
                     approver_id, head_sha, target_branch, credential_origin, on_behalf_of, \
                     approver_org) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb) RETURNING {}",
                    approval_columns()
                ))
                .bind(mr_id.get())
                .bind(project.get())
                .bind(viewer.get())
                .bind(head_ref.as_str())
                .bind(target.as_str())
                .bind(origin)
                .bind(on_behalf_of)
                .bind(org)
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
                carried_from: None,
            },
        )?;
        events.insert(0, event);
        if let Some(using) = act_as {
            affected.push(consume_on(&mut tx, using, &request).await?);
        }
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
/// An act-as unapprove (`act_as`, M3 S2) consumes its assertion in the
/// same transaction, also when there was nothing to withdraw.
pub async fn unapprove_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    act_as: Option<&ActAsUse>,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(ApproveOutcome, Option<StoredApproval>, P), ForgeError> {
    const ACTION: &str = "merge_request.unapproved";
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let mut facts = facts(&mut tx, project, Some(iid), viewer).await?;
        facts.via = act_as_facts(&mut tx, act_as, project).await?;
        let decided = decide(&facts)?;
        let mr = facts.merge_request.ok_or_else(absent)?;
        let reason = UnapproveReason::Unapproved;
        let mr_ref = &mr;
        let mut ended = tx
            .step(|c| async move { end_on(c, mr_ref, reason, Some(viewer), &[]).await })
            .await?;
        // clone: the answer keeps the row
        let Some(first) = ended.first().cloned() else {
            let answer = (ApproveOutcome::Unchanged, None, decided);
            return match act_as {
                None => Ok(Settle::Discard(answer)),
                Some(using) => {
                    let entry = consume_on(&mut tx, using, &request).await?;
                    Ok(Settle::Commit(consumed_only(entry), answer))
                }
            };
        };
        let (mut rows, events) = ended_records(&mr, std::mem::take(&mut ended), reason)?;
        let audit = rows.remove(0);
        if let Some(using) = act_as {
            rows.push(consume_on(&mut tx, using, &request).await?);
        }
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
