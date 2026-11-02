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
pub(super) async fn live_on(
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
pub(super) async fn gate(
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
    Ok(MergeGate {
        facts,
        protection,
        mergeability,
        live,
        credential,
    })
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
pub async fn request_merge_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    new: &NewMerge,
    decide: impl FnOnce(&MergeGate) -> Result<P, ForgeError>,
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
                .bind(new.message.as_deref())
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
