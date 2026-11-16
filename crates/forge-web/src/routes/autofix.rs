//! A project's deterministic autofix switch (M4 C14, ADR-0017 Amendment
//! C14):
//!
//! - `GET /api/v1/projects/{id}/autofix` (`ProjectRead`): `enabled` (off by
//!   default), who changed it last and when, and `viewer.can_change` (a
//!   trial decision: a hint only).
//! - `PUT …/autofix` (`AutofixSettingsManage`: people only, Maintainer,
//!   `api`): `{"enabled": true | false}` → `200`; audited
//!   (`autofix.settings_changed`). Turning it on makes the caller the
//!   enabler whose role every push of the bot is decided with.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, ProjectCtx, ResourceCtx};
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{Review, ReviewRequest};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` / `PUT` a project's autofix switch.
pub const ONE: &str = "/api/v1/projects/{id}/autofix";
/// The body cap (one flag).
pub const MAX_BODY: usize = 128;

/// `PUT …/autofix`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutAutofix {
    pub enabled: bool,
}

/// What the caller could do now (a hint; the route decides again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutofixViewerBody {
    pub can_change: bool,
}

/// A project's autofix switch.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutofixBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// Off by default (no project fixes itself until a Maintainer turns it
    /// on).
    pub enabled: bool,
    /// Who changed it last: the enabler (`null`: never set, or the caller
    /// is no Maintainer — only one who may change it sees who did).
    #[schema(required = true, minimum = 1)]
    pub changed_by: Option<i64>,
    /// UTC, RFC 3339.
    #[schema(required = true)]
    pub changed_at: Option<String>,
    pub viewer: AutofixViewerBody,
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
    if tail != "autofix" {
        return Err(bad());
    }
    parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)
}

/// The project of an autofix route.
pub struct AutofixOf;

impl AutofixOf {
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

impl Loader<AppState> for AutofixOf {
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

impl BodyLoader<AppState, PutAutofix> for AutofixOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PutAutofix,
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

fn body<B>(s: forge_review::AutofixSettings, w: &WriteParts<ProjectCtx, B>) -> AutofixBody {
    let can_change = forge_auth::authorize(
        w.proof.principal(),
        Action::AutofixSettingsManage,
        &w.proof.ctx().resource(),
    )
    .is_ok();
    AutofixBody {
        project_id: s.project.get(),
        enabled: s.enabled,
        // Review (C14, LOW): who the enabler is is a Maintainer's to see.
        changed_by: s.changed_by.filter(|_| can_change).map(UserId::get),
        changed_at: s.changed_at,
        viewer: AutofixViewerBody { can_change },
    }
}

fn respond(ctx: &crate::Ctx, built: Result<serde_json::Value, ForgeError>) -> Response {
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/autofix` → `200`.
pub async fn read(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let project = *w.proof.id();
        let settings = bounded(
            &state,
            "autofix_read",
            review(&state)?.autofix_settings(project),
        )
        .await?;
        json(body(settings, &w))
    }
    .await;
    respond(&w.ctx, built)
}

/// `PUT …/autofix` → `200`.
pub async fn put(state: AppState, request: Dispatched<ProjectCtx, PutAutofix>) -> Response {
    let w = request.into_write();
    let built = async {
        let (_, settings) = review(&state)?
            .set_autofix_enabled(&request_of(&w), &w.proof, w.input.enabled)
            .await?;
        json(body(settings, &w))
    }
    .await;
    respond(&w.ctx, built)
}
