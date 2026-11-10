//! Integrations and project bindings (M3 I1a, plan 11 §5 I1, 02 §2,
//! ADR-0016 Amendment I1a).
//!
//! The administration (administrators only):
//! - `POST /api/v1/admin/integrations` (`IntegrationManage`): a kanflow
//!   integration acting as a service account. **An administrator's web
//!   session only** (a token, an `admin`-scoped one included, is
//!   `FG-AUTH-0002 reason=session_required`). `201`.
//! - `GET /api/v1/admin/integrations` (`IntegrationRead`, people only;
//!   `admin` + `read_api` on a token): every integration, at most 500.
//! - `GET /api/v1/admin/integrations/{id}` (`IntegrationRead`) and `PATCH`
//!   (`IntegrationManage`, a session only): enable or disable it, replace
//!   its event type allowlist.
//! - `GET /api/v1/admin/integrations/{id}/bindings` (`IntegrationRead`):
//!   the projects bound to it, at most 200.
//!
//! A project's binding:
//! - `GET /api/v1/projects/{id}/integration-bindings` (`ProjectRead`): the
//!   project's binding (none or one).
//! - `PUT /api/v1/projects/{id}/integration-bindings/{integration_id}`
//!   (`IntegrationBindingManage`: Maintainer, people only): bind the
//!   project to `external_project_id`; the integration's service account
//!   becomes a Reporter when it is no member. `200` with `outcome` `bound`
//!   or `unchanged`;
//!   another binding of the project is `FG-INT-0016`, a 201st binding of
//!   the integration `FG-INT-0012`, a missing integration `FG-REPO-0005`.
//! - `DELETE …/integration-bindings/{integration_id}`
//!   (`IntegrationBindingManage`): unbind; the membership the binding
//!   granted is removed (when still Reporter). `204`.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{InstanceCtx, IntegrationBindingCtx, IntegrationCtx, ProjectCtx};
use forge_core::{parse_positive_id, IntegrationId, IntegrationKind, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::integrations::{
    BindOutcome, Integration, IntegrationBinding, IntegrationChanges, IntegrationFields,
    IntegrationUpdate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::{param_invalid, request_invalid};
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::projects::PREFIX as PROJECTS_PREFIX;
use crate::routes::protected_refs::{found, project_of};
use crate::routes::write::{absent, bounded, hosting, json, no_store, write_request};

/// `GET` / `POST` the integrations.
pub const COLLECTION: &str = "/api/v1/admin/integrations";
/// `GET` / `PATCH` one.
pub const ONE: &str = "/api/v1/admin/integrations/{id}";
/// `GET` its bindings.
pub const BINDINGS: &str = "/api/v1/admin/integrations/{id}/bindings";
/// `GET` a project's binding.
pub const PROJECT_BINDINGS: &str = "/api/v1/projects/{id}/integration-bindings";
/// `PUT` / `DELETE` a project's binding to one integration.
pub const PROJECT_BINDING: &str = "/api/v1/projects/{id}/integration-bindings/{integration_id}";
const PREFIX: &str = "/api/v1/admin/integrations/";
const BINDINGS_WORD: &str = "/integration-bindings";
/// The largest body.
pub const MAX_BODY: usize = 8 * 1024;

/// `POST /api/v1/admin/integrations`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIntegration {
    /// `kanflow` (the only kind).
    pub kind: String,
    /// The service account it acts as (one integration per account).
    #[schema(minimum = 1)]
    pub service_account_id: i64,
    /// The event type allowlist of its feed: `*` (every integration-class
    /// event, the default), a type (`merge_request.opened`) or a family
    /// (`merge_request.*`) of the integration class; 1..=64 entries.
    pub event_types: Option<Vec<String>>,
}

/// `PATCH /api/v1/admin/integrations/{id}`: the fields to change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIntegration {
    pub enabled: Option<bool>,
    /// Replaces the allowlist (the rules of `CreateIntegration`).
    pub event_types: Option<Vec<String>>,
}

/// `PUT /api/v1/projects/{id}/integration-bindings/{integration_id}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BindProject {
    /// The external (kanflow) project's id: 1..=100 characters of
    /// `A-Z a-z 0-9 . _ : -`.
    pub external_project_id: String,
}

