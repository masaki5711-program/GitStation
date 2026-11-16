//! Manual jobs (M4 C12, plan 14 §5 C12): a job written `when: manual` in
//! `.forge-ci.yml` is created `manual` and runs only when a person starts
//! it (`PipelineCreate`: Developer and above — the same right as running a
//! pipeline by hand). Nothing waits for it (forge-ci), and the pipeline's
//! state is decided by its other jobs while it waits (`settle_pipeline`).
//!
//! M4 C12 review (HIGH): a job of a **protected** pipeline is given the
//! project's secrets and may deploy, so starting one also needs
//! `PipelinePlayProtected` (Maintainer, people only), decided in the
//! transaction on the `protected` flag read under the row's lock.

use forge_auth::{prove, Action, Authorized, ProjectCtx};
use forge_db::pipelines as db;
use forge_errors::ForgeError;

use crate::{caller, change_context, loaded, read_back, reconfirm, require};
use crate::{ChangeRequest, PipelineDetail, Pipelines};

impl Pipelines {
    /// `POST …/pipelines/{pipeline_id}/jobs/{job_id}/play` (`PipelineCreate`):
    /// start a `when: manual` job — it becomes `pending` for a runner, and a
    /// finished pipeline is `running` again — with one `pipeline.job_played`
    /// audit row (decided again in the transaction). Whether the job gets
    /// secrets stays its pipeline's answer
    /// (`StoredPipeline::secrets_allowed`, ADR-0029 obligation 5): starting
    /// it by hand changes nothing about that. Errors: `FG-REPO-0005
    /// resource=pipeline|job`, `FG-CI-0003 operation=play` with `reason`
    /// (`not_manual`, `pipeline_state`, `needs_not_succeeded`).
    pub async fn play_job(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        pipeline: i64,
        job: i64,
    ) -> Result<PipelineDetail, ForgeError> {
        require(proof.action(), Action::PipelineCreate)?;
        let user = caller(proof.principal(), Action::PipelineCreate)?;
        let ctx = change_context(request, proof.principal(), user);
        let (name, ()) = db::play_job_decided(
            &self.db,
            &ctx,
            (*proof.id(), pipeline, job),
            user,
            |facts| reconfirm(proof, facts),
            |facts| {
                // The same principal, on the project as the transaction
                // locked it (its account was reconfirmed just before).
                let fresh = ProjectCtx::new(loaded(&facts.project));
                prove(proof.principal(), Action::PipelinePlayProtected, fresh).map(drop)
            },
        )
        .await?;
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.job_played",
            request_id = %request.request_id,
            project_id = proof.id().get(),
            pipeline_id = pipeline,
            job_id = job,
            job = name.as_str(),
            user_id = user.get(),
            "manual job started"
        );
        read_back(&self.db, *proof.id(), pipeline).await
    }
}
