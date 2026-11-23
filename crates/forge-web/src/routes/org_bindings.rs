//! Org bindings (ADR-0038 §2, §5) and the org units to pick them from:
//!
//! - `GET /api/v1/{projects|groups}/{id}/org-bindings` (`MemberList`): the
//!   bindings of the group or project, the people they give a role to now
//!   (the members screen's 「組織から」 rows; at most 200) and `viewer`
//!   (whether the caller could change them: a hint, the route decides).
//!   ADR-0038 §6: rank and concurrent units are the person's and the
//!   administrators'. A reader without administrator rights in effect
//!   (`UserOrgAdminRead`, the decision of the admin org-profile route) —
//!   the Owner included — gets only the people reached through their
//!   primary unit by bindings without `min_rank` (`members_limited`), so
//!   varying `min_rank` or binding a concurrent unit infers nothing.
//! - `POST …/org-bindings` (`OrgBindingManage`: the Owner, a person in a
//!   web session): `{plant_code, org_code, include_descendants, membership,
//!   min_rank, role, reason}` → `201` the binding. A role above Developer
//!   is `FG-AUTH-0031`, bad fields or a duplicate `FG-AUTH-0029`, a unit
//!   the org master's copy does not hold `FG-AUTH-0030`. Audited.
//! - `DELETE …/org-bindings/{binding_id}` (`OrgBindingManage`) → `204`.
//! - `GET /api/v1/org-units?search=&plant=&limit=` (`UserSearch`: every
//!   signed-in user, as the people pickers that show the same names): the
//!   units of the org master's copy for the picker (names, level, parent;
//!   never the head's staff id). A token with `read_api` reaches it too,
//!   as it reaches `GET /api/v1/users?search=` with the same unit names:
//!   the unit tree is organisation data, not a person's (§6 lists what is
//!   personal: rank, positions, plant, concurrent units, the head's staff
//!   id — none of them is here), so it is not session-only.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, GroupCtx, InstanceCtx, ProjectCtx, Resource, ResourceCtx};
use forge_core::{parse_positive_id, GroupId, ProjectId, UserId};
use forge_errors::ForgeError;
use forge_hosting::{
    BindingTarget, BoundMember, NewBinding, OrgMemberDisclosure, OrgMembership, PickUnit,
    StoredBinding, UnitName, MAX_ORG_UNITS,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::members::{found, group_of, project_of, RoleName};
use crate::routes::namespaces::percent_decode;
use crate::routes::org_profiles::OrgNameBody;
use crate::routes::projects::PREFIX as PROJECT_PREFIX;
use crate::routes::write::{absent, bounded, hosting, json, no_store, write_request};

/// `GET` / `POST` a project's bindings.
pub const PROJECT_COLLECTION: &str = "/api/v1/projects/{id}/org-bindings";
/// `DELETE` one of a project's bindings.
pub const PROJECT_ONE: &str = "/api/v1/projects/{id}/org-bindings/{binding_id}";
/// `GET` / `POST` a group's bindings.
pub const GROUP_COLLECTION: &str = "/api/v1/groups/{id}/org-bindings";
/// `DELETE` one of a group's bindings.
pub const GROUP_ONE: &str = "/api/v1/groups/{id}/org-bindings/{binding_id}";
/// `GET` the org master's units (the picker).
pub const UNITS: &str = "/api/v1/org-units";

const GROUP_PREFIX: &str = "/api/v1/groups/";
const SEGMENT: &str = "/org-bindings";

/// The body cap of `POST …/org-bindings` (codes, flags and a reason of at
/// most 500 characters).
pub const MAX_BODY: usize = 4096;

/// Units per answer when `limit` is absent.
pub const DEFAULT_UNITS: usize = 20;

/// The query parameters of `GET /api/v1/org-units`.
pub const UNITS_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "search",
        description:
            "1..=64 characters: a code prefix or part of a name (ja / en / id; ASCII case folded)",
        integer_range: None,
    },
    QueryParam {
        name: "plant",
        description: "Only this plant's units (^[A-Z0-9]{1,4}$)",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "At most this many units, 1..=50 (default 20)",
        integer_range: Some((1, 50)),
    },
];

