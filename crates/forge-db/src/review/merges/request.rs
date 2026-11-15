//! `request_merge_mr_decided`: accept a merge request's merge (ADR-0024
//! §4.2 step 1) — decided inside the transaction like every review command
//! — and the facts every merge decision reads under the locks
//! ([`MergeGate`]).

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{MergeMethod, ObjectId, ProtectionRules};
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::credential::{credential_on, CredentialFacts};
use super::{columns, in_progress, merge_job, CredentialRef, MergeRow, StoredMerge};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::protected_refs::rules_on;
use crate::review::commands::{absent, facts, MergeRequestFacts};
use crate::review::mergeability::{mergeability_on, StoredMergeability};
use crate::{query_error, Db};

/// What a merge decision sees, all read under the project, merge request
/// and caller locks: the review facts (account, project, merge request),
/// the project's protection rules (the push path's snapshot shape), the
/// stored mergeability, the live attempt (another one at the request; this
/// one in the job), and the caller's credential as it is now.
#[derive(Debug, Clone)]
pub struct MergeGate {
    pub facts: MergeRequestFacts,
    pub protection: ProtectionRules,
    pub mergeability: Option<StoredMergeability>,
    pub live: Option<StoredMerge>,
    pub credential: CredentialFacts,
    /// M3 R5: the live approvals with their approvers as they are now, and
    /// the project's approval rules (ADR-0024 §8.6).
    pub approvals: crate::review::approvals::ApprovalFacts,
    /// M3 R6: the merge request's unresolved discussions now (the merge
    /// rule's `require_resolved_discussions`, ADR-0024 §7).
    pub unresolved_discussions: u32,
    /// M3 C1: every check run on the merge request's head as stored now
    /// (empty without a head or a required check), read in this transaction with the same SQL
    /// as the merge status (`crate::checks::RUNS_SQL`). M4 C7: with the runs
    /// on every merge result of the head (resolved against the live target
    /// tip by `forge_core::merge_results`).
    pub check_runs: forge_core::MergeCheckRuns,
    /// M3 I1d: the merge request has an open link to a work item of its
    /// project's current binding (read only when the target's merge rule
    /// requires one; `false` otherwise — never consulted then).
    pub work_item_linked: bool,
    /// M4 C9 (review): the head's `forge/risk` run and its summary, read in
    /// this transaction for the auto-merge principal only (`None` for every
    /// other credential; `Some((None, None))`: no run yet).
    pub auto_risk: Option<(Option<forge_core::CheckRun>, Option<String>)>,
}

/// The merge a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMerge {
    pub method: MergeMethod,
    /// The head the requester saw.
    pub head_sha: ObjectId,
    pub message: Option<String>,
    pub delete_source_branch: bool,
    pub credential: CredentialRef,
}

