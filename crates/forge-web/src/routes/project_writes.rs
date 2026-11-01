//! Project writes (M1 phase 8b MR7a): `POST /api/v1/projects`
//! (`ProjectCreate`; the parent namespace in the body), `PATCH
//! /api/v1/projects/{id}` (`ProjectUpdate`, Maintainer) and `PUT
//! /api/v1/projects/{id}/visibility` (`ProjectVisibilityChange`, Owner).
//!
//! As for groups: the registry authenticates, reads the strict body, loads
//! (the parent with `forge_hosting::Hosting::parent`, a project with the
//! one `ProjectAccessLoader` call of `GET /api/v1/projects/{id}`) and
//! proves; forge-hosting decides again inside the write transaction. A
//! parent that is absent, or is a project namespace, is the same
//! `FG-REPO-0005` (projects live under a user or a group). The answer is
//! the project re-read for the caller after the commit ([`ProjectBody`]
//! through the one builder).
//!
//! [`ProjectBody`]: crate::routes::projects::ProjectBody

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    Authorized, LoadedGroup, LoadedParent, LoadedUserNamespace, ParentCtx, ProjectCtx, ResourceCtx,
    UnresolvedCtx, UnresolvedReason, VisibilityChangeCtx,
};
use forge_core::{NamespaceId, ProjectId, RepositoryKind, UserId, Visibility};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{NewProjectFields, Parent as HostedParent, Patch};
use serde::Deserialize;
use serde_json::Value;

use crate::app::AppState;
use crate::body::{param_invalid, request_invalid};
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::projects::{self, written_project_view, ProjectById, PREFIX};
use crate::routes::write::{absent, bounded, hosting, id_in, no_store, write_request};

/// `POST` a project.
pub const COLLECTION: &str = "/api/v1/projects";
/// `PUT` a project's visibility.
pub const VISIBILITY: &str = "/api/v1/projects/{id}/visibility";

/// The body cap of the project write routes.
pub const MAX_BODY: usize = 16 * 1024;

/// A project's visibility in a request.
#[derive(Debug, Clone, Copy, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum VisibilityName {
    Private,
    Internal,
}

impl VisibilityName {
    fn visibility(self) -> Visibility {
        match self {
            Self::Private => Visibility::Private,
            Self::Internal => Visibility::Internal,
        }
    }
}

/// A repository kind in a request (ADR-0004; fixed at creation).
#[derive(Debug, Clone, Copy, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum KindName {
    Spec,
    System,
}

/// `POST /api/v1/projects`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    /// The parent: a personal or a group namespace (`namespace_id` of a
    /// user, of `GET /api/v1/groups/{id}` or of the resolver).
    #[schema(minimum = 1)]
    pub namespace_id: i64,
    /// The path segment (ADR-0015 rules).
    pub path: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Default `private`.
    #[serde(default)]
    pub visibility: Option<VisibilityName>,
    /// Default `system`.
    #[serde(default)]
    pub kind: Option<KindName>,
}

/// `PATCH /api/v1/projects/{id}`: at least one field.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateProject {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// `PUT /api/v1/projects/{id}/visibility`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetVisibility {
    pub visibility: VisibilityName,
}

/// `POST /api/v1/projects`: the parent namespace named by the body.
pub struct Parent;

