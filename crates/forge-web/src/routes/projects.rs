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
//! decimal (`0`, `01`, `+1`, `%31`, beyond `i64`) is answered like an
//! absent project without a lookup (logged `lookup=id_param_invalid`) until
//! request validation gets its own code (FG-WEB-0006, phase 8b).
//!
//! The body is [`project_view`], the one builder of a project's API shape
//! (ADR-0015 MR5 amendment): reused by the list and write endpoints of
//! phase 8b. `HEAD` is the `GET` route (axum answers it without the body).

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::request::Parts;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{Authorized, LoadedProject, ProjectCtx, UnresolvedCtx, UnresolvedReason};
use forge_core::{parse_positive_id, ProjectId, ProjectRecord, Role, UserId};
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::dispatch::loaded_project;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup};

/// The route's path (axum style); the loader reads the id from the raw path.
pub const PATH: &str = "/api/v1/projects/{id}";

/// What precedes the id in the raw path.
const PREFIX: &str = "/api/v1/projects/";

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
                return Ok(Lookup::Unresolved {
                    ctx: UnresolvedCtx::new(UnresolvedReason::Absent),
                    why: Some("id_param_invalid"),
                });
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
            })
        })
    }
}

/// forge-auth's loaded project from the one row (id and facts together).
fn loaded(record: &ProjectRecord) -> LoadedProject {
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
async fn load_bounded(
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

/// The API shape of one project for the caller (the single builder):
/// `id`, `namespace_id`, `path` (current full path), `name` (its last
/// segment), `display_name`, `description`, `visibility`, `kind` (the
/// repository kind, `spec` / `system`), `created_at` (UTC, RFC 3339) and
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
    if record.id != *proof.id() {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "project_details_mismatch")
            .with_context("project_id", proof.id().get())
            .with_context("details_project_id", record.id.get()));
    }
    let view = proof.ctx().view();
    let name = record
        .path
        .segments()
        .last()
        .map(|s| s.as_str().to_owned())
        .unwrap_or_default();
    Ok(json!({
        "id": proof.id().get(),
        "namespace_id": record.namespace_id.get(),
        "path": record.path.full_path(),
        "name": name,
        "display_name": record.display_name,
        "description": record.description,
        "visibility": view.visibility.as_str(),
        "kind": record.kind.as_str(),
        "created_at": record.created_at,
        "role": view.role.map(Role::as_str),
    }))
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
