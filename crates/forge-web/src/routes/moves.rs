//! Namespace moves (step D5 MR5, ADR-0015 "名前の変更、移管、削除と墓標" and
//! Amendment D5): `POST /api/v1/projects/{id}/transfer` (`ProjectTransfer`),
//! `POST /api/v1/groups/{id}/transfer` (`GroupTransfer`), `POST
//! /api/v1/users/{id}/rename` (`UserRename`, administrators) and `POST
//! /api/v1/namespaces/tombstones/release` (`NamespaceTombstoneRelease`).
//!
//! A transfer is also the rename: the body names the new parent
//! (`namespace_id`; omitted = the current parent) and the new last segment
//! (`path`; omitted = the current one). The current parent is the target
//! `CurrentParent`, which `authorize()` decides (ADR-0015: "名前だけの変更
//! は、移動先 = 今の親として扱う"; a project's or a subgroup's Owner renames
//! it without any role on the parent; a top-level group's rename is an
//! administrator's) — the loaders only describe what they read. A group moves to the top level only with `"top_level": true`
//! (never because a field was left out).
//!
//! As every write route: the registry authenticates, reads the strict body,
//! loads (the resource, its current parent, the target parent — a target
//! that does not exist, is a project, or cannot be seen is the same
//! `FG-REPO-0005 reason=target_not_visible`) and proves; forge-hosting
//! decides again inside the transaction on what it locks (an Owner demoted
//! meanwhile is refused, nothing written) and runs a move again after a
//! retryable conflict (`FG-REPO-0022` when that keeps happening). Agents
//! are denied every one of these (D5 plan). Answers: `200` with the old and
//! new path and how many `moved` tombstones now keep the old paths (git
//! fetches through them are redirected `302` after `authorize()`, pushes
//! are refused); `FG-REPO-0009` (409) a path that is taken or tombstoned,
//! `FG-REPO-0015` (409) a move the subtree rules refuse (also
//! `no_active_owner`), `FG-REPO-0018` (429) the 11th move in 24 hours.
//! Users see `user_message` and `request_id` only.
//!
//! A tombstone is released by its path (the path a `FG-REPO-0009
//! reason=tombstoned` named); no tombstone, and one the caller may not see,
//! are the same `FG-REPO-0005`.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    GroupTransferCtx, LoadedGroup, LoadedTransferTarget, LoadedUserNamespace, Principal,
    TombstoneCtx, TransferCtx, UserRenameCtx,
};
use forge_core::{
    GroupId, LookupPath, NamespaceId, NamespacePath, Resolution, ResolvedTarget, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{MoveOutcome, MoveRequest, Parent as HostedParent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::{param_invalid, request_invalid};
use crate::dispatch::resolve_bounded;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Lookup};
use crate::routes::projects::{self, PREFIX as PROJECTS};
use crate::routes::write::{absent, bounded, hosting, id_in, json, no_store, write_request};

/// `POST` a project's move or rename.
pub const PROJECT_TRANSFER: &str = "/api/v1/projects/{id}/transfer";
/// `POST` a group's move or rename.
pub const GROUP_TRANSFER: &str = "/api/v1/groups/{id}/transfer";
/// `POST` a user's rename (administrators).
pub const USER_RENAME: &str = "/api/v1/users/{id}/rename";
/// `POST` a tombstone's release.
pub const TOMBSTONE_RELEASE: &str = "/api/v1/namespaces/tombstones/release";

const GROUPS: &str = "/api/v1/groups/";
const USERS: &str = "/api/v1/users/";

/// The body cap of these routes (a path of 255 characters fits with room).
pub const MAX_BODY: usize = 4 * 1024;

/// `POST /api/v1/projects/{id}/transfer`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferProject {
    /// The new parent: a group or a personal namespace (`namespace_id` of
    /// `GET /api/v1/groups/{id}`, of a user, or of the resolver). Omitted:
    /// the current parent (a rename).
    #[serde(default)]
    #[schema(minimum = 1)]
    pub namespace_id: Option<i64>,
    /// The new last segment (ADR-0015 rules). Omitted: the current one.
    #[serde(default)]
    pub path: Option<String>,
    /// Take back an old path of this very project (A → B → A): release, in
    /// the same change, the `moved` tombstone there that names it. Default
    /// `false` (a tombstone always blocks until released).
    #[serde(default)]
    pub reclaim: Option<bool>,
}

