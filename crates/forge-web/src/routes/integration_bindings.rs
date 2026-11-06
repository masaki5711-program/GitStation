//! A kanflow project's repositories (M3 X3, plan 03 §7.1, plan 02 §2 /
//! §5.3, ADR-0016 Amendment X3, ADR-0025 Amendment X3). Kanflow's PO / SM
//! registers repositories from kanflow's screen; forge decides every
//! change as the person who pressed it (act-as with an assertion they
//! confirmed in their forge session), never as the integration's service
//! account on its own.
//!
//! - `GET /api/v1/repositories?q=&limit=` (`RepositorySearch`): the
//!   repositories the caller — or the person acted for, without an
//!   assertion — could bind (Maintainer and above), whose path or name
//!   contains `q`. Every candidate is decided with `authorize()`; nothing is
//!   said about the others (no count, no hint: no existence oracle).
//! - `GET /api/v1/integrations/{id}/bindings?kanflow_project_id=`
//!   (`IntegrationBindingList`): the repositories bound to the integration
//!   (to one kanflow project). The integration's own service account.
//! - `POST /api/v1/integrations/{id}/bindings` (`IntegrationBindingManage`)
//!   `{kanflow_project_id, repository_id, settings: {resolves_work_items}}`:
//!   bind; `200` `bound` / `unchanged`. A repository with code
//!   (`resolves_work_items: true`, the default) is in one kanflow project
//!   only, a shared spec repository (`false`) in several (`FG-INT-0016`).
//! - `PATCH …/bindings/{binding_id}` (`IntegrationBindingManage`)
//!   `{settings: {resolves_work_items}}`: change the setting (`false` →
//!   `true` only); `DELETE …/bindings/{binding_id}`
//!   (`IntegrationBindingRemove`): unbind; `204`.
//! - `POST /-/confirm/projects/{id}/integration-bindings/assertions`
//!   (`ActAsAssertionIssue`, a person's web session, CSRF): the person
//!   confirms `binding_set` (with the setting) or `binding_remove` for one
//!   kanflow project; a pending assertion for the integration to claim.
//!
//! The loaders of the three binding writes add, for an act-as request, the
//! binding the request writes (the assertion is compared with it); the
//! project is loaded for the caller (the person), so an invisible project
//! and an absent one answer the same `FG-REPO-0005`.

use std::time::Instant;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    prove, ActAsBindingConfirmCtx, Action, BindingSubject, InstanceCtx, IntegrationBindingCtx,
    IntegrationFeedCtx, Principal, ProjectCtx,
};
use forge_core::act_as::Intent;
use forge_core::integration::external_project_id_ok;
use forge_core::{parse_positive_id, IntegrationId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::integrations::{
    BindOutcome, BindingUpdate, BoundRepository, IntegrationBinding,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::{param_invalid, request_invalid};
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::protected_refs::{found, project_of};
use crate::routes::write::{absent, bounded, hosting, json, no_store, write_request};

/// `GET` the repositories the caller may bind.
pub const SEARCH: &str = "/api/v1/repositories";
/// `GET` / `POST` an integration's bindings.
pub const BINDINGS: &str = "/api/v1/integrations/{id}/bindings";
/// `PATCH` / `DELETE` one binding.
pub const BINDING: &str = "/api/v1/integrations/{id}/bindings/{binding_id}";
/// `POST` the person's confirmation of a binding intent.
pub const CONFIRM: &str = "/-/confirm/projects/{id}/integration-bindings/assertions";
const PREFIX: &str = "/api/v1/integrations/";
const BINDINGS_WORD: &str = "/bindings";
const CONFIRM_PREFIX: &str = "/-/confirm/projects/";
const CONFIRM_SUFFIX: &str = "/integration-bindings/assertions";
/// The bodies are small.
pub const MAX_BODY: usize = 4 * 1024;
/// The search's default and largest page.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
pub const MAX_SEARCH_LIMIT: usize = 50;
/// Searches acting for a person, per integration's service account a
/// minute (review, security MEDIUM): `FG-WEB-0010` beyond.
pub const ACT_AS_SEARCHES_PER_MINUTE: u32 = 30;

/// The limiter of [`ACT_AS_SEARCHES_PER_MINUTE`].
pub fn act_as_search_limiter() -> crate::routes::namespaces::CallerLimiter {
    crate::routes::namespaces::CallerLimiter::new(
        "act_as_repository_search",
        ACT_AS_SEARCHES_PER_MINUTE,
        std::time::Duration::from_secs(60),
    )
}

pub const SEARCH_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "q",
        description:
            "Text in the repository's path or name, 1..=100 characters (ASCII case folded)",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "At most this many repositories, 1..=50 (default 20)",
        integer_range: Some((1, 50)),
    },
];

