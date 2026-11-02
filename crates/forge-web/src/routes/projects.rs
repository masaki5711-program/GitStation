//! `GET|HEAD /api/v1/projects/{id}` (`ProjectRead`, M1 phase 8 MR5): one
//! project by id, for a caller `authorize()` lets read it.
//!
//! The registry authenticates, then [`ProjectById`] loads the project with
//! **one** `ProjectAccessLoader::load_project` call (one statement in
//! forge-db: the caller's access facts and the details in the same row),
//! then `prove(ProjectRead)` decides. No such id and a project the caller
//! cannot see answer the same `FG-REPO-0005` (only the log `reason`
//! differs: `absent` / `not_visible`); pending deletion is `FG-REPO-0006`
//! to a caller who can see the project and the same `FG-REPO-0005` to
//! anyone else (ADR-0010 matrix rows). An id that is not a plain positive
//! decimal (`0`, `01`, `+1`, `%31`, beyond `i64`) is `FG-WEB-0006
//! reason=param_invalid param=id` (phase 8b MR6), decided from the string
//! alone, after authentication and without a lookup: the same 400 whatever
//! the digits would name.
//!
//! The body is [`project_view`], the one builder of a project's API shape
//! ([`ProjectBody`], ADR-0015 MR5 amendment): reused by the list and write
//! endpoints of phase 8b. `HEAD` is the `GET` route (axum answers it
//! without the body).

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::request::Parts;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{Authorized, LoadedProject, ProjectCtx, UnresolvedCtx, UnresolvedReason};
use forge_core::{parse_positive_id, ProjectId, ProjectRecord, Role, UserId};
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::dispatch::loaded_project;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup};

/// The route's path (axum style); the loader reads the id from the raw path.
pub const PATH: &str = "/api/v1/projects/{id}";

/// What precedes the id in the raw path.
pub(crate) const PREFIX: &str = "/api/v1/projects/";

/// Loads the project named by the path's `{id}` for `ProjectRead`.
pub struct ProjectById;

impl Loader<AppState> for ProjectById {
    type Ctx = ProjectCtx;

    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let Some(id) = id_param(parts.uri.path()) else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            let Some(record) = load_bounded(state, id, viewer).await? else {
                return Ok(Lookup::Unresolved {
                    ctx: UnresolvedCtx::new(UnresolvedReason::Absent),
                    why: None,
                });
            };
            if record.id != id {
                // The loader answered for another project: never decide on it.
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "loaded_another_project")
                    .with_context("project_id", id.get())
                    .with_context("loaded_project_id", record.id.get()));
            }
            Ok(Lookup::Found {
                ctx: ProjectCtx::new(loaded(&record)),
                canonical: Some(record.path.clone()), // clone: the record moves on too
                tombstone: None,
                project: Some(record),
                group: None,
            })
        })
    }
}

/// forge-auth's loaded project from the one row (id and facts together).
pub(crate) fn loaded(record: &ProjectRecord) -> LoadedProject {
    loaded_project(record.id, record.access)
}

/// The id in `/api/v1/projects/{id}` (the raw path: never decoded, so
/// `%31` is not `1`), by the permalink's rule ([`parse_positive_id`]).
pub(crate) fn id_param(raw_path: &str) -> Option<ProjectId> {
    let raw = raw_path.strip_prefix(PREFIX)?;
    parse_positive_id(raw).map(ProjectId::new)
}

/// One loader call, bounded on the client side (forge-db bounds its own
/// statement too): `FG-WEB-0002 operation=project_load` when it does not
/// answer in time.
pub(crate) async fn load_bounded(
    state: &AppState,
    id: ProjectId,
    viewer: Option<UserId>,
) -> Result<Option<ProjectRecord>, ForgeError> {
    let limit = state.lookup_timeout;
    match tokio::time::timeout(limit, state.lookups.projects.load_project(id, viewer)).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", "project_load")
            .with_context("timeout_ms", limit.as_millis())),
    }
}

// Built only by `project_view`; its doc comment is the schema description.
/// A project as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub namespace_id: i64,
    /// The current full path (`group/sub/project`).
    pub path: String,
    /// The path's last segment.
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// `private` or `internal`.
    pub visibility: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// The caller's effective role (`guest` … `owner`); `null` without one
    /// (an administrator, or a reader of an internal project).
    #[schema(required = true)]
    pub role: Option<String>,
}

/// The API shape of one project for the caller (the single builder):
/// `id`, `namespace_id`, `path` (current full path), `name` (its last
/// segment), `display_name`, `description`, `visibility`, `created_at` (UTC, RFC 3339) and
/// `role` (the caller's effective role, `null` without one — an
/// administrator or a reader of an internal project). The id, visibility
/// and role come from the proof; the rest from the same loaded row, whose
/// id must be the proof's (`FG-SYS-0001 reason=project_details_mismatch`).
/// Pending deletion is not a field: `authorize()` refuses `ProjectRead` on
/// such a project. No internal data (no repository path, state or token).
pub fn project_view(
    proof: &Authorized<ProjectCtx>,
    record: &ProjectRecord,
) -> Result<Value, ForgeError> {
    to_json(project_body(proof, record)?)
}

/// [`project_view`] as the typed body (the lists embed it, MR7b).
pub fn project_body(
    proof: &Authorized<ProjectCtx>,
    record: &ProjectRecord,
) -> Result<ProjectBody, ForgeError> {
    if record.id != *proof.id() {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "project_details_mismatch")
            .with_context("project_id", proof.id().get())
            .with_context("details_project_id", record.id.get()));
    }
    let view = proof.ctx().view();
    Ok(body_of(record, view.visibility, view.role))
}

/// The API shape of a project the caller just wrote (phase 8b MR7a): the
/// same shape as [`project_view`], from the row forge-hosting re-read
/// after the commit for the caller (its access facts give visibility and
/// role). Only a handler whose command committed calls it.
pub(crate) fn written_project_view(record: &ProjectRecord) -> Result<Value, ForgeError> {
    to_json(body_of(
        record,
        record.access.visibility,
        record.access.role,
    ))
}

fn to_json(body: ProjectBody) -> Result<Value, ForgeError> {
    serde_json::to_value(body).map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "project_view_serialize")
            .with_source(e)
    })
}

/// The one place the body's fields are filled.
fn body_of(
    record: &ProjectRecord,
    visibility: forge_core::Visibility,
    role: Option<Role>,
) -> ProjectBody {
    let name = record
        .path
        .segments()
        .last()
        .map(|s| s.as_str().to_owned())
        .unwrap_or_default();
    ProjectBody {
        id: record.id.get(),
        namespace_id: record.namespace_id.get(),
        path: record.path.full_path(),
        name,
        display_name: record.display_name.clone(), // clone: the record is borrowed
        description: record.description.clone(),
        visibility: visibility.as_str().to_owned(),
        created_at: record.created_at.clone(),
        role: role.map(|r| Role::as_str(r).to_owned()),
    }
}

/// `GET|HEAD /api/v1/projects/{id}`: the project's view, never cached by a
/// shared cache (it depends on who asks).
pub async fn read(_state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let built = match request.project() {
        Some(record) => project_view(request.proof(), record),
        None => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "project_details_missing")
            .with_context("project_id", request.proof().id().get())),
    };
    let body = match built {
        Ok(body) => body,
        Err(e) => return request.ctx().fail(e).into_response(),
    };
    let mut res = Json(body).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}

#[cfg(test)]
#[path = "projects_tests.rs"]
mod tests;
