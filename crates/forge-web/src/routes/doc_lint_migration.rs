//! A project's document lint migration setting (plan 13 Q13, ADR-0026
//! "決まったこと"): broken document references (`ref_broken`) are errors
//! in `forge/doc-lint`; while the repository migrates its documents a
//! Maintainer may lower them to warnings **until a date**.
//!
//! - `GET /api/v1/projects/{id}/doc-lint/migration` (`ProjectRead`): the
//!   date (or none), whether it is in force, who set it and when, the
//!   database's date (`today`, UTC) and the latest date a new setting may
//!   name (`max_until` = today + 180 days), and `viewer.can_manage` (a
//!   trial `authorize()` of the write, a hint only).
//! - `PUT …/doc-lint/migration` (`DocLintMigrationManage`: people only,
//!   Maintainer): `{"warn_until": "YYYY-MM-DD"}` → `200` the setting.
//!   Not a calendar date: `400 FG-WEB-0006 param=warn_until`; today or
//!   earlier, or more than 180 days ahead (the database's date):
//!   `400 FG-DOC-0005`.
//! - `DELETE …/doc-lint/migration` (`DocLintMigrationManage`): `204`;
//!   `404 FG-REPO-0005` when there is no setting.
//!
//! Past its date a setting stops applying by itself (nothing is written;
//! `in_force: false` until it is set again or cleared).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, ProjectCtx, ResourceCtx};
use forge_checks::{ChecksRequest, DocLintMigrations, MigrationRead};
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` / `PUT` / `DELETE` a project's migration setting.
pub const ONE: &str = "/api/v1/projects/{id}/doc-lint/migration";
/// The body cap (one date).
pub const MAX_BODY: usize = 1024;

/// `PUT …/doc-lint/migration`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutDocLintMigration {
    /// `YYYY-MM-DD`: the last day (UTC) broken references are warnings.
    /// Required; after today and at most 180 days ahead (the database's
    /// date).
    pub warn_until: String,
}

/// What the caller could do now: a trial `authorize()` of the write (a
/// hint for the screen; the route decides again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocLintMigrationViewer {
    /// `PUT` / `DELETE …/doc-lint/migration`.
    pub can_manage: bool,
}

/// A project's migration setting.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocLintMigrationBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// `YYYY-MM-DD`, or `null` when there is no setting.
    #[schema(required = true)]
    pub warn_until: Option<String>,
    /// The date is today or later: broken references are warnings now.
    pub in_force: bool,
    /// Who set the date last (`null` without a setting).
    #[schema(required = true, minimum = 1)]
    pub set_by: Option<i64>,
    /// When (UTC, RFC 3339; `null` without a setting).
    #[schema(required = true)]
    pub set_at: Option<String>,
    /// The database's date (UTC), `YYYY-MM-DD`.
    pub today: String,
    /// The latest date a new setting may name (today + 180 days).
    pub max_until: String,
    pub viewer: DocLintMigrationViewer,
}

fn body(read: MigrationRead, can_manage: bool) -> DocLintMigrationBody {
    let (warn_until, in_force, set_by, set_at) = match read.setting {
        Some(s) => (
            Some(s.warn_until),
            s.in_force,
            Some(s.set_by.get()),
            Some(s.set_at),
        ),
        None => (None, false, None, None),
    };
    DocLintMigrationBody {
        project_id: read.project.get(),
        warn_until,
        in_force,
        set_by,
        set_at,
        today: read.today,
        max_until: read.max_until,
        viewer: DocLintMigrationViewer { can_manage },
    }
}

fn migrations(state: &AppState) -> Result<&DocLintMigrations, ForgeError> {
    state.doc_lint_migrations.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "checks_unconfigured")
    })
}

/// The path's project id.
fn project_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    if tail != "doc-lint/migration" {
        return Err(bad());
    }
    parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)
}

/// `…/doc-lint/migration`: the project.
pub struct MigrationOf;

impl MigrationOf {
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

impl Loader<AppState> for MigrationOf {
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

impl BodyLoader<AppState, PutDocLintMigration> for MigrationOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PutDocLintMigration,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

fn request_of<B>(w: &WriteParts<ProjectCtx, B>) -> ChecksRequest {
    ChecksRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// The trial decision of the write on the loaded project (a hint).
fn can_manage<B>(w: &WriteParts<ProjectCtx, B>) -> bool {
    forge_auth::authorize(
        w.proof.principal(),
        Action::DocLintMigrationManage,
        &w.proof.ctx().resource(),
    )
    .is_ok()
}

/// `GET …/doc-lint/migration` → `200`.
pub async fn read(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let got = bounded(
            &state,
            "doc_lint_migration_read",
            migrations(&state)?.get(&w.proof),
        )
        .await?;
        json(body(got, can_manage(&w)))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT …/doc-lint/migration` → `200`.
pub async fn set(
    state: AppState,
    request: Dispatched<ProjectCtx, PutDocLintMigration>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let (_, stored) = migrations(&state)?
            .set(&request_of(&w), &w.proof, &w.input.warn_until)
            .await?;
        json(body(stored, can_manage(&w)))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE …/doc-lint/migration` → `204`.
pub async fn clear(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let done = async { migrations(&state)?.clear(&request_of(&w), &w.proof).await }.await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_path_names_the_project() {
        assert_eq!(
            project_id("/api/v1/projects/7/doc-lint/migration").expect("one"),
            ProjectId::new(7)
        );
        for bad in [
            "/api/v1/projects/0/doc-lint/migration",
            "/api/v1/projects/x/doc-lint/migration",
            "/api/v1/projects/7/doc-lint/migrationx",
            "/api/v1/projects/7/doc-lint",
        ] {
            assert!(project_id(bad).is_err(), "{bad}");
        }
    }
}