pub const BINDINGS_QUERY: &[QueryParam] = &[QueryParam {
    name: "kanflow_project_id",
    description: "Only the bindings of this kanflow project (1..=100 of A-Z a-z 0-9 . _ : -)",
    integer_range: None,
}];

// ── bodies ──────────────────────────────────────────────────────────────

/// A binding's settings.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BindingSettings {
    /// Resolve `PBI-123` in this repository's branches and merge requests
    /// (a repository with code: one kanflow project only). `false`: a
    /// shared spec repository, bound to several kanflow projects.
    pub resolves_work_items: bool,
}

/// `POST /api/v1/integrations/{id}/bindings`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewRepositoryBinding {
    /// The kanflow project (1..=100 of `A-Z a-z 0-9 . _ : -`).
    pub kanflow_project_id: String,
    /// The repository's id (`GET /api/v1/repositories` answers it).
    #[schema(minimum = 1)]
    pub repository_id: i64,
    /// Default `{"resolves_work_items": true}`.
    pub settings: Option<BindingSettings>,
}

/// `PATCH /api/v1/integrations/{id}/bindings/{binding_id}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeRepositoryBinding {
    pub settings: BindingSettings,
}

/// A repository as the binding routes name it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BindableRepository {
    #[schema(minimum = 1)]
    pub id: i64,
    /// The full path (`group/name`).
    pub path: String,
    pub display_name: String,
}

/// `GET /api/v1/repositories`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BindableRepositoryList {
    pub items: Vec<BindableRepository>,
}

/// One binding of a repository to a kanflow project.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBindingBody {
    #[schema(minimum = 1)]
    pub binding_id: i64,
    #[schema(minimum = 1)]
    pub integration_id: i64,
    pub kanflow_project_id: String,
    #[schema(minimum = 1)]
    pub repository_id: i64,
    /// `null` only when the repository's path cannot be read (never for a
    /// live one).
    #[schema(required = true)]
    pub repository_path: Option<String>,
    pub settings: BindingSettings,
    /// The binding made the integration's service account a Reporter.
    pub granted_membership: bool,
    #[schema(required = true)]
    pub created_by: Option<i64>,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// `GET /api/v1/integrations/{id}/bindings`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBindingList {
    pub items: Vec<RepositoryBindingBody>,
}

/// What a bind or a change did, and the binding as stored.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBindingOutcome {
    /// `bound` / `updated`, or `unchanged` (nothing written).
    pub outcome: String,
    pub binding: RepositoryBindingBody,
}

/// `POST /-/confirm/projects/{id}/integration-bindings/assertions`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueBindingAssertion {
    /// `binding_set` or `binding_remove`.
    pub intent: String,
    #[schema(minimum = 1)]
    pub integration: i64,
    pub kanflow_project_id: String,
    /// The setting confirmed: required for `binding_set`, absent for
    /// `binding_remove`.
    pub resolves_work_items: Option<bool>,
    /// The integration's state from the screen's URL (22..=128 of
    /// `[A-Za-z0-9_-]`; only its SHA-256 is kept).
    pub state: String,
}

/// `POST …/integration-bindings/assertions` → `201`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BindingAssertionIssued {
    /// Always `issued`.
    pub status: String,
}

