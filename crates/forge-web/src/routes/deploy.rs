//! Deployments (M4 C11, plan 14 §4.2, ADR-0031, `forge-deploy`).
//!
//! - `GET /api/v1/projects/{id}/deploy/environments` (`DeployRead`): the
//!   environments with their targets (pinned fingerprints, never a key).
//! - `PUT …/deploy/environments/{tier}` (`DeployEnvironmentManage`,
//!   Maintainer, people only): `{"requires_approval", "auto_rollback",
//!   "approval_ttl_minutes"?, "auto_deploy_ref"?, "freeze_windows"?}` →
//!   `201` / `200`. prod always requires an approval and never rolls back
//!   or deploys by itself (`400 FG-WEB-0006` otherwise).
//! - `PUT …/deploy/environments/{tier}/targets/{name}`
//!   (`DeployTargetManage`, administrators, people only): host, port,
//!   user, the pinned host key (`host_key_type`, `host_key`: the last two
//!   fields of a known_hosts line), the script name → `201` / `200`.
//! - `GET /api/v1/projects/{id}/deployments` (`DeployRead`): the newest
//!   deployments.
//! - `POST …/deployments` (`DeployCreate`, Developer): `{"environment",
//!   "sha"}` → `201` (`pending_approval` or `queued`); `409 FG-DEPLOY-0002`
//!   in a freeze window.
//! - `POST …/deployments/{deployment_id}/approve` (`DeployApprove`): a
//!   Maintainer who is a person in a web session (every tier), not the
//!   requester nor anyone behind the SHA → `200` (`queued`);
//!   `409 FG-DEPLOY-0010` when it is not waiting for an approval.
//! - ADR-0038 §3.4–§3.6 (step C): `GET …/deploy/approvers` (`DeployRead`)
//!   lists each environment's approvers; `PUT
//!   …/deploy/environments/{tier}/approvers` (`DeployApproversManage`,
//!   Owner, people only) sets them (users, groups, selectors,
//!   `approvals_required`, `allow_delegation`). With approvers, only a
//!   person they name approves (`403 FG-DEPLOY-0014` otherwise), and
//!   `?on_behalf_of=<username>` approves for a delegator where the
//!   environment allows it.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{DeploymentCtx, ProjectCtx};
use forge_core::deploy::DeployTier;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_deploy::{
    ApproverNames, ApproversSpec, ChangeRequest, Deploy, EnvironmentSpec, StoredDeployment,
    StoredEnvironment, StoredTarget, TargetSpec,
};
use forge_errors::{ErrorCode, ForgeError};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the environments.
pub const ENVIRONMENTS: &str = "/api/v1/projects/{id}/deploy/environments";
/// `PUT` one environment.
pub const ENVIRONMENT: &str = "/api/v1/projects/{id}/deploy/environments/{tier}";
/// `PUT` one target.
pub const TARGET: &str = "/api/v1/projects/{id}/deploy/environments/{tier}/targets/{name}";
/// `GET` the deployments, `POST` a request.
pub const DEPLOYMENTS: &str = "/api/v1/projects/{id}/deployments";
/// `POST` an approval.
pub const APPROVE: &str = "/api/v1/projects/{id}/deployments/{deployment_id}/approve";
/// `GET` every environment's approvers.
pub const APPROVERS: &str = "/api/v1/projects/{id}/deploy/approvers";
/// `PUT` one environment's approvers.
pub const ENVIRONMENT_APPROVERS: &str =
    "/api/v1/projects/{id}/deploy/environments/{tier}/approvers";
/// The query of [`APPROVE`].
pub const APPROVE_QUERY: &[crate::registry::QueryParam] = &[crate::registry::QueryParam {
    name: "on_behalf_of",
    description: "Approve for this person (a username) who delegated to the caller for now, where the environment allows delegation (ADR-0038 §3.5)",
    integer_range: None,
}];
/// The body cap (fifty freeze windows, a host key).
pub const MAX_BODY: usize = 16 * 1024;

/// One freeze window `[starts_at, ends_at)`, timestamps with a time zone.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FreezeWindow {
    pub starts_at: String,
    pub ends_at: String,
}

