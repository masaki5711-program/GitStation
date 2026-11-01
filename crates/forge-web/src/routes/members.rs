//! Direct members of projects and groups (M1 phase 8b MR8):
//! `GET|POST /api/v1/projects/{id}/members`,
//! `PATCH|DELETE /api/v1/projects/{id}/members/{user_id}` and the same for
//! `/api/v1/groups/{id}/members…`. Reading is `MemberList` (a project's as
//! `ProjectRead`, a group's for every signed-in user); every change is
//! `MemberManage`.
//!
//! As for the MR7a writes: the registry authenticates, reads the strict
//! body, loads (the project or group with the one loader call of its `GET`
//! route, and — for a change or a removal — the member's **direct** role,
//! never the request's) and proves; forge-hosting decides again inside the
//! write transaction on the member's role read under the lock and on
//! whether the change would leave the project or group without an Owner
//! (computed there, so the registry's decision always says "not the last
//! Owner"). A user who is not a direct member is decided as
//! `MembershipChange::Missing` (Maintainer needed) and then answered
//! `FG-REPO-0010 not_member`, so only a member manager learns it. Agents are
//! refused every change (`FG-AUTH-0003 reason=agent_token`, ADR-0017
//! amendment MR8). The answer is the member re-read after the commit
//! (`204` for a removal).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    GroupCtx, GroupMemberCtx, LoadedGroup, LoadedMembership, MembershipChange, ProjectCtx,
    ProjectMemberCtx,
};
use forge_core::{parse_positive_id, GroupId, ProjectId, Role, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{MemberChange, MemberRow, MAX_MEMBER_PAGE};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::{param_invalid, request_invalid};
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::projects::{self, PREFIX as PROJECT_PREFIX};
use crate::routes::write::{absent, bounded, hosting, id_in, json, no_store, write_request};

/// `GET` / `POST` a project's members.
pub const PROJECT_MEMBERS: &str = "/api/v1/projects/{id}/members";
/// `PATCH` / `DELETE` one project member.
pub const PROJECT_MEMBER: &str = "/api/v1/projects/{id}/members/{user_id}";
/// `GET` / `POST` a group's members.
pub const GROUP_MEMBERS: &str = "/api/v1/groups/{id}/members";
/// `PATCH` / `DELETE` one group member.
pub const GROUP_MEMBER: &str = "/api/v1/groups/{id}/members/{user_id}";

const GROUP_PREFIX: &str = "/api/v1/groups/";

/// The body cap of the member routes (a user id and a role).
pub const MAX_BODY: usize = 1024;

/// Members per page when `limit` is absent.
pub const DEFAULT_LIMIT: usize = 20;

/// The query parameters of the member lists (`openapi.json`).
pub const QUERY: &[QueryParam] = &[
    QueryParam {
        name: "limit",
        description: "Members per page, 1..=100 (default 20)",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "after",
        description: "The previous page's next_after (a user id): members with a larger user id",
        integer_range: Some((1, i64::MAX)),
    },
];

/// A role in a request.
#[derive(Debug, Clone, Copy, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RoleName {
    Guest,
    Reporter,
    Developer,
    Maintainer,
    Owner,
}

impl RoleName {
    pub(crate) fn role(self) -> Role {
        match self {
            Self::Guest => Role::Guest,
            Self::Reporter => Role::Reporter,
            Self::Developer => Role::Developer,
            Self::Maintainer => Role::Maintainer,
            Self::Owner => Role::Owner,
        }
    }
}

/// `POST …/members`: make a user a direct member.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddMember {
    #[schema(minimum = 1)]
    pub user_id: i64,
    pub role: RoleName,
}

/// `PATCH …/members/{user_id}`: the member's new direct role.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeMember {
    pub role: RoleName,
}

// Built only by `member_view`; its doc comment is the schema description.
/// One direct member as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MemberBody {
    #[schema(minimum = 1)]
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    /// The direct role (`guest` … `owner`); inherited roles are not listed.
    pub role: String,
    /// `false` while the account is disabled (it holds no role meanwhile).
    pub active: bool,
    /// When the membership was created (UTC, RFC 3339).
    pub created_at: String,
}

/// One page of direct members, in user id order.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MemberPage {
    pub members: Vec<MemberBody>,
    /// Pass as `after` for the next page; `null` on the last page.
    #[schema(required = true, minimum = 1)]
    pub next_after: Option<i64>,
}

