//! `forge admin repo reprovision <repo_id>` (step D4 MR3b): enqueue the
//! provisioning job of a repository still in `provisioning` again, after
//! its job died (`forge doctor`: `repositories.provisioning`,
//! FG-REPO-0012). A mutating operator command: the host operator is the
//! trust root (outside `authorize()`), `forge_db::repositories::reprovision`
//! enforces the rules itself (a ready repository is refused with
//! `FG-REPO-0014 reason=repository_ready`), and in production running it is
//! an approved runbook step (ops/runbooks/repository-provisioning.md).
//! The log event carries ids only.

use forge_core::config::DatabaseConfig;
use forge_core::ids::RepoId;
use forge_core::RequestId;
use forge_db::repositories::{self, Reprovisioned};
use forge_db::Db;
use forge_errors::ForgeError;

use crate::admin_token::{context, with_db};

pub fn reprovision(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    repo_id: i64,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| {
        reprovision_with(db, request_id, repo_id).await
    })
}

pub(crate) async fn reprovision_with(
    db: &Db,
    request_id: &RequestId,
    repo_id: i64,
) -> Result<String, ForgeError> {
    let repo = RepoId::new(repo_id);
    let done = repositories::reprovision(db, &context(request_id), repo).await?;
    let (project_id, job, after, outcome, job_state) = match done {
        Reprovisioned::Queued {
            project_id,
            job,
            after,
        } => (project_id, job, after, "queued", "queued"),
        Reprovisioned::AlreadyLive {
            project_id,
            job,
            state,
        } => (project_id, job, None, "already_live", state.as_str()),
    };
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.repo.reprovisioned",
        request_id = %request_id,
        repo_id,
        project_id = project_id.get(),
        job_id = job.get(),
        after_job_id = after.map(|a| a.get()),
        outcome,
        job_state,
        "repository reprovision"
    );
    Ok(match (done, after) {
        (Reprovisioned::Queued { .. }, None) => format!(
            "repository {repo_id}: provisioning job {} queued; a running `forge worker` \
             picks it up (check with `forge doctor`, repositories.provisioning)\n",
            job.get()
        ),
        (Reprovisioned::Queued { .. }, Some(after)) => format!(
            "repository {repo_id}: provisioning job {} queued after job {} (which succeeded \
             without the repository becoming ready: a database restore or a manual change); \
             a running `forge worker` picks it up\n",
            job.get(),
            after.get()
        ),
        (Reprovisioned::AlreadyLive { .. }, _) => format!(
            "repository {repo_id}: provisioning job {} is already {job_state} \
             (nothing changed); make sure a `forge worker` runs\n",
            job.get()
        ),
    })
}

#[cfg(test)]
#[path = "admin_repo_tests.rs"]
mod tests;
