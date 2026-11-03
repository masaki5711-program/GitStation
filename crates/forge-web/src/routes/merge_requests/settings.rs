//! A project's merge settings and the CODEOWNERS switch (M3 R6, ADR-0024
//! §4.1, §8.2, §8.3, §9, §12):
//!
//! - `GET /api/v1/projects/{id}/merge-settings` (`ProjectRead`): the
//!   methods allowed and the default, the source deletion default, whether
//!   new commits clear approvals, whether committers' approvals are
//!   excluded, and CODEOWNERS enforcement now;
//! - `PUT /api/v1/projects/{id}/merge-settings` (`MergeSettingsManage`:
//!   people only, Maintainer) replaces those values (not the switch);
//! - `PUT /api/v1/projects/{id}/merge-settings/codeowners-enforcement`
//!   (`CodeownersEnforcementToggle`: people only, Maintainer; a suspension
//!   needs a role at or above every protection of the project and an
//!   expiry of 1 to 1440 minutes) suspends or enforces CODEOWNERS.
//!
//! U2: every answer carries `viewer` (what the caller could do now: trial
//! `authorize()` calls on the write routes' contexts), and a settings write
//! names the `updated_at` it read (`expected_updated_at`; another one is
//! FG-REVIEW-0017, optimistic concurrency).
//!
//! The registry proves the action on what the loader read (the toggle's
//! loader reads the project's highest protection with it); forge-review
//! decides again inside the transaction.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::{
    CodeownersToggle, CodeownersToggleCtx, MergeSettingsCtx, MergeSettingsViewer, ProjectCtx,
};
use forge_core::{parse_positive_id, MergeMethod, MergeSettingsValues, ProjectId, UserId};
use forge_errors::ForgeError;
use forge_review::{CodeownersEnforcement, ReviewRequest, StoredMergeSettings};
use serde::{Deserialize, Serialize};

use super::loaders::{found, project_of};
use super::{respond, review, viewer_read};
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, WriteParts};
use crate::routes::projects::PREFIX;
use crate::routes::write::{absent, bounded, json, read_if_manager};

/// `GET` / `PUT` a project's merge settings.
pub const SETTINGS: &str = "/api/v1/projects/{id}/merge-settings";
/// `PUT` suspend or enforce CODEOWNERS.
pub const CODEOWNERS: &str = "/api/v1/projects/{id}/merge-settings/codeowners-enforcement";
/// The body cap of the settings routes.
pub const MAX_BODY: usize = 4 * 1024;

/// `PUT …/merge-settings` (every field: the settings are replaced).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutMergeSettings {
    /// One to three of `merge_commit`, `squash`, `fast_forward`.
    pub allowed_methods: Vec<String>,
    /// One of `allowed_methods`.
    pub default_method: String,
    /// Delete the source branch after a merge unless the request says.
    pub delete_source_default: bool,
    /// New commits end the approvals (the default `true`); `false` carries
    /// them when the change allows it (never into a protected branch, never
    /// a configuration or CODEOWNERS path).
    pub reset_approvals_on_push: bool,
    /// Committers' approvals do not count (the default `true`).
    pub committer_approvals_excluded: bool,
    /// The `updated_at` the caller read, echoed verbatim — an opaque string
    /// (never round-tripped through a date type: it is to the microsecond)
    /// — or `null` for the defaults, never stored. Required (U2): another
    /// value under the lock is FG-REVIEW-0017 — the settings changed in
    /// between and nothing is overwritten silently.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schema(required = true)]
    pub expected_updated_at: Option<String>,
}

/// `PUT …/merge-settings/codeowners-enforcement`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutCodeownersEnforcement {
    /// `false` suspends CODEOWNERS (with `for_minutes`); `true` enforces it
    /// again.
    pub enforced: bool,
    /// The suspension's length, 1..=1440 minutes (required to suspend; it
    /// comes back by itself at the expiry).
    pub for_minutes: Option<u32>,
}

/// CODEOWNERS enforcement as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeownersEnforcementBody {
    /// Enforced now (an expired suspension is enforced).
    pub enforced: bool,
    /// While suspended: since when and until when (UTC, RFC 3339).
    #[schema(required = true)]
    pub suspended_at: Option<String>,
    #[schema(required = true)]
    pub suspended_until: Option<String>,
}