/// Which of a person's units a binding looks at.
#[derive(Debug, Clone, Copy, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OrgMembershipName {
    /// The primary unit only.
    Primary,
    /// The primary unit and the concurrent ones.
    Any,
}

/// `POST …/org-bindings`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOrgBinding {
    /// The unit's plant (`^[A-Z0-9]{1,4}$`).
    pub plant_code: String,
    /// The unit's code in the org master (`^[A-Z0-9][A-Z0-9_-]{0,19}$`).
    pub org_code: String,
    /// Also the units below it (by the org master's hierarchy). Default
    /// `false`.
    #[serde(default)]
    pub include_descendants: bool,
    /// Default `any`.
    #[serde(default)]
    pub membership: Option<OrgMembershipName>,
    /// Only people whose position rank is at least this (0..=999).
    #[serde(default)]
    pub min_rank: Option<i32>,
    /// `reporter` or `developer` (a binding never gives more).
    pub role: RoleName,
    /// Why (1..=500 characters), shown with the binding.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A unit's level and names as a binding shows them.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgBindingUnitBody {
    /// `division`, `department`, `section` or `team`.
    pub level: String,
    pub name_ja: String,
    #[schema(required = true)]
    pub name_en: Option<String>,
    #[schema(required = true)]
    pub name_id: Option<String>,
}

/// Who added a binding.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgBindingAuthorBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
}

/// One binding.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgBindingBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub plant_code: String,
    pub org_code: String,
    pub include_descendants: bool,
    /// `primary` or `any`.
    pub membership: String,
    #[schema(required = true)]
    pub min_rank: Option<i32>,
    /// `reporter` or `developer`.
    pub role: String,
    #[schema(required = true)]
    pub reason: Option<String>,
    pub created_by: OrgBindingAuthorBody,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// The unit in the org master's copy; `null` when the copy no longer
    /// has it (the binding then gives nothing: show the code).
    #[schema(required = true)]
    pub unit: Option<OrgBindingUnitBody>,
}

/// A person a binding gives a role to now.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgBindingMemberBody {
    #[schema(minimum = 1)]
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    /// The highest role the bindings give (`reporter` / `developer`).
    pub role: String,
    #[schema(minimum = 1)]
    pub binding_id: i64,
    /// The bound unit (the person is in it or below it).
    pub org_code: String,
    #[schema(required = true)]
    pub unit: Option<OrgBindingUnitBody>,
}

/// What the caller could do now (a hint; the route decides again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgBindingsViewerBody {
    pub can_manage: bool,
}

/// A group's or project's bindings and the people they bring in.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgBindingsBody {
    pub bindings: Vec<OrgBindingBody>,
    /// In user id order; people without a valid, recent org snapshot are
    /// not listed (a binding gives them nothing).
    pub members: Vec<OrgBindingMemberBody>,
    /// More people than listed (200).
    pub members_truncated: bool,
    /// `true` unless the caller has administrator rights: `members` then
    /// lists only the people reached through their primary unit by
    /// bindings without `min_rank`, with the highest role among those
    /// (rank and concurrent units are the person's and the
    /// administrators', ADR-0038 §6). The roles themselves are unchanged.
    pub members_limited: bool,
    pub viewer: OrgBindingsViewerBody,
}

/// One unit of the org master's copy, for the picker.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgPickUnitBody {
    pub plant_code: String,
    pub unit: OrgNameBody,
    #[schema(required = true)]
    pub parent_code: Option<String>,
}

/// `GET /api/v1/org-units`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgUnitsBody {
    pub items: Vec<OrgPickUnitBody>,
}

fn unit_body(u: UnitName) -> OrgBindingUnitBody {
    OrgBindingUnitBody {
        level: u.level.as_str().to_owned(),
        name_ja: u.name.ja,
        name_en: u.name.en,
        name_id: u.name.id,
    }
}

fn binding_body(b: StoredBinding) -> OrgBindingBody {
    OrgBindingBody {
        id: b.id,
        plant_code: b.plant_code,
        org_code: b.org_code,
        include_descendants: b.include_descendants,
        membership: b.membership.as_str().to_owned(),
        min_rank: b.min_rank,
        role: b.role.as_str().to_owned(),
        reason: b.reason,
        created_by: OrgBindingAuthorBody {
            id: b.created_by.get(),
            username: b.created_by_username,
        },
        created_at: b.created_at,
        unit: b.unit.map(unit_body),
    }
}