fn binding_body(b: IntegrationBinding, path: Option<String>) -> RepositoryBindingBody {
    RepositoryBindingBody {
        binding_id: b.id,
        integration_id: b.integration.get(),
        kanflow_project_id: b.external_project_id,
        repository_id: b.project.get(),
        repository_path: path,
        settings: BindingSettings {
            resolves_work_items: b.resolves_work_items,
        },
        granted_membership: b.granted_membership,
        created_by: b.created_by.map(UserId::get),
        created_at: b.created_at,
    }
}

fn listed(b: BoundRepository) -> RepositoryBindingBody {
    binding_body(b.binding, b.path)
}

fn external_param(field: &'static str, raw: &str) -> Result<(), ForgeError> {
    if external_project_id_ok(raw) {
        Ok(())
    } else {
        Err(request_invalid("field_invalid")
            .with_context("field", field)
            .with_context("rule", "format"))
    }
}

/// `{id}` and the rest of `/api/v1/integrations/{id}/bindings[/{binding_id}]`.
fn binding_path(raw: &str) -> Result<(IntegrationId, Option<i64>), ForgeError> {
    let (id, rest) = raw
        .strip_prefix(PREFIX)
        .and_then(|r| r.split_once(BINDINGS_WORD))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let binding = match rest {
        "" => None,
        other => Some(
            other
                .strip_prefix('/')
                .filter(|r| !r.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("binding_id", "not_positive_decimal"))?,
        ),
    };
    Ok((IntegrationId::new(id), binding))
}

/// Whether the request acts for a person.
fn acting(parts: &Parts) -> bool {
    parts
        .extensions
        .get::<Principal>()
        .is_some_and(|p| p.act_as_credential().is_some())
}

// ── the search ──────────────────────────────────────────────────────────

/// `GET /api/v1/repositories`: decided on the instance.
pub struct SearchOf;

impl Loader<AppState> for SearchOf {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async { Ok(found(InstanceCtx)) })
    }
}

/// `q` and `limit`, strictly (each once; nothing else).
fn search_query(query: Option<&str>) -> Result<(String, usize), ForgeError> {
    let (mut q, mut limit) = (None, None);
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "q" => (&mut q, "q"),
            "limit" => (&mut limit, "limit"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let q = q.ok_or_else(|| param_invalid("q", "missing"))?;
    let q = crate::routes::namespaces::percent_decode(&q.replace('+', " "))
        .ok_or_else(|| param_invalid("q", "not_utf8"))?;
    let limit = match limit {
        None => DEFAULT_SEARCH_LIMIT,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_SEARCH_LIMIT).contains(n) && !raw.starts_with('0'))
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    Ok((q, limit))
}

