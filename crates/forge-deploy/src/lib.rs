//! Deployments over SSH (M4 C11, plan 14 §5 C11, ADR-0031).
//!
//! - [`Deploy`]: the route side on a route's proof — read a project's
//!   environments, targets and deployments (`DeployRead`), set an
//!   environment (`DeployEnvironmentManage`) or a target
//!   (`DeployTargetManage`), request a deployment (`DeployCreate`) and
//!   approve one (`DeployApprove`). Every change is decided again inside
//!   its transaction (`Authorized::reconfirm`) and audited there.
//! - [`run`]: the worker side (`forge worker`'s `deploy_runner`).
//! - [`transport`]: ssh as an argument vector, behind a trait.
//!
//! Validation that a value may reach a command line or a known_hosts file
//! lives in `forge_core::deploy` and runs here before anything is written.

use forge_auth::{
    AccountFacts, Action, Authorized, DeploymentCtx, LoadedDeployment, LoadedProject, Principal,
    ProjectCtx,
};
use forge_core::deploy::{
    check_host, check_port, check_script_name, check_target_name, check_user, DeployTier, HostKey,
    DEFAULT_APPROVAL_TTL_MINUTES, MAX_APPROVAL_TTL_MINUTES,
};
use forge_core::{ObjectId, ProjectId, RefName, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{ProjectAccessFacts, ProjectUpdateFacts};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

pub use forge_db::deploy::{StoredDeployment, StoredEnvironment, StoredTarget};

pub mod run;
pub mod transport;

/// Who asked and with which credential (the audit row's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequest {
    pub request_id: RequestId,
    pub credential_id: Option<i64>,
}

/// An environment as a Maintainer sends it (before validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSpec {
    pub tier: DeployTier,
    pub requires_approval: bool,
    pub approval_ttl_minutes: Option<i32>,
    pub auto_rollback: bool,
    pub auto_deploy_ref: Option<String>,
    pub freeze_windows: Vec<(String, String)>,
}

/// A target as an administrator sends it (before validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec {
    pub name: String,
    pub host: String,
    pub port: i32,
    pub user: String,
    pub host_key_type: String,
    pub host_key: String,
    pub script: String,
    pub enabled: bool,
}

fn param(param: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("param", param)
        .with_context("reason", reason)
}

fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

fn caller(principal: &Principal, action: Action) -> Result<UserId, ForgeError> {
    principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", action.name())
    })
}

fn change_context(request: &ChangeRequest, principal: &Principal, user: UserId) -> ChangeContext {
    let actor = match principal.account_kind() {
        forge_auth::AccountKind::ServiceAccount => Actor::ServiceAccount(user),
        forge_auth::AccountKind::Human | forge_auth::AccountKind::Bot => Actor::User(user),
    };
    let mut ctx = ChangeContext::new(actor, request.request_id.clone()); // clone: the context owns it
    ctx.credential_id = request.credential_id;
    ctx.effective_scope = principal.scope_names();
    ctx
}

fn loaded(facts: &ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}

fn account(facts: &ProjectUpdateFacts) -> AccountFacts {
    AccountFacts {
        active: facts.account.active,
        is_admin: facts.account.is_admin,
    }
}

/// The route's decision made again on the facts locked by the change.
fn reconfirm(proof: &Authorized<ProjectCtx>, facts: &ProjectUpdateFacts) -> Result<(), ForgeError> {
    proof
        .reconfirm(account(facts), ProjectCtx::new(loaded(&facts.project)))
        .map(drop)
}

/// The loaded deployment of an approval's context.
pub fn loaded_deployment(d: &StoredDeployment) -> LoadedDeployment {
    LoadedDeployment {
        id: d.id,
        tier: d.tier,
        requested_by: d.requested_by.map(UserId::new),
        merge_request_author: d.merge_request_author.map(UserId::new),
    }
}