fn member_body(m: BoundMember) -> OrgBindingMemberBody {
    OrgBindingMemberBody {
        user_id: m.user_id.get(),
        username: m.username,
        display_name: m.display_name,
        role: m.role.as_str().to_owned(),
        binding_id: m.binding_id,
        org_code: m.org_code,
        unit: m.unit.map(unit_body),
    }
}

fn pick_body(u: PickUnit) -> OrgPickUnitBody {
    OrgPickUnitBody {
        plant_code: u.plant_code,
        unit: OrgNameBody {
            code: u.code,
            level: u.level.as_str().to_owned(),
            name_ja: u.name.ja,
            name_en: u.name.en,
            name_id: u.name.id,
        },
        parent_code: u.parent_code,
    }
}

fn new_binding(body: &NewOrgBinding) -> NewBinding {
    NewBinding {
        plant_code: body.plant_code.clone(), // clone: the command owns its ask
        org_code: body.org_code.clone(),
        include_descendants: body.include_descendants,
        membership: match body.membership {
            Some(OrgMembershipName::Primary) => OrgMembership::Primary,
            Some(OrgMembershipName::Any) | None => OrgMembership::Any,
        },
        min_rank: body.min_rank,
        role: body.role.role(),
        reason: body.reason.clone(),
    }
}

// ── paths ──

fn bad_id() -> ForgeError {
    param_invalid("id", "not_positive_decimal")
}

fn collection_id(raw_path: &str, prefix: &str) -> Result<i64, ForgeError> {
    crate::routes::write::id_in(raw_path, prefix, SEGMENT).ok_or_else(bad_id)
}

/// `{prefix}{id}/org-bindings/{binding_id}` (raw path, never decoded).
fn ids(raw_path: &str, prefix: &str) -> Result<(i64, i64), ForgeError> {
    let rest = raw_path.strip_prefix(prefix).ok_or_else(bad_id)?;
    let (id, binding) = rest.split_once("/org-bindings/").ok_or_else(bad_id)?;
    let id = parse_positive_id(id).ok_or_else(bad_id)?;
    if binding.contains('/') {
        return Err(param_invalid("binding_id", "not_positive_decimal"));
    }
    let binding = parse_positive_id(binding)
        .ok_or_else(|| param_invalid("binding_id", "not_positive_decimal"))?;
    Ok((id, binding))
}

// ── loaders ──

async fn project_lookup(
    state: &AppState,
    id: i64,
    viewer: Option<UserId>,
) -> Result<Lookup<ProjectCtx>, ForgeError> {
    Ok(match project_of(state, ProjectId::new(id), viewer).await? {
        Some(project) => found(ProjectCtx::new(project)),
        None => absent(),
    })
}

async fn group_lookup(
    state: &AppState,
    id: i64,
    viewer: Option<UserId>,
) -> Result<Lookup<GroupCtx>, ForgeError> {
    Ok(match group_of(state, GroupId::new(id), viewer).await? {
        Some(group) => found(GroupCtx::new(group)),
        None => absent(),
    })
}

/// The project of `…/org-bindings` (list and create).
pub struct ProjectBindings;

impl Loader<AppState> for ProjectBindings {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            project_lookup(
                state,
                collection_id(parts.uri.path(), PROJECT_PREFIX)?,
                viewer,
            )
            .await
        })
    }
}

impl BodyLoader<AppState, NewOrgBinding> for ProjectBindings {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a NewOrgBinding,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            project_lookup(
                state,
                collection_id(parts.uri.path(), PROJECT_PREFIX)?,
                viewer,
            )
            .await
        })
    }
}

/// The project of `…/org-bindings/{binding_id}` (the binding is looked up
/// inside the decided command: one of another target is not found).
pub struct ProjectBinding;

impl Loader<AppState> for ProjectBinding {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let (id, _) = ids(parts.uri.path(), PROJECT_PREFIX)?;
            project_lookup(state, id, viewer).await
        })
    }
}

/// The group of `…/org-bindings` (list and create).
pub struct GroupBindings;

