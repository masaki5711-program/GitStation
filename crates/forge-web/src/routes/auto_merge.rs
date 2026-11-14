//! Auto-merge of a project and of the instance (M4 C9; plan 04 M-5;
//! ADR-0017 Amendment 2026-11-13):
//!
//! - `GET /api/v1/projects/{id}/auto-merge` (`MergeQueueRead`: as the
//!   repository browse API): the project's `merge.auto`, its emergency stop
//!   and the instance's, and `viewer` (a trial decision: a hint only).
//! - `PUT …/auto-merge` (`AutoMergeSettingsManage`: people only, a web
//!   session, the Owner): `{"mode": "off" | "reserve" | "conditional" |
//!   "full"}` → `200`.
//! - `POST …/auto-merge/stop` and `…/resume` (`AutoMergeStopToggle`:
//!   people only, Maintainer) → `200`. Stopping holds every auto-merge of
//!   the project (nothing is queued; queued auto-merge entries wait);
//!   people still merge.
//! - `POST /api/v1/admin/auto-merge/stop` and `…/resume`
//!   (`AutoMergeInstanceStopToggle`: people only, administrators) → `200`.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, InstanceCtx, ProjectCtx, ResourceCtx};
use forge_core::auto_merge::AutoMergeMode;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{Review, ReviewRequest};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` / `PUT` a project's auto-merge.
pub const ONE: &str = "/api/v1/projects/{id}/auto-merge";
/// `POST` the project's emergency stop.
pub const STOP: &str = "/api/v1/projects/{id}/auto-merge/stop";
/// `POST` resume it.
pub const RESUME: &str = "/api/v1/projects/{id}/auto-merge/resume";
/// `POST` the instance's emergency stop.
pub const ADMIN_STOP: &str = "/api/v1/admin/auto-merge/stop";
/// `POST` resume it.
pub const ADMIN_RESUME: &str = "/api/v1/admin/auto-merge/resume";
/// The body cap (one word).
pub const MAX_BODY: usize = 256;

/// `PUT …/auto-merge`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutAutoMerge {
    /// `off`, `reserve`, `conditional` or `full`.
    pub mode: String,
}

/// Who turned an emergency stop on, and when.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoMergeStopBody {
    #[schema(required = true, minimum = 1)]
    pub by: Option<i64>,
    /// UTC, RFC 3339.
    pub at: String,
}

/// What the caller could do now (a hint; the routes decide again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoMergeViewerBody {
    pub can_change_mode: bool,
    pub can_stop: bool,
}

/// A project's auto-merge.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoMergeBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// `off`, `reserve`, `conditional` or `full` (`full`: a new project's
    /// default; projects older than auto-merge start `off`).
    pub mode: String,
    /// The auto-merge principal acts in this mode (`conditional` / `full`).
    pub acts: bool,
    #[schema(required = true, minimum = 1)]
    pub mode_by: Option<i64>,
    #[schema(required = true)]
    pub mode_at: Option<String>,
    /// The project's emergency stop (`null`: off).
    #[schema(required = true)]
    pub project_stop: Option<AutoMergeStopBody>,
    /// The instance's emergency stop (`null`: off).
    #[schema(required = true)]
    pub instance_stop: Option<AutoMergeStopBody>,
    pub viewer: AutoMergeViewerBody,
}

/// The instance's emergency stop.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceAutoMergeBody {
    #[schema(required = true)]
    pub stop: Option<AutoMergeStopBody>,
}

fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

/// The path's project id.
fn project_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    if !matches!(tail, "auto-merge" | "auto-merge/stop" | "auto-merge/resume") {
        return Err(bad());
    }
    parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)
}

/// The project of an auto-merge route.
pub struct AutoMergeOf;

impl AutoMergeOf {
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

impl Loader<AppState> for AutoMergeOf {
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

impl BodyLoader<AppState, PutAutoMerge> for AutoMergeOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PutAutoMerge,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

fn request_of<C, B>(w: &WriteParts<C, B>) -> ReviewRequest
where
    C: ResourceCtx,
{
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn viewer<B>(w: &WriteParts<ProjectCtx, B>) -> AutoMergeViewerBody {
    let can = |action| {
        forge_auth::authorize(w.proof.principal(), action, &w.proof.ctx().resource()).is_ok()
    };
    AutoMergeViewerBody {
        can_change_mode: can(Action::AutoMergeSettingsManage),
        can_stop: can(Action::AutoMergeStopToggle),
    }
}

fn stop_body(mark: Option<forge_review::StopMark>) -> Option<AutoMergeStopBody> {
    mark.map(|m| AutoMergeStopBody {
        by: m.by.map(UserId::get),
        at: m.at,
    })
}

fn body(s: forge_review::AutoMergeSettings, viewer: AutoMergeViewerBody) -> AutoMergeBody {
    AutoMergeBody {
        project_id: s.project.get(),
        mode: s.mode.as_str().to_owned(),
        acts: s.mode.acts(),
        mode_by: s.mode_by.map(UserId::get),
        mode_at: s.mode_at,
        project_stop: stop_body(s.project_stop),
        instance_stop: stop_body(s.instance_stop),
        viewer,
    }
}

fn respond(ctx: &crate::Ctx, built: Result<serde_json::Value, ForgeError>) -> Response {
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/auto-merge` → `200`.
pub async fn read(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let project = *w.proof.id();
        let settings = bounded(
            &state,
            "auto_merge_read",
            review(&state)?.auto_merge_settings(project),
        )
        .await?;
        json(body(settings, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

/// `PUT …/auto-merge` → `200`.
pub async fn put(state: AppState, request: Dispatched<ProjectCtx, PutAutoMerge>) -> Response {
    let w = request.into_write();
    let built = async {
        let mode = AutoMergeMode::parse(&w.input.mode)
            .ok_or_else(|| param_invalid("mode", "unknown_value"))?;
        let (_, settings) = review(&state)?
            .set_auto_merge_mode(&request_of(&w), &w.proof, mode)
            .await?;
        json(body(settings, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

async fn set_stop(state: AppState, request: Dispatched<ProjectCtx>, stopped: bool) -> Response {
    let w = request.into_write();
    let built = async {
        let (_, settings) = review(&state)?
            .set_auto_merge_stop(&request_of(&w), &w.proof, stopped)
            .await?;
        json(body(settings, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

/// `POST …/auto-merge/stop` → `200`.
pub async fn stop(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    set_stop(state, request, true).await
}

/// `POST …/auto-merge/resume` → `200`.
pub async fn resume(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    set_stop(state, request, false).await
}

async fn set_instance_stop(
    state: AppState,
    request: Dispatched<InstanceCtx>,
    stopped: bool,
) -> Response {
    let w = request.into_write();
    let built = async {
        let (_, mark) = review(&state)?
            .set_instance_auto_merge_stop(&request_of(&w), &w.proof, stopped)
            .await?;
        json(InstanceAutoMergeBody {
            stop: stop_body(mark),
        })
    }
    .await;
    respond(&w.ctx, built)
}

/// `POST /api/v1/admin/auto-merge/stop` → `200`.
pub async fn instance_stop(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    set_instance_stop(state, request, true).await
}

/// `POST /api/v1/admin/auto-merge/resume` → `200`.
pub async fn instance_resume(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    set_instance_stop(state, request, false).await
}
