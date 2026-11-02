//! The reads of a merge request's versions and mergeability (M3 R2,
//! ADR-0024 §1–§3), every one `GET` and decided as `MergeRequestRead` (as
//! the repository browse API; no new action):
//!
//! - `GET …/merge-requests/{iid}/versions` — the versions, oldest first,
//!   with their head, bases, pusher and whether their refs were purged;
//! - `GET …/versions/{version}/diff` — one version's diff,
//!   `merge_base_sha..head_sha` (the target's tip when the version was
//!   recorded when there was no common history; `FG-REPO-0023
//!   reason=ref_not_found version_base=pending` while the version job has
//!   not recorded the bases yet);
//! - `GET …/compare?from=<version>&to=<version>` — two versions compared,
//!   `head(from)..head(to)` (two-dot: what changed between the reviews);
//! - `GET …/mergeability` — the stored status of the mergeability check
//!   (a separate route rather than part of the merge request body: the
//!   list and the writes do not pay for it, and its answer changes on its
//!   own). Reading never enqueues a check.
//!
//! Every commit id goes through the browse API's merge request view
//! (`Browser::merge_request_commit` / `merge_request_compare`): reachable
//! from a branch, a tag or THIS merge request's version refs, counted
//! against the caller like every id lookup. An unknown version and a
//! purged one (its refs removed, secret removal) are the same
//! `FG-REPO-0023 reason=ref_not_found`, before any git runs. Diff and
//! commit bodies are the M2 history bodies (`routes/history.rs`).

use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::MergeRequestCtx;
use forge_core::{parse_positive_id, MergeRequestId, ObjectId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{StoredMergeability, StoredVersion};
use serde::Serialize;

use super::loaders::version_ids;
use super::{respond, review};
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, QueryParam};
use crate::routes::browse::{answer, query, required, Target};
use crate::routes::history::{commit_body, diff_body, CommitBody, DiffBody};
use crate::routes::write::{bounded, json};

/// The path after `{iid}` of [`VERSIONS`].
pub const VERSIONS_SUFFIX: &str = "/versions";
/// Between `{iid}` and `{version}` in [`VERSION_DIFF`].
pub const VERSIONS_INFIX: &str = "/versions/";
/// The path after `{version}` in [`VERSION_DIFF`].
pub const DIFF_SUFFIX: &str = "/diff";
/// The path after `{iid}` of [`COMPARE`].
pub const COMPARE_SUFFIX: &str = "/compare";
/// The path after `{iid}` of [`MERGEABILITY`].
pub const MERGEABILITY_SUFFIX: &str = "/mergeability";

/// `GET` a merge request's versions.
pub const VERSIONS: &str = "/api/v1/projects/{id}/merge-requests/{iid}/versions";
/// `GET` one version's diff.
pub const VERSION_DIFF: &str = "/api/v1/projects/{id}/merge-requests/{iid}/versions/{version}/diff";
/// `GET` two versions compared.
pub const COMPARE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/compare";
/// `GET` the stored mergeability.
pub const MERGEABILITY: &str = "/api/v1/projects/{id}/merge-requests/{iid}/mergeability";

/// The query parameters of `compare`.
pub const COMPARE_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "from",
        description: "Required. The earlier version's number",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "to",
        description: "Required. The later version's number",
        integer_range: Some((1, i64::MAX)),
    },
];

/// One version of a merge request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionBody {
    #[schema(minimum = 1)]
    pub version: i64,
    pub head_sha: String,
    /// The target's tip when the version was recorded (`null` until the
    /// version job recorded it, or when the target was missing).
    #[schema(required = true)]
    pub base_sha: Option<String>,
    /// The merge base of the head and `base_sha` (`null`: not recorded, or
    /// no common history). The version's diff starts here.
    #[schema(required = true)]
    pub merge_base_sha: Option<String>,
    /// The pusher whose push made the version (`null` for the first
    /// version of a new or reopened merge request).
    #[schema(required = true)]
    pub pushed_by: Option<i64>,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// The version's refs were removed (secret removal): its diff is no
    /// longer served.
    pub refs_purged: bool,
}

/// A merge request's versions, oldest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionsBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    pub versions: Vec<VersionBody>,
}