impl BodyLoader<AppState, CreateProject> for Parent {
    type Ctx = ParentCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        _: &'a Parts,
        body: &'a CreateProject,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ParentCtx> {
        Box::pin(async move {
            if body.namespace_id < 1 {
                return Err(request_invalid("field_invalid")
                    .with_context("field", "namespace_id")
                    .with_context("rule", "not_positive"));
            }
            let namespace = NamespaceId::new(body.namespace_id);
            let hosting = hosting(state)?;
            let parent = bounded(state, "parent_load", hosting.parent(namespace, viewer)).await?;
            let loaded = match parent {
                None => return Ok(absent()),
                // A project is never a parent: the same 404 as an absent id.
                Some(HostedParent::Project) => {
                    return Ok(Lookup::Unresolved {
                        ctx: UnresolvedCtx::new(UnresolvedReason::KindMismatch),
                        why: Some("parent_is_project"),
                    })
                }
                Some(HostedParent::Group {
                    namespace,
                    group,
                    role,
                }) => LoadedParent::Group {
                    namespace,
                    group: LoadedGroup { id: group, role },
                },
                Some(HostedParent::User { namespace, owner }) => {
                    LoadedParent::User(LoadedUserNamespace { namespace, owner })
                }
            };
            let ctx = ParentCtx::new(loaded);
            if ctx.id() != namespace {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "loaded_another_namespace")
                    .with_context("namespace_id", namespace.get()));
            }
            Ok(Lookup::Found {
                ctx,
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `PATCH /api/v1/projects/{id}`: the project by id, exactly as `GET`
/// loads it.
pub struct ProjectToUpdate;

impl BodyLoader<AppState, UpdateProject> for ProjectToUpdate {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a UpdateProject,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        ProjectById.load(state, parts, viewer)
    }
}

/// `PUT /api/v1/projects/{id}/visibility`: the project by id and the
/// requested visibility.
pub struct VisibilityTarget;

impl BodyLoader<AppState, SetVisibility> for VisibilityTarget {
    type Ctx = VisibilityChangeCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a SetVisibility,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, VisibilityChangeCtx> {
        Box::pin(async move {
            let Some(id) = id_in(parts.uri.path(), PREFIX, "/visibility").map(ProjectId::new)
            else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
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
                ctx: VisibilityChangeCtx::new(
                    projects::loaded(&record),
                    body.visibility.visibility(),
                ),
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// The project `id` re-read for the caller after a committed write.
async fn written<R: ResourceCtx>(
    state: &AppState,
    proof: &Authorized<R>,
    id: ProjectId,
) -> Result<Value, ForgeError> {
    let hosting = hosting(state)?;
    let record = bounded(
        state,
        "project_load",
        hosting.project(id, proof.principal().user()),
    )
    .await?
    .ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "written_project_missing")
            .with_context("project_id", id.get())
    })?;
    written_project_view(&record)
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `POST /api/v1/projects` → `201` the new project.
pub async fn create(state: AppState, request: Dispatched<ParentCtx, CreateProject>) -> Response {
    let w = request.into_write();
    let built = async {
        let input = &w.input;
        let fields = NewProjectFields {
            path: input.path.clone(), // clone: the body stays with the request
            display_name: input.display_name.clone(),
            description: input.description.clone().unwrap_or_default(),
            visibility: input
                .visibility
                .map_or(Visibility::Private, VisibilityName::visibility),
            kind: match input.kind {
                Some(KindName::Spec) => RepositoryKind::Spec,
                Some(KindName::System) | None => RepositoryKind::System,
            },
        };
        let created = hosting(&state)?
            .create_project(&write_request(&w), &w.proof, &fields)
            .await?;
        written(&state, &w.proof, created.project_id).await
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `PATCH /api/v1/projects/{id}` → `200` the project as it is now (also
/// when nothing changed).
pub async fn update(state: AppState, request: Dispatched<ProjectCtx, UpdateProject>) -> Response {
    let w = request.into_write();
    let built = async {
        let patch = Patch {
            display_name: w.input.display_name.clone(), // clone: the body stays with the request
            description: w.input.description.clone(),
        };
        hosting(&state)?
            .update_project(&write_request(&w), &w.proof, &patch)
            .await?;
        written(&state, &w.proof, *w.proof.id()).await
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PUT /api/v1/projects/{id}/visibility` → `200` the project as it is now
/// (also when it already had that visibility).
pub async fn set_visibility(
    state: AppState,
    request: Dispatched<VisibilityChangeCtx, SetVisibility>,
) -> Response {
    let w = request.into_write();
    let built = async {
        hosting(&state)?
            .change_visibility(&write_request(&w), &w.proof)
            .await?;
        written(&state, &w.proof, *w.proof.id()).await
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}
