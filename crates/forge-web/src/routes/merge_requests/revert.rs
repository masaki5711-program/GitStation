//! One-step revert (M4 C10, plan 04 M-4, ADR-0024 Amendment C10):
//! `POST /api/v1/projects/{id}/merge-requests/{iid}/revert` opens a merge
//! request that reverts merged merge request `{iid}` (`forge_review::revert`).
//!
//! No new action (plan 14 C10): the loader builds the revert merge
//! request's `MergeRequestCreate` context — the caller's own,
//! `revert/mr-<iid>` → the merged merge request's target — so the registry
//! decides the route as opening a merge request (Developer, `api`); the
//! handler then decides the branch's creation as a push (`RepoPush` on the
//! project and `authorize_ref_updates` on the protection rules, inside
//! forge-review: Developer, `write_repository`, a protected `revert/*`
//! refuses). A merged merge request that is not visible, or no such `iid`,
//! is the same `FG-REPO-0005`. A refusal of the merge request itself is
//! `FG-REVIEW-0029` (`reason`), a conflicting revert `FG-REVIEW-0030`:
//! nothing is opened then.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::{prove, Action, MergeRequestCtx, ProjectCtx};
use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;

use super::loaders::{found, ids, project_of};
use super::{body_of, repository, respond, review, review_request, MergeRequestBody};
use crate::app::AppState;
use crate::registry::{Dispatched, LoadFuture, Loader};
use crate::routes::projects;
use crate::routes::write::{absent, bounded, json};

/// The path after `{iid}` of [`REVERT`].
pub const REVERT_SUFFIX: &str = "/revert";
/// `POST` revert a merged merge request.
pub const REVERT: &str = "/api/v1/projects/{id}/merge-requests/{iid}/revert";

/// The revert merge request as opened.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RevertBody {
    /// The merge request that reverts `reverted_iid` (source
    /// `revert/mr-<iid>`, target the reverted one's target).
    pub merge_request: MergeRequestBody,
    /// The merged merge request it reverts.
    #[schema(minimum = 1)]
    pub reverted_iid: i64,
    /// The revert commit (the new branch's tip).
    pub commit_sha: String,
    /// The target's tip it was built on.
    pub parent_sha: String,
}

/// `POST …/revert`: the revert merge request about to be opened by the
/// caller, from the merged one.
pub struct MergeRequestToRevert;

impl Loader<AppState> for MergeRequestToRevert {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let (id, iid) = ids(parts.uri.path(), REVERT_SUFFIX)?;
            // Nobody signed in: decided by `authorize()` (FG-AUTH-0001).
            let Some(author) = viewer else {
                return Ok(absent());
            };
            let Some((project, record)) = project_of(state, id, viewer).await? else {
                return Ok(absent());
            };
            let stored = bounded(
                state,
                "merge_request_load",
                review(state)?.merge_request(id, iid),
            )
            .await?;
            let Some(stored) = stored else {
                return Ok(absent());
            };
            let ctx = forge_review::revert_ctx(
                project,
                (stored.iid, &stored.target_branch),
                author,
                false,
            )?;
            Ok(found(ctx, record))
        })
    }
}

/// `POST …/revert` → `201` the revert merge request.
pub async fn revert(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let repo = repository(&request);
    let iid = ids(request.raw_path(), REVERT_SUFFIX).map(|(_, iid)| iid);
    let record = request.project().cloned();
    let w = request.into_write();
    let built = async {
        let iid = iid?;
        let record = record.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_project")
        })?;
        let Some(smart) = state.smart_http.as_deref() else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "smart_http_unconfigured"));
        };
        // The branch's creation is a push: decided on the same principal.
        let push = prove(
            w.proof.principal(),
            Action::RepoPush,
            ProjectCtx::new(projects::loaded(&record)),
        )?;
        let reverter = review(&state)?
            .reverter(smart.git.clone(), smart.storage.clone()) // clone: Arc handles and the root path
            .with_browser(smart.browse.clone()); // clone: shares the ref cache
        let opened = reverter
            .revert(&review_request(&w), &w.proof, &push, repo?, iid)
            .await?;
        json(RevertBody {
            merge_request: body_of(&opened.merge_request),
            reverted_iid: opened.reverted_iid,
            commit_sha: opened.commit.as_str().to_owned(),
            parent_sha: opened.parent.as_str().to_owned(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}