/// `GET /api/v1/repositories` → `200`.
pub async fn search(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let w = request.into_write();
    let built = async {
        let (q, limit) = search_query(query.as_deref())?;
        let principal = w.proof.principal();
        // Acting for a person: a budget per integration's service account.
        if let Some(c) = principal.act_as_credential() {
            state
                .act_as_search_limiter
                .check(c.via, Instant::now())
                .map_err(|retry| state.act_as_search_limiter.refused(retry))?;
        }
        let hosting = hosting(&state)?;
        let mut items = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..forge_hosting::integrations::SEARCH_PAGES {
            let candidates = bounded(
                &state,
                "repository_search",
                hosting.repository_candidates(&w.proof, &q, after.as_deref()),
            )
            .await?;
            let full = candidates.len()
                >= usize::try_from(forge_hosting::integrations::SEARCH_PAGE).unwrap_or(usize::MAX);
            after = candidates.last().map(|r| r.path.full_path());
            for record in candidates {
                if items.len() >= limit {
                    break;
                }
                // Every candidate through the single decision point: only
                // what the caller (the person acted for) could bind.
                let ctx = ProjectCtx::new(crate::routes::projects::loaded(&record));
                match prove(principal, Action::RepositorySearch, ctx) {
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.code(),
                            ErrorCode::NotFound | ErrorCode::Forbidden | ErrorCode::PendingDeletion
                        ) =>
                    {
                        continue
                    }
                    Err(e) => return Err(e),
                }
                items.push(BindableRepository {
                    id: record.id.get(),
                    path: record.path.full_path(),
                    display_name: record.display_name.clone(), // clone: the body owns it
                });
            }
            if items.len() >= limit || !full {
                break;
            }
        }
        hosting
            .record_repository_search(&w.proof, &write_request(&w), &q, items.len())
            .await?;
        json(BindableRepositoryList { items })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

// ── the integration's list ──────────────────────────────────────────────

/// `GET /api/v1/integrations/{id}/bindings`: the integration, for the
/// caller (its own service account decides).
pub struct ListOf;

impl Loader<AppState> for ListOf {
    type Ctx = IntegrationFeedCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationFeedCtx> {
        Box::pin(async move {
            let (integration, binding) = binding_path(parts.uri.path())?;
            if binding.is_some() {
                return Err(param_invalid("id", "not_positive_decimal"));
            }
            let loaded = bounded(
                state,
                "integration_load",
                hosting(state)?.integration_feed_target(integration),
            )
            .await?;
            Ok(match loaded {
                Some(loaded) => found(IntegrationFeedCtx::new(loaded, viewer, acting(parts))),
                None => absent(),
            })
        })
    }
}

fn list_query(query: Option<&str>) -> Result<Option<String>, ForgeError> {
    let mut external = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "kanflow_project_id" {
            return Err(param_invalid("query", "unknown_parameter"));
        }
        if external.replace(value.to_owned()).is_some() {
            return Err(param_invalid("kanflow_project_id", "repeated"));
        }
    }
    if let Some(e) = external.as_deref() {
        if !external_project_id_ok(e) {
            return Err(param_invalid("kanflow_project_id", "format"));
        }
    }
    Ok(external)
}

