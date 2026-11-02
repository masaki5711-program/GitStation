//! Merging a merge request (M3 R4, ADR-0024 §4):
//! `POST …/merge-requests/{iid}/merge` (`MergeRequestMerge`: people only,
//! the target's push role, a token needs `merge_execute`) queues the merge
//! and answers `202` with the attempt; `GET …/merge-requests/{iid}/merge`
//! (`MergeRequestRead`) answers the latest attempt. The merge itself runs
//! in `forge serve`'s merge job (`forge_review::Merger`), never inside the
//! request.
//!
//! The loader reads the target's protection (the push path's rules) with
//! the merge request, so `authorize()` decides the merge with it; forge-review
//! decides again under the review locks with the rules as stored then.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::{MergeRequestCtx, MergeRequestEdit};
use forge_core::{MergeMethod, ObjectId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{CredentialRef, MergeAsk, StoredMerge};
use serde::{Deserialize, Serialize};

use super::loaders::{found, ids, project_of};
use super::{respond, review, review_request};
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader};
use crate::routes::write::{absent, bounded, json};

/// The path after `{iid}` of [`MERGE`].
pub const MERGE_SUFFIX: &str = "/merge";
/// `POST` merge a merge request; `GET` its latest merge attempt.
pub const MERGE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/merge";
/// The body cap: the registry's JSON limit (64 KiB). A message near the
/// 64 KiB message rule can exceed it once JSON-escaped (FG-WEB-0009), as a
/// merge request's description can.
pub const MAX_BODY: usize = 64 * 1024;

/// `POST …/merge-requests/{iid}/merge`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeMergeRequest {
    /// The head commit the requester reviewed (required): a merge request
    /// whose head is another commit is refused (FG-REVIEW-0008 stale_view).
    pub sha: String,
    /// `merge_commit` or `squash` (`fast_forward` when the project allows
    /// it); absent: the project's default (`merge_commit`).
    pub method: Option<String>,
    /// Delete the source branch after the merge (decided as a branch delete
    /// of the requester: a protected branch is kept). Absent: `false`.
    pub delete_source_branch: Option<bool>,
    /// The commit message (1 character to 64 KiB); absent: built from the
    /// merge request's title and description.
    pub message: Option<String>,
}

// Built only by `attempt_body`; its doc comment is the schema description.
/// One merge attempt (ADR-0024 §4.2): `queued` → `prepared` → `executing`
/// → `applied`, or `superseded` / `failed`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeAttemptBody {
    #[schema(minimum = 1)]
    pub merge_id: i64,
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `queued`, `prepared`, `executing`, `applied`, `superseded` or `failed`.
    pub state: String,
    /// `merge_commit`, `squash` or `fast_forward`.
    pub method: String,
    /// The head the requester asked to merge.
    pub head_sha: String,
    /// The target branch without `refs/heads/`.
    pub target_branch: String,
    pub delete_source_branch: bool,
    /// The target's tip the merge was built on (from `prepared`).
    #[schema(required = true)]
    pub expected_old: Option<String>,
    /// The commit the target moves to (from `prepared`).
    #[schema(required = true)]
    pub new_sha: Option<String>,
    /// Why it was superseded or failed (`conflict`, `target_moved`, …).
    #[schema(required = true)]
    pub reason: Option<String>,
    /// The error code of a superseded or failed attempt.
    #[schema(required = true)]
    pub error_code: Option<String>,
    /// After `applied` with `delete_source_branch`: `deleted`,
    /// `kept_refused`, `kept_moved` or `kept_failed`.
    #[schema(required = true)]
    pub source_outcome: Option<String>,
    /// The request that asked for the merge (the job's log lines carry it).
    pub request_id: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
    #[schema(required = true)]
    pub finished_at: Option<String>,
}

/// The latest merge attempt of a merge request (`null`: never asked).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeStatusBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    #[schema(required = true)]
    pub merge: Option<MergeAttemptBody>,
}