/// The live attempt of `mr`, locked (`None`: none).
pub(in crate::review) async fn live_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
) -> Result<Option<StoredMerge>, ForgeError> {
    let row: Option<MergeRow> = sqlx::query_as(&format!(
        "SELECT {} FROM forge.merge_request_merges WHERE merge_request_id = $1 \
         AND state IN ('queued', 'prepared', 'executing') FOR UPDATE",
        columns()
    ))
    .bind(mr.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.merge_live", e))?;
    row.map(MergeRow::parse).transpose()
}

/// The facts after [`facts`] (which locked the project, the merge request
/// and the caller), for `credential` of the caller.
pub(in crate::review) async fn gate(
    tx: &mut ChangeTx,
    facts: MergeRequestFacts,
    viewer: UserId,
    credential: CredentialRef,
    live: Option<StoredMerge>,
) -> Result<MergeGate, ForgeError> {
    let project = facts.project.project;
    let mr = facts.merge_request.as_ref().ok_or_else(absent)?.id;
    let protection = tx
        .step(|c| async move { rules_on(c, project).await })
        .await?;
    let mergeability = tx
        .step(|c| async move { mergeability_on(c, mr).await })
        .await?;
    let credential = tx
        .step(|c| async move { credential_on(c, viewer, credential).await })
        .await?;
    let approvals = tx
        .step(|c| async move { crate::review::approvals::approval_facts_on(c, project, mr).await })
        .await?;
    let unresolved_discussions = tx
        .step(|c| async move { crate::review::discussions::unresolved_on(c, mr).await })
        .await?;
    // M3 C1: the head's runs, only when the target's merge rule requires a
    // check (one bounded statement; a failure fails the decision).
    let stored = facts.merge_request.as_ref().ok_or_else(absent)?;
    let head = stored
        .head_sha
        .as_deref()
        .filter(|_| {
            !protection
                .merge_rule(&stored.target_branch)
                .required_checks
                .is_empty()
        })
        .map(ObjectId::parse)
        .transpose()?;
    let check_runs = match head {
        Some(head) => {
            tx.step(|c| async move {
                crate::review::merge_results::merge_runs_on(c, project, mr, &head).await
            })
            .await?
        }
        None => forge_core::MergeCheckRuns::default(),
    };
    // M3 I1d: the work item link condition, only when the rule asks.
    let work_item_linked = if protection
        .merge_rule(&stored.target_branch)
        .require_work_item_link
    {
        tx.step(|c| async move { crate::work_items::linked_on(c, mr).await })
            .await?
    } else {
        false
    };
    // M4 C9 (review): the auto-merge principal decides forge/risk under the
    // same locks (whatever the merge rule requires).
    let auto_risk = match (&credential.credential, stored.head_sha.as_deref()) {
        (super::credential::CredentialState::AutoMerge { .. }, Some(head)) => {
            let head = head.to_owned();
            Some(
                tx.step(|c| async move { risk_run_on(c, project, &head).await })
                    .await?,
            )
        }
        _ => None,
    };
    Ok(MergeGate {
        auto_risk,
        work_item_linked,
        unresolved_discussions,
        check_runs,
        facts,
        protection,
        mergeability,
        live,
        credential,
        approvals,
    })
}

/// The `forge/risk` run of `head` and its summary (`(None, None)`: none).
async fn risk_run_on(
    conn: &mut sqlx::PgConnection,
    project: forge_core::ProjectId,
    head: &str,
) -> Result<(Option<forge_core::CheckRun>, Option<String>), ForgeError> {
    const QUERY: &str = "review.merge_gate_risk";
    let row: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT status, conclusion, summary FROM forge.check_runs \
         WHERE project_id = $1 AND head_sha = $2 AND name = $3",
    )
    .bind(project.get())
    .bind(head)
    .bind(forge_core::checks::RISK_CHECK)
    .fetch_optional(conn)
    .await
    .map_err(|e| crate::query_error(QUERY, e))?;
    let Some((status, conclusion, summary)) = row else {
        return Ok((None, None));
    };
    let invalid = || {
        ForgeError::new(forge_errors::ErrorCode::Internal)
            .with_context("reason", "stored_risk_run_invalid")
    };
    let status = forge_core::CheckStatus::parse(&status).ok_or_else(invalid)?;
    let conclusion = conclusion
        .map(|c| forge_core::CheckConclusion::parse(&c).ok_or_else(invalid))
        .transpose()?;
    let run = forge_core::CheckRun::new(status, conclusion).ok_or_else(invalid)?;
    Ok((Some(run), Some(summary)))
}

/// `FG-REVIEW-0011` when merge request `mr` has a live attempt (read under
/// the merge request's row lock, which the caller holds): a close, a
/// retarget or a draft toggle waits for the merge to finish (ADR-0024
/// §4.2 "マージの実行中").
pub(crate) async fn refuse_live_merge(
    tx: &mut ChangeTx,
    mr: MergeRequestId,
    action: &'static str,
) -> Result<(), ForgeError> {
    let live: Option<(i64, String)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT id, state FROM forge.merge_request_merges WHERE merge_request_id = $1 \
                 AND state IN ('queued', 'prepared', 'executing')",
            )
            .bind(mr.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("review.merge_live", e))
        })
        .await?;
    match live {
        Some((id, state)) => Err(in_progress(mr, Some(id), action).with_context("state", state)),
        None => Ok(()),
    }
}