/// `PUT …/deploy/environments/{tier}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutEnvironment {
    /// A Maintainer approves each deployment (always true for prod).
    pub requires_approval: bool,
    /// Roll every target back when a deployment fails (never for prod).
    pub auto_rollback: bool,
    /// How long a request waits for an approval and an approval for the
    /// worker (1..=1440, default 60).
    #[serde(default)]
    pub approval_ttl_minutes: Option<i32>,
    /// Deploy the newest succeeded push pipeline of this branch
    /// (`refs/heads/...`; never for prod).
    #[serde(default)]
    pub auto_deploy_ref: Option<String>,
    /// No deployment is requested or run inside these (at most 50).
    #[serde(default)]
    pub freeze_windows: Vec<FreezeWindow>,
}

/// `PUT …/targets/{name}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutTarget {
    /// A DNS name or an IPv4 address (lowercase).
    pub host: String,
    /// Default 22.
    #[serde(default)]
    pub port: Option<i32>,
    /// The login user on the host.
    pub user: String,
    /// `ssh-ed25519`, `ecdsa-sha2-nistp256|384|521`, `rsa-sha2-512|256`.
    pub host_key_type: String,
    /// The base64 public key (the last field of a known_hosts line).
    pub host_key: String,
    /// The registered script name run on the host.
    pub script: String,
    /// Default true.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `POST …/deployments`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewDeployment {
    /// `dev`, `stg` or `prod`.
    pub environment: String,
    /// The full commit id (lowercase hex).
    pub sha: String,
}

/// `PUT …/deploy/environments/{tier}/approvers` (ADR-0038 §3.4).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutApprovers {
    /// Usernames (at most 20). Named people approve from Reporter.
    #[serde(default)]
    pub users: Vec<String>,
    /// Group full paths (at most 10): their direct members who are
    /// Maintainer and above.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Organisation selectors (at most 5, ADR-0038 §3.1): Maintainer and
    /// above whom they find for the requester.
    #[serde(default)]
    #[schema(value_type = Vec<Object>)]
    pub selectors: Vec<serde_json::Value>,
    /// How many of them must approve (1..=20).
    pub approvals_required: i32,
    /// A delegate may approve for a named person (default false).
    #[serde(default)]
    pub allow_delegation: bool,
}

/// A named approver user.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeployApproverUser {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
    pub display_name: String,
}

/// A named approver group.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeployApproverGroup {
    #[schema(minimum = 1)]
    pub id: i64,
    pub full_path: String,
    pub display_name: String,
}

/// One environment's approvers.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproversBody {
    pub tier: String,
    /// No users, groups or selectors: any Maintainer approves (as before).
    pub unrestricted: bool,
    pub users: Vec<DeployApproverUser>,
    pub groups: Vec<DeployApproverGroup>,
    #[schema(value_type = Vec<Object>)]
    pub selectors: Vec<serde_json::Value>,
    pub approvals_required: i32,
    pub allow_delegation: bool,
}

/// `GET …/deploy/approvers`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproversList {
    pub environments: Vec<ApproversBody>,
}

/// A target (its pin by fingerprint; never a private key).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetBody {
    pub name: String,
    pub host: String,
    pub port: i32,
    pub user: String,
    pub host_key_type: String,
    pub host_key_fingerprint: String,
    pub script: String,
    pub enabled: bool,
    pub updated_at: String,
}

/// An environment and its targets.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentBody {
    pub tier: String,
    pub requires_approval: bool,
    pub approval_ttl_minutes: i32,
    pub auto_rollback: bool,
    pub auto_deploy_ref: Option<String>,
    /// `<start>/<end>` in UTC.
    pub freeze_windows: Vec<String>,
    pub frozen_now: bool,
    pub targets: Vec<TargetBody>,
    pub updated_at: String,
    /// ADR-0038 §3: how many approvals a deployment needs.
    pub approvals_required: i32,
    /// ADR-0038 §3.5: a delegate may approve.
    pub allow_delegation: bool,
}

/// `GET …/deploy/environments`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentList {
    pub environments: Vec<EnvironmentBody>,
}

/// A deployment.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeploymentBody {
    pub id: i64,
    pub environment: String,
    pub sha: String,
    /// `pending_approval`, `queued`, `running`, `succeeded`, `failed`,
    /// `rolled_back`, `rollback_failed`, `blocked`.
    pub status: String,
    /// `None`: the system (an automatic deployment).
    pub requested_by: Option<i64>,
    pub approved_by: Option<i64>,
    pub approved_at: Option<String>,
    pub approval_expires_at: Option<String>,
    pub previous_sha: Option<String>,
    pub error_code: Option<String>,
    pub failed_target: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// The commit as git read it at the request (shown to the approver;
    /// `None` for an automatic request).
    pub commit_subject: Option<String>,
    pub commit_author_name: Option<String>,
    pub commit_author_email: Option<String>,
    /// ADR-0038 §3: the approvals given so far.
    pub approvals: i64,
}