fn member_body(row: &MemberRow) -> MemberBody {
    MemberBody {
        user_id: row.user_id.get(),
        username: row.username.clone(), // clone: the row is borrowed
        display_name: row.display_name.clone(),
        role: row.role.as_str().to_owned(),
        active: row.active,
        created_at: row.created_at.clone(),
    }
}

/// The two ids of `{prefix}{id}/members/{user_id}` (raw path, never
/// decoded): `FG-WEB-0006 reason=param_invalid` naming the bad one.
fn ids(raw_path: &str, prefix: &str) -> Result<(i64, UserId), ForgeError> {
    let rest = raw_path
        .strip_prefix(prefix)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let (id, user) = rest
        .split_once("/members/")
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    if user.contains('/') {
        return Err(param_invalid("user_id", "not_positive_decimal"));
    }
    let user =
        parse_positive_id(user).ok_or_else(|| param_invalid("user_id", "not_positive_decimal"))?;
    Ok((id, UserId::new(user)))
}

fn collection_id(raw_path: &str, prefix: &str) -> Result<i64, ForgeError> {
    id_in(raw_path, prefix, "/members").ok_or_else(|| param_invalid("id", "not_positive_decimal"))
}

// ── loaders ──

/// The project of `id` for `viewer` (`None` = absent), checked to be it.
async fn project_of(
    state: &AppState,
    id: ProjectId,
    viewer: Option<UserId>,
) -> Result<Option<forge_auth::LoadedProject>, ForgeError> {
    let Some(record) = projects::load_bounded(state, id, viewer).await? else {
        return Ok(None);
    };
    if record.id != id {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_project")
            .with_context("project_id", id.get())
            .with_context("loaded_project_id", record.id.get()));
    }
    Ok(Some(projects::loaded(&record)))
}

/// The group of `id` for `viewer` (`None` = absent), checked to be it.
async fn group_of(
    state: &AppState,
    id: GroupId,
    viewer: Option<UserId>,
) -> Result<Option<LoadedGroup>, ForgeError> {
    let hosting = hosting(state)?;
    let Some(record) = bounded(state, "group_load", hosting.group(id, viewer)).await? else {
        return Ok(None);
    };
    if record.id != id {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_group")
            .with_context("group_id", id.get())
            .with_context("loaded_group_id", record.id.get()));
    }
    Ok(Some(LoadedGroup {
        id: record.id,
        role: record.role,
    }))
}

fn found<R>(ctx: R) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
    }
}

/// What a change or removal does to the member's loaded direct role.
#[derive(Debug, Clone, Copy)]
enum Intent {
    Change(Role),
    Remove,
}

impl Intent {
    fn on(self, from: Option<Role>) -> MembershipChange {
        match (self, from) {
            (Intent::Change(to), Some(from)) => MembershipChange::Change { from, to },
            (Intent::Change(to), None) => MembershipChange::Missing { to: Some(to) },
            (Intent::Remove, Some(from)) => MembershipChange::Remove { from },
            (Intent::Remove, None) => MembershipChange::Missing { to: None },
        }
    }
}

fn membership(member: UserId, change: MembershipChange) -> LoadedMembership {
    // The last-owner fact is computed inside the write transaction (the
    // decision that binds the write); the registry's never claims it.
    LoadedMembership {
        member,
        change,
        removes_last_owner: false,
    }
}

async fn project_member_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
    intent: Intent,
) -> Result<Lookup<ProjectMemberCtx>, ForgeError> {
    let (id, member) = ids(parts.uri.path(), PROJECT_PREFIX)?;
    let id = ProjectId::new(id);
    let Some(project) = project_of(state, id, viewer).await? else {
        return Ok(absent());
    };
    let hosting = hosting(state)?;
    let from = bounded(
        state,
        "member_role_load",
        hosting.project_member_role(id, member),
    )
    .await?;
    Ok(found(ProjectMemberCtx::new(
        project,
        membership(member, intent.on(from)),
    )))
}

async fn group_member_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
    intent: Intent,
) -> Result<Lookup<GroupMemberCtx>, ForgeError> {
    let (id, member) = ids(parts.uri.path(), GROUP_PREFIX)?;
    let id = GroupId::new(id);
    let Some(group) = group_of(state, id, viewer).await? else {
        return Ok(absent());
    };
    let hosting = hosting(state)?;
    let from = bounded(
        state,
        "member_role_load",
        hosting.group_member_role(id, member),
    )
    .await?;
    Ok(found(GroupMemberCtx::new(
        group,
        membership(member, intent.on(from)),
    )))
}