/// `POST /api/v1/groups/{id}/transfer`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferGroup {
    /// The new parent group's `namespace_id`. Omitted: the current parent
    /// (a rename; a top-level group stays at the top level).
    #[serde(default)]
    #[schema(minimum = 1)]
    pub namespace_id: Option<i64>,
    /// `true`: move to the top level (administrators only); not together
    /// with `namespace_id`.
    #[serde(default)]
    pub top_level: Option<bool>,
    /// The new last segment. Omitted: the current one.
    #[serde(default)]
    pub path: Option<String>,
    /// As for a project: take back an old path of this group's subtree.
    #[serde(default)]
    pub reclaim: Option<bool>,
}

/// `POST /api/v1/users/{id}/rename`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameUser {
    /// The new username, which is also the personal namespace's path
    /// (ADR-0015 rules; not a reserved word).
    pub username: String,
    /// Take back an old username of this very user.
    #[serde(default)]
    pub reclaim: Option<bool>,
}

/// `POST /api/v1/namespaces/tombstones/release`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseTombstone {
    /// The tombstoned path, exactly (`group/sub/project`).
    pub path: String,
}

/// A move as it was written.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveBody {
    /// The moved project, group or user.
    #[schema(minimum = 1)]
    pub id: i64,
    /// `project`, `group` or `user`.
    pub kind: String,
    /// The moved namespace.
    #[schema(minimum = 1)]
    pub namespace_id: i64,
    /// The path before the move.
    pub old_path: String,
    /// The path now.
    pub path: String,
    /// The `moved` tombstones written: one per namespace of the subtree, at
    /// its old path. Each keeps that path blocked until released; a clone
    /// or fetch through it is redirected (`302`) to the new path for
    /// callers who may read the project, a push through it is refused.
    pub tombstones: i64,
    /// Projects whose path changed.
    pub projects: i64,
    /// Tombstones of this very subtree released by `reclaim`.
    pub reclaimed_tombstones: i64,
}

/// A released tombstone.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleasedBody {
    /// The path that can be used again.
    pub path: String,
    /// `moved` or `deleted`.
    pub kind: String,
}

fn invalid_namespace_id() -> ForgeError {
    request_invalid("field_invalid")
        .with_context("field", "namespace_id")
        .with_context("rule", "not_positive")
}

/// The last segment of `path` (a live path always has one).
fn last_segment(path: &NamespacePath) -> Result<String, ForgeError> {
    path.segments()
        .last()
        .map(|s| s.as_str().to_owned())
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "empty_namespace_path")
        })
}

/// The parent's path of `path` (`None` at the top level), folded for the
/// resolver.
fn parent_path(path: &NamespacePath) -> Result<Option<LookupPath>, ForgeError> {
    let segments: Vec<&str> = path.segments().iter().map(|s| s.as_str()).collect();
    match segments.split_last() {
        Some((_, [])) | None => Ok(None),
        Some((_, parent)) => LookupPath::fold(parent).map(Some),
    }
}

/// How often a loader reads the resource and its parent again when the
/// parent's path no longer resolves (the resource moved in between).
const LOAD_ATTEMPTS: u32 = 3;