/// The service account an integration acts as.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationAccountBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
}

/// One integration (no secrets: it has none yet).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// `kanflow`.
    pub kind: String,
    pub service_account: IntegrationAccountBody,
    pub enabled: bool,
    /// Sorted.
    pub event_types: Vec<String>,
    /// How many projects are bound to it (at most 200).
    pub bindings: i64,
    #[schema(required = true)]
    pub created_by: Option<i64>,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
}

/// The integrations.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationList {
    pub items: Vec<IntegrationBody>,
    /// More than 500 exist (only the first 500 by id are listed).
    pub truncated: bool,
}

/// What an update changed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatedIntegration {
    /// `updated`, or `unchanged` (nothing was written).
    pub outcome: String,
    /// The fields that changed: `enabled`, `event_types`.
    pub fields: Vec<String>,
    pub integration: IntegrationBody,
}

/// One binding of a project to an external project.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationBindingBody {
    #[schema(minimum = 1)]
    pub integration_id: i64,
    /// `kanflow`.
    pub kind: String,
    #[schema(minimum = 1)]
    pub project_id: i64,
    pub external_project_id: String,
    /// The binding made the integration's service account a Reporter of
    /// the project (unbinding removes that membership).
    pub granted_membership: bool,
    /// `true`: this is the kanflow project's code repository (its merge
    /// requests resolve work items; one kanflow project only). `false`: a
    /// shared specification repository (03 §7.1, ADR-0016 Amendment X3).
    pub resolves_work_items: bool,
    #[schema(required = true)]
    pub created_by: Option<i64>,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// Bindings (an integration's, or a project's: none or one).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationBindingList {
    pub items: Vec<IntegrationBindingBody>,
}

/// What a bind did, and the binding as stored.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundProject {
    /// `bound`, or `unchanged` (it was bound so already; nothing written).
    pub outcome: String,
    pub binding: IntegrationBindingBody,
}

fn body(i: Integration) -> IntegrationBody {
    IntegrationBody {
        id: i.id.get(),
        kind: i.kind.as_str().to_owned(),
        service_account: IntegrationAccountBody {
            id: i.service_account.get(),
            username: i.service_account_username,
        },
        enabled: i.enabled,
        event_types: i.event_types,
        bindings: i.bindings,
        created_by: i.created_by.map(UserId::get),
        created_at: i.created_at,
        updated_at: i.updated_at,
    }
}

fn binding_body(b: IntegrationBinding) -> IntegrationBindingBody {
    IntegrationBindingBody {
        integration_id: b.integration.get(),
        kind: b.kind.as_str().to_owned(),
        project_id: b.project.get(),
        external_project_id: b.external_project_id,
        granted_membership: b.granted_membership,
        resolves_work_items: b.resolves_work_items,
        created_by: b.created_by.map(UserId::get),
        created_at: b.created_at,
    }
}

fn kind(raw: &str) -> Result<IntegrationKind, ForgeError> {
    IntegrationKind::parse(raw).ok_or_else(|| {
        request_invalid("field_invalid")
            .with_context("field", "kind")
            .with_context("rule", "unknown_kind")
    })
}

fn positive(field: &'static str, raw: i64) -> Result<i64, ForgeError> {
    if raw <= 0 {
        return Err(request_invalid("field_invalid")
            .with_context("field", field)
            .with_context("rule", "not_positive"));
    }
    Ok(raw)
}

/// The `{id}` of a path under [`PREFIX`] and what follows it (`""` or
/// `bindings`).
fn admin_id(path: &str) -> Result<(IntegrationId, &str), ForgeError> {
    let rest = path
        .strip_prefix(PREFIX)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let (raw, tail) = rest.split_once('/').map_or((rest, ""), |(id, t)| (id, t));
    let id = parse_positive_id(raw).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    Ok((IntegrationId::new(id), tail))
}