fn new_member(body: &AddMember) -> Result<(UserId, MembershipChange), ForgeError> {
    if body.user_id < 1 {
        return Err(request_invalid("field_invalid")
            .with_context("field", "user_id")
            .with_context("rule", "not_positive"));
    }
    Ok((
        UserId::new(body.user_id),
        MembershipChange::Add(body.role.role()),
    ))
}

/// `GET /api/v1/projects/{id}/members`: the project.
pub struct ProjectMembersOf;

impl Loader<AppState> for ProjectMembersOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let id = ProjectId::new(collection_id(parts.uri.path(), PROJECT_PREFIX)?);
            Ok(match project_of(state, id, viewer).await? {
                Some(project) => found(ProjectCtx::new(project)),
                None => absent(),
            })
        })
    }
}

/// `POST /api/v1/projects/{id}/members`: the project and the new member.
pub struct NewProjectMember;

impl BodyLoader<AppState, AddMember> for NewProjectMember {
    type Ctx = ProjectMemberCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a AddMember,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectMemberCtx> {
        Box::pin(async move {
            let id = ProjectId::new(collection_id(parts.uri.path(), PROJECT_PREFIX)?);
            let (member, change) = new_member(body)?;
            Ok(match project_of(state, id, viewer).await? {
                Some(project) => found(ProjectMemberCtx::new(project, membership(member, change))),
                None => absent(),
            })
        })
    }
}

/// `PATCH /api/v1/projects/{id}/members/{user_id}`.
pub struct ProjectMemberToChange;

impl BodyLoader<AppState, ChangeMember> for ProjectMemberToChange {
    type Ctx = ProjectMemberCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a ChangeMember,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectMemberCtx> {
        Box::pin(project_member_lookup(
            state,
            parts,
            viewer,
            Intent::Change(body.role.role()),
        ))
    }
}

/// `DELETE /api/v1/projects/{id}/members/{user_id}`.
pub struct ProjectMemberToRemove;

impl Loader<AppState> for ProjectMemberToRemove {
    type Ctx = ProjectMemberCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectMemberCtx> {
        Box::pin(project_member_lookup(state, parts, viewer, Intent::Remove))
    }
}

/// `GET /api/v1/groups/{id}/members`: the group.
pub struct GroupMembersOf;

impl Loader<AppState> for GroupMembersOf {
    type Ctx = GroupCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupCtx> {
        Box::pin(async move {
            let id = GroupId::new(collection_id(parts.uri.path(), GROUP_PREFIX)?);
            Ok(match group_of(state, id, viewer).await? {
                Some(group) => found(GroupCtx::new(group)),
                None => absent(),
            })
        })
    }
}

/// `POST /api/v1/groups/{id}/members`.
pub struct NewGroupMember;

impl BodyLoader<AppState, AddMember> for NewGroupMember {
    type Ctx = GroupMemberCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a AddMember,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupMemberCtx> {
        Box::pin(async move {
            let id = GroupId::new(collection_id(parts.uri.path(), GROUP_PREFIX)?);
            let (member, change) = new_member(body)?;
            Ok(match group_of(state, id, viewer).await? {
                Some(group) => found(GroupMemberCtx::new(group, membership(member, change))),
                None => absent(),
            })
        })
    }
}

/// `PATCH /api/v1/groups/{id}/members/{user_id}`.
pub struct GroupMemberToChange;

impl BodyLoader<AppState, ChangeMember> for GroupMemberToChange {
    type Ctx = GroupMemberCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a ChangeMember,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupMemberCtx> {
        Box::pin(group_member_lookup(
            state,
            parts,
            viewer,
            Intent::Change(body.role.role()),
        ))
    }
}

/// `DELETE /api/v1/groups/{id}/members/{user_id}`.
pub struct GroupMemberToRemove;

impl Loader<AppState> for GroupMemberToRemove {
    type Ctx = GroupMemberCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupMemberCtx> {
        Box::pin(group_member_lookup(state, parts, viewer, Intent::Remove))
    }
}

// ── handlers ──

/// `limit` and `after`, strictly (as the project list's query):
/// `FG-WEB-0006 reason=param_invalid`.
fn page_query(query: Option<&str>) -> Result<(usize, Option<UserId>), ForgeError> {
    let mut limit: Option<&str> = None;
    let mut after: Option<&str> = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "limit" => (&mut limit, "limit"),
            "after" => (&mut after, "after"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_MEMBER_PAGE)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    let after = match after {
        None => None,
        Some(raw) => {
            Some(UserId::new(parse_positive_id(raw).ok_or_else(|| {
                param_invalid("after", "not_positive_decimal")
            })?))
        }
    };
    Ok((limit, after))
}