/// Accept the merge of merge request `iid` of `project` asked for by
/// `viewer` (ADR-0024 §4.2 step 1): under the review locks the facts are
/// read and `decide` runs (forge-review: the reconfirmed `authorize()` and
/// `evaluate_merge()`); then a live attempt is `FG-REVIEW-0011`
/// MergeInProgress, else the `queued` attempt and its job are written in
/// one derived-only commit (the merge request row is not changed: the
/// merge is audited when it lands). Returns the attempt.
///
/// The commit message is frozen here (review round 1): `new.message`, or
/// `default_message` of the merge request as locked — a title or
/// description edited while the merge is queued never reaches the commit.
pub async fn request_merge_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    new: &NewMerge,
    decide: impl FnOnce(&MergeGate) -> Result<P, ForgeError>,
    default_message: impl FnOnce(&crate::review::StoredMergeRequest) -> String,
) -> Result<(StoredMerge, P), ForgeError> {
    const ACTION: &str = "merge_request.merge_requested";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let mr = facts.merge_request.as_ref().ok_or_else(absent)?.clone(); // clone: the gate keeps the facts
        let live = tx.step(|c| async move { live_on(c, mr.id).await }).await?;
        let gate = gate(&mut tx, facts, viewer, new.credential, live).await?;
        let decided = decide(&gate)?;
        if let Some(live) = &gate.live {
            return Err(in_progress(mr.id, Some(live.id), ACTION)
                .with_context("state", live.state.as_str()));
        }
        // M4 C8 review LOW: a merge request in its target's merge queue is
        // merged by the queue (on a tested train), never by hand meanwhile.
        let mr_id = mr.id;
        let queued = tx
            .step(|c| async move { super::queue::live_entry_on(c, mr_id).await })
            .await?;
        if let Some(entry) = queued {
            return Err(crate::review::merge_queue::refused("already_queued", mr_id)
                .with_context("entry_id", entry.id)
                .with_context("action", ACTION));
        }
        let message = new
            .message
            .clone() // clone: the step owns its message
            .unwrap_or_else(|| default_message(&mr));
        let new = new.clone(); // clone: moved into the step
        let request_id = ctx.request_id.as_str().to_owned();
        let row: MergeRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.merge_request_merges (merge_request_id, project_id, \
                     method, state, requested_by, credential_kind, credential_id, request_id, \
                     head_sha, target_branch, message, delete_source_branch) VALUES ($1, $2, $3, \
                     'queued', $4, $5, $6, $7, $8, $9, $10, $11) RETURNING {}",
                    columns()
                ))
                .bind(mr.id.get())
                .bind(project.get())
                .bind(new.method.as_str())
                .bind(viewer.get())
                .bind(new.credential.kind())
                .bind(new.credential.id())
                .bind(request_id)
                .bind(new.head_sha.as_str())
                .bind(mr.target_branch.as_str())
                .bind(message)
                .bind(new.delete_source_branch)
                .fetch_one(c)
                .await
                .map_err(|e| {
                    let unique = e
                        .as_database_error()
                        .and_then(|d| d.constraint())
                        .is_some_and(|c| c == "merge_request_merges_one_live");
                    if unique {
                        in_progress(mr.id, None, ACTION)
                    } else {
                        query_error("review.merge_request", e)
                    }
                })
            })
            .await?;
        let stored = row.parse()?;
        tx.enqueue_job(&merge_job(stored.id, ctx.request_id.clone())?) // clone: the job keeps its origin
            .await?;
        Ok(Settle::CommitDerived((stored, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
