//! A project's `ai_assets.enabled` (plan 17 §0.4, P17-3, ADR-0035):
//!
//! - `GET /api/v1/projects/{id}/ai-assets` (`ProjectRead`): whether the AI
//!   asset lint, the `forge/risk` rules `ai_case_raw_data` / `ai_asset` and
//!   asset releases apply to the project, who changed it last, and `viewer`
//!   (a trial decision: a hint only).
//! - `PUT …/ai-assets` (`AiAssetsSettingManage`: people only, a web
//!   session, Maintainer and above): `{"enabled": true | false}` → `200`;
//!   audited. Turning it off stops those checks and new releases only.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, ProjectCtx, ResourceCtx};
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{AiAssetsSetting, Review, ReviewRequest};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` / `PUT` a project's `ai_assets.enabled`.
pub const ONE: &str = "/api/v1/projects/{id}/ai-assets";
/// The body cap.
pub const MAX_BODY: usize = 256;

/// `PUT …/ai-assets`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutAiAssets {
    pub enabled: bool,
}

/// What the caller could do now (a hint; the route decides again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAssetsViewerBody {
    pub can_change: bool,
}

/// A project's `ai_assets.enabled`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAssetsBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// On: the AI asset lint, `forge/risk`'s `ai_case_raw_data` (stop) and
    /// `ai_asset` (warn) and asset releases apply. Off (the default): none.
    pub enabled: bool,
    #[schema(required = true, minimum = 1)]
    pub changed_by: Option<i64>,
    /// UTC, RFC 3339 (`null`: never changed).
    #[schema(required = true)]
    pub changed_at: Option<String>,
    pub viewer: AiAssetsViewerBody,
}

fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

fn project_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    if tail != "ai-assets" {
        return Err(bad());
    }
    parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)
}

/// The project of the route.
pub struct AiAssetsOf;

impl AiAssetsOf {
    async fn lookup(
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let id = project_id(parts.uri.path())?;
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

impl Loader<AppState> for AiAssetsOf {
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

impl BodyLoader<AppState, PutAiAssets> for AiAssetsOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PutAiAssets,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

fn viewer<B>(w: &WriteParts<ProjectCtx, B>) -> AiAssetsViewerBody {
    AiAssetsViewerBody {
        can_change: forge_auth::authorize(
            w.proof.principal(),
            Action::AiAssetsSettingManage,
            &w.proof.ctx().resource(),
        )
        .is_ok(),
    }
}

fn body(s: AiAssetsSetting, viewer: AiAssetsViewerBody) -> AiAssetsBody {
    AiAssetsBody {
        project_id: s.project.get(),
        enabled: s.enabled,
        changed_by: s.changed_by.map(UserId::get),
        changed_at: s.changed_at,
        viewer,
    }
}

fn respond(ctx: &crate::Ctx, built: Result<serde_json::Value, ForgeError>) -> Response {
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/ai-assets` → `200`.
pub async fn read(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let project = *w.proof.id();
        let setting = bounded(
            &state,
            "ai_assets_read",
            review(&state)?.ai_assets_setting(project),
        )
        .await?;
        json(body(setting, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

/// `PUT …/ai-assets` → `200`.
pub async fn put(state: AppState, request: Dispatched<ProjectCtx, PutAiAssets>) -> Response {
    let w = request.into_write();
    let built = async {
        let req = ReviewRequest {
            request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
            credential_id: w.credential_id,
        };
        let (_, setting) = review(&state)?
            .set_ai_assets_enabled(&req, &w.proof, w.input.enabled)
            .await?;
        json(body(setting, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}
