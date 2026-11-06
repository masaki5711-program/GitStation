//! `GET …/merge-requests/{iid}/conflicts` (M3 X6, plan 04 C-2, ADR-0024
//! Amendment X6): the stored conflict scan of a merge request and its
//! current predictions — other open merge requests on the same target
//! (drafts included) whose head conflicts with this one's in a path both
//! change, or whose pair could not be decided. Read like the merge request
//! itself (`MergeRequestRead`); never starts a scan.

use axum::response::Response;
use forge_auth::MergeRequestCtx;
use forge_core::ObjectId;
use forge_errors::ErrorCode;
use forge_review::{StoredPrediction, StoredScan};
use serde::Serialize;

use super::review;
use super::versions::{merge_request, proof_ids};
use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::browse::{answer, query, Target};
use crate::routes::write::bounded;

/// The path after `{iid}` of [`CONFLICTS`].
pub const CONFLICTS_SUFFIX: &str = "/conflicts";
/// `GET` the predicted conflicts.
pub const CONFLICTS: &str = "/api/v1/projects/{id}/merge-requests/{iid}/conflicts";

/// The last scan of the merge request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConflictScanBody {
    /// `none` (never scanned), `pending`, `scanning`, `scanned`,
    /// `incomplete` (some pairs were not tried: see `reason`) or `failed`.
    pub status: String,
    /// `incomplete`: `peers_capped`, `pairs_capped`, `budget_exhausted`,
    /// `pair_failed`; `failed`: `no_version`, `target_missing`,
    /// `unrelated_histories`, `git_timeout`, `git_busy`, `git_failed`,
    /// `job_failed`, `job_stale`.
    #[schema(required = true)]
    pub reason: Option<String>,
    /// The finished scan looked at another head, target branch or target
    /// tip than the current ones (its predictions about the head still
    /// stand; a new head is scanned by the version job, a moved target by
    /// the next push).
    pub stale: bool,
    #[schema(required = true)]
    pub scanned_head_sha: Option<String>,
    /// The target tip the finished scan read.
    #[schema(required = true)]
    pub scanned_target_sha: Option<String>,
    /// UTC, RFC 3339.
    #[schema(required = true)]
    pub scanned_at: Option<String>,
    /// Other open merge requests considered, trial merges run, candidates
    /// left out by a cap or the time budget.
    pub peers: i32,
    pub pairs_tried: i32,
    pub pairs_skipped: i32,
}

/// One current prediction.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConflictPredictionBody {
    /// The other merge request's number in the project.
    #[schema(minimum = 1)]
    pub iid: i64,
    pub draft: bool,
    /// The other merge request's head the prediction is about (its current
    /// head).
    pub head_sha: String,
    /// `conflict` (predicted) or `failed` (could not be decided).
    pub outcome: String,
    /// `failed`: `unrelated_histories`, `git_timeout`, `git_busy`,
    /// `git_failed`.
    #[schema(required = true)]
    pub reason: Option<String>,
    /// The conflicted paths both change (at most 100;
    /// `conflicts_truncated` beyond).
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
    /// UTC, RFC 3339.
    pub checked_at: String,
}

/// A merge request's predicted conflicts.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConflictsBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// The merge request's current head (`null` before its first version).
    #[schema(required = true)]
    pub head_sha: Option<String>,
    pub scan: ConflictScanBody,
    /// By the other merge request's number; only pairs whose two heads are
    /// both merge requests' current heads.
    pub predictions: Vec<ConflictPredictionBody>,
}

fn scan_body(
    stored: Option<StoredScan>,
    head: Option<&str>,
    target: &str,
    tip: Option<&str>,
) -> ConflictScanBody {
    let Some(s) = stored else {
        return ConflictScanBody {
            status: "none".to_owned(),
            reason: None,
            stale: false,
            scanned_head_sha: None,
            scanned_target_sha: None,
            scanned_at: None,
            peers: 0,
            pairs_tried: 0,
            pairs_skipped: 0,
        };
    };
    let stale = s.status.is_finished()
        && (s.scanned_head_sha.as_ref().map(ObjectId::as_str) != head
            || s.scanned_target_branch.as_ref().map(|t| t.as_str()) != Some(target)
            || s.scanned_target_sha.as_ref().map(ObjectId::as_str) != tip);
    ConflictScanBody {
        status: s.status.as_str().to_owned(),
        reason: s.reason,
        stale,
        scanned_head_sha: s.scanned_head_sha.map(|h| h.as_str().to_owned()),
        scanned_target_sha: s.scanned_target_sha.map(|h| h.as_str().to_owned()),
        scanned_at: s.scanned_at,
        peers: s.peers,
        pairs_tried: s.pairs_tried,
        pairs_skipped: s.pairs_skipped,
    }
}

fn prediction_body(p: StoredPrediction) -> ConflictPredictionBody {
    ConflictPredictionBody {
        iid: p.other_iid,
        draft: p.other_draft,
        head_sha: p.other_head.as_str().to_owned(),
        outcome: p.outcome.as_str().to_owned(),
        reason: p.reason,
        conflicts: p.conflicts,
        conflicts_truncated: p.conflicts_truncated,
        checked_at: p.checked_at,
    }
}

/// `GET …/conflicts` → `200` the scan and the current predictions, the scan
/// compared with the live head and target tip (never enqueues).
pub async fn conflicts(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    let review_state = state.clone(); // clone: cheap handles; the closure owns its copy
    answer(
        state,
        request,
        "merge_request_conflicts",
        |t: Target, q| async move {
            let (project, iid) = ids?;
            query(q.as_deref(), &[])?;
            let mr = merge_request(&review_state, project, iid).await?;
            let (scan, predictions) = bounded(
                &review_state,
                "merge_request_conflicts",
                review(&review_state)?.conflicts(mr.id),
            )
            .await?;
            // A branch name: no reachability walk. A missing branch is `None`.
            let tip = match t
                .smart
                .browse
                .resolve_ref(t.repository.id, mr.target_branch.as_str(), &t.principal)
                .await
            {
                Ok(rev) => Some(rev.commit),
                Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => None,
                Err(e) => return Err(e),
            };
            Ok(ConflictsBody {
                iid,
                scan: scan_body(
                    scan,
                    mr.head_sha.as_deref(),
                    mr.target_branch.as_str(),
                    tip.as_deref(),
                ),
                head_sha: mr.head_sha,
                predictions: predictions.into_iter().map(prediction_body).collect(),
            })
        },
    )
    .await
}