/// One version's diff.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionDiffBody {
    pub version: VersionBody,
    /// The commit the diff starts at: `merge_base_sha`, or `base_sha` when
    /// there was no common history.
    pub from: String,
    /// The version's head.
    pub to: String,
    /// The commits `to` has and `from` does not, newest first.
    pub commits: Vec<CommitBody>,
    /// More commits than the server lists (200).
    pub commits_truncated: bool,
    pub diff: DiffBody,
}

/// Two versions compared (`head(from)..head(to)`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionCompareBody {
    #[schema(minimum = 1)]
    pub from_version: i64,
    #[schema(minimum = 1)]
    pub to_version: i64,
    pub from_sha: String,
    pub to_sha: String,
    /// The commits `to` has and `from` does not, newest first.
    pub commits: Vec<CommitBody>,
    pub commits_truncated: bool,
    /// The tree diff from `from` to `to`.
    pub diff: DiffBody,
}

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
    /// `no_version`, `git_timeout`, `git_busy`, `git_failed` (`null` when
    /// mergeable or not checked yet).
    #[schema(required = true)]
    pub reason: Option<String>,
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
    /// The conflicted paths (at most 100; `conflicts_truncated` beyond).
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
}

fn version_body(v: &StoredVersion) -> VersionBody {
    VersionBody {
        version: v.version_no,
        head_sha: v.head_sha.as_str().to_owned(),
        base_sha: v.base_sha.as_ref().map(|s| s.as_str().to_owned()),
        merge_base_sha: v.merge_base_sha.as_ref().map(|s| s.as_str().to_owned()),
        pushed_by: v.pushed_by.map(UserId::get),
        created_at: v.created_at.clone(), // clone: the answer owns its strings
        refs_purged: v.refs_purged,
    }
}

/// `FG-REPO-0023 reason=ref_not_found` (an unknown or purged version
/// answers like an unknown id).
fn not_found(version: i64, why: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RepositoryPathNotResolved)
        .with_context("reason", "ref_not_found")
        .with_context("version", version)
        .with_context("version_state", why)
}

/// The proof's ids (a read route always names a stored merge request).
fn proof_ids(request: &Dispatched<MergeRequestCtx>) -> Result<(ProjectId, i64), ForgeError> {
    match *request.proof().id() {
        (project, Some(iid)) => Ok((project, iid)),
        (_, None) => {
            Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid"))
        }
    }
}

/// The merge request's id (read again after the decision).
async fn merge_request_id(
    state: &AppState,
    project: ProjectId,
    iid: i64,
) -> Result<(MergeRequestId, Option<String>), ForgeError> {
    let stored = bounded(
        state,
        "merge_request_load",
        review(state)?.merge_request(project, iid),
    )
    .await?
    .ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "decided_merge_request_missing")
            .with_context("iid", iid)
    })?;
    Ok((stored.id, stored.head_sha))
}

async fn versions_of(
    state: &AppState,
    project: ProjectId,
    iid: i64,
) -> Result<Vec<StoredVersion>, ForgeError> {
    let (id, _) = merge_request_id(state, project, iid).await?;
    bounded(state, "merge_request_versions", review(state)?.versions(id)).await
}

/// Version `n`, served only while its refs exist.
fn served(versions: &[StoredVersion], n: i64) -> Result<&StoredVersion, ForgeError> {
    let v = versions
        .iter()
        .find(|v| v.version_no == n)
        .ok_or_else(|| not_found(n, "unknown"))?;
    if v.refs_purged {
        return Err(not_found(n, "purged"));
    }
    Ok(v)
}

