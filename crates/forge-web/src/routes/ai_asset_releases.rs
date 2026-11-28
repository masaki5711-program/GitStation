//! `pfs-ai-bundle` releases (plan 17 P17-7, ADR-0031 Amendment P17-7,
//! `forge-deploy`):
//!
//! - `GET /api/v1/projects/{id}/ai-asset-releases` (`DeployRead`): the
//!   newest releases (no manifest) with their withdrawal and their state on
//!   each stage, what runs on each stage and what a rollback would request,
//!   and what the caller may press (the releases screen, P17-7b).
//! - `POST …/ai-asset-releases` (`AiAssetReleaseCreate`: a person,
//!   Maintainer and above, a session or an `api` token): `{"tag":
//!   "assets/v2026.48.1"}` → `201` the signed release; `422 FG-AIASSET-0008`
//!   when the tag, the bench, the lint or the setting refuses it.
//! - `GET …/ai-asset-releases/{version}` (`DeployRead`): one release with
//!   its manifest, digest, key and signature (what the PFS loader
//!   verifies).
//! - `POST …/ai-asset-releases/{version}/deployments` (`DeployCreate`):
//!   `{"environment": "pfs-ai-shadow" | "pfs-ai-canary" | "pfs-ai-prod"}`
//!   (or `dev` / `stg` / `prod`) → `201` a deployment waiting for a
//!   person's approval; `409 FG-AIASSET-0009` out of stage order.
//! - `POST …/ai-asset-rollbacks` (`DeployCreate`): `{"environment"}` →
//!   `201` a deployment of the release last approved before the running
//!   one, waiting for an approval; `409 FG-AIASSET-0009
//!   no_rollback_target`.
//! - `POST …/ai-asset-releases/{version}/withdrawal`
//!   (`AiAssetReleaseWithdraw`, P17-7b): `{"reason"}` → `201` the
//!   withdrawal; `409 FG-AIASSET-0010` (no such release, withdrawn
//!   already, the reason).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, ProjectCtx, ResourceCtx};
use forge_core::deploy::DeployTier;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_deploy::{
    ChangeRequest, Deploy, ReleaseOverview, ReleaseRow, StageRow, StoredRelease, StoredWithdrawal,
};
use forge_errors::{ErrorCode, ForgeError};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::deploy::{deployment_body, DeploymentBody};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the releases, `POST` a release.
pub const RELEASES: &str = "/api/v1/projects/{id}/ai-asset-releases";
/// `GET` one release.
pub const RELEASE: &str = "/api/v1/projects/{id}/ai-asset-releases/{version}";
/// `POST` a deployment of a release.
pub const RELEASE_DEPLOYMENTS: &str =
    "/api/v1/projects/{id}/ai-asset-releases/{version}/deployments";
/// `POST` a rollback.
pub const ROLLBACKS: &str = "/api/v1/projects/{id}/ai-asset-rollbacks";
/// `POST` a withdrawal (P17-7b).
pub const WITHDRAWAL: &str = "/api/v1/projects/{id}/ai-asset-releases/{version}/withdrawal";
/// The body cap.
pub const MAX_BODY: usize = 1024;
/// The withdrawal's body cap (a reason of 500 characters, 4 bytes each).
pub const MAX_WITHDRAW_BODY: usize = 4096;

/// `POST …/ai-asset-releases`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewRelease {
    /// `assets/v<yyyy>.<ww>.<n>`.
    pub tag: String,
}

/// `POST …/{version}/deployments`, `POST …/ai-asset-rollbacks`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseEnvironment {
    /// `pfs-ai-shadow` (dev), `pfs-ai-canary` (stg) or `pfs-ai-prod` (prod).
    pub environment: String,
}

/// `POST …/{version}/withdrawal`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WithdrawRelease {
    /// Why the release is bad: 1 to 500 characters, one line.
    pub reason: String,
}

/// A release's withdrawal (P17-7b): for good.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalBody {
    pub version: String,
    pub reason: String,
    pub withdrawn_by: i64,
    pub withdrawn_at: String,
}

/// A release's state on one stage (lists only).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseStageBody {
    /// `pfs-ai-shadow`, `pfs-ai-canary` or `pfs-ai-prod`.
    pub environment: String,
    /// `dev`, `stg` or `prod`.
    pub tier: String,
    /// Its newest deployment there (`null`: never requested there).
    pub deployment_id: Option<i64>,
    pub status: Option<String>,
    /// A deployment of it succeeded there with a person's approval.
    pub approved_success: bool,
}

/// One stage of the project (lists only).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StageBody {
    /// `pfs-ai-shadow`, `pfs-ai-canary` or `pfs-ai-prod`.
    pub environment: String,
    pub tier: String,
    /// The release of its last succeeded deployment (`null`: none, or no
    /// release).
    pub running_version: Option<String>,
    /// The release a rollback would request (`null`: none to go back to).
    pub rollback_version: Option<String>,
}

/// What the caller may press on the releases screen.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleasesViewerBody {
    /// `AiAssetReleaseCreate`.
    pub can_release: bool,
    /// `DeployCreate`: a stage request and a rollback.
    pub can_request: bool,
    /// `AiAssetReleaseWithdraw`.
    pub can_withdraw: bool,
}