/// Check an environment as sent: the approval window, the branch of the
/// automatic deployments, and prod's rules (approval always, no automatic
/// rollback or deployment: a person decides).
pub fn check_environment(
    spec: &EnvironmentSpec,
) -> Result<forge_db::deploy::EnvironmentInput, ForgeError> {
    let ttl = spec
        .approval_ttl_minutes
        .unwrap_or(DEFAULT_APPROVAL_TTL_MINUTES);
    if !(1..=MAX_APPROVAL_TTL_MINUTES).contains(&ttl) {
        return Err(param("approval_ttl_minutes", "out_of_range"));
    }
    if let Some(r) = &spec.auto_deploy_ref {
        let ok = r.starts_with("refs/heads/") && r.len() > "refs/heads/".len() && r.len() <= 255;
        if !ok || RefName::parse(r).is_err() {
            return Err(param("auto_deploy_ref", "not_a_branch"));
        }
    }
    if spec.tier.is_prod() {
        if !spec.requires_approval {
            return Err(param("requires_approval", "prod_requires_approval"));
        }
        if spec.auto_rollback {
            return Err(param("auto_rollback", "prod_rolls_back_by_hand"));
        }
        if spec.auto_deploy_ref.is_some() {
            return Err(param("auto_deploy_ref", "prod_deploys_by_hand"));
        }
    }
    Ok(forge_db::deploy::EnvironmentInput {
        tier: spec.tier,
        requires_approval: spec.requires_approval || spec.tier.is_prod(),
        approval_ttl_minutes: ttl,
        auto_rollback: spec.auto_rollback,
        auto_deploy_ref: spec.auto_deploy_ref.clone(), // clone: the input owns it
        freeze_windows: spec.freeze_windows.clone(),   // clone: as above
    })
}

/// Check a target as sent: every value that reaches ssh's command line or
/// the known_hosts file.
pub fn check_target(spec: &TargetSpec) -> Result<forge_db::deploy::TargetInput, ForgeError> {
    check_target_name(&spec.name).map_err(|r| param("name", r))?;
    check_host(&spec.host).map_err(|r| param("host", r))?;
    let port = check_port(spec.port).map_err(|r| param("port", r))?;
    check_user(&spec.user).map_err(|r| param("user", r))?;
    check_script_name(&spec.script).map_err(|r| param("script", r))?;
    let host_key =
        HostKey::parse(&spec.host_key_type, &spec.host_key).map_err(|r| param("host_key", r))?;
    Ok(forge_db::deploy::TargetInput {
        name: spec.name.clone(), // clone: the input owns it
        host: spec.host.clone(), // clone: as name
        port,
        login_user: spec.user.clone(), // clone: as name
        host_key,
        script: spec.script.clone(), // clone: as name
        enabled: spec.enabled,
    })
}

/// The deployments' service (`forge serve` builds one).
#[derive(Debug, Clone)]
pub struct Deploy {
    db: Db,
}

impl Deploy {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// `GET …/deploy/environments` (`DeployRead`): the environments and
    /// their targets.
    pub async fn environments(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<(Vec<StoredEnvironment>, Vec<StoredTarget>), ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        let project = *proof.id();
        let envs = forge_db::deploy::list_environments(&self.db, project).await?;
        let targets = forge_db::deploy::list_targets(&self.db, project).await?;
        Ok((envs, targets))
    }

    /// `GET …/deployments` (`DeployRead`): the newest deployments.
    pub async fn deployments(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<Vec<StoredDeployment>, ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        forge_db::deploy::list_deployments(&self.db, *proof.id()).await
    }

    /// One deployment of `project` for an approval's loader (`None`: no
    /// such deployment).
    pub async fn deployment(
        &self,
        project: ProjectId,
        id: i64,
    ) -> Result<Option<StoredDeployment>, ForgeError> {
        forge_db::deploy::get_deployment(&self.db, project, id).await
    }