fn page_view(rows: &[MemberRow], limit: usize) -> Result<Value, ForgeError> {
    let next_after = if rows.len() == limit {
        rows.last().map(|r| r.user_id.get())
    } else {
        None
    };
    json(MemberPage {
        members: rows.iter().map(member_body).collect(),
        next_after,
    })
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

fn respond_empty(ctx: &crate::Ctx, done: Result<(), ForgeError>) -> Response {
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// The member re-read after a committed write.
fn written(row: Option<MemberRow>, member: UserId) -> Result<Value, ForgeError> {
    let row = row.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "written_member_missing")
            .with_context("user", member.get())
    })?;
    json(member_body(&row))
}

/// `GET /api/v1/projects/{id}/members` → `200` one page.
pub async fn list_project(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let query = page_query(request.raw_query());
    let id = *request.proof().id();
    let built = async {
        let (limit, after) = query?;
        let rows = bounded(
            &state,
            "member_page",
            hosting(&state)?.project_members(id, after, limit),
        )
        .await?;
        page_view(&rows, limit)
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `GET /api/v1/groups/{id}/members` → `200` one page.
pub async fn list_group(state: AppState, request: Dispatched<GroupCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let query = page_query(request.raw_query());
    let id = *request.proof().id();
    let built = async {
        let (limit, after) = query?;
        let rows = bounded(
            &state,
            "member_page",
            hosting(&state)?.group_members(id, after, limit),
        )
        .await?;
        page_view(&rows, limit)
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `POST /api/v1/projects/{id}/members` → `201` the member.
pub async fn add_project(
    state: AppState,
    request: Dispatched<ProjectMemberCtx, AddMember>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let hosting = hosting(&state)?;
        hosting
            .add_project_member(&write_request(&w), &w.proof)
            .await?;
        let (project, member) = *w.proof.id();
        let row = bounded(
            &state,
            "member_load",
            hosting.project_member(project, member),
        )
        .await?;
        written(row, member)
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `PATCH /api/v1/projects/{id}/members/{user_id}` → `200` the member as
/// it is now (also when the role was already that).
pub async fn change_project(
    state: AppState,
    request: Dispatched<ProjectMemberCtx, ChangeMember>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let hosting = hosting(&state)?;
        // Changed or already that role: both answer the member as it is.
        let (MemberChange::Changed { .. } | MemberChange::Unchanged) = hosting
            .change_project_member(&write_request(&w), &w.proof)
            .await?;
        let (project, member) = *w.proof.id();
        let row = bounded(
            &state,
            "member_load",
            hosting.project_member(project, member),
        )
        .await?;
        written(row, member)
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `DELETE /api/v1/projects/{id}/members/{user_id}` → `204`.
pub async fn remove_project(state: AppState, request: Dispatched<ProjectMemberCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .remove_project_member(&write_request(&w), &w.proof)
            .await
    }
    .await;
    respond_empty(&w.ctx, done)
}

/// `POST /api/v1/groups/{id}/members` → `201` the member.
pub async fn add_group(
    state: AppState,
    request: Dispatched<GroupMemberCtx, AddMember>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let hosting = hosting(&state)?;
        hosting
            .add_group_member(&write_request(&w), &w.proof)
            .await?;
        let (group, member) = *w.proof.id();
        let row = bounded(&state, "member_load", hosting.group_member(group, member)).await?;
        written(row, member)
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `PATCH /api/v1/groups/{id}/members/{user_id}` → `200` the member.
pub async fn change_group(
    state: AppState,
    request: Dispatched<GroupMemberCtx, ChangeMember>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let hosting = hosting(&state)?;
        // Changed or already that role: both answer the member as it is.
        let (MemberChange::Changed { .. } | MemberChange::Unchanged) = hosting
            .change_group_member(&write_request(&w), &w.proof)
            .await?;
        let (group, member) = *w.proof.id();
        let row = bounded(&state, "member_load", hosting.group_member(group, member)).await?;
        written(row, member)
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `DELETE /api/v1/groups/{id}/members/{user_id}` → `204`.
pub async fn remove_group(state: AppState, request: Dispatched<GroupMemberCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .remove_group_member(&write_request(&w), &w.proof)
            .await
    }
    .await;
    respond_empty(&w.ctx, done)
}