/// The project id of `/api/v1/projects/{id}/integration-bindings[/{integration_id}]`
/// and the integration id (raw path).
fn binding_ids(raw_path: &str) -> Result<(ProjectId, Option<IntegrationId>), ForgeError> {
    let (id, rest) = raw_path
        .strip_prefix(PROJECTS_PREFIX)
        .and_then(|rest| rest.split_once(BINDINGS_WORD))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let integration = match rest {
        "" => None,
        other => {
            let raw = other
                .strip_prefix('/')
                .filter(|r| !r.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("integration_id", "not_positive_decimal"))?;
            Some(IntegrationId::new(raw))
        }
    };
    Ok((ProjectId::new(id), integration))
}

fn found_ctx<R>(ctx: R) -> Lookup<R> {
    found(ctx)
}

/// `GET|PATCH /api/v1/admin/integrations/{id}` and `GET …/bindings`.
pub struct OneIntegration {
    /// The path's tail after the id: `""` or `bindings`.
    tail: &'static str,
}

/// The loader of `GET|PATCH …/{id}`.
pub const INTEGRATION: OneIntegration = OneIntegration { tail: "" };
/// The loader of `GET …/{id}/bindings`.
pub const INTEGRATION_BINDINGS: OneIntegration = OneIntegration { tail: "bindings" };

impl OneIntegration {
    fn load_ctx<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
    ) -> LoadFuture<'a, IntegrationCtx> {
        Box::pin(async move {
            let (id, tail) = admin_id(parts.uri.path())?;
            if tail != self.tail {
                return Err(param_invalid("id", "not_positive_decimal"));
            }
            let exists = bounded(
                state,
                "integration_load",
                hosting(state)?.integration_exists(id),
            )
            .await?;
            Ok(if exists {
                found_ctx(IntegrationCtx::new(id))
            } else {
                absent()
            })
        })
    }
}

impl Loader<AppState> for OneIntegration {
    type Ctx = IntegrationCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationCtx> {
        self.load_ctx(state, parts)
    }
}

impl BodyLoader<AppState, UpdateIntegration> for OneIntegration {
    type Ctx = IntegrationCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a UpdateIntegration,
        _: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationCtx> {
        self.load_ctx(state, parts)
    }
}

/// `POST /api/v1/admin/integrations`: decided on the instance (the service
/// account named in the body is checked by the command, under its lock).
pub struct NewIntegration;

impl BodyLoader<AppState, CreateIntegration> for NewIntegration {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a CreateIntegration,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async { Ok(found_ctx(InstanceCtx)) })
    }
}

/// `GET /api/v1/projects/{id}/integration-bindings`: the project.
pub struct BindingsOf;

impl Loader<AppState> for BindingsOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let (id, integration) = binding_ids(parts.uri.path())?;
            if integration.is_some() {
                return Err(param_invalid("id", "not_positive_decimal"));
            }
            Ok(match project_of(state, id, viewer).await? {
                Some(project) => found_ctx(ProjectCtx::new(project)),
                None => absent(),
            })
        })
    }
}

/// `PUT|DELETE …/integration-bindings/{integration_id}`: the project as
/// loaded for the caller and the integration of the path (whether it
/// exists is the command's, after the decision).
pub struct BindingOf;

async fn binding_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<Lookup<IntegrationBindingCtx>, ForgeError> {
    let (id, integration) = binding_ids(parts.uri.path())?;
    let integration =
        integration.ok_or_else(|| param_invalid("integration_id", "not_positive_decimal"))?;
    Ok(match project_of(state, id, viewer).await? {
        Some(project) => found_ctx(IntegrationBindingCtx::new(project, integration)),
        None => absent(),
    })
}