    /// `PUT …/deploy/environments/{tier}` (`DeployEnvironmentManage`).
    pub async fn put_environment(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        spec: &EnvironmentSpec,
    ) -> Result<(StoredEnvironment, bool), ForgeError> {
        require(proof.action(), Action::DeployEnvironmentManage)?;
        let user = caller(proof.principal(), Action::DeployEnvironmentManage)?;
        let input = check_environment(spec)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let (stored, created) =
            forge_db::deploy::put_environment_decided(&self.db, &ctx, project, &input, |facts| {
                reconfirm(proof, facts)
            })
            .await?;
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.environment_set",
            request_id = %request.request_id,
            project_id = project.get(),
            environment = stored.tier.as_str(),
            created,
            requires_approval = stored.requires_approval,
            auto_rollback = stored.auto_rollback,
            freeze_windows = stored.freeze_windows.len(),
            user_id = user.get(),
            "deploy environment set"
        );
        Ok((stored, created))
    }

    /// `PUT …/deploy/environments/{tier}/targets/{name}`
    /// (`DeployTargetManage`).
    pub async fn put_target(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tier: DeployTier,
        spec: &TargetSpec,
    ) -> Result<(StoredTarget, bool), ForgeError> {
        require(proof.action(), Action::DeployTargetManage)?;
        let user = caller(proof.principal(), Action::DeployTargetManage)?;
        let input = check_target(spec)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let (stored, created) =
            forge_db::deploy::put_target_decided(&self.db, &ctx, project, tier, &input, |facts| {
                reconfirm(proof, facts)
            })
            .await?;
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.target_set",
            request_id = %request.request_id,
            project_id = project.get(),
            environment = tier.as_str(),
            target = %stored.name,
            host = %stored.host,
            port = stored.port,
            pinned_fingerprint = %stored.host_key_fingerprint,
            enabled = stored.enabled,
            created,
            user_id = user.get(),
            "deploy target set"
        );
        Ok((stored, created))
    }

    /// `POST …/deployments` (`DeployCreate`): request `sha` (lowercase
    /// hex, 40 or 64) on the environment `tier`.
    pub async fn request(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tier: DeployTier,
        sha: &str,
    ) -> Result<StoredDeployment, ForgeError> {
        require(proof.action(), Action::DeployCreate)?;
        let user = caller(proof.principal(), Action::DeployCreate)?;
        let sha = ObjectId::parse(sha).map_err(|_| param("sha", "not_an_object_id"))?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let stored = forge_db::deploy::request_decided(
            &self.db,
            &ctx,
            project,
            tier,
            sha.as_str(),
            |facts| reconfirm(proof, facts),
        )
        .await?;
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.requested",
            request_id = %request.request_id,
            deployment_id = stored.id,
            project_id = project.get(),
            environment = tier.as_str(),
            sha = %stored.sha,
            status = stored.status.as_str(),
            user_id = user.get(),
            "deployment requested"
        );
        Ok(stored)
    }

    /// `POST …/deployments/{deployment_id}/approve` (`DeployApprove`).
    pub async fn approve(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<DeploymentCtx>,
    ) -> Result<StoredDeployment, ForgeError> {
        require(proof.action(), Action::DeployApprove)?;
        let user = caller(proof.principal(), Action::DeployApprove)?;
        let (project, id) = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let stored =
            forge_db::deploy::approve_decided(&self.db, &ctx, project, id, |facts, locked| {
                let fresh = DeploymentCtx::new(loaded(&facts.project), loaded_deployment(locked));
                proof.reconfirm(account(facts), fresh).map(drop)
            })
            .await?;
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.approved",
            request_id = %request.request_id,
            deployment_id = id,
            project_id = project.get(),
            environment = stored.tier.as_str(),
            sha = %stored.sha,
            user_id = user.get(),
            "deployment approved"
        );
        Ok(stored)
    }
}

#[cfg(test)]
mod tests;