/// `GET …/deployments`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeploymentList {
    pub deployments: Vec<DeploymentBody>,
    /// Plan 19 §3.3 (L5-9): when the list was read (UTC, RFC 3339).
    pub as_of: String,
}

fn target_body(t: StoredTarget) -> TargetBody {
    TargetBody {
        name: t.name,
        host: t.host,
        port: t.port,
        user: t.login_user,
        host_key_type: t.host_key_type,
        host_key_fingerprint: t.host_key_fingerprint,
        script: t.script,
        enabled: t.enabled,
        updated_at: t.updated_at,
    }
}

fn environment_body(e: StoredEnvironment, targets: Vec<TargetBody>) -> EnvironmentBody {
    EnvironmentBody {
        tier: e.tier.as_str().to_owned(),
        requires_approval: e.requires_approval,
        approval_ttl_minutes: e.approval_ttl_minutes,
        auto_rollback: e.auto_rollback,
        auto_deploy_ref: e.auto_deploy_ref,
        freeze_windows: e.freeze_windows,
        frozen_now: e.frozen_now,
        targets,
        updated_at: e.updated_at,
        approvals_required: e.approvals_required,
        allow_delegation: e.allow_delegation,
    }
}

fn deployment_body(d: StoredDeployment) -> DeploymentBody {
    DeploymentBody {
        id: d.id,
        environment: d.tier.as_str().to_owned(),
        sha: d.sha,
        status: d.status.as_str().to_owned(),
        requested_by: d.requested_by,
        approved_by: d.approved_by,
        approved_at: d.approved_at,
        approval_expires_at: d.approval_expires_at,
        previous_sha: d.previous_sha,
        error_code: d.error_code,
        failed_target: d.failed_target,
        created_at: d.created_at,
        started_at: d.started_at,
        finished_at: d.finished_at,
        commit_subject: d.commit_subject,
        commit_author_name: d.commit_author_name,
        commit_author_email: d.commit_author_email,
        approvals: d.approvals,
    }
}

fn deploy(state: &AppState) -> Result<&Deploy, ForgeError> {
    state.deploy.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "deploy_unconfigured")
    })
}

fn tier(raw: &str) -> Result<DeployTier, ForgeError> {
    DeployTier::parse(raw).ok_or_else(|| param_invalid("tier", "unknown_environment"))
}

/// What a path names below the project.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    Environments,
    Environment(DeployTier),
    Target(DeployTier, String),
    Deployments,
    Approve(i64),
    Approvers,
    EnvironmentApprovers(DeployTier),
}

/// The project and the shape of a deployment path (the raw path: never
/// decoded).
fn path_of(raw_path: &str) -> Result<(ProjectId, Shape), ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)?;
    let parts: Vec<&str> = tail.split('/').collect();
    let shape = match parts.as_slice() {
        ["deploy", "environments"] => Shape::Environments,
        ["deploy", "environments", t] => Shape::Environment(tier(t)?),
        ["deploy", "approvers"] => Shape::Approvers,
        ["deploy", "environments", t, "approvers"] => Shape::EnvironmentApprovers(tier(t)?),
        ["deploy", "environments", t, "targets", name] => {
            if name.is_empty() {
                return Err(param_invalid("name", "invalid_target_name"));
            }
            Shape::Target(tier(t)?, (*name).to_owned())
        }
        ["deployments"] => Shape::Deployments,
        ["deployments", id, "approve"] => Shape::Approve(
            parse_positive_id(id)
                .ok_or_else(|| param_invalid("deployment_id", "not_positive_decimal"))?,
        ),
        _ => return Err(param_invalid("path", "not_a_deploy_route")),
    };
    Ok((project, shape))
}

/// The project of a deployment route.
pub struct DeployOf;

/// Every project-scoped deployment route but the list.
pub const OF_PROJECT: DeployOf = DeployOf;