impl Loader<AppState> for BindingOf {
    type Ctx = IntegrationBindingCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationBindingCtx> {
        Box::pin(binding_lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, BindProject> for BindingOf {
    type Ctx = IntegrationBindingCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a BindProject,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationBindingCtx> {
        Box::pin(binding_lookup(state, parts, viewer))
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, built: Result<Value, ForgeError>) -> Response {
    match built {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// The row vanished between the write and the read (integrations and
/// bindings are only removed by commands that could not run in between):
/// `FG-SYS-0001`.
fn vanished(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("id", id)
}

/// `POST /api/v1/admin/integrations` → `201`.
pub async fn create(
    state: AppState,
    request: Dispatched<InstanceCtx, CreateIntegration>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let input = &w.input;
        let fields = IntegrationFields {
            kind: kind(&input.kind)?,
            service_account: UserId::new(positive("service_account_id", input.service_account_id)?),
            event_types: input.event_types.clone(), // clone: the body stays with the request
        };
        let hosting = hosting(&state)?;
        let id = hosting
            .create_integration(&w.proof, &write_request(&w), &fields)
            .await?;
        let stored = bounded(
            &state,
            "integration_read",
            hosting.created_integration(&w.proof, id),
        )
        .await?
        .ok_or_else(|| vanished("integration_vanished", id.get()))?;
        json(body(stored))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `GET /api/v1/admin/integrations` → `200`.
pub async fn list(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let listed = bounded(
            &state,
            "integration_list",
            hosting(&state)?.integrations(&w.proof),
        )
        .await?;
        json(IntegrationList {
            items: listed.items.into_iter().map(body).collect(),
            truncated: listed.truncated,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/admin/integrations/{id}` → `200`.
pub async fn read(state: AppState, request: Dispatched<IntegrationCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let id = *w.proof.id();
        let got = bounded(
            &state,
            "integration_read",
            hosting(&state)?.integration(&w.proof),
        )
        .await?
        .ok_or_else(|| vanished("integration_vanished", id.get()))?;
        json(body(got))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PATCH /api/v1/admin/integrations/{id}` → `200`.
pub async fn update(
    state: AppState,
    request: Dispatched<IntegrationCtx, UpdateIntegration>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let changes = IntegrationChanges {
            enabled: w.input.enabled,
            event_types: w.input.event_types.clone(), // clone: the body stays with the request
        };
        let hosting = hosting(&state)?;
        let outcome = hosting
            .update_integration(&w.proof, &write_request(&w), &changes)
            .await?;
        let id = *w.proof.id();
        let stored = bounded(&state, "integration_read", hosting.integration(&w.proof))
            .await?
            .ok_or_else(|| vanished("integration_vanished", id.get()))?;
        let (outcome, fields) = match outcome {
            IntegrationUpdate::Updated(fields) => (
                "updated",
                fields.iter().map(|f| f.as_str().to_owned()).collect(),
            ),
            IntegrationUpdate::Unchanged => ("unchanged", Vec::new()),
        };
        json(UpdatedIntegration {
            outcome: outcome.to_owned(),
            fields,
            integration: body(stored),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/admin/integrations/{id}/bindings` → `200`.
pub async fn bindings(state: AppState, request: Dispatched<IntegrationCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let listed = bounded(
            &state,
            "integration_bindings",
            hosting(&state)?.integration_bindings(&w.proof),
        )
        .await?;
        json(IntegrationBindingList {
            items: listed.into_iter().map(binding_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/projects/{id}/integration-bindings` → `200`.
pub async fn project_bindings(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let listed = bounded(
            &state,
            "project_integration_bindings",
            hosting(&state)?.project_integration_bindings(&w.proof),
        )
        .await?;
        json(IntegrationBindingList {
            items: listed.into_iter().map(binding_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PUT /api/v1/projects/{id}/integration-bindings/{integration_id}` →
/// `200` with `outcome` `bound` or `unchanged`.
pub async fn bind(
    state: AppState,
    request: Dispatched<IntegrationBindingCtx, BindProject>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let (outcome, stored) = hosting(&state)?
            .bind_integration(&w.proof, &write_request(&w), &w.input.external_project_id)
            .await?;
        let word = match outcome {
            BindOutcome::Bound { .. } => "bound",
            BindOutcome::Unchanged => "unchanged",
        };
        json(BoundProject {
            outcome: word.to_owned(),
            binding: binding_body(stored),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `DELETE /api/v1/projects/{id}/integration-bindings/{integration_id}` →
/// `204`.
pub async fn unbind(state: AppState, request: Dispatched<IntegrationBindingCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .unbind_integration(&w.proof, &write_request(&w))
            .await
            .map(|_| ())
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