/// What the caller could do on the settings now (U2): each flag is
/// `authorize()` on the context the write route decides on
/// (`forge_auth::MergeSettingsViewer`) — a hint for the screen, never a
/// permission (the route decides again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeSettingsViewerBody {
    /// `PUT …/merge-settings` that does not loosen the approvals.
    pub can_manage: bool,
    /// `PUT …/merge-settings` that counts committers' approvals or carries
    /// approvals on push (a role at or above every protection).
    pub can_loosen: bool,
    /// `PUT …/codeowners-enforcement` suspending CODEOWNERS (a role at or
    /// above every protection).
    pub can_suspend_codeowners: bool,
}

/// A project's merge settings.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeSettingsBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    pub allowed_methods: Vec<String>,
    pub default_method: String,
    pub delete_source_default: bool,
    pub reset_approvals_on_push: bool,
    pub committer_approvals_excluded: bool,
    pub codeowners: CodeownersEnforcementBody,
    /// `false`: the project never changed them (the defaults).
    pub stored: bool,
    /// UTC, RFC 3339 to the microsecond (`null` for the defaults): an
    /// opaque token — send it back verbatim as `expected_updated_at`.
    #[schema(required = true)]
    pub updated_at: Option<String>,
    /// What the caller could do now (U2): a hint; on a write's answer it
    /// is decided on the facts read before the lock.
    pub viewer: MergeSettingsViewerBody,
}

fn settings_body(
    project: ProjectId,
    s: &StoredMergeSettings,
    viewer: MergeSettingsViewer,
) -> MergeSettingsBody {
    let (suspended_at, suspended_until) = match &s.codeowners {
        CodeownersEnforcement::Suspended { since, until } => {
            (Some(since.clone()), Some(until.clone()))
        } // clone: the body owns them
        CodeownersEnforcement::Enforced | CodeownersEnforcement::Expired { .. } => (None, None),
    };
    MergeSettingsBody {
        project_id: project.get(),
        allowed_methods: s
            .values
            .allowed_methods
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect(),
        default_method: s.values.default_method.as_str().to_owned(),
        delete_source_default: s.values.delete_source_default,
        reset_approvals_on_push: s.values.reset_approvals_on_push,
        committer_approvals_excluded: s.values.committer_approvals_excluded,
        codeowners: CodeownersEnforcementBody {
            enforced: s.codeowners.enforced(),
            suspended_at,
            suspended_until,
        },
        stored: s.stored,
        updated_at: s.updated_at.clone(), // clone: as above
        viewer: MergeSettingsViewerBody {
            can_manage: viewer.can_manage,
            can_loosen: viewer.can_loosen,
            can_suspend_codeowners: viewer.can_suspend_codeowners,
        },
    }
}

/// The project id of `/api/v1/projects/{id}/merge-settings[…]`.
fn project_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    let id = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/merge-settings"))
        .map(|(id, _)| id)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    parse_positive_id(id)
        .map(ProjectId::new)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))
}

/// `GET` / `PUT …/merge-settings`: the project.
pub struct SettingsProject;

async fn project_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<crate::registry::Lookup<ProjectCtx>, ForgeError> {
    let id = project_id(parts.uri.path())?;
    Ok(match project_of(state, id, viewer).await? {
        Some((project, record)) => found(ProjectCtx::new(project), record),
        None => absent(),
    })
}

impl Loader<AppState> for SettingsProject {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(project_lookup(state, parts, viewer))
    }
}

/// `PUT …/merge-settings`: the project, its highest protection and
/// whether the change loosens the approvals against the stored settings
/// (R6 review; read only for a caller who could manage the project, the
/// decision that binds the write reads them again under the lock).
pub struct SettingsChangeOf;

impl BodyLoader<AppState, PutMergeSettings> for SettingsChangeOf {
    type Ctx = MergeSettingsCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a PutMergeSettings,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeSettingsCtx> {
        Box::pin(async move {
            let id = project_id(parts.uri.path())?;
            let Some((project, record)) = project_of(state, id, viewer).await? else {
                return Ok(absent());
            };
            let review = review(state)?;
            let facts = read_if_manager(project.role, || async {
                let current =
                    bounded(state, "merge_settings", review.stored_merge_settings(id)).await?;
                let highest = bounded(
                    state,
                    "merge_settings_protection",
                    review.highest_protection(id),
                )
                .await?;
                Ok((current, highest))
            })
            .await?;
            let (loosens, highest) = match facts {
                Some((current, highest)) => {
                    let new = MergeSettingsValues {
                        reset_approvals_on_push: body.reset_approvals_on_push,
                        committer_approvals_excluded: body.committer_approvals_excluded,
                        ..current.values.clone() // clone: only the approval switches matter here
                    };
                    (
                        forge_review::settings_loosen_approvals(&current.values, &new),
                        highest,
                    )
                }
                None => (false, None),
            };
            Ok(found(
                MergeSettingsCtx::new(project, highest, loosens),
                record,
            ))
        })
    }
}

