//! Groups (M1 phase 8b MR7a): `POST /api/v1/groups` (`GroupCreateTopLevel`,
//! administrators), `POST /api/v1/groups/{id}/subgroups` (`SubgroupCreate`,
//! group Owner), `GET /api/v1/groups/{id}` (`GroupRead`, every signed-in
//! user) and `PATCH /api/v1/groups/{id}` (`GroupUpdate`, group Owner).
//!
//! The registry authenticates, reads the strict JSON body (write routes),
//! loads the group with **one** `forge_hosting::Hosting::group` call (the
//! caller's role and the details in one row) and proves the action. A write
//! handler then passes the proof to forge-hosting, whose command decides
//! again inside its transaction on the roles it locks (no time-of-check /
//! time-of-use gap); the response is the group re-read for the caller after
//! the commit. An id that is not a plain positive decimal is `FG-WEB-0006
//! reason=param_invalid param=id`, an unknown group the same `FG-REPO-0005`
//! as everywhere else.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Authorized, GroupCtx, InstanceCtx, LoadedGroup, ResourceCtx};
use forge_core::{GroupId, GroupRecord, Role, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{NewGroupFields, Patch};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::write::{absent, bounded, hosting, id_in, json, write_request};

/// `POST` (top-level create).
pub const COLLECTION: &str = "/api/v1/groups";
/// `GET` / `PATCH` one group.
pub const ONE: &str = "/api/v1/groups/{id}";
/// `POST` a subgroup.
pub const SUBGROUPS: &str = "/api/v1/groups/{id}/subgroups";

const PREFIX: &str = "/api/v1/groups/";

/// The body cap of the group routes (names and a description ≤ 2000
/// characters fit with room to spare).
pub const MAX_BODY: usize = 16 * 1024;

/// `POST /api/v1/groups` and `…/{id}/subgroups`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateGroup {
    /// The path segment (ADR-0015 rules; a top-level name must not be
    /// reserved).
    pub path: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `PATCH /api/v1/groups/{id}`: at least one field.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateGroup {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

// Built only by `group_view`; its doc comment is the schema description.
/// A group as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub namespace_id: i64,
    /// The parent group; `null` at the top level.
    #[schema(required = true, minimum = 1)]
    pub parent_id: Option<i64>,
    /// The current full path (`group/sub`).
    pub path: String,
    /// The path's last segment.
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// The caller's effective role (`guest` … `owner`); `null` without one.
    #[schema(required = true)]
    pub role: Option<String>,
}

/// The API shape of one group for the caller (the single builder).
fn group_view(record: &GroupRecord) -> Result<Value, ForgeError> {
    json(GroupBody {
        id: record.id.get(),
        namespace_id: record.namespace_id.get(),
        parent_id: record.parent_id.map(GroupId::get),
        path: record.path.full_path(),
        name: record
            .path
            .segments()
            .last()
            .map(|s| s.as_str().to_owned())
            .unwrap_or_default(),
        display_name: record.display_name.clone(), // clone: the record is borrowed
        description: record.description.clone(),
        created_at: record.created_at.clone(),
        role: record.role.map(|r| Role::as_str(r).to_owned()),
    })
}

/// The group id of `/api/v1/groups/{id}` (`suffix` = "") or
/// `/api/v1/groups/{id}/subgroups`.
fn group_id(parts: &Parts, suffix: &str) -> Result<GroupId, ForgeError> {
    id_in(parts.uri.path(), PREFIX, suffix)
        .map(GroupId::new)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))
}

/// Load the group for `viewer` (one bounded call) into the lookup.
async fn load_group(
    state: &AppState,
    id: GroupId,
    viewer: Option<UserId>,
) -> Result<Lookup<GroupCtx>, ForgeError> {
    let hosting = hosting(state)?;
    let Some(record) = bounded(state, "group_load", hosting.group(id, viewer)).await? else {
        return Ok(absent());
    };
    if record.id != id {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_group")
            .with_context("group_id", id.get())
            .with_context("loaded_group_id", record.id.get()));
    }
    Ok(Lookup::Found {
        ctx: GroupCtx::new(LoadedGroup {
            id: record.id,
            role: record.role,
        }),
        canonical: Some(record.path.clone()), // clone: the record moves on too
        tombstone: None,
        project: None,
        group: Some(Box::new(record)),
    })
}

