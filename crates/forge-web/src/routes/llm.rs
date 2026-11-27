//! The local LLM's settings (plan 19 §8.2, ADR-0039 Amendment
//! `feat/core-llm-settings-and-rerun`):
//!
//! - `GET /api/v1/projects/{id}/llm` (`ProjectRead`): the project's
//!   `llm.review` (`off` / `advisory` / `required`), who changed it last and
//!   `viewer.can_change` (a trial decision: a hint only).
//! - `PUT …/llm` (`LlmSettingsManage`: a Maintainer and above, a person, a
//!   web session — never a token, an agent, a bot, a service account or the
//!   auto-merge principal): `{"review": "off" | "advisory" | "required"}` →
//!   `200`; audited (`llm.review_changed`). Turning it off removes nothing.
//! - `POST /api/v1/admin/llm/pause` and `…/resume` (`LlmPause` /
//!   `LlmResume`: an administrator, a person; a token needs `admin`) →
//!   `200`; audited. The worker follows the stored state within 10 s (no
//!   new inference while paused; what is running finishes). In production
//!   only as an approved runbook step (`llm-gateway`).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, InstanceCtx, ProjectCtx, ResourceCtx};
use forge_core::llm::ReviewMode;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{GatewayPause, LlmReviewSetting, Review, ReviewRequest};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` / `PUT` a project's `llm.review`.
pub const ONE: &str = "/api/v1/projects/{id}/llm";
/// `POST`: pause the LLM gateway.
pub const PAUSE: &str = "/api/v1/admin/llm/pause";
/// `POST`: resume it.
pub const RESUME: &str = "/api/v1/admin/llm/resume";
/// The body cap.
pub const MAX_BODY: usize = 256;

/// `PUT …/llm`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutLlm {
    /// `off`, `advisory` or `required`.
    pub review: String,
}

/// What the caller could do now (a hint; the route decides again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LlmViewerBody {
    pub can_change: bool,
}

/// A project's `llm.review`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LlmSettingBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// `off` (the default: no AI first pass, no `forge/ai-review` run),
    /// `advisory` (the first pass as advice; `forge/ai-review` is never
    /// `failure`) or `required` (critical / high findings make it
    /// `failure`; it blocks a merge only if a branch rule names it).
    pub review: String,
    #[schema(required = true, minimum = 1)]
    pub changed_by: Option<i64>,
    /// UTC, RFC 3339 (`null`: never changed).
    #[schema(required = true)]
    pub changed_at: Option<String>,
    pub viewer: LlmViewerBody,
}

/// The instance's pause of the LLM gateway.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LlmGatewayBody {
    /// Stored: the worker follows within 10 s (`llm.paused` in the config
    /// also pauses it).
    pub paused: bool,
    #[schema(required = true, minimum = 1)]
    pub changed_by: Option<i64>,
    /// UTC, RFC 3339 (`null`: never changed).
    #[schema(required = true)]
    pub changed_at: Option<String>,
    /// `changed` or `unchanged` (it was so already).
    pub outcome: String,
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
    if tail != "llm" {
        return Err(bad());
    }
    parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)
}

/// The project of the route.
pub struct LlmOf;

impl LlmOf {
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

impl Loader<AppState> for LlmOf {
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

impl BodyLoader<AppState, PutLlm> for LlmOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PutLlm,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

fn request_of<C: ResourceCtx, B>(w: &WriteParts<C, B>) -> ReviewRequest {
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn viewer<B>(w: &WriteParts<ProjectCtx, B>) -> LlmViewerBody {
    LlmViewerBody {
        can_change: forge_auth::authorize(
            w.proof.principal(),
            Action::LlmSettingsManage,
            &w.proof.ctx().resource(),
        )
        .is_ok(),
    }
}

fn setting_body(s: LlmReviewSetting, viewer: LlmViewerBody) -> LlmSettingBody {
    LlmSettingBody {
        project_id: s.project.get(),
        review: s.mode.as_str().to_owned(),
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

/// `GET …/llm` → `200`.
pub async fn read(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let project = *w.proof.id();
        let setting = bounded(
            &state,
            "llm_setting_read",
            review(&state)?.llm_review_setting(project),
        )
        .await?;
        json(setting_body(setting, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

/// `PUT …/llm` → `200`.
pub async fn put(state: AppState, request: Dispatched<ProjectCtx, PutLlm>) -> Response {
    let w = request.into_write();
    let built = async {
        let mode = ReviewMode::parse(&w.input.review)
            .ok_or_else(|| param_invalid("review", "off_advisory_or_required"))?;
        let (_, setting) = review(&state)?
            .set_llm_review(&request_of(&w), &w.proof, mode)
            .await?;
        json(setting_body(setting, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

fn gateway_body(s: GatewayPause, outcome: forge_review::AutoMergeOutcome) -> LlmGatewayBody {
    LlmGatewayBody {
        paused: s.paused,
        changed_by: s.changed_by.map(UserId::get),
        changed_at: s.changed_at,
        outcome: match outcome {
            forge_review::AutoMergeOutcome::Changed => "changed",
            forge_review::AutoMergeOutcome::Unchanged => "unchanged",
        }
        .to_owned(),
    }
}

async fn set_paused(state: AppState, request: Dispatched<InstanceCtx>, paused: bool) -> Response {
    let w = request.into_write();
    let built = async {
        let (outcome, s) = review(&state)?
            .set_llm_paused(&request_of(&w), &w.proof, paused)
            .await?;
        json(gateway_body(s, outcome))
    }
    .await;
    respond(&w.ctx, built)
}

/// `POST /api/v1/admin/llm/pause` → `200`.
pub async fn pause(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    set_paused(state, request, true).await
}

/// `POST /api/v1/admin/llm/resume` → `200`.
pub async fn resume(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    set_paused(state, request, false).await
}