/// `GET /api/v1/integrations/{id}/bindings` → `200`.
pub async fn list(state: AppState, request: Dispatched<IntegrationFeedCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let w = request.into_write();
    let built = async {
        let external = list_query(query.as_deref())?;
        let rows = bounded(
            &state,
            "integration_repositories",
            hosting(&state)?.integration_repositories(&w.proof, external.as_deref()),
        )
        .await?;
        json(RepositoryBindingList {
            items: rows.into_iter().map(listed).collect(),
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

// ── the writes ──────────────────────────────────────────────────────────

/// `POST …/bindings`: the repository as loaded for the caller, the
/// integration of the path, and — acting for a person — what is written.
pub struct NewBindingOf;

impl BodyLoader<AppState, NewRepositoryBinding> for NewBindingOf {
    type Ctx = IntegrationBindingCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a NewRepositoryBinding,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationBindingCtx> {
        Box::pin(async move {
            let (integration, binding) = binding_path(parts.uri.path())?;
            if binding.is_some() {
                return Err(param_invalid("id", "not_positive_decimal"));
            }
            external_param("kanflow_project_id", &body.kanflow_project_id)?;
            if body.repository_id <= 0 {
                return Err(request_invalid("field_invalid")
                    .with_context("field", "repository_id")
                    .with_context("rule", "not_positive"));
            }
            let project = ProjectId::new(body.repository_id);
            let Some(loaded) = project_of(state, project, viewer).await? else {
                return Ok(absent());
            };
            let ctx = IntegrationBindingCtx::new(loaded, integration);
            let resolves = body.settings.as_ref().is_none_or(|s| s.resolves_work_items);
            Ok(found(if acting(parts) {
                ctx.acting(BindingSubject {
                    integration,
                    project,
                    external_project_id: body.kanflow_project_id.clone(), // clone: the subject owns it
                    resolves_work_items: Some(resolves),
                })
            } else {
                ctx
            }))
        })
    }
}

/// `PATCH|DELETE …/bindings/{binding_id}`: the binding of the integration
/// (another integration's, or none: absent), its project as loaded for the
/// caller, and — acting for a person — what is written.
pub struct BindingOf;

async fn binding_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
    resolves: Option<bool>,
) -> Result<Lookup<IntegrationBindingCtx>, ForgeError> {
    let (integration, binding) = binding_path(parts.uri.path())?;
    let binding = binding.ok_or_else(|| param_invalid("binding_id", "not_positive_decimal"))?;
    let Some(stored) = bounded(
        state,
        "integration_binding_load",
        hosting(state)?.integration_binding(integration, binding),
    )
    .await?
    else {
        return Ok(absent());
    };
    let Some(loaded) = project_of(state, stored.project, viewer).await? else {
        return Ok(absent());
    };
    let ctx = IntegrationBindingCtx::new(loaded, integration).with_binding(binding);
    Ok(found(if acting(parts) {
        ctx.acting(BindingSubject {
            integration,
            project: stored.project,
            external_project_id: stored.external_project_id,
            resolves_work_items: resolves,
        })
    } else {
        ctx
    }))
}

impl Loader<AppState> for BindingOf {
    type Ctx = IntegrationBindingCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationBindingCtx> {
        Box::pin(binding_lookup(state, parts, viewer, None))
    }
}

impl BodyLoader<AppState, ChangeRepositoryBinding> for BindingOf {
    type Ctx = IntegrationBindingCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a ChangeRepositoryBinding,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationBindingCtx> {
        Box::pin(binding_lookup(
            state,
            parts,
            viewer,
            Some(body.settings.resolves_work_items),
        ))
    }
}

fn respond(ctx: &crate::Ctx, built: Result<Value, ForgeError>) -> Response {
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `POST /api/v1/integrations/{id}/bindings` → `200` `bound` / `unchanged`.
pub async fn bind(
    state: AppState,
    request: Dispatched<IntegrationBindingCtx, NewRepositoryBinding>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let resolves = w
            .input
            .settings
            .as_ref()
            .is_none_or(|s| s.resolves_work_items);
        let (outcome, stored) = hosting(&state)?
            .bind_repository(
                &w.proof,
                &write_request(&w),
                &w.input.kanflow_project_id,
                resolves,
            )
            .await?;
        let word = match outcome {
            BindOutcome::Bound { .. } => "bound",
            BindOutcome::Unchanged => "unchanged",
        };
        json(RepositoryBindingOutcome {
            outcome: word.to_owned(),
            binding: binding_body(stored, None),
        })
    }
    .await;
    respond(&w.ctx, built)
}

/// `PATCH …/bindings/{binding_id}` → `200` `updated` / `unchanged`.
pub async fn change(
    state: AppState,
    request: Dispatched<IntegrationBindingCtx, ChangeRepositoryBinding>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let (outcome, stored) = hosting(&state)?
            .change_binding(
                &w.proof,
                &write_request(&w),
                w.input.settings.resolves_work_items,
            )
            .await?;
        let word = match outcome {
            BindingUpdate::Updated => "updated",
            BindingUpdate::Unchanged => "unchanged",
        };
        json(RepositoryBindingOutcome {
            outcome: word.to_owned(),
            binding: binding_body(stored, None),
        })
    }
    .await;
    respond(&w.ctx, built)
}

/// `DELETE …/bindings/{binding_id}` → `204`.
pub async fn remove(state: AppState, request: Dispatched<IntegrationBindingCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .remove_binding(&w.proof, &write_request(&w))
            .await
            .map(|_| ())
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

// ── the person's confirmation ───────────────────────────────────────────

fn binding_intent(raw: &str) -> Result<Intent, ForgeError> {
    match Intent::parse(raw) {
        Some(i @ (Intent::BindingSet | Intent::BindingRemove)) => Ok(i),
        Some(Intent::Approve | Intent::Unapprove) | None => Err(request_invalid("field_invalid")
            .with_context("field", "intent")
            .with_context("rule", "unknown_intent")),
    }
}

/// `POST …/integration-bindings/assertions`: the project as loaded for the
/// person, the intent, and the integration with whether it is bound to
/// the project for that kanflow project (absent: the same 404).
pub struct ConfirmOf;

impl BodyLoader<AppState, IssueBindingAssertion> for ConfirmOf {
    type Ctx = ActAsBindingConfirmCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a IssueBindingAssertion,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ActAsBindingConfirmCtx> {
        Box::pin(async move {
            let project =
                crate::routes::write::id_in(parts.uri.path(), CONFIRM_PREFIX, CONFIRM_SUFFIX)
                    .map(ProjectId::new)
                    .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
            let intent = binding_intent(&body.intent)?;
            external_param("kanflow_project_id", &body.kanflow_project_id)?;
            if (intent == Intent::BindingSet) != body.resolves_work_items.is_some() {
                return Err(request_invalid("field_invalid")
                    .with_context("field", "resolves_work_items")
                    .with_context("rule", "required_for_binding_set_only"));
            }
            if body.integration <= 0 {
                return Err(param_invalid("integration", "not_positive_decimal"));
            }
            let Some(loaded) = project_of(state, project, viewer).await? else {
                return Ok(absent());
            };
            let act_as = state.act_as.as_deref().ok_or_else(|| {
                ForgeError::new(ErrorCode::Forbidden)
                    .with_context("reason", "act_as_not_configured")
            })?;
            let integration = bounded(
                state,
                "act_as_confirm_integration",
                act_as.confirm_binding_integration(
                    IntegrationId::new(body.integration),
                    project,
                    &body.kanflow_project_id,
                ),
            )
            .await?;
            Ok(match integration {
                Some(i) => found(ActAsBindingConfirmCtx::new(loaded, intent, i)),
                None => absent(),
            })
        })
    }
}

/// `POST …/integration-bindings/assertions` → `201` a pending assertion.
pub async fn confirm(
    state: AppState,
    request: Dispatched<ActAsBindingConfirmCtx, IssueBindingAssertion>,
) -> Response {
    let session = request.session().map(|s| s.session_id);
    let w = request.into_write();
    let built = async {
        let person = w.proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
        })?;
        state
            .act_as_issue_limiter
            .check(person, Instant::now())
            .map_err(|retry| state.act_as_issue_limiter.refused(retry))?;
        let act_as = state.act_as.as_deref().ok_or_else(|| {
            ForgeError::new(ErrorCode::Forbidden).with_context("reason", "act_as_not_configured")
        })?;
        act_as
            .issue_binding(
                &w.proof,
                &w.ctx.request_id,
                session,
                &w.input.kanflow_project_id,
                w.input.resolves_work_items,
                &w.input.state,
            )
            .await?;
        json(BindingAssertionIssued {
            status: "issued".to_owned(),
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::CREATED, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binding_paths_parse_strictly() {
        assert_eq!(
            binding_path("/api/v1/integrations/5/bindings").ok(),
            Some((IntegrationId::new(5), None))
        );
        assert_eq!(
            binding_path("/api/v1/integrations/5/bindings/9").ok(),
            Some((IntegrationId::new(5), Some(9)))
        );
        for bad in [
            "/api/v1/integrations/0/bindings",
            "/api/v1/integrations/5/bindings/x",
            "/api/v1/integrations/5/bindings/9/more",
        ] {
            assert!(binding_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_search_query_is_strict() {
        assert_eq!(
            search_query(Some("q=spec%2Fzeva")).ok(),
            Some(("spec/zeva".to_owned(), DEFAULT_SEARCH_LIMIT))
        );
        assert_eq!(
            search_query(Some("q=a&limit=50")).ok(),
            Some(("a".to_owned(), 50))
        );
        for bad in [
            "",
            "limit=5",
            "q=a&q=b",
            "q=a&limit=51",
            "q=a&limit=0",
            "q=a&x=1",
        ] {
            assert!(search_query(Some(bad)).is_err(), "{bad}");
        }
        assert!(list_query(Some("kanflow_project_id=a/b")).is_err());
        assert_eq!(
            list_query(Some("kanflow_project_id=KF-1")).ok(),
            Some(Some("KF-1".to_owned()))
        );
        assert!(binding_intent("approve").is_err());
    }
}
