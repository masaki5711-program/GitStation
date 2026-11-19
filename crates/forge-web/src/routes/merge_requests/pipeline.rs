//! "Run the pipeline on the current target" (plan 18 S5, ADR-0037):
//! `POST …/merge-requests/{iid}/pipeline` (`PipelineCreate` on the project,
//! the action of a manual run: Developer; a token needs `api`) → `202`.
//!
//! Since S5 a target move writes the merge request's new merge result
//! without a pipeline; this asks for the merge request pipeline
//! (`ci/pipeline`) on the merge result of the target tip the merge
//! request's mergeability answers for (`forge_pipelines::Pipelines::
//! run_merge_request`): `queued` (its job was enqueued), `exists` (it was
//! asked for already: nothing written) or `computing` (that merge result
//! is being written first; the pipeline follows it). A second press asks
//! nothing more. `404 FG-REPO-0005` without such a merge request (or an
//! invisible project); `409 FG-CI-0003 operation=run_merge_request` when it
//! is not open, has no version yet, or conflicts with the target.
//!
//! Not `POST …/pipelines` (a manual run of a branch tip, `source = manual`
//! with no merge request): that pipeline never reports the merge request's
//! `ci/pipeline`, which is the merge result's.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::ProjectCtx;
use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};
use forge_pipelines::{ChangeRequest, MergePipelineRun, Pipelines};
use serde::Serialize;

use super::loaders::{found, ids, project_of};
use super::respond;
use crate::app::AppState;
use crate::registry::{Dispatched, LoadFuture, Loader};
use crate::routes::write::{absent, json};

/// The path after `{iid}` of [`PIPELINE`].
pub const PIPELINE_SUFFIX: &str = "/pipeline";
/// `POST` run the merge request pipeline on the current target.
pub const PIPELINE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/pipeline";

/// What an ask did (`202`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestPipelineBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `queued` (the pipeline's job was enqueued), `exists` (it had been
    /// asked for already) or `computing` (the merge result of the current
    /// target is being written; the pipeline follows it).
    pub outcome: String,
    /// The merge result the pipeline runs on (`queued`, `exists`).
    #[schema(required = true)]
    pub merge_result_sha: Option<String>,
    /// The target tip of that merge result (`queued`, `exists`).
    #[schema(required = true)]
    pub target_sha: Option<String>,
    /// The mergeability check the pipeline waits for (`computing`).
    #[schema(required = true, minimum = 1)]
    pub check_seq: Option<i64>,
}

fn body(run: &MergePipelineRun) -> MergeRequestPipelineBody {
    MergeRequestPipelineBody {
        iid: run.iid,
        outcome: run.outcome.as_str().to_owned(),
        merge_result_sha: run.merge_result.as_ref().map(|s| s.as_str().to_owned()),
        target_sha: run.target.as_ref().map(|s| s.as_str().to_owned()),
        check_seq: run.check_seq,
    }
}

fn pipelines(state: &AppState) -> Result<&Pipelines, ForgeError> {
    state.pipelines.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "pipelines_unconfigured")
    })
}

/// The route's loader: the project (the decision is `PipelineCreate` on
/// it); the merge request is read in the change's transaction.
#[derive(Debug, Clone, Copy)]
pub struct MergeRequestToRunPipeline;

impl Loader<AppState> for MergeRequestToRunPipeline {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let (id, _iid) = ids(parts.uri.path(), PIPELINE_SUFFIX)?;
            let Some((project, record)) = project_of(state, id, viewer).await? else {
                return Ok(absent());
            };
            Ok(found(ProjectCtx::new(project), record))
        })
    }
}

/// `POST …/merge-requests/{iid}/pipeline` → `202`.
pub async fn run_pipeline(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, iid) = ids(&path, PIPELINE_SUFFIX)?;
        let ask = ChangeRequest {
            request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
            credential_id: w.credential_id,
        };
        let run = pipelines(&state)?
            .run_merge_request(&ask, &w.proof, iid)
            .await?;
        json(body(&run))
    }
    .await;
    respond(&w.ctx, StatusCode::ACCEPTED, built)
}

#[cfg(test)]
mod unit {
    #![allow(clippy::expect_used)] // tests may fail loudly
    use super::*;

    #[test]
    fn the_path_names_the_project_and_the_merge_request() {
        assert_eq!(
            ids(
                "/api/v1/projects/7/merge-requests/3/pipeline",
                PIPELINE_SUFFIX
            )
            .expect("ids"),
            (forge_core::ProjectId::new(7), 3)
        );
        assert!(ids(
            "/api/v1/projects/7/merge-requests/3/pipelines",
            PIPELINE_SUFFIX
        )
        .is_err());
    }
}