/// `GET …/deployments`: the project with the act-as facts (plan 19 §3.3,
/// ADR-0025 Amendment L5-4/L5-9) — the only deployment route kanflow's read
/// tools reach acting for a person. The environments (target host keys)
/// and approvers (people's names) load none and stay
/// `act_as_facts_missing`.
pub struct DeploymentsOf;

/// See [`DeploymentsOf`].
pub const OF_DEPLOYMENTS: DeploymentsOf = DeploymentsOf;

impl Loader<AppState> for DeploymentsOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let found = DeployOf.lookup(state, parts, viewer).await?;
            let Lookup::Found {
                ctx,
                canonical,
                tombstone,
                project,
                group,
            } = found
            else {
                return Ok(found);
            };
            let id = path_of(parts.uri.path())?.0;
            let ctx = crate::routes::act_as::project_ctx(state, parts, ctx, id).await?;
            Ok(Lookup::Found {
                ctx,
                canonical,
                tombstone,
                project,
                group,
            })
        })
    }
}

impl DeployOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let (id, _) = path_of(parts.uri.path())?;
        let Some(record) = projects::load_bounded(state, id, viewer).await? else {
            return Ok(absent());
        };
        if record.id != id {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "loaded_another_project")
                .with_context("project_id", id.get())
                .with_context("loaded_project_id", record.id.get()));
        }
        Ok(Lookup::Found {
            ctx: ProjectCtx::new(projects::loaded(&record)),
            canonical: None,
            tombstone: None,
            project: Some(record),
            group: None,
        })
    }
}

impl Loader<AppState> for DeployOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

macro_rules! body_loader {
    ($body:ty) => {
        impl BodyLoader<AppState, $body> for DeployOf {
            type Ctx = ProjectCtx;
            fn load<'a>(
                &'a self,
                state: &'a AppState,
                parts: &'a Parts,
                _: &'a $body,
                viewer: Option<UserId>,
            ) -> LoadFuture<'a, ProjectCtx> {
                Box::pin(self.lookup(state, parts, viewer))
            }
        }
    };
}
body_loader!(PutEnvironment);
body_loader!(PutApprovers);
body_loader!(PutTarget);
body_loader!(NewDeployment);

/// The deployment an approval names, with its project.
pub struct ApprovalOf;

/// `POST …/deployments/{deployment_id}/approve`.
pub const OF_APPROVAL: ApprovalOf = ApprovalOf;

impl ApprovalOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<DeploymentCtx>, ForgeError> {
        let (id, shape) = path_of(parts.uri.path())?;
        let Shape::Approve(deployment) = shape else {
            return Err(param_invalid("path", "not_an_approval"));
        };
        let Some(record) = projects::load_bounded(state, id, viewer).await? else {
            return Ok(absent());
        };
        // An unknown deployment is the invisible project 404 too.
        let service = deploy(state)?;
        let Some(stored) = service.deployment(id, deployment).await? else {
            return Ok(absent());
        };
        // ADR-0038 §3.4, §3.5: the caller (or the delegator they approve
        // for) against the environment's approvers.
        let params = crate::routes::browse::query(parts.uri.query(), &["on_behalf_of"])?;
        // Without a caller `authorize()` refuses before any fact counts. A
        // delegator that does not resolve or holds no delegation to the
        // caller is a fact too: `authorize()` refuses first (review of
        // ADR-0038 step C: no username or delegation oracle for someone who
        // may not approve).
        let fact = match viewer {
            None => forge_deploy::ApproverFact::own(forge_auth::ApproverGate::Unrestricted),
            Some(caller) => match crate::routes::browse::take(&params, "on_behalf_of") {
                Some(name) => match service.delegator(name, caller).await? {
                    Some(d) => {
                        service
                            .approver_gate(id, &stored, caller, Some(d))
                            .await?
                            .fact
                    }
                    None => service.unknown_delegator(id, &stored).await?,
                },
                None => service.approver_gate(id, &stored, caller, None).await?.fact,
            },
        };
        Ok(Lookup::Found {
            ctx: DeploymentCtx::new(
                projects::loaded(&record),
                forge_deploy::loaded_deployment(&stored, fact),
            ),
            canonical: None,
            tombstone: None,
            project: Some(record),
            group: None,
        })
    }
}