/// A release.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseBody {
    pub id: i64,
    /// `pfs-ai-bundle`.
    pub kind: String,
    pub version: String,
    pub tag: String,
    /// The commit the tag named when the release was made.
    pub commit_sha: String,
    pub tag_object: String,
    /// `sha256:<hex>` of the manifest.
    pub digest: String,
    pub key_id: String,
    /// Base64 Ed25519 in the `forge-pfs-ai-bundle-signature-v1` domain.
    pub signature: String,
    pub files: i32,
    pub bench_check_run_id: i64,
    pub created_by: i64,
    pub created_at: String,
    /// The signed manifest (one release only; `null` in lists).
    #[schema(value_type = Option<Object>, required = true)]
    pub manifest: Option<serde_json::Value>,
    /// P17-7b: its withdrawal (`null`: not withdrawn).
    #[schema(required = true)]
    pub withdrawal: Option<WithdrawalBody>,
    /// Signed by a key revoked since (lists only; `null` elsewhere).
    #[schema(required = true)]
    pub key_revoked: Option<bool>,
    /// Shadow, canary, prod (lists only; `null` elsewhere).
    #[schema(required = true)]
    pub stages: Option<Vec<ReleaseStageBody>>,
    /// The stage a request of it would be accepted on now (lists only;
    /// `null`: withdrawn, a revoked key, one under way, or done).
    #[schema(required = true)]
    pub next_stage: Option<String>,
}

/// `GET …/ai-asset-releases`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseList {
    pub releases: Vec<ReleaseBody>,
    /// Shadow, canary, prod.
    pub stages: Vec<StageBody>,
    pub viewer: ReleasesViewerBody,
}

/// A rollback: the release gone back to and its deployment.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RollbackBody {
    pub release: ReleaseBody,
    pub deployment: DeploymentBody,
}

fn withdrawal_body(w: StoredWithdrawal) -> WithdrawalBody {
    WithdrawalBody {
        version: w.version,
        reason: w.reason,
        withdrawn_by: w.withdrawn_by,
        withdrawn_at: w.withdrawn_at,
    }
}

fn row_body(row: ReleaseRow) -> ReleaseBody {
    let mut body = release_body(row.release, false);
    body.withdrawal = row.withdrawal.map(withdrawal_body);
    body.key_revoked = Some(row.key_revoked);
    body.next_stage = row.next_stage.map(|t| t.bundle_stage().to_owned());
    body.stages = Some(
        row.stages
            .into_iter()
            .map(|s| ReleaseStageBody {
                environment: s.tier.bundle_stage().to_owned(),
                tier: s.tier.as_str().to_owned(),
                deployment_id: s.deployment_id,
                status: s.status.map(|x| x.as_str().to_owned()),
                approved_success: s.approved_success,
            })
            .collect(),
    );
    body
}

fn stage_body(s: StageRow) -> StageBody {
    StageBody {
        environment: s.tier.bundle_stage().to_owned(),
        tier: s.tier.as_str().to_owned(),
        running_version: s.running,
        rollback_version: s.rollback_to,
    }
}

fn viewer<B>(w: &WriteParts<ProjectCtx, B>) -> ReleasesViewerBody {
    let may = |action: Action| {
        forge_auth::authorize(w.proof.principal(), action, &w.proof.ctx().resource()).is_ok()
    };
    ReleasesViewerBody {
        can_release: may(Action::AiAssetReleaseCreate),
        can_request: may(Action::DeployCreate),
        can_withdraw: may(Action::AiAssetReleaseWithdraw),
    }
}

fn release_body(r: StoredRelease, with_manifest: bool) -> ReleaseBody {
    ReleaseBody {
        id: r.id,
        kind: r.kind,
        version: r.version,
        tag: r.tag,
        commit_sha: r.commit_sha,
        tag_object: r.tag_object,
        digest: r.digest,
        key_id: r.key_id,
        signature: r.signature,
        files: r.files,
        bench_check_run_id: r.bench_check_run_id,
        created_by: r.created_by,
        created_at: r.created_at,
        manifest: with_manifest.then_some(r.manifest),
        withdrawal: None,
        key_revoked: None,
        stages: None,
        next_stage: None,
    }
}

fn deploy(state: &AppState) -> Result<&Deploy, ForgeError> {
    state.deploy.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "deploy_unconfigured")
    })
}

fn stage(raw: &str) -> Result<DeployTier, ForgeError> {
    DeployTier::parse_stage(raw).ok_or_else(|| param_invalid("environment", "unknown_environment"))
}

/// What a path names below the project.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    Releases,
    Release(String),
    Deployments(String),
    Withdrawal(String),
    Rollbacks,
}

fn version(raw: &str) -> Result<String, ForgeError> {
    if version_shaped(raw) {
        Ok(raw.to_owned())
    } else {
        Err(param_invalid("version", "not_a_release_version"))
    }
}

