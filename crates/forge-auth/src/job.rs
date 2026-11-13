//! The CI arm of `authorize()` (M4 C0, ADR-0029 §2–§4): a registered
//! runner's token and a job's short-lived token. `authorize()` calls
//! [`decide`] first for every `Credential::Runner` / `Credential::Job`
//! (as it does `act_as::decide` for an act-as request), so no other rule —
//! the public actions, the roles, the AI copy refusal (`FG-AUTH-0022`),
//! the context API's agent audience — is ever reached by a CI principal.
//!
//! Order of checks:
//! 1. a job's token that is not live: `FG-AUTH-0001` (`credential=job`,
//!    `reason=expired|revoked`, `job_id`);
//! 2. the action's [`CiRule`] for the credential: `Denied` (and the other
//!    principal's rule) → `FG-AUTH-0003 reason=ci_not_allowed`, before the
//!    resource is looked at;
//! 3. a job's copy: only `Resource::PinnedCopy`; anything else
//!    (`job_requires_pinned_copy`), another project (`job_other_project`)
//!    or another SHA (`job_sha_mismatch`) is the invisible project's
//!    `FG-REPO-0005` — no oracle on what exists outside the job, the
//!    reason only in the log. The confidential marks are not looked at (§3):
//!    the outputs inherit the marks instead (§5, M4 C5).
//!
//! Runners read no repository and no API (§1); the runner API's actions
//! (`CiRule::Runner`, M4 C3) are a claim on the instance and the heartbeat,
//! log and completion of a job whose lease the runner holds. Every refusal
//! carries `action`, `ci_rule`, `credential` and `job_id` / `runner_id`.

use forge_errors::{ErrorCode, ForgeError};

use crate::action::{Action, CiRule};
use crate::authorize::authorize;
use crate::model::{
    Credential, JobCredential, JobTokenState, PinnedCopy, Principal, Resource, RunnerCredential,
};
use forge_core::push::ObjectId;
use forge_core::ProjectId;

/// `authorize(principal, RepoRead, PinnedCopy { project, sha })`: a CI
/// job's clone / fetch of its own project at its pipeline's pinned SHA
/// (ADR-0029 §3). It and [`crate::PinnedCopyCtx`] (the job fetch route's
/// context, M4 C4) are the only builders of [`Resource::PinnedCopy`]; both
/// decide through `authorize()` and hold no rule of their own. Every
/// principal but a live job of that project and SHA is refused (a person or
/// a service account: `FG-AUTH-0003 reason=pinned_copy_without_job`). The
/// Smart HTTP route (`/api/v1/runner/jobs/{job_id}/repo.git`) advertises
/// only `refs/pipelines/<id>/head`.
pub fn authorize_pinned_copy(
    principal: &Principal,
    project: ProjectId,
    sha: &ObjectId,
) -> Result<(), ForgeError> {
    authorize(
        principal,
        Action::RepoRead,
        &Resource::PinnedCopy(PinnedCopy::new(
            project,
            sha.clone(), // clone: the resource owns its SHA
        )),
    )
    .map_err(|e| e.with_context("project_id", project.get()))
}

/// Decide `action` on `resource` for a CI principal (`authorize()` calls
/// it first for every `Credential::Runner` / `Credential::Job`).
pub(crate) fn decide(
    principal: &Principal,
    action: Action,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let rule = action.ci_rule();
    // Structural guard (ADR-0029 §1): `Principal::runner` / `Principal::job`
    // are the only builders of a CI principal and give it no user. A CI
    // credential with a user was built through `Principal::authenticated`
    // — a defect, never a pass.
    if principal.user().is_some() {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "ci_credential_with_user")
            .with_context("action", action.name()));
    }
    let decided = match principal.credential() {
        Credential::Runner(c) => runner(c, action, rule, resource),
        Credential::Job(c) => job(c, action, rule, resource),
        // `authorize()` calls this for the CI credentials only.
        Credential::Session
        | Credential::Token(_)
        | Credential::ActAs(_)
        | Credential::AutoMerge => {
            Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "not_a_ci_principal"))
        }
    };
    decided.map_err(|e| {
        e.with_context("action", action.name())
            .with_context("ci_rule", rule.as_str())
    })
}

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden).with_context("reason", reason)
}

