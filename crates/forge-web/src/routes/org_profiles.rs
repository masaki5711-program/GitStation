//! The org claims' snapshot (ADR-0021 amendment 2026-11-20, display only):
//!
//! - `GET /api/v1/user/org-profile` (`UserSelfRead`, `read_api`): the
//!   caller's own snapshot — departments (the primary first), positions,
//!   rank, plants.
//! - `GET /api/v1/admin/users/{id}/org-profile` (`UserOrgAdminRead`):
//!   anyone's (administrators; a person's credential only, a token needs
//!   `read_api` and `admin`).
//!
//! Every other signed-in user sees only the **primary department code**,
//! next to a name in the people pickers and the approval list
//! ([`OrgCodes`]), and, once the PFS org master is copied (ADR-0038 §1),
//! the names of that unit, its section and its department ([`OrgPaths`],
//! [`OrgPathBody`]). A code the copy does not resolve has no `path` /
//! `primary_org` (`null`): the screens show the code. `org` is `null`
//! before the first sign-in through the PFS Keycloak (or with
//! `claims_source = id_token`).

use std::collections::HashMap;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{InstanceCtx, UserTargetCtx};
use forge_core::{parse_positive_id, UserId};
use forge_errors::ForgeError;
use forge_hosting::accounts::{OrgPath, OrgProfile, OrgRef};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::write::{bounded, hosting, json, no_store};

/// `GET` the caller's own snapshot.
pub const OWN: &str = "/api/v1/user/org-profile";
/// `GET` a user's snapshot (administrators).
pub const ADMIN: &str = "/api/v1/admin/users/{id}/org-profile";
const ADMIN_PREFIX: &str = "/api/v1/admin/users/";
const ADMIN_SUFFIX: &str = "/org-profile";

/// One unit of the PFS org master (ADR-0038 §1): its code, level and
/// names.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgNameBody {
    pub code: String,
    /// `division`, `department`, `section` or `team`.
    pub level: String,
    pub name_ja: String,
    #[schema(required = true)]
    pub name_en: Option<String>,
    #[schema(required = true)]
    pub name_id: Option<String>,
}

/// A unit and the section and department above it, from the copy of the
/// PFS org master (found by the parents' levels, never by the code).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgPathBody {
    pub unit: OrgNameBody,
    /// `null`: no section at or above the unit.
    #[schema(required = true)]
    pub section: Option<OrgNameBody>,
    /// `null`: no department at or above the unit.
    #[schema(required = true)]
    pub department: Option<OrgNameBody>,
}

fn name_body(r: OrgRef) -> OrgNameBody {
    OrgNameBody {
        code: r.code,
        level: r.level.as_str().to_owned(),
        name_ja: r.name.ja,
        name_en: r.name.en,
        name_id: r.name.id,
    }
}

pub(crate) fn path_body(p: OrgPath) -> OrgPathBody {
    OrgPathBody {
        unit: name_body(p.unit),
        section: p.section.map(name_body),
        department: p.department.map(name_body),
    }
}

/// One department.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgUnitBody {
    /// The PFS org code (`^[A-Z0-9][A-Z0-9_-]{0,19}$`).
    pub code: String,
    /// The person's main department (at most one; listed first).
    pub primary: bool,
    /// The unit's names from the org master's copy (ADR-0038 §1); `null`
    /// when the copy does not have the code (show the code).
    #[schema(required = true)]
    pub path: Option<OrgPathBody>,
}

/// The snapshot of the identity provider's org claims.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgProfileBody {
    pub org_units: Vec<OrgUnitBody>,
    /// Position codes.
    pub positions: Vec<String>,
    /// 0..=999.
    #[schema(required = true)]
    pub position_rank: Option<i32>,
    pub plants: Vec<String>,
    #[schema(required = true)]
    pub plant_code: Option<String>,
    /// UTC, RFC 3339: the provider last sent these values.
    pub confirmed_at: String,
    /// UTC, RFC 3339: the values last changed.
    pub changed_at: String,
    /// UTC, RFC 3339: the provider's claims fail validation since then (the
    /// values shown are the last valid ones).
    #[schema(required = true)]
    pub invalid_since: Option<String>,
}

/// One user's snapshot (`org` is `null` without one).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserOrgProfile {
    #[schema(minimum = 1)]
    pub user_id: i64,
    #[schema(required = true)]
    pub org: Option<OrgProfileBody>,
}