/// `v<yyyy>.<ww>.<n>` (the release's own check, kept here to bound the
/// path before anything is read).
fn version_shaped(raw: &str) -> bool {
    raw.len() <= 16
        && raw.starts_with('v')
        && raw[1..].split('.').count() == 3
        && raw[1..].bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// The project and the shape (the raw path: never decoded).
fn path_of(raw_path: &str) -> Result<(ProjectId, Shape), ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)?;
    let parts: Vec<&str> = tail.split('/').collect();
    let shape = match parts.as_slice() {
        ["ai-asset-releases"] => Shape::Releases,
        ["ai-asset-releases", v] => Shape::Release(version(v)?),
        ["ai-asset-releases", v, "deployments"] => Shape::Deployments(version(v)?),
        ["ai-asset-releases", v, "withdrawal"] => Shape::Withdrawal(version(v)?),
        ["ai-asset-rollbacks"] => Shape::Rollbacks,
        _ => return Err(param_invalid("path", "not_an_ai_asset_release_route")),
    };
    Ok((project, shape))
}

/// The project of every release route.
pub struct ReleasesOf;

/// See [`ReleasesOf`].
pub const OF_PROJECT: ReleasesOf = ReleasesOf;

impl ReleasesOf {
    async fn lookup(
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

impl Loader<AppState> for ReleasesOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

macro_rules! body_loader {
    ($body:ty) => {
        impl BodyLoader<AppState, $body> for ReleasesOf {
            type Ctx = ProjectCtx;
            fn load<'a>(
                &'a self,
                state: &'a AppState,
                parts: &'a Parts,
                _: &'a $body,
                viewer: Option<UserId>,
            ) -> LoadFuture<'a, ProjectCtx> {
                Box::pin(Self::lookup(state, parts, viewer))
            }
        }
    };
}
body_loader!(NewRelease);
body_loader!(ReleaseEnvironment);
body_loader!(WithdrawRelease);

fn change_request<B>(w: &WriteParts<ProjectCtx, B>) -> ChangeRequest {
    ChangeRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn respond(
    ctx: &crate::Ctx,
    status: StatusCode,
    built: Result<serde_json::Value, ForgeError>,
) -> Response {
    match built {
        Ok(value) => no_store(status, value),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/ai-asset-releases` → `200`.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let ReleaseOverview { releases, stages } = bounded(
            &state,
            "ai_asset_releases",
            deploy(&state)?.release_overview(&w.proof),
        )
        .await?;
        json(ReleaseList {
            releases: releases.into_iter().map(row_body).collect(),
            stages: stages.into_iter().map(stage_body).collect(),
            viewer: viewer(&w),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET …/ai-asset-releases/{version}` → `200`.
pub async fn one(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, Shape::Release(v)) = path_of(&path)? else {
            return Err(param_invalid("path", "not_a_release"));
        };
        let found = bounded(
            &state,
            "ai_asset_release",
            deploy(&state)?.release(&w.proof, &v),
        )
        .await?;
        let Some(r) = found else {
            return Err(
                ForgeError::new(ErrorCode::NotFound).with_context("resource", "ai_asset_release")
            );
        };
        let withdrawn = bounded(
            &state,
            "ai_asset_release_withdrawal",
            deploy(&state)?.release_withdrawal(&w.proof, &v),
        )
        .await?;
        let mut body = release_body(r, true);
        body.withdrawal = withdrawn.map(withdrawal_body);
        json(body)
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/ai-asset-releases` → `201`.
pub async fn create(state: AppState, request: Dispatched<ProjectCtx, NewRelease>) -> Response {
    let w = request.into_write();
    let built = async {
        let stored = deploy(&state)?
            .create_release(&change_request(&w), &w.proof, &w.input.tag)
            .await?;
        json(release_body(stored, true))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `POST …/ai-asset-releases/{version}/deployments` → `201`.
pub async fn request_deployment(
    state: AppState,
    request: Dispatched<ProjectCtx, ReleaseEnvironment>,
) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, Shape::Deployments(v)) = path_of(&path)? else {
            return Err(param_invalid("path", "not_a_release_deployment"));
        };
        let tier = stage(&w.input.environment)?;
        let stored = deploy(&state)?
            .request_release(&change_request(&w), &w.proof, tier, &v)
            .await?;
        json(deployment_body(stored))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `POST …/ai-asset-releases/{version}/withdrawal` → `201` (P17-7b).
pub async fn withdraw(
    state: AppState,
    request: Dispatched<ProjectCtx, WithdrawRelease>,
) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, Shape::Withdrawal(v)) = path_of(&path)? else {
            return Err(param_invalid("path", "not_a_release_withdrawal"));
        };
        let stored = deploy(&state)?
            .withdraw_release(&change_request(&w), &w.proof, &v, &w.input.reason)
            .await?;
        json(withdrawal_body(stored))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `POST …/ai-asset-rollbacks` → `201`.
pub async fn rollback(
    state: AppState,
    request: Dispatched<ProjectCtx, ReleaseEnvironment>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let tier = stage(&w.input.environment)?;
        let (release, deployment) = deploy(&state)?
            .rollback_release(&change_request(&w), &w.proof, tier)
            .await?;
        json(RollbackBody {
            release: release_body(release, false),
            deployment: deployment_body(deployment),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}