/// The namespace of a live namespace's current parent, by the parent's
/// path (a parent moved a moment ago is followed through its tombstone).
/// `None` when the path no longer resolves: the resource moved between the
/// two reads, and the caller reads both again.
async fn current_parent(
    state: &AppState,
    path: &LookupPath,
    viewer: Option<UserId>,
) -> Result<Option<NamespaceId>, ForgeError> {
    let resolved = match resolve_bounded(state, path, viewer).await? {
        Resolution::Live(r) | Resolution::Moved { resolved: r, .. } => r,
        Resolution::Deleted { .. } | Resolution::Absent => return Ok(None),
    };
    if let ResolvedTarget::Project { .. } = resolved.target {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "parent_is_project")
            .with_context("namespace_id", resolved.namespace_id.get()));
    }
    Ok(Some(resolved.namespace_id))
}

/// The target of a body naming `requested` (`None` = keep the parent) for a
/// resource whose parent is `current` (`None` = the top level): the
/// current parent is a rename (`CurrentParent`, decided by `authorize()`),
/// any other is loaded as the caller sees it.
async fn target_for(
    state: &AppState,
    requested: Option<NamespaceId>,
    current: Option<NamespaceId>,
    viewer: Option<UserId>,
) -> Result<LoadedTransferTarget, ForgeError> {
    match requested {
        None => Ok(LoadedTransferTarget::CurrentParent { namespace: current }),
        Some(ns) if Some(ns) == current => {
            Ok(LoadedTransferTarget::CurrentParent { namespace: current })
        }
        Some(ns) => named_target(state, ns, viewer).await,
    }
}

/// The parent named by a body's `namespace_id`, as `viewer` sees it: an
/// absent namespace, a project's and one the caller cannot see are all
/// `NotVisible` (the same 404).
async fn named_target(
    state: &AppState,
    namespace: NamespaceId,
    viewer: Option<UserId>,
) -> Result<LoadedTransferTarget, ForgeError> {
    let parent = bounded(
        state,
        "parent_load",
        hosting(state)?.parent(namespace, viewer),
    )
    .await?;
    Ok(match parent {
        None | Some(HostedParent::Project) => LoadedTransferTarget::NotVisible,
        Some(HostedParent::Group {
            namespace,
            group,
            role,
        }) => LoadedTransferTarget::Group {
            namespace,
            group: LoadedGroup { id: group, role },
        },
        Some(HostedParent::User { namespace, owner }) => {
            LoadedTransferTarget::User(LoadedUserNamespace { namespace, owner })
        }
    })
}

/// `POST /api/v1/projects/{id}/transfer`: the project by id (as `GET`
/// loads it, also while pending deletion — `authorize()` answers 410 / 404)
/// and the target.
pub struct ProjectToMove;