impl Loader<AppState> for GroupBindings {
    type Ctx = GroupCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupCtx> {
        Box::pin(async move {
            group_lookup(
                state,
                collection_id(parts.uri.path(), GROUP_PREFIX)?,
                viewer,
            )
            .await
        })
    }
}

impl BodyLoader<AppState, NewOrgBinding> for GroupBindings {
    type Ctx = GroupCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a NewOrgBinding,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupCtx> {
        Box::pin(async move {
            group_lookup(
                state,
                collection_id(parts.uri.path(), GROUP_PREFIX)?,
                viewer,
            )
            .await
        })
    }
}

/// The group of `…/org-bindings/{binding_id}`.
pub struct GroupBinding;

impl Loader<AppState> for GroupBinding {
    type Ctx = GroupCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, GroupCtx> {
        Box::pin(async move {
            let (id, _) = ids(parts.uri.path(), GROUP_PREFIX)?;
            group_lookup(state, id, viewer).await
        })
    }
}

// ── handlers ──

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

async fn listed(
    state: &AppState,
    target: BindingTarget,
    can_manage: bool,
    full: bool,
) -> Result<Value, ForgeError> {
    let disclosure = if full {
        OrgMemberDisclosure::Full
    } else {
        OrgMemberDisclosure::PrimaryUnit
    };
    let hosting = hosting(state)?;
    let bindings = bounded(state, "org_binding_list", hosting.org_bindings(target)).await?;
    let (members, members_truncated) = if bindings.is_empty() {
        (Vec::new(), false)
    } else {
        bounded(
            state,
            "org_binding_members",
            hosting.org_binding_members(target, disclosure),
        )
        .await?
    };
    json(OrgBindingsBody {
        bindings: bindings.into_iter().map(binding_body).collect(),
        members: members.into_iter().map(member_body).collect(),
        members_truncated,
        members_limited: !full,
        viewer: OrgBindingsViewerBody { can_manage },
    })
}

/// Whether the proof's caller may see everyone's org facts (ADR-0038 §6):
/// the same decision as the admin org-profile route (`UserOrgAdminRead`:
/// administrator rights in effect, a person's credential).
fn sees_org_facts<R: ResourceCtx>(proof: &forge_auth::Authorized<R>) -> bool {
    forge_auth::authorize(
        proof.principal(),
        Action::UserOrgAdminRead,
        &Resource::Instance,
    )
    .is_ok()
}

/// Whether the proof's caller could add or remove bindings (a hint).
fn can_manage<R: ResourceCtx>(proof: &forge_auth::Authorized<R>) -> bool {
    forge_auth::authorize(
        proof.principal(),
        Action::OrgBindingManage,
        &proof.ctx().resource(),
    )
    .is_ok()
}