/// `GET /api/v1/groups/{id}`.
pub struct GroupById;

impl Loader<AppState> for GroupById {
    type Ctx = GroupCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupCtx> {
        Box::pin(async move { load_group(state, group_id(parts, "")?, viewer).await })
    }
}

/// `PATCH /api/v1/groups/{id}`.
pub struct GroupToUpdate;

impl BodyLoader<AppState, UpdateGroup> for GroupToUpdate {
    type Ctx = GroupCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a UpdateGroup,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupCtx> {
        Box::pin(async move { load_group(state, group_id(parts, "")?, viewer).await })
    }
}

/// `POST /api/v1/groups/{id}/subgroups`: the parent group.
pub struct ParentGroup;

impl BodyLoader<AppState, CreateGroup> for ParentGroup {
    type Ctx = GroupCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a CreateGroup,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupCtx> {
        Box::pin(async move { load_group(state, group_id(parts, "/subgroups")?, viewer).await })
    }
}

/// `POST /api/v1/groups`: decided on the instance (no lookup).
pub struct TopLevel;

impl BodyLoader<AppState, CreateGroup> for TopLevel {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a CreateGroup,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async {
            Ok(Lookup::Found {
                ctx: InstanceCtx,
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

fn fields(body: &CreateGroup) -> NewGroupFields {
    NewGroupFields {
        path: body.path.clone(), // clone: the body stays with the request
        display_name: body.display_name.clone(),
        description: body.description.clone().unwrap_or_default(),
    }
}

/// The group `id` re-read for the caller after a committed write.
async fn written<R: ResourceCtx>(
    state: &AppState,
    proof: &Authorized<R>,
    id: GroupId,
) -> Result<Value, ForgeError> {
    let hosting = hosting(state)?;
    let record = bounded(
        state,
        "group_load",
        hosting.group(id, proof.principal().user()),
    )
    .await?
    .ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "written_group_missing")
            .with_context("group_id", id.get())
    })?;
    group_view(&record)
}

fn respond(
    request_ctx: &crate::Ctx,
    status: StatusCode,
    body: Result<Value, ForgeError>,
) -> Response {
    match body {
        Ok(body) => crate::routes::write::no_store(status, body),
        Err(e) => request_ctx.fail(e).into_response(),
    }
}

/// `POST /api/v1/groups` → `201` the new group.
pub async fn create(state: AppState, request: Dispatched<InstanceCtx, CreateGroup>) -> Response {
    let w = request.into_write();
    let built = async {
        let created = hosting(&state)?
            .create_group(&write_request(&w), &w.proof, &fields(&w.input))
            .await?;
        written(&state, &w.proof, created.group_id).await
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `POST /api/v1/groups/{id}/subgroups` → `201` the new group.
pub async fn create_subgroup(
    state: AppState,
    request: Dispatched<GroupCtx, CreateGroup>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let created = hosting(&state)?
            .create_subgroup(&write_request(&w), &w.proof, &fields(&w.input))
            .await?;
        written(&state, &w.proof, created.group_id).await
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `GET /api/v1/groups/{id}` → `200` the group.
pub async fn read(_state: AppState, request: Dispatched<GroupCtx>) -> Response {
    let built = match request.group() {
        Some(record) if record.id == *request.proof().id() => group_view(record),
        Some(_) | None => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "group_details_missing")
            .with_context("group_id", request.proof().id().get())),
    };
    respond(request.ctx(), StatusCode::OK, built)
}

/// `PATCH /api/v1/groups/{id}` → `200` the group as it is now (also when
/// nothing changed).
pub async fn update(state: AppState, request: Dispatched<GroupCtx, UpdateGroup>) -> Response {
    let w = request.into_write();
    let built = async {
        let patch = Patch {
            display_name: w.input.display_name.clone(), // clone: the body stays with the request
            description: w.input.description.clone(),
        };
        hosting(&state)?
            .update_group(&write_request(&w), &w.proof, &patch)
            .await?;
        written(&state, &w.proof, *w.proof.id()).await
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}