impl BodyLoader<AppState, TransferProject> for ProjectToMove {
    type Ctx = TransferCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a TransferProject,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TransferCtx> {
        Box::pin(async move {
            let Some(id) =
                id_in(parts.uri.path(), PROJECTS, "/transfer").map(forge_core::ProjectId::new)
            else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            if body.namespace_id.is_some_and(|n| n < 1) {
                return Err(invalid_namespace_id());
            }
            // The project and its parent's namespace; read again when the
            // parent's path stopped resolving in between (a move raced). If
            // that keeps happening the answer is the absent 404 decided by
            // `authorize()` — never an error a stranger could tell apart.
            let mut tries = 0;
            let (record, current) = loop {
                tries += 1;
                let Some(record) = projects::load_bounded(state, id, viewer).await? else {
                    return Ok(absent());
                };
                if record.id != id {
                    return Err(ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "loaded_another_project")
                        .with_context("project_id", id.get())
                        .with_context("loaded_project_id", record.id.get()));
                }
                let parent = parent_path(&record.path)?.ok_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "project_at_top_level")
                        .with_context("project_id", id.get())
                })?;
                match current_parent(state, &parent, viewer).await? {
                    Some(ns) => break (record, ns),
                    None if tries < LOAD_ATTEMPTS => {}
                    None => return Ok(absent()),
                }
            };
            let target = target_for(
                state,
                body.namespace_id.map(NamespaceId::new),
                Some(current),
                viewer,
            )
            .await?;
            Ok(Lookup::Found {
                ctx: TransferCtx::new(projects::loaded(&record), target),
                canonical: Some(record.path.clone()), // clone: the record moves on too
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `POST /api/v1/groups/{id}/transfer`: the group by id and the target.
pub struct GroupToMove;

impl BodyLoader<AppState, TransferGroup> for GroupToMove {
    type Ctx = GroupTransferCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a TransferGroup,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupTransferCtx> {
        Box::pin(async move {
            let Some(id) = id_in(parts.uri.path(), GROUPS, "/transfer").map(GroupId::new) else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            if body.namespace_id.is_some_and(|n| n < 1) {
                return Err(invalid_namespace_id());
            }
            let top_level = body.top_level.unwrap_or(false);
            if top_level && body.namespace_id.is_some() {
                return Err(request_invalid("field_invalid")
                    .with_context("field", "top_level")
                    .with_context("rule", "with_namespace_id"));
            }
            let hosting = hosting(state)?;
            // As for a project: read again when the parent's path stopped
            // resolving; the absent 404 if that keeps happening.
            let mut tries = 0;
            let (record, current) = loop {
                tries += 1;
                let Some(record) = bounded(state, "group_load", hosting.group(id, viewer)).await?
                else {
                    return Ok(absent());
                };
                if record.id != id {
                    return Err(ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "loaded_another_group")
                        .with_context("group_id", id.get())
                        .with_context("loaded_group_id", record.id.get()));
                }
                let Some(parent) = parent_path(&record.path)? else {
                    break (record, None);
                };
                match current_parent(state, &parent, viewer).await? {
                    Some(ns) => break (record, Some(ns)),
                    None if tries < LOAD_ATTEMPTS => {}
                    None => return Ok(absent()),
                }
            };
            let target = match (top_level, current) {
                // Already at the top level: a rename there.
                (true, None) => LoadedTransferTarget::CurrentParent { namespace: None },
                (true, Some(_)) => LoadedTransferTarget::TopLevel,
                (false, _) => {
                    target_for(
                        state,
                        body.namespace_id.map(NamespaceId::new),
                        current,
                        viewer,
                    )
                    .await?
                }
            };
            Ok(Lookup::Found {
                ctx: GroupTransferCtx::new(
                    LoadedGroup {
                        id: record.id,
                        role: record.role,
                    },
                    target,
                ),
                canonical: Some(record.path.clone()), // clone: the record moves on too
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `POST /api/v1/users/{id}/rename`: decided on the user id alone
/// (administrators only — nobody else learns whether the user exists; the
/// command answers `FG-REPO-0005 resource=user` for an absent one).
pub struct UserToRename;

impl BodyLoader<AppState, RenameUser> for UserToRename {
    type Ctx = UserRenameCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        parts: &'a Parts,
        _: &'a RenameUser,
        _: Option<UserId>,
    ) -> LoadFuture<'a, UserRenameCtx> {
        Box::pin(async move {
            let Some(id) = id_in(parts.uri.path(), USERS, "/rename").map(UserId::new) else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            Ok(Lookup::Found {
                ctx: UserRenameCtx::new(id),
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `POST /api/v1/namespaces/tombstones/release`: the tombstone at the
/// body's path and what its release is decided on for the caller.
pub struct TombstoneAtPath;

impl BodyLoader<AppState, ReleaseTombstone> for TombstoneAtPath {
    type Ctx = TombstoneCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a ReleaseTombstone,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TombstoneCtx> {
        Box::pin(async move {
            let trimmed = body.path.strip_prefix('/').unwrap_or(&body.path);
            let segments: Vec<&str> = trimmed.split('/').collect();
            let path = LookupPath::fold(&segments).map_err(|_| {
                request_invalid("field_invalid")
                    .with_context("field", "path")
                    .with_context("rule", "invalid")
            })?;
            // A public action never reaches here; without a user the
            // registry denies before any handler (no lookup is needed).
            let Some(viewer) = viewer else {
                return Ok(absent());
            };
            let admin = parts
                .extensions
                .get::<Principal>()
                .is_some_and(Principal::admin_rights);
            let full = path.full_path();
            let found = bounded(
                state,
                "tombstone_load",
                hosting(state)?.tombstone_at(&full, viewer, admin),
            )
            .await?;
            Ok(match found {
                Some(tombstone) => Lookup::Found {
                    ctx: TombstoneCtx::new(tombstone),
                    canonical: None,
                    tombstone: None,
                    project: None,
                    group: None,
                },
                None => absent(),
            })
        })
    }
}

fn respond(ctx: &crate::Ctx, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

fn move_body(kind: &str, id: i64, moved: &MoveOutcome) -> Result<Value, ForgeError> {
    json(MoveBody {
        id,
        kind: kind.to_owned(),
        namespace_id: moved.summary.namespace_id.get(),
        old_path: moved.old_path.clone(), // clone: the outcome is borrowed
        path: moved.new_path.clone(),
        tombstones: moved.summary.nodes,
        projects: moved.summary.projects,
        reclaimed_tombstones: moved.summary.reclaimed_tombstones,
    })
}

/// The new segment: the body's, else the current path's last one.
fn segment_of(
    requested: Option<&String>,
    current: Option<&NamespacePath>,
) -> Result<String, ForgeError> {
    match (requested, current) {
        (Some(s), _) => Ok(s.clone()), // clone: the body stays with the request
        (None, Some(path)) => last_segment(path),
        (None, None) => {
            Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "canonical_path_missing"))
        }
    }
}

/// `POST /api/v1/projects/{id}/transfer` → `200` the move.
pub async fn transfer_project(
    state: AppState,
    request: Dispatched<TransferCtx, TransferProject>,
) -> Response {
    let segment = segment_of(request.input().path.as_ref(), request.canonical_path());
    let w = request.into_write();
    let built = async {
        let to = MoveRequest {
            segment: segment?,
            reclaim: w.input.reclaim.unwrap_or(false),
        };
        let moved = hosting(&state)?
            .move_project(&write_request(&w), &w.proof, &to)
            .await?;
        move_body("project", w.proof.id().0.get(), &moved)
    }
    .await;
    respond(&w.ctx, built)
}

/// `POST /api/v1/groups/{id}/transfer` → `200` the move.
pub async fn transfer_group(
    state: AppState,
    request: Dispatched<GroupTransferCtx, TransferGroup>,
) -> Response {
    let segment = segment_of(request.input().path.as_ref(), request.canonical_path());
    let w = request.into_write();
    let built = async {
        let to = MoveRequest {
            segment: segment?,
            reclaim: w.input.reclaim.unwrap_or(false),
        };
        let moved = hosting(&state)?
            .move_group(&write_request(&w), &w.proof, &to)
            .await?;
        move_body("group", w.proof.id().0.get(), &moved)
    }
    .await;
    respond(&w.ctx, built)
}

/// `POST /api/v1/users/{id}/rename` → `200` the move.
pub async fn rename_user(
    state: AppState,
    request: Dispatched<UserRenameCtx, RenameUser>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let to = MoveRequest {
            segment: w.input.username.clone(), // clone: the body stays with the request
            reclaim: w.input.reclaim.unwrap_or(false),
        };
        let moved = hosting(&state)?
            .rename_user(&write_request(&w), &w.proof, &to)
            .await?;
        move_body("user", w.proof.id().get(), &moved)
    }
    .await;
    respond(&w.ctx, built)
}

/// `POST /api/v1/namespaces/tombstones/release` → `200` the released path.
pub async fn release_tombstone(
    state: AppState,
    request: Dispatched<TombstoneCtx, ReleaseTombstone>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let released = hosting(&state)?
            .release_tombstone(&write_request(&w), &w.proof)
            .await?;
        json(ReleasedBody {
            path: released.path,
            kind: released.kind.as_str().to_owned(),
        })
    }
    .await;
    respond(&w.ctx, built)
}