fn attempt_body(iid: i64, m: &StoredMerge) -> MergeAttemptBody {
    let oid = |o: &Option<ObjectId>| o.as_ref().map(|o| o.as_str().to_owned());
    MergeAttemptBody {
        merge_id: m.id,
        iid,
        state: m.state.as_str().to_owned(),
        method: m.method.as_str().to_owned(),
        head_sha: m.head_sha.as_str().to_owned(),
        target_branch: m
            .target_branch
            .branch()
            .unwrap_or(m.target_branch.as_str())
            .to_owned(),
        delete_source_branch: m.delete_source_branch,
        expected_old: oid(&m.expected_old),
        new_sha: oid(&m.new_sha),
        reason: m.reason.clone(),         // clone: the row is borrowed
        error_code: m.error_code.clone(), // clone: as above
        source_outcome: m.source_outcome.map(|o| o.as_str().to_owned()),
        request_id: m.request_id.as_str().to_owned(),
        created_at: m.created_at.clone(),   // clone: as above
        updated_at: m.updated_at.clone(),   // clone: as above
        finished_at: m.finished_at.clone(), // clone: as above
    }
}

/// The merge request about to be merged, with its target's protection.
async fn merge_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<crate::registry::Lookup<MergeRequestCtx>, ForgeError> {
    let (id, iid) = ids(parts.uri.path(), MERGE_SUFFIX)?;
    let Some((project, record)) = project_of(state, id, viewer).await? else {
        return Ok(absent());
    };
    let review = review(state)?;
    let stored = bounded(state, "merge_request_load", review.merge_request(id, iid)).await?;
    let Some(stored) = stored else {
        return Ok(absent());
    };
    let protection = bounded(
        state,
        "merge_target_protection",
        review.target_protection(id, &stored.target_branch),
    )
    .await?;
    let mr = forge_review::loaded_merge_request(&stored);
    Ok(found(
        MergeRequestCtx::merging(project, mr, protection),
        record,
    ))
}

/// `POST …/merge`: the merge request and its target's protection.
pub struct MergeRequestToMerge;

impl BodyLoader<AppState, MergeMergeRequest> for MergeRequestToMerge {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a MergeMergeRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(merge_lookup(state, parts, viewer))
    }
}

/// `GET …/merge`: the merge request as stored.
pub struct MergeStatusOf;

impl Loader<AppState> for MergeStatusOf {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path(), MERGE_SUFFIX)?;
            super::loaders::lookup_ids(state, ids, viewer, MergeRequestEdit::default()).await
        })
    }
}

/// The body's values, strictly (`FG-WEB-0006 param_invalid`).
fn ask_of(body: &MergeMergeRequest) -> Result<MergeAsk, ForgeError> {
    let sha = ObjectId::parse(&body.sha).map_err(|_| param_invalid("sha", "not_an_object_id"))?;
    let method = body
        .method
        .as_deref()
        .map(|m| MergeMethod::parse(m).ok_or_else(|| param_invalid("method", "unknown_value")))
        .transpose()?;
    Ok(MergeAsk {
        sha,
        method,
        delete_source_branch: body.delete_source_branch,
        message: body.message.clone(), // clone: the request owns its message
    })
}

fn proof_iid(project: ProjectId, iid: Option<i64>) -> Result<(ProjectId, i64), ForgeError> {
    let iid = iid.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
    })?;
    Ok((project, iid))
}

/// `POST …/merge` → `202` the queued attempt (the merge job runs it).
pub async fn merge(
    state: AppState,
    request: Dispatched<MergeRequestCtx, MergeMergeRequest>,
) -> Response {
    let repo = super::repository(&request);
    let session = request.session().map(|s| s.session_id);
    let w = request.into_write();
    let built = async {
        let (_, iid) = proof_iid(w.proof.id().0, w.proof.id().1)?;
        let credential = match (w.credential_id, session) {
            (Some(token), _) => CredentialRef::Token(token),
            (None, Some(session)) => CredentialRef::Session(session),
            (None, None) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "merge_without_credential"))
            }
        };
        let ask = ask_of(&w.input)?;
        let queued = review(&state)?
            .request_merge(&review_request(&w), credential, &w.proof, repo?, &ask)
            .await?;
        json(attempt_body(iid, &queued))
    }
    .await;
    respond(&w.ctx, StatusCode::ACCEPTED, built)
}

/// `GET …/merge` → `200` the latest attempt (`merge: null` when none).
pub async fn merge_status(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let (project, iid) = *request.proof().id();
    let built = async {
        let (project, iid) = proof_iid(project, iid)?;
        let review = review(&state)?;
        let stored = bounded(
            &state,
            "merge_request_load",
            review.merge_request(project, iid),
        )
        .await?
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "written_merge_request_missing")
                .with_context("iid", iid)
        })?;
        let latest = bounded(&state, "merge_latest", review.latest_merge(stored.id)).await?;
        json(MergeStatusBody {
            iid,
            merge: latest.as_ref().map(|m| attempt_body(iid, m)),
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}