fn body(profile: OrgProfile, mut paths: HashMap<String, OrgPath>) -> OrgProfileBody {
    let s = profile.snapshot;
    OrgProfileBody {
        org_units: s
            .org_units
            .into_iter()
            .map(|u| OrgUnitBody {
                path: paths.remove(&u.code).map(path_body),
                code: u.code,
                primary: u.primary,
            })
            .collect(),
        positions: s.positions,
        position_rank: s.position_rank,
        plants: s.plants,
        plant_code: s.plant_code,
        confirmed_at: profile.confirmed_at,
        changed_at: profile.changed_at,
        invalid_since: profile.invalid_since,
    }
}

/// The body of a snapshot the route read, with its units' names.
async fn with_paths(
    state: &AppState,
    profile: Option<OrgProfile>,
) -> Result<Option<OrgProfileBody>, ForgeError> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    let paths = bounded(
        state,
        "org_unit_paths",
        hosting(state)?.org_unit_paths(&profile),
    )
    .await?;
    Ok(Some(body(profile, paths)))
}

fn respond(ctx: &crate::Ctx, built: Result<Value, ForgeError>) -> Response {
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET /api/v1/user/org-profile`.
pub async fn own(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let viewer = request.principal().user();
    let w = request.into_write();
    let built = async {
        let user = viewer.ok_or_else(|| {
            ForgeError::new(forge_errors::ErrorCode::Internal)
                .with_context("reason", "org_profile_viewer_missing")
        })?;
        let profile = bounded(
            &state,
            "own_org_profile",
            hosting(&state)?.own_org_profile(&w.proof),
        )
        .await?;
        json(UserOrgProfile {
            user_id: user.get(),
            org: with_paths(&state, profile).await?,
        })
    }
    .await;
    respond(&w.ctx, built)
}

/// `GET /api/v1/admin/users/{id}/org-profile`: the user.
pub struct OrgProfileTarget;

impl Loader<AppState> for OrgProfileTarget {
    type Ctx = UserTargetCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, UserTargetCtx> {
        Box::pin(async move {
            let id = parts
                .uri
                .path()
                .strip_prefix(ADMIN_PREFIX)
                .and_then(|rest| rest.strip_suffix(ADMIN_SUFFIX))
                .filter(|raw| !raw.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
            Ok(Lookup::Found {
                ctx: UserTargetCtx::new(UserId::new(id)),
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `GET /api/v1/admin/users/{id}/org-profile`.
pub async fn admin(state: AppState, request: Dispatched<UserTargetCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let profile = bounded(
            &state,
            "admin_org_profile",
            hosting(&state)?.admin_org_profile(&w.proof),
        )
        .await?;
        json(UserOrgProfile {
            user_id: w.proof.id().get(),
            org: with_paths(&state, profile).await?,
        })
    }
    .await;
    respond(&w.ctx, built)
}

/// The primary department codes of the users a route shows (the people
/// pickers, the approval list).
pub(crate) struct OrgCodes(std::collections::HashMap<UserId, String>);

impl OrgCodes {
    /// Read them through `read` (forge-hosting's or forge-review's lookup;
    /// at most a page's worth of users), bounded like every read.
    pub(crate) async fn of(
        state: &AppState,
        read: impl std::future::Future<
            Output = Result<std::collections::HashMap<UserId, String>, ForgeError>,
        >,
    ) -> Result<Self, ForgeError> {
        Ok(Self(bounded(state, "primary_org_codes", read).await?))
    }

    /// `user`'s primary department code, if any.
    pub(crate) fn code(&self, user: UserId) -> Option<String> {
        self.0.get(&user).cloned() // clone: the body owns its copy
    }
}

/// The names of the primary units of the users a route shows (ADR-0038 §1:
/// the people pickers, the approval list), next to [`OrgCodes`].
pub(crate) struct OrgPaths(HashMap<UserId, OrgPath>);

impl OrgPaths {
    /// Read them through `read` (forge-hosting's or forge-review's lookup;
    /// at most a page's worth of users), bounded like every read.
    pub(crate) async fn of(
        state: &AppState,
        read: impl std::future::Future<Output = Result<HashMap<UserId, OrgPath>, ForgeError>>,
    ) -> Result<Self, ForgeError> {
        Ok(Self(bounded(state, "primary_org_paths", read).await?))
    }

    /// `user`'s primary unit with its section and department, if resolved.
    pub(crate) fn path(&mut self, user: UserId) -> Option<OrgPathBody> {
        self.0.remove(&user).map(path_body)
    }
}