/// `GET /api/v1/projects/{id}/org-bindings` → `200`.
pub async fn list_project(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let target = BindingTarget::Project(*w.proof.id());
    let built = listed(
        &state,
        target,
        can_manage(&w.proof),
        sees_org_facts(&w.proof),
    )
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/groups/{id}/org-bindings` → `200`.
pub async fn list_group(state: AppState, request: Dispatched<GroupCtx>) -> Response {
    let w = request.into_write();
    let target = BindingTarget::Group(*w.proof.id());
    let built = listed(
        &state,
        target,
        can_manage(&w.proof),
        sees_org_facts(&w.proof),
    )
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST /api/v1/projects/{id}/org-bindings` → `201` the binding.
pub async fn create_project(
    state: AppState,
    request: Dispatched<ProjectCtx, NewOrgBinding>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let stored = hosting(&state)?
            .create_project_org_binding(&write_request(&w), &w.proof, &new_binding(&w.input))
            .await?;
        json(binding_body(stored))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `POST /api/v1/groups/{id}/org-bindings` → `201` the binding.
pub async fn create_group(
    state: AppState,
    request: Dispatched<GroupCtx, NewOrgBinding>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let stored = hosting(&state)?
            .create_group_org_binding(&write_request(&w), &w.proof, &new_binding(&w.input))
            .await?;
        json(binding_body(stored))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

fn respond_empty(ctx: &crate::Ctx, done: Result<(), ForgeError>) -> Response {
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `DELETE /api/v1/projects/{id}/org-bindings/{binding_id}` → `204`.
pub async fn delete_project(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = ids(request.raw_path(), PROJECT_PREFIX);
    let w = request.into_write();
    let done = async {
        let (_, binding) = path?;
        hosting(&state)?
            .delete_project_org_binding(&write_request(&w), &w.proof, binding)
            .await
            .map(drop)
    }
    .await;
    respond_empty(&w.ctx, done)
}

/// `DELETE /api/v1/groups/{id}/org-bindings/{binding_id}` → `204`.
pub async fn delete_group(state: AppState, request: Dispatched<GroupCtx>) -> Response {
    let path = ids(request.raw_path(), GROUP_PREFIX);
    let w = request.into_write();
    let done = async {
        let (_, binding) = path?;
        hosting(&state)?
            .delete_group_org_binding(&write_request(&w), &w.proof, binding)
            .await
            .map(drop)
    }
    .await;
    respond_empty(&w.ctx, done)
}

/// The query of `GET /api/v1/org-units`, strictly.
struct UnitsQuery {
    search: Option<String>,
    plant: Option<String>,
    limit: usize,
}

fn units_query(query: Option<&str>) -> Result<UnitsQuery, ForgeError> {
    let mut out = UnitsQuery {
        search: None,
        plant: None,
        limit: DEFAULT_UNITS,
    };
    let mut seen: Vec<&str> = Vec::new();
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = match key {
            "search" => "search",
            "plant" => "plant",
            "limit" => "limit",
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if seen.contains(&name) {
            return Err(param_invalid(name, "repeated"));
        }
        seen.push(name);
        match name {
            "search" => {
                // A form-encoded `+` is a space in a search term.
                let term = percent_decode(&value.replace('+', " "))
                    .ok_or_else(|| param_invalid("search", "malformed"))?;
                let term = term.trim().to_owned();
                if !(1..=64).contains(&term.chars().count()) {
                    return Err(param_invalid("search", "length"));
                }
                out.search = Some(term);
            }
            "plant" => {
                let ok = (1..=4).contains(&value.len())
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
                if !ok {
                    return Err(param_invalid("plant", "format"));
                }
                out.plant = Some(value.to_owned());
            }
            _ => {
                out.limit = parse_positive_id(value)
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n <= MAX_ORG_UNITS)
                    .ok_or_else(|| param_invalid("limit", "out_of_range"))?;
            }
        }
    }
    Ok(out)
}

/// `GET /api/v1/org-units` → `200` the units.
pub async fn units(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let query = units_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let q = query?;
        let found = bounded(
            &state,
            "org_units",
            hosting(&state)?.org_units(q.search.as_deref(), q.plant.as_deref(), q.limit),
        )
        .await?;
        json(OrgUnitsBody {
            items: found.into_iter().map(pick_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_paths_are_parsed_strictly() {
        assert_eq!(
            ids("/api/v1/projects/7/org-bindings/12", PROJECT_PREFIX).ok(),
            Some((7, 12))
        );
        assert!(ids("/api/v1/projects/7/org-bindings/12/x", PROJECT_PREFIX).is_err());
        assert!(ids("/api/v1/projects/0/org-bindings/1", PROJECT_PREFIX).is_err());
        assert!(ids("/api/v1/groups/3/org-bindings/-1", GROUP_PREFIX).is_err());
        assert_eq!(
            collection_id("/api/v1/groups/3/org-bindings", GROUP_PREFIX).ok(),
            Some(3)
        );
    }

    #[test]
    fn the_units_query_is_strict() {
        let q = units_query(Some("search=sec&plant=P001&limit=5")).expect("query");
        assert_eq!(
            (q.search.as_deref(), q.plant.as_deref(), q.limit),
            (Some("sec"), Some("P001"), 5)
        );
        assert_eq!(units_query(None).expect("empty").limit, DEFAULT_UNITS);
        for bad in [
            "search=",
            "plant=p1",
            "limit=51",
            "limit=0",
            "other=1",
            "search=a&search=b",
        ] {
            assert!(units_query(Some(bad)).is_err(), "{bad}");
        }
    }
}
