//! Protected ref rules of a project (M1 phase 8b MR8, ADR-0008):
//! `GET|POST /api/v1/projects/{id}/protected-refs` and
//! `PATCH|DELETE /api/v1/projects/{id}/protected-refs/{rule_id}`. Reading
//! is `ProtectedRefList` (as `ProjectRead`); every change is
//! `ProtectedRefManage`: Maintainer, and nobody sets or lifts a protection
//! above their own role.
//!
//! The registry loads the project (the one loader call of its `GET` route)
//! and, for a change or a delete by a caller who could manage protections
//! (Maintainer and above; others are decided on `from = None` without the
//! rule being read), the rule's stored minimum push role (the
//! decision's `from`, never the request's; a rule this build cannot read
//! counts as Owner-level, an absent one as none, then `FG-REPO-0005
//! resource=protected_ref`), and proves; forge-hosting decides again inside
//! the transaction on the level read under the rule's row lock. The
//! pattern is parsed after the decision (`FG-REPO-0013`); a duplicate is
//! `FG-REPO-0017` (409). Agents are refused every change (`FG-AUTH-0003
//! reason=agent_token`, ADR-0017 amendment MR8). The answer is the rule
//! re-read after the commit (`204` for a delete).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{LoadedProtection, ProjectCtx, RefProtectionCtx};
use forge_core::{parse_positive_id, ProjectId, ProtectedRefId, RefPattern, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{protection_level, RuleChange, StoredRule};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::members::RoleName;
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{
    absent, bounded, hosting, id_in, json, no_store, read_if_manager, write_request,
};

/// `GET` / `POST` a project's rules.
pub const COLLECTION: &str = "/api/v1/projects/{id}/protected-refs";
/// `PATCH` / `DELETE` one rule.
pub const ONE: &str = "/api/v1/projects/{id}/protected-refs/{rule_id}";

/// The body cap of the rule routes (a pattern ≤ 255 bytes, JSON-escaped).
pub const MAX_BODY: usize = 4 * 1024;

/// `POST …/protected-refs`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateProtectedRef {
    /// `refs/heads/…` or `refs/tags/…`, ≤ 255 bytes, at most one `*` and
    /// only in the last part (ADR-0008); unique in the project ignoring case.
    pub pattern: String,
    /// `developer`, `maintainer` or `owner`.
    pub push_min_role: RoleName,
}

/// `PATCH …/protected-refs/{rule_id}` (the pattern is fixed: delete the
/// rule and create another).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeProtectedRef {
    pub push_min_role: RoleName,
}

// Built only by `rule_body`; its doc comment is the schema description.
/// One protected ref rule as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProtectedRefBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub pattern: String,
    /// The minimum role that may push (create / fast-forward) a matching
    /// ref; force-push and deletion are refused for everyone.
    pub push_min_role: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
}

/// Every rule of a project, in id order.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProtectedRefRules {
    pub rules: Vec<ProtectedRefBody>,
}

fn rule_body(rule: &StoredRule) -> ProtectedRefBody {
    ProtectedRefBody {
        id: rule.id.get(),
        pattern: rule.pattern.as_str().to_owned(),
        push_min_role: rule.push_min_role.as_str().to_owned(),
        created_at: rule.created_at.clone(), // clone: the rule is borrowed
        updated_at: rule.updated_at.clone(),
    }
}

/// The ids of `/api/v1/projects/{id}/protected-refs/{rule_id}` (raw path).
fn ids(raw_path: &str) -> Result<(ProjectId, ProtectedRefId), ForgeError> {
    let (id, rule) = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/protected-refs/"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    if rule.contains('/') {
        return Err(param_invalid("rule_id", "not_positive_decimal"));
    }
    let rule =
        parse_positive_id(rule).ok_or_else(|| param_invalid("rule_id", "not_positive_decimal"))?;
    Ok((ProjectId::new(id), ProtectedRefId::new(rule)))
}

fn collection_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    id_in(raw_path, PREFIX, "/protected-refs")
        .map(ProjectId::new)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))
}

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

fn found<R>(ctx: R) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
    }
}

/// A change (`to: Some`) or a delete (`to: None`) of the rule in the path.
async fn rule_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
    to: Option<forge_core::Role>,
) -> Result<Lookup<RefProtectionCtx>, ForgeError> {
    let (id, rule) = ids(parts.uri.path())?;
    let Some(project) = project_of(state, id, viewer).await? else {
        return Ok(absent());
    };
    let hosting = hosting(state)?;
    // The rule's level only for a caller who could manage protections.
    let stored = read_if_manager(project.role, || {
        bounded(
            state,
            "protected_ref_load",
            hosting.protected_ref_state(id, rule),
        )
    })
    .await?;
    Ok(found(RefProtectionCtx::new(
        project,
        LoadedProtection {
            rule: Some(rule),
            from: stored.and_then(protection_level),
            to,
        },
    )))
}