/// `PUT …/codeowners-enforcement`: the project, its highest protection
/// and the change asked for.
pub struct CodeownersOf;

impl BodyLoader<AppState, PutCodeownersEnforcement> for CodeownersOf {
    type Ctx = CodeownersToggleCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a PutCodeownersEnforcement,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, CodeownersToggleCtx> {
        Box::pin(async move {
            let id = project_id(parts.uri.path())?;
            let Some((project, record)) = project_of(state, id, viewer).await? else {
                return Ok(absent());
            };
            // R6 review: the protection is read only for a caller who could
            // manage the project (others are refused on their role).
            let review = review(state)?;
            let highest = read_if_manager(project.role, || {
                bounded(
                    state,
                    "codeowners_protection",
                    review.highest_protection(id),
                )
            })
            .await?
            .flatten();
            let change = if body.enforced {
                CodeownersToggle::Enforce
            } else {
                CodeownersToggle::Suspend {
                    minutes: body.for_minutes,
                }
            };
            Ok(found(
                CodeownersToggleCtx::new(project, highest, change),
                record,
            ))
        })
    }
}

fn request_of<R: forge_auth::ResourceCtx, B>(w: &WriteParts<R, B>) -> ReviewRequest {
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// `GET …/merge-settings` → `200` the settings.
pub async fn get_settings(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let w = request.into_write();
    let built = async {
        let project = *w.proof.id();
        let review = review(&state)?;
        let settings = bounded(&state, "merge_settings", review.merge_settings(&w.proof)).await?;
        // The protection the write routes' loaders read, as they read it
        // (only for a caller who could manage the project): U2 viewer flags.
        // A failed read closes the flags only (review round).
        let highest = read_if_manager(w.proof.ctx().view().role, || {
            bounded(
                &state,
                "merge_settings_protection",
                review.highest_protection(project),
            )
        })
        .await;
        let viewer = viewer_read(&w.ctx, "merge_settings_protection", highest).map_or(
            MergeSettingsViewer::CLOSED,
            |highest| {
                w.proof
                    .ctx()
                    .merge_settings_viewer(w.proof.principal(), highest.flatten())
            },
        );
        json(settings_body(project, &settings, viewer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PUT …/merge-settings` → `200` the settings as stored.
pub async fn put_settings(
    state: AppState,
    request: Dispatched<MergeSettingsCtx, PutMergeSettings>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let method = |raw: &str| {
            MergeMethod::parse(raw)
                .ok_or_else(|| param_invalid("allowed_methods", "unknown_method"))
        };
        let allowed_methods = w
            .input
            .allowed_methods
            .iter()
            .map(|m| method(m))
            .collect::<Result<Vec<_>, _>>()?;
        let default_method = MergeMethod::parse(&w.input.default_method)
            .ok_or_else(|| param_invalid("default_method", "unknown_method"))?;
        let values = MergeSettingsValues {
            allowed_methods,
            default_method,
            delete_source_default: w.input.delete_source_default,
            reset_approvals_on_push: w.input.reset_approvals_on_push,
            committer_approvals_excluded: w.input.committer_approvals_excluded,
        };
        let project = *w.proof.id();
        let (_, stored) = review(&state)?
            .put_merge_settings(
                &request_of(&w),
                &w.proof,
                &values,
                w.input.expected_updated_at.as_deref(),
            )
            .await?;
        let viewer = w.proof.ctx().viewer(w.proof.principal());
        json(settings_body(project, &stored, viewer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PUT …/merge-settings/codeowners-enforcement` → `200` the settings.
pub async fn put_codeowners(
    state: AppState,
    request: Dispatched<CodeownersToggleCtx, PutCodeownersEnforcement>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let project = *w.proof.id();
        let (_, stored) = review(&state)?
            .toggle_codeowners(&request_of(&w), &w.proof)
            .await?;
        let viewer = w.proof.ctx().viewer(w.proof.principal());
        json(settings_body(project, &stored, viewer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}