impl Loader<AppState> for ApprovalOf {
    type Ctx = DeploymentCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, DeploymentCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

fn change_request<R: forge_auth::ResourceCtx, B>(w: &WriteParts<R, B>) -> ChangeRequest {
    ChangeRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn created_or_ok(created: bool) -> StatusCode {
    if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    }
}

/// `GET …/deploy/environments` → `200`.
pub async fn environments(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let (envs, targets) = bounded(
            &state,
            "deploy_environments",
            deploy(&state)?.environments(&w.proof),
        )
        .await?;
        let mut targets: Vec<StoredTarget> = targets;
        let environments = envs
            .into_iter()
            .map(|e| {
                let (mine, rest): (Vec<StoredTarget>, Vec<StoredTarget>) =
                    std::mem::take(&mut targets)
                        .into_iter()
                        .partition(|t| t.tier == e.tier);
                targets = rest;
                environment_body(e, mine.into_iter().map(target_body).collect())
            })
            .collect();
        json(EnvironmentList { environments })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT …/deploy/environments/{tier}` → `201` / `200`.
pub async fn put_environment(
    state: AppState,
    request: Dispatched<ProjectCtx, PutEnvironment>,
) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, Shape::Environment(tier)) = path_of(&path)? else {
            return Err(param_invalid("tier", "unknown_environment"));
        };
        let spec = EnvironmentSpec {
            tier,
            requires_approval: w.input.requires_approval,
            approval_ttl_minutes: w.input.approval_ttl_minutes,
            auto_rollback: w.input.auto_rollback,
            auto_deploy_ref: w.input.auto_deploy_ref.clone(), // clone: the spec owns it
            freeze_windows: w
                .input
                .freeze_windows
                .iter()
                .map(|f| (f.starts_at.clone(), f.ends_at.clone())) // clone: as above
                .collect(),
        };
        let (stored, created) = deploy(&state)?
            .put_environment(&change_request(&w), &w.proof, &spec)
            .await?;
        Ok::<_, ForgeError>((
            created_or_ok(created),
            json(environment_body(stored, Vec::new()))?,
        ))
    }
    .await;
    match built {
        Ok((status, value)) => no_store(status, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT …/targets/{name}` → `201` / `200`.
pub async fn put_target(state: AppState, request: Dispatched<ProjectCtx, PutTarget>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, Shape::Target(tier, name)) = path_of(&path)? else {
            return Err(param_invalid("name", "invalid_target_name"));
        };
        let spec = TargetSpec {
            name,
            host: w.input.host.clone(), // clone: the spec owns it
            port: w.input.port.unwrap_or(22),
            user: w.input.user.clone(),                   // clone: as host
            host_key_type: w.input.host_key_type.clone(), // clone: as host
            host_key: w.input.host_key.clone(),           // clone: as host
            script: w.input.script.clone(),               // clone: as host
            enabled: w.input.enabled.unwrap_or(true),
        };
        let (stored, created) = deploy(&state)?
            .put_target(&change_request(&w), &w.proof, tier, &spec)
            .await?;
        Ok::<_, ForgeError>((created_or_ok(created), json(target_body(stored))?))
    }
    .await;
    match built {
        Ok((status, value)) => no_store(status, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `GET …/deployments` → `200`.
pub async fn deployments(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let as_of = crate::routes::ai_tools::as_of_now()?;
        let rows = bounded(
            &state,
            "deployment_list",
            deploy(&state)?.deployments(&w.proof),
        )
        .await?;
        // Plan 19 §3.3 (L5-9): kanflow's AI chat acting for a person is no
        // person reading — the commit author's name and address stay out of it.
        let acting = w.proof.principal().act_as_credential().is_some();
        json(DeploymentList {
            deployments: rows
                .into_iter()
                .map(deployment_body)
                .map(|mut d| {
                    if acting {
                        d.commit_author_name = None;
                        d.commit_author_email = None;
                    }
                    d
                })
                .collect(),
            as_of,
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/deployments` → `201`.
pub async fn request(state: AppState, request: Dispatched<ProjectCtx, NewDeployment>) -> Response {
    let w = request.into_write();
    let built = async {
        let tier = DeployTier::parse(&w.input.environment)
            .ok_or_else(|| param_invalid("environment", "unknown_environment"))?;
        let stored = deploy(&state)?
            .request(&change_request(&w), &w.proof, tier, &w.input.sha)
            .await?;
        json(deployment_body(stored))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::CREATED, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/deployments/{deployment_id}/approve` → `200`.
pub async fn approve(state: AppState, request: Dispatched<DeploymentCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let on_behalf_of = w.proof.ctx().deployment().on_behalf_of;
        let stored = deploy(&state)?
            .approve(&change_request(&w), &w.proof, on_behalf_of)
            .await?;
        json(deployment_body(stored))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

fn approvers_body(e: forge_deploy::EnvironmentApprovers, names: &ApproverNames) -> ApproversBody {
    ApproversBody {
        tier: e.tier.as_str().to_owned(),
        unrestricted: e.unrestricted(),
        users: e
            .users
            .iter()
            .filter_map(|u| names.users.get(u))
            .map(|u| DeployApproverUser {
                id: u.id.get(),
                username: u.username.clone(), // clone: the names stay for the next
                display_name: u.display_name.clone(),
            })
            .collect(),
        groups: e
            .groups
            .iter()
            .filter_map(|g| names.groups.get(g))
            .map(|g| DeployApproverGroup {
                id: g.id.get(),
                full_path: g.full_path.clone(), // clone: as above
                display_name: g.display_name.clone(),
            })
            .collect(),
        selectors: e
            .selectors
            .iter()
            .map(forge_core::approver::ApproverSelector::to_value)
            .collect(),
        approvals_required: e.required,
        allow_delegation: e.allow_delegation,
    }
}

async fn approver_names(
    state: &AppState,
    envs: &[forge_deploy::EnvironmentApprovers],
) -> Result<ApproverNames, ForgeError> {
    let users: Vec<UserId> = envs.iter().flat_map(|e| e.users.iter().copied()).collect();
    let groups: Vec<forge_core::GroupId> =
        envs.iter().flat_map(|e| e.groups.iter().copied()).collect();
    bounded(
        state,
        "deploy_approver_names",
        deploy(state)?.approver_names(&users, &groups),
    )
    .await
}

/// `GET …/deploy/approvers` → `200`.
pub async fn approvers(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let envs = bounded(
            &state,
            "deploy_approvers",
            deploy(&state)?.approvers(&w.proof),
        )
        .await?;
        let names = approver_names(&state, &envs).await?;
        json(ApproversList {
            environments: envs
                .into_iter()
                .map(|e| approvers_body(e, &names))
                .collect(),
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT …/deploy/environments/{tier}/approvers` → `200`.
pub async fn put_approvers(
    state: AppState,
    request: Dispatched<ProjectCtx, PutApprovers>,
) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, Shape::EnvironmentApprovers(tier)) = path_of(&path)? else {
            return Err(param_invalid("tier", "unknown_environment"));
        };
        let spec = ApproversSpec {
            users: w.input.users.clone(), // clone: the spec owns them
            groups: w.input.groups.clone(),
            selectors: w.input.selectors.clone(),
            required: w.input.approvals_required,
            allow_delegation: w.input.allow_delegation,
        };
        let stored = deploy(&state)?
            .put_approvers(&change_request(&w), &w.proof, tier, &spec)
            .await?;
        let names = approver_names(&state, std::slice::from_ref(&stored)).await?;
        json(approvers_body(stored, &names))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_path_names_the_project_and_the_shape() {
        assert_eq!(
            path_of("/api/v1/projects/7/deploy/environments").expect("envs"),
            (ProjectId::new(7), Shape::Environments)
        );
        assert_eq!(
            path_of("/api/v1/projects/7/deploy/environments/prod/targets/web-1").expect("t"),
            (
                ProjectId::new(7),
                Shape::Target(DeployTier::Prod, "web-1".to_owned())
            )
        );
        assert_eq!(
            path_of("/api/v1/projects/7/deployments/12/approve").expect("approve"),
            (ProjectId::new(7), Shape::Approve(12))
        );
        assert_eq!(
            path_of("/api/v1/projects/7/deploy/approvers").expect("approvers"),
            (ProjectId::new(7), Shape::Approvers)
        );
        assert_eq!(
            path_of("/api/v1/projects/7/deploy/environments/prod/approvers").expect("env"),
            (
                ProjectId::new(7),
                Shape::EnvironmentApprovers(DeployTier::Prod)
            )
        );
        for bad in [
            "/api/v1/projects/0/deployments",
            "/api/v1/projects/7/deploy/environments/qa",
            "/api/v1/projects/7/deployments/x/approve",
            "/api/v1/projects/7/deploy/environments/stg/targets/",
        ] {
            assert!(path_of(bad).is_err(), "{bad}");
        }
    }
}