/// A runner: only the runner API (M4 C3). Its token reads no repository
/// and no API (ADR-0029 §1). A claim is decided on the instance; every
/// other runner action on the job's lease: the runner must hold it
/// (`lease_other_runner`, or `job_finished` for an ended job no runner
/// holds — FG-AUTH-0003, the runner stops the job).
fn runner(
    credential: &RunnerCredential,
    action: Action,
    rule: CiRule,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let refusal = |e: ForgeError| {
        e.with_context("credential", "runner")
            .with_context("runner_id", credential.runner_id)
    };
    match rule {
        CiRule::Denied | CiRule::Job => return Err(refusal(refused("ci_not_allowed"))),
        CiRule::Runner => {}
    }
    match (action, resource) {
        (Action::RunnerJobRequest, Resource::Instance) => Ok(()),
        (
            Action::RunnerJobHeartbeat | Action::RunnerLogAppend | Action::RunnerJobComplete,
            Resource::RunnerJob(lease),
        ) => {
            if lease.holder() == Some(credential.runner_id) {
                Ok(())
            } else if lease.finished() {
                Err(refusal(refused("job_finished")))
            } else {
                Err(refusal(refused("lease_other_runner")))
            }
        }
        // No such job: the same 404 as for everyone.
        (_, Resource::Unresolved(reason)) => Err(refusal(
            ForgeError::new(ErrorCode::NotFound).with_context("reason", reason.as_str()),
        )),
        // The route's loader built another resource than its action
        // decides on: a defect, never a pass.
        _ => Err(refusal(refused("runner_resource_mismatch"))),
    }
}

/// A job: its own project at its pinned SHA, while the token is live.
fn job(
    credential: &JobCredential,
    action: Action,
    rule: CiRule,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let refusal = |e: ForgeError| {
        e.with_context("credential", "job")
            .with_context("job_id", credential.job_id)
    };
    match credential.state {
        JobTokenState::Live => {}
        JobTokenState::Expired | JobTokenState::Revoked => {
            return Err(refusal(
                ForgeError::new(ErrorCode::Unauthenticated)
                    .with_context("reason", credential.state.as_str()),
            ))
        }
    }
    match rule {
        CiRule::Denied | CiRule::Runner => return Err(refusal(refused("ci_not_allowed"))),
        CiRule::Job => {}
    }
    let not_found = |reason: &'static str| {
        refusal(ForgeError::new(ErrorCode::NotFound).with_context("reason", reason))
    };
    match resource {
        Resource::PinnedCopy(copy) => {
            if action != Action::RepoRead {
                return Err(not_found("job_resource_mismatch"));
            }
            if copy.project() != credential.project {
                return Err(not_found("job_other_project"));
            }
            if *copy.sha() != credential.sha {
                return Err(not_found("job_sha_mismatch"));
            }
            Ok(())
        }
        // M4 C6: its own artifacts and its needs' — its own job only.
        Resource::JobSelf(target) => {
            if !matches!(
                action,
                Action::JobArtifactUpload | Action::JobArtifactDownload
            ) {
                return Err(not_found("job_resource_mismatch"));
            }
            if target.project() != credential.project {
                return Err(not_found("job_other_project"));
            }
            if target.job() != credential.job_id {
                return Err(not_found("job_other_job"));
            }
            Ok(())
        }
        // Nothing at the path: the same 404 as for everyone.
        Resource::Unresolved(reason) => Err(not_found(reason.as_str())),
        // A plain project read or a measured copy is a person's or an
        // agent's route: a job copies through `authorize_pinned_copy` only.
        // The invisible project's 404 whatever the project (visible or
        // hidden, existing or not): no existence oracle; the cause stays in
        // the log's `reason`.
        Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Project(_)
        | Resource::RepoCopy { .. }
        | Resource::Ref { .. }
        | Resource::Transfer { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Tombstone(_)
        | Resource::Membership { .. }
        | Resource::RefProtectionChange { .. }
        | Resource::VisibilityChange { .. }
        | Resource::Token { .. }
        | Resource::NotificationPreferences { .. }
        | Resource::UserMergeRequests { .. }
        | Resource::UserActivity { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::MergeRequest { .. }
        | Resource::Discussion { .. }
        | Resource::CodeownersEnforcement { .. }
        | Resource::MergeSettingsChange { .. }
        | Resource::Resolved { .. }
        | Resource::ServiceAccount(_)
        | Resource::ServiceAccountTokenRequest { .. }
        | Resource::CheckRun { .. }
        | Resource::CiOutput { .. }
        | Resource::IntegrationFeed(_)
        | Resource::ProjectEvents { .. }
        | Resource::ActAs(_)
        | Resource::RunnerJob(_)
        | Resource::AutoMerge(_)
        | Resource::ActAsConfirm(_) => Err(not_found("job_requires_pinned_copy")),
    }
}
