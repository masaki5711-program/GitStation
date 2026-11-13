//! `GET …/merge-requests/{iid}/mergeability` (M3 R2, ADR-0024 §3): the
//! stored status of the mergeability check, trusted only while its checked
//! pair is the current head and the target's live tip (see `versions.rs`).
//! An AI agent is not given the conflicted paths of confidential documents
//! (plan 06 D-5, ADR-0026 §6): they are left out, the list says
//! `conflicts_truncated`, and `withheld` counts them.

use axum::response::Response;
use forge_auth::MergeRequestCtx;
use forge_core::{MergeabilityStatus, ObjectId};
use forge_errors::ErrorCode;
use forge_review::StoredMergeability;
use serde::Serialize;

use super::review;
use super::versions::{merge_request, proof_ids};
use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::browse::{answer, query, Target};
use crate::routes::context::{
    audience, marked_files, withheld_logged, without_marked_paths, ContextWithheldBody,
};
use crate::routes::write::bounded;

/// What the mergeability check last stored.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeabilityBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `unchecked`, `checking`, `can_be_merged`, `cannot_be_merged` or
    /// `check_failed`. Only `can_be_merged` lets a merge go ahead; every
    /// status but the two decisions means "unknown".
    pub status: String,
    /// Why: `conflict`, `target_missing`, `unrelated_histories`,
    /// `no_version`, `git_timeout`, `git_busy`, `git_failed`, `job_failed`,
    /// `job_stale` (`null` when mergeable or not checked yet).
    #[schema(required = true)]
    pub reason: Option<String>,
    /// The stored result is about another head or target tip than the
    /// current ones: `status` says `unchecked` until the next check.
    pub stale: bool,
    /// The merge request's current head (`null` before its first version).
    #[schema(required = true)]
    pub head_sha: Option<String>,
    /// The head and target tip the last finished check looked at.
    #[schema(required = true)]
    pub checked_head_sha: Option<String>,
    #[schema(required = true)]
    pub checked_target_sha: Option<String>,
    /// UTC, RFC 3339.
    #[schema(required = true)]
    pub checked_at: Option<String>,
    /// The conflicted paths (at most 100; `conflicts_truncated` beyond, or
    /// when paths an AI agent is not given were left out).
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
    /// For an AI agent (plan 06 D-5): the conflicted paths of confidential
    /// documents left out (absent when none was).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withheld: Option<ContextWithheldBody>,
}

/// The status a reader may trust: a stored result counts only while the
/// pair it checked is the merge request's head and the target's live tip;
/// otherwise the answer is `unchecked` (or `checking` while a run is on),
/// `stale: true`, without the old reason and conflicts (review: a stale
/// `can_be_merged` must never show).
fn mergeability_body(
    iid: i64,
    head: Option<String>,
    tip: Option<&str>,
    stored: Option<StoredMergeability>,
) -> MergeabilityBody {
    let oid = |o: Option<ObjectId>| o.map(|o| o.as_str().to_owned());
    let Some(s) = stored else {
        return MergeabilityBody {
            iid,
            status: MergeabilityStatus::Unchecked.as_str().to_owned(),
            reason: None,
            stale: false,
            head_sha: head,
            checked_head_sha: None,
            checked_target_sha: None,
            checked_at: None,
            conflicts: Vec::new(),
            conflicts_truncated: false,
            withheld: None,
        };
    };
    let finished = match s.status {
        MergeabilityStatus::CanBeMerged
        | MergeabilityStatus::CannotBeMerged
        | MergeabilityStatus::CheckFailed => true,
        MergeabilityStatus::Unchecked | MergeabilityStatus::Checking => false,
    };
    let current = s.checked_head_sha.as_ref().map(ObjectId::as_str) == head.as_deref()
        && s.checked_target_sha.as_ref().map(ObjectId::as_str) == tip;
    let stale = finished && !current;
    let (status, reason, conflicts, conflicts_truncated) = if stale {
        (MergeabilityStatus::Unchecked, None, Vec::new(), false)
    } else {
        (s.status, s.reason, s.conflicts, s.conflicts_truncated)
    };
    MergeabilityBody {
        iid,
        status: status.as_str().to_owned(),
        reason: reason.map(|r| r.as_str().to_owned()),
        stale,
        head_sha: head,
        checked_head_sha: oid(s.checked_head_sha),
        checked_target_sha: oid(s.checked_target_sha),
        checked_at: s.checked_at,
        conflicts,
        conflicts_truncated,
        withheld: None,
    }
}

/// `GET …/mergeability` → `200` the stored status, checked against the
/// live head and target tip (never enqueues).
pub async fn mergeability(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    let review_state = state.clone(); // clone: cheap handles; the closure owns its copy
    let audience = audience(&request);
    let ctx = request.ctx().clone(); // clone: the log of what was withheld
    answer(
        state,
        request,
        "merge_request_mergeability",
        |t: Target, q| async move {
            let (project, iid) = ids?;
            query(q.as_deref(), &[])?;
            let audience = audience?;
            let stored_mr = merge_request(&review_state, project, iid).await?;
            let stored = bounded(
                &review_state,
                "merge_request_mergeability",
                review(&review_state)?.mergeability(stored_mr.id),
            )
            .await?;
            // A branch name: no reachability walk, never counted as an id
            // lookup. A missing branch is a tip of `None`.
            let tip = match t
                .smart
                .browse
                .resolve_ref(
                    t.repository.id,
                    stored_mr.target_branch.as_str(),
                    &t.principal,
                )
                .await
            {
                Ok(rev) => Some(rev.commit),
                Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => None,
                Err(e) => return Err(e),
            };
            let mut body = mergeability_body(iid, stored_mr.head_sha, tip.as_deref(), stored);
            if audience.is_agent() && !body.conflicts.is_empty() {
                // Marked at the head, the target tip or the default tip; an
                // error refuses the answer (fail closed).
                let commits: Vec<&str> = [body.head_sha.as_deref(), tip.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect();
                let marked =
                    marked_files(&t.smart.browse, t.repository.id, &t.principal, &commits).await?;
                let hidden = without_marked_paths(
                    &mut body.conflicts,
                    &mut body.conflicts_truncated,
                    &marked,
                );
                body.withheld =
                    withheld_logged(&ctx, "merge_request_mergeability", audience, hidden);
            }
            Ok(body)
        },
    )
    .await
}