/// `GET /api/v1/projects/{id}/protected-refs`: the project.
pub struct RulesOf;

impl Loader<AppState> for RulesOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let id = collection_id(parts.uri.path())?;
            Ok(match project_of(state, id, viewer).await? {
                Some(project) => found(ProjectCtx::new(project)),
                None => absent(),
            })
        })
    }
}

/// `POST /api/v1/projects/{id}/protected-refs`: the project and the level.
pub struct NewRule;

impl BodyLoader<AppState, CreateProtectedRef> for NewRule {
    type Ctx = RefProtectionCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a CreateProtectedRef,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, RefProtectionCtx> {
        Box::pin(async move {
            let id = collection_id(parts.uri.path())?;
            Ok(match project_of(state, id, viewer).await? {
                Some(project) => found(RefProtectionCtx::new(
                    project,
                    LoadedProtection {
                        rule: None,
                        from: None,
                        to: Some(body.push_min_role.role()),
                    },
                )),
                None => absent(),
            })
        })
    }
}

/// `PATCH /api/v1/projects/{id}/protected-refs/{rule_id}`.
pub struct RuleToChange;

impl BodyLoader<AppState, ChangeProtectedRef> for RuleToChange {
    type Ctx = RefProtectionCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a ChangeProtectedRef,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, RefProtectionCtx> {
        Box::pin(rule_lookup(
            state,
            parts,
            viewer,
            Some(body.push_min_role.role()),
        ))
    }
}

/// `DELETE /api/v1/projects/{id}/protected-refs/{rule_id}`.
pub struct RuleToDelete;

impl Loader<AppState> for RuleToDelete {
    type Ctx = RefProtectionCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, RefProtectionCtx> {
        Box::pin(rule_lookup(state, parts, viewer, None))
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// The rule re-read after a committed write.
async fn written(
    state: &AppState,
    project: ProjectId,
    rule: ProtectedRefId,
) -> Result<Value, ForgeError> {
    let rules = bounded(
        state,
        "protected_ref_list",
        hosting(state)?.protected_refs(project),
    )
    .await?;
    let rule = rules.iter().find(|r| r.id == rule).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "written_protected_ref_missing")
            .with_context("protected_ref_id", rule.get())
    })?;
    json(rule_body(rule))
}

/// `GET /api/v1/projects/{id}/protected-refs` → `200` every rule.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let id = *request.proof().id();
    let built = async {
        let rules = bounded(
            &state,
            "protected_ref_list",
            hosting(&state)?.protected_refs(id),
        )
        .await?;
        json(ProtectedRefRules {
            rules: rules.iter().map(rule_body).collect(),
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `POST /api/v1/projects/{id}/protected-refs` → `201` the rule.
pub async fn create(
    state: AppState,
    request: Dispatched<RefProtectionCtx, CreateProtectedRef>,
) -> Response {
    let w = request.into_write();
    let built = async {
        // After the decision: a caller who may not manage protections gets
        // the denial, never a hint about the pattern.
        let pattern = RefPattern::parse(&w.input.pattern)?;
        let rule = hosting(&state)?
            .create_protected_ref(&write_request(&w), &w.proof, &pattern)
            .await?;
        written(&state, w.proof.id().0, rule).await
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `PATCH /api/v1/projects/{id}/protected-refs/{rule_id}` → `200` the rule
/// as it is now (also when the level was already that).
pub async fn change(
    state: AppState,
    request: Dispatched<RefProtectionCtx, ChangeProtectedRef>,
) -> Response {
    let w = request.into_write();
    let built = async {
        // Changed or already that level: both answer the rule as it is.
        let (RuleChange::Changed { .. } | RuleChange::Unchanged) = hosting(&state)?
            .change_protected_ref(&write_request(&w), &w.proof)
            .await?;
        let (project, rule) = *w.proof.id();
        let rule = rule.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_rule")
        })?;
        written(&state, project, rule).await
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `DELETE /api/v1/projects/{id}/protected-refs/{rule_id}` → `204`.
pub async fn delete(state: AppState, request: Dispatched<RefProtectionCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .delete_protected_ref(&write_request(&w), &w.proof)
            .await
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