/// `GET …/merge-requests/{iid}/versions` → `200` the versions.
pub async fn versions(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    // Owned before the first await: the request (its body) is not `Sync`.
    let params = query(request.raw_query(), &[]).map(drop);
    let built = async {
        let (project, iid) = ids?;
        params?;
        let all = versions_of(&state, project, iid).await?;
        json(VersionsBody {
            iid,
            versions: all.iter().map(version_body).collect(),
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `GET …/versions/{version}/diff` → `200` the version's diff.
pub async fn version_diff(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    // The version is checked after the decision (the loader parsed it too).
    let version = version_ids(request.raw_path()).map(|(_, _, v)| v);
    let review_state = state.clone(); // clone: cheap handles; the closure owns its copy
    answer(
        state,
        request,
        "merge_request_version_diff",
        |t: Target, q| async move {
            let (project, iid) = ids?;
            query(q.as_deref(), &[])?;
            let n = version?;
            let all = versions_of(&review_state, project, iid).await?;
            let v = served(&all, n)?;
            let from = v
                .merge_base_sha
                .as_ref()
                .or(v.base_sha.as_ref())
                .ok_or_else(|| not_found(n, "unknown").with_context("version_base", "pending"))?;
            let head = v.head_sha.as_str();
            let cmp = t
                .smart
                .browse
                .merge_request_compare(t.repository.id, iid, from.as_str(), head, &t.principal)
                .await
                .map_err(|e| e.with_context("version", n))?;
            Ok(VersionDiffBody {
                version: version_body(v),
                from: from.as_str().to_owned(),
                to: head.to_owned(),
                commits: cmp.commits.iter().map(commit_body).collect(),
                commits_truncated: cmp.commits_truncated,
                diff: diff_body(&cmp.diff),
            })
        },
    )
    .await
}

/// A version number parameter (`FG-WEB-0006 param_invalid`).
fn version_param(params: &[(&'static str, String)], name: &'static str) -> Result<i64, ForgeError> {
    let raw = required(params, name)?;
    parse_positive_id(raw).ok_or_else(|| param_invalid(name, "not_positive_decimal"))
}

/// `GET …/compare?from=&to=` → `200` the two versions compared.
pub async fn compare(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    let review_state = state.clone(); // clone: cheap handles; the closure owns its copy
    answer(
        state,
        request,
        "merge_request_compare",
        |t: Target, q| async move {
            let (project, iid) = ids?;
            let params = query(q.as_deref(), &["from", "to"])?;
            let from_n = version_param(&params, "from")?;
            let to_n = version_param(&params, "to")?;
            let all = versions_of(&review_state, project, iid).await?;
            let from = served(&all, from_n)?;
            let to = served(&all, to_n)?;
            let cmp = t
                .smart
                .browse
                .merge_request_compare(
                    t.repository.id,
                    iid,
                    from.head_sha.as_str(),
                    to.head_sha.as_str(),
                    &t.principal,
                )
                .await
                .map_err(|e| e.with_context("versions", format!("{from_n}..{to_n}")))?;
            Ok(VersionCompareBody {
                from_version: from_n,
                to_version: to_n,
                from_sha: from.head_sha.as_str().to_owned(),
                to_sha: to.head_sha.as_str().to_owned(),
                commits: cmp.commits.iter().map(commit_body).collect(),
                commits_truncated: cmp.commits_truncated,
                diff: diff_body(&cmp.diff),
            })
        },
    )
    .await
}

fn mergeability_body(
    iid: i64,
    head: Option<String>,
    stored: Option<StoredMergeability>,
) -> MergeabilityBody {
    let oid = |o: Option<ObjectId>| o.map(|o| o.as_str().to_owned());
    match stored {
        None => MergeabilityBody {
            iid,
            status: forge_core::MergeabilityStatus::Unchecked
                .as_str()
                .to_owned(),
            reason: None,
            head_sha: head,
            checked_head_sha: None,
            checked_target_sha: None,
            checked_at: None,
            conflicts: Vec::new(),
            conflicts_truncated: false,
        },
        Some(s) => MergeabilityBody {
            iid,
            status: s.status.as_str().to_owned(),
            reason: s.reason,
            head_sha: head,
            checked_head_sha: oid(s.checked_head_sha),
            checked_target_sha: oid(s.checked_target_sha),
            checked_at: s.checked_at,
            conflicts: s.conflicts,
            conflicts_truncated: s.conflicts_truncated,
        },
    }
}

/// `GET …/mergeability` → `200` the stored status (never enqueues).
pub async fn mergeability(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    // Owned before the first await: the request (its body) is not `Sync`.
    let params = query(request.raw_query(), &[]).map(drop);
    let built = async {
        let (project, iid) = ids?;
        params?;
        let (id, head) = merge_request_id(&state, project, iid).await?;
        let stored = bounded(
            &state,
            "merge_request_mergeability",
            review(&state)?.mergeability(id),
        )
        .await?;
        json(mergeability_body(iid, head, stored))
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}
