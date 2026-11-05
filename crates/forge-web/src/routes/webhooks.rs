//! Project webhooks (M3 I2, ADR-0016 §8, plan 02 §5.5 / §11.1,
//! docs/api/webhooks.md). Maintainers and above; managing and redelivering
//! are people's (an agent's token, a service account and a bot are
//! refused).
//!
//! - `GET /api/v1/projects/{id}/webhooks` (`WebhookRead`): the project's
//!   webhooks (at most 20), never a secret.
//! - `GET …/webhooks/event-types` (`WebhookRead`): the event types a webhook
//!   may name, each with its class, and per class whether the caller may
//!   subscribe to it (the form's checkboxes).
//! - `POST …/webhooks` (`WebhookManage`): create one owned by the caller;
//!   `201` with the webhook and its `secret`, shown this once. The URL
//!   passes the operator's target policy after the decision (`400
//!   FG-INT-0010`, one sentence; the reason is in the log); a 21st is
//!   `409 FG-INT-0012`.
//! - `GET|PATCH|DELETE …/webhooks/{webhook_id}` (`WebhookRead` /
//!   `WebhookManage`): read, change (`url`, `event_types`, `enabled`;
//!   enabling clears the failure count), delete (with its history). A
//!   webhook of another project is not found after the decision.
//! - `POST …/webhooks/{webhook_id}/secret` (`WebhookManage`): a new
//!   secret, shown once; the previous one still signs for 24 hours.
//! - `GET …/webhooks/{webhook_id}/deliveries?before=&limit=`
//!   (`WebhookDeliveryRead`): the delivery history, newest first.
//! - `POST …/deliveries/{delivery_id}/redeliver` (`WebhookRedeliver`):
//!   `202`; the pair is looked up together after the decision (a delivery
//!   of another webhook is not found); `429 FG-WEB-0010` while it is
//!   pending or within 60 s of its last attempt (`Retry-After`).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{ProjectCtx, WebhookCtx, WebhookDeliveryCtx};
use forge_core::{parse_positive_id, ProjectId, UserId, WebhookDeliveryId, WebhookId};
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::webhooks::{
    Delivery, UpdateOutcome, Webhook, WebhookChanges, WebhookInput, WebhookRequest, Webhooks,
    MAX_DELIVERIES_LISTED,
};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` / `POST` a project's webhooks.
pub const COLLECTION: &str = "/api/v1/projects/{id}/webhooks";
/// `GET`: the event types a webhook may name (a static segment, ahead of
/// `{webhook_id}`).
pub const EVENT_TYPES: &str = "/api/v1/projects/{id}/webhooks/event-types";
/// `GET` / `PATCH` / `DELETE` one.
pub const ONE: &str = "/api/v1/projects/{id}/webhooks/{webhook_id}";
/// `POST`: rotate its secret.
pub const SECRET: &str = "/api/v1/projects/{id}/webhooks/{webhook_id}/secret";
/// `GET` its deliveries.
pub const DELIVERIES: &str = "/api/v1/projects/{id}/webhooks/{webhook_id}/deliveries";
/// `POST`: deliver one again.
pub const REDELIVER: &str =
    "/api/v1/projects/{id}/webhooks/{webhook_id}/deliveries/{delivery_id}/redeliver";
/// The largest body (a URL of 2 KiB and 64 event types).
pub const MAX_BODY: usize = 16 * 1024;
/// The default page of the delivery history.
const DEFAULT_DELIVERIES: i64 = 20;

/// The query parameters of the delivery history (`openapi.json`).
pub const DELIVERIES_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "before",
        description: "Only deliveries with a smaller id (the last id of the previous page)",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "limit",
        description: "Deliveries per page, 1..=100 (default 20), newest first",
        integer_range: Some((1, 100)),
    },
];

/// `POST …/webhooks`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWebhook {
    /// An absolute `https` URL (`http` only where the operator allows it)
    /// of at most 2048 bytes, without user information or a fragment, whose
    /// host and port the operator's allowlist names.
    pub url: String,
    /// `*` (every integration-class event, the default), a family
    /// (`merge_request.*`) of the integration class, or an exact type name;
    /// a maintainer- or deployment-class type only by its name; 1..=64.
    pub event_types: Option<Vec<String>>,
}

/// `PATCH …/webhooks/{webhook_id}`: the fields to change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateWebhook {
    pub url: Option<String>,
    /// Replaces the allowlist (the rules of `CreateWebhook`).
    pub event_types: Option<Vec<String>>,
    /// `true` enables it again and clears its failure count.
    pub enabled: Option<bool>,
}

/// One webhook (never a secret).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The person who answers for it: who created it, or last changed its
    /// URL or event types or rotated its secret. Deliveries stop when they
    /// can no longer manage the project's webhooks.
    #[schema(minimum = 1)]
    pub owner_id: i64,
    /// `scheme://host:port` of the URL. The full URL is write-only: its
    /// path or query may hold the receiver's token (I2 review).
    pub origin: String,
    /// Sorted.
    pub event_types: Vec<String>,
    pub enabled: bool,
    /// `manual`, `failures` (50 failed attempts in a row) or `failing_24h`.
    #[schema(required = true)]
    pub disabled_reason: Option<String>,
    /// The version of the current secret (`Forge-Signature-Kid`).
    pub secret_version: i32,
    /// The previous secret still signs until then (UTC, RFC 3339).
    #[schema(required = true)]
    pub previous_secret_valid_until: Option<String>,
    pub consecutive_failures: i32,
    #[schema(required = true)]
    pub failing_since: Option<String>,
    #[schema(required = true)]
    pub last_delivery_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A project's webhooks.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookList {
    pub items: Vec<WebhookBody>,
}

/// One event type a webhook may name.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventTypeBody {
    /// The exact name (`merge_request.opened`).
    pub name: String,
    /// `integration`, `maintainer` or `deployment` (the admin class is never
    /// listed).
    pub class: String,
}

/// One class of event types and whether the caller may subscribe to it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventClassBody {
    /// `integration` (also what `*` and `<family>.*` name), `maintainer`
    /// or `deployment` (the last two only by exact type name).
    pub class: String,
    /// `false` for a credential that may read but not subscribe (an agent's
    /// token, a service account): create and update would refuse it.
    pub subscribable: bool,
}

/// The event types a webhook may name.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventTypeList {
    /// Sorted by name.
    pub items: Vec<EventTypeBody>,
    pub classes: Vec<EventClassBody>,
}

/// A created webhook and its secret (shown this once).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatedWebhook {
    pub webhook: WebhookBody,
    /// `whsec_…`: the HMAC-SHA256 key of `Forge-Signature`. Never shown
    /// again.
    pub secret: String,
}

/// What an update changed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatedWebhook {
    /// `updated`, or `unchanged` (nothing was written).
    pub outcome: String,
    /// `url`, `event_types`, `enabled`.
    pub fields: Vec<String>,
    pub webhook: WebhookBody,
}

/// A rotated secret (shown this once).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RotatedWebhookSecret {
    pub secret: String,
    pub secret_version: i32,
    /// The previous secret still signs until then (UTC, RFC 3339).
    pub previous_secret_valid_until: String,
}

/// One delivery (never the body sent or the answer's body).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// The `Forge-Delivery` value.
    pub delivery_id: String,
    #[schema(minimum = 1)]
    pub event_seq: i64,
    /// `evt_…` (the feed's id).
    pub event_id: String,
    pub event_type: String,
    /// `pending`, `succeeded`, `failed` or `skipped`.
    pub state: String,
    pub attempts: i32,
    #[schema(required = true)]
    pub last_status: Option<i32>,
    /// `status`, `redirect`, `connect`, `timeout`, `tls`, `dns`,
    /// `target_forbidden`, … (docs/api/webhooks.md).
    #[schema(required = true)]
    pub last_reason: Option<String>,
    #[schema(required = true)]
    pub last_error_code: Option<String>,
    #[schema(required = true)]
    pub last_duration_ms: Option<i32>,
    pub redeliveries: i32,
    pub created_at: String,
    #[schema(required = true)]
    pub last_attempt_at: Option<String>,
    #[schema(required = true)]
    pub finished_at: Option<String>,
}

/// A page of the delivery history.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryList {
    pub items: Vec<DeliveryBody>,
    /// Pass as `before` for the next (older) page; `null`: no more.
    #[schema(required = true)]
    pub next_before: Option<i64>,
}

/// A redelivery was queued.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RedeliveryQueued {
    /// `queued`.
    pub outcome: String,
}

fn body(w: Webhook) -> WebhookBody {
    WebhookBody {
        id: w.id.get(),
        project_id: w.project.get(),
        owner_id: w.owner.get(),
        origin: w.origin,
        event_types: w.event_types,
        enabled: w.enabled,
        disabled_reason: w.disabled_reason.map(|r| r.as_str().to_owned()),
        secret_version: w.secret_version,
        previous_secret_valid_until: w.previous_secret_until,
        consecutive_failures: w.consecutive_failures,
        failing_since: w.failing_since,
        last_delivery_at: w.last_delivery_at,
        created_at: w.created_at,
        updated_at: w.updated_at,
    }
}

fn delivery_body(d: Delivery) -> DeliveryBody {
    DeliveryBody {
        id: d.id.get(),
        delivery_id: d.delivery_uid.to_string(),
        event_seq: d.event_seq,
        event_id: format!("evt_{}", d.event_id.simple()),
        event_type: d.event_type,
        state: d.state.as_str().to_owned(),
        attempts: d.attempts,
        last_status: d.last_status,
        last_reason: d.last_reason,
        last_error_code: d.last_error_code,
        last_duration_ms: d.last_duration_ms,
        redeliveries: d.redeliveries,
        created_at: d.created_at,
        last_attempt_at: d.last_attempt_at,
        finished_at: d.finished_at,
    }
}

pub(crate) fn webhooks(state: &AppState) -> Result<&Webhooks, ForgeError> {
    state.webhooks.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "webhooks_unconfigured")
    })
}

/// The path's ids: the project, then the webhook and delivery when the
/// route names them, and the tail (`""`, `secret`, `deliveries`,
/// `redeliver`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ids {
    project: ProjectId,
    webhook: Option<WebhookId>,
    delivery: Option<WebhookDeliveryId>,
    tail: Tail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    Collection,
    One,
    Secret,
    Deliveries,
    Redeliver,
    EventTypes,
}

fn ids(raw_path: &str) -> Result<Ids, ForgeError> {
    let bad = |name: &'static str| param_invalid(name, "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(|| bad("id"))?;
    let (project, rest) = rest.split_once("/webhooks").ok_or_else(|| bad("id"))?;
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(|| bad("id"))?;
    let segments: Vec<&str> = match rest.strip_prefix('/') {
        None if rest.is_empty() => Vec::new(),
        Some(after) => after.split('/').collect(),
        None => return Err(bad("id")),
    };
    let id = |raw: &str, name| parse_positive_id(raw).ok_or_else(|| bad(name));
    let (webhook, delivery, tail) = match segments.as_slice() {
        [] => (None, None, Tail::Collection),
        ["event-types"] => (None, None, Tail::EventTypes),
        [w] => (Some(id(w, "webhook_id")?), None, Tail::One),
        [w, "secret"] => (Some(id(w, "webhook_id")?), None, Tail::Secret),
        [w, "deliveries"] => (Some(id(w, "webhook_id")?), None, Tail::Deliveries),
        [w, "deliveries", d, "redeliver"] => (
            Some(id(w, "webhook_id")?),
            Some(id(d, "delivery_id")?),
            Tail::Redeliver,
        ),
        _ => return Err(bad("webhook_id")),
    };
    Ok(Ids {
        project,
        webhook: webhook.map(WebhookId::new),
        delivery: delivery.map(WebhookDeliveryId::new),
        tail,
    })
}

fn found_project<R>(ctx: R, record: forge_core::ProjectRecord) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: Some(record),
        group: None,
    }
}

async fn project_record(
    state: &AppState,
    id: ProjectId,
    viewer: Option<UserId>,
) -> Result<Option<forge_core::ProjectRecord>, ForgeError> {
    let Some(record) = projects::load_bounded(state, id, viewer).await? else {
        return Ok(None);
    };
    if record.id != id {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_project")
            .with_context("project_id", id.get())
            .with_context("loaded_project_id", record.id.get()));
    }
    Ok(Some(record))
}

fn wrong_tail() -> ForgeError {
    param_invalid("webhook_id", "not_positive_decimal")
}

/// `GET|POST …/webhooks`: the project.
pub struct WebhooksOf;

async fn project_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<Lookup<ProjectCtx>, ForgeError> {
    let ids = ids(parts.uri.path())?;
    if ids.tail != Tail::Collection {
        return Err(wrong_tail());
    }
    project_found(state, ids, viewer).await
}

async fn project_found(
    state: &AppState,
    ids: Ids,
    viewer: Option<UserId>,
) -> Result<Lookup<ProjectCtx>, ForgeError> {
    Ok(match project_record(state, ids.project, viewer).await? {
        Some(record) => found_project(ProjectCtx::new(projects::loaded(&record)), record),
        None => absent(),
    })
}

impl Loader<AppState> for WebhooksOf {
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

impl BodyLoader<AppState, CreateWebhook> for WebhooksOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a CreateWebhook,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(project_lookup(state, parts, viewer))
    }
}

/// `GET …/webhooks/event-types`: the project.
pub struct EventTypesOf;

impl Loader<AppState> for EventTypesOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path())?;
            if ids.tail != Tail::EventTypes {
                return Err(wrong_tail());
            }
            project_found(state, ids, viewer).await
        })
    }
}

/// `…/webhooks/{webhook_id}[/secret|/deliveries]`: the project and the
/// path's webhook id (whether it is the project's is decided after).
pub struct WebhookOf {
    tail: Tail,
}

/// `GET|PATCH|DELETE …/webhooks/{webhook_id}`.
pub const WEBHOOK: WebhookOf = WebhookOf { tail: Tail::One };
/// `POST …/{webhook_id}/secret`.
pub const WEBHOOK_SECRET: WebhookOf = WebhookOf { tail: Tail::Secret };
/// `GET …/{webhook_id}/deliveries`.
pub const WEBHOOK_DELIVERIES: WebhookOf = WebhookOf {
    tail: Tail::Deliveries,
};

impl WebhookOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<WebhookCtx>, ForgeError> {
        let ids = ids(parts.uri.path())?;
        let (Some(webhook), true) = (ids.webhook, ids.tail == self.tail) else {
            return Err(wrong_tail());
        };
        Ok(match project_record(state, ids.project, viewer).await? {
            Some(record) => {
                found_project(WebhookCtx::new(projects::loaded(&record), webhook), record)
            }
            None => absent(),
        })
    }
}

impl Loader<AppState> for WebhookOf {
    type Ctx = WebhookCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, WebhookCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, UpdateWebhook> for WebhookOf {
    type Ctx = WebhookCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a UpdateWebhook,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, WebhookCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

/// `POST …/deliveries/{delivery_id}/redeliver`.
pub struct RedeliveryOf;

impl Loader<AppState> for RedeliveryOf {
    type Ctx = WebhookDeliveryCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, WebhookDeliveryCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path())?;
            let (Some(webhook), Some(delivery), Tail::Redeliver) =
                (ids.webhook, ids.delivery, ids.tail)
            else {
                return Err(wrong_tail());
            };
            Ok(match project_record(state, ids.project, viewer).await? {
                Some(record) => found_project(
                    WebhookDeliveryCtx::new(projects::loaded(&record), webhook, delivery),
                    record,
                ),
                None => absent(),
            })
        })
    }
}

fn request_of<R: forge_auth::ResourceCtx, B>(w: &WriteParts<R, B>) -> WebhookRequest {
    WebhookRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn respond(
    ctx: &crate::Ctx,
    status: StatusCode,
    built: Result<serde_json::Value, ForgeError>,
) -> Response {
    match built {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/webhooks` → `200`.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let items = bounded(&state, "webhook_list", webhooks(&state)?.list(&w.proof)).await?;
        json(WebhookList {
            items: items.into_iter().map(body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET …/webhooks/event-types` → `200`.
pub async fn event_types(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let catalog = webhooks(&state)?.event_types(&w.proof)?;
        json(EventTypeList {
            items: catalog
                .items
                .into_iter()
                .map(|t| EventTypeBody {
                    name: t.name.to_owned(),
                    class: t.class.as_str().to_owned(),
                })
                .collect(),
            classes: catalog
                .classes
                .into_iter()
                .map(|c| EventClassBody {
                    class: c.class.as_str().to_owned(),
                    subscribable: c.subscribable,
                })
                .collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/webhooks` → `201`.
pub async fn create(state: AppState, request: Dispatched<ProjectCtx, CreateWebhook>) -> Response {
    let w = request.into_write();
    let built = async {
        let input = WebhookInput {
            url: w.input.url.clone(), // clone: the body stays with the request
            event_types: w.input.event_types.clone(), // clone: as above
        };
        let created = webhooks(&state)?
            .create(&request_of(&w), &w.proof, &input)
            .await?;
        json(CreatedWebhook {
            webhook: body(created.webhook),
            secret: created.secret.expose().to_owned(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `GET …/webhooks/{webhook_id}` → `200`.
pub async fn read(state: AppState, request: Dispatched<WebhookCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let got = bounded(&state, "webhook_read", webhooks(&state)?.get(&w.proof)).await?;
        json(body(got))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PATCH …/webhooks/{webhook_id}` → `200`.
pub async fn update(state: AppState, request: Dispatched<WebhookCtx, UpdateWebhook>) -> Response {
    let w = request.into_write();
    let built = async {
        let changes = WebhookChanges {
            url: w.input.url.clone(), // clone: the body stays with the request
            event_types: w.input.event_types.clone(), // clone: as above
            enabled: w.input.enabled,
        };
        let (outcome, stored) = webhooks(&state)?
            .update(&request_of(&w), &w.proof, &changes)
            .await?;
        let (word, fields) = match outcome {
            UpdateOutcome::Updated(fields) => (
                "updated",
                fields.iter().map(|f| f.as_str().to_owned()).collect(),
            ),
            UpdateOutcome::Unchanged => ("unchanged", Vec::new()),
        };
        json(UpdatedWebhook {
            outcome: word.to_owned(),
            fields,
            webhook: body(stored),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `DELETE …/webhooks/{webhook_id}` → `204`.
pub async fn delete(state: AppState, request: Dispatched<WebhookCtx>) -> Response {
    let w = request.into_write();
    let done = async { webhooks(&state)?.delete(&request_of(&w), &w.proof).await }.await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/webhooks/{webhook_id}/secret` → `200`.
pub async fn rotate(state: AppState, request: Dispatched<WebhookCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let rotated = webhooks(&state)?
            .rotate_secret(&request_of(&w), &w.proof)
            .await?;
        json(RotatedWebhookSecret {
            secret: rotated.secret.expose().to_owned(),
            secret_version: rotated.secret_version,
            previous_secret_valid_until: rotated.previous_valid_until,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `before` and `limit`, strictly.
fn page_query(query: Option<&str>) -> Result<(Option<WebhookDeliveryId>, i64), ForgeError> {
    let mut before: Option<&str> = None;
    let mut limit: Option<&str> = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "before" => (&mut before, "before"),
            "limit" => (&mut limit, "limit"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let before = before
        .map(|raw| {
            parse_positive_id(raw)
                .map(WebhookDeliveryId::new)
                .ok_or_else(|| param_invalid("before", "not_positive_decimal"))
        })
        .transpose()?;
    let limit = match limit {
        None => DEFAULT_DELIVERIES,
        Some(raw) => parse_positive_id(raw)
            .filter(|n| *n <= MAX_DELIVERIES_LISTED)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    Ok((before, limit))
}

/// `GET …/webhooks/{webhook_id}/deliveries` → `200`.
pub async fn deliveries(state: AppState, request: Dispatched<WebhookCtx>) -> Response {
    let query = page_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let (before, limit) = query?;
        let items = bounded(
            &state,
            "webhook_deliveries",
            webhooks(&state)?.deliveries(&w.proof, before, limit),
        )
        .await?;
        let full = i64::try_from(items.len()).unwrap_or(i64::MAX) >= limit;
        let next_before = full.then(|| items.last().map(|d| d.id.get())).flatten();
        json(DeliveryList {
            items: items.into_iter().map(delivery_body).collect(),
            next_before,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/deliveries/{delivery_id}/redeliver` → `202`.
pub async fn redeliver(state: AppState, request: Dispatched<WebhookDeliveryCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        webhooks(&state)?
            .redeliver(&request_of(&w), &w.proof)
            .await?;
        json(RedeliveryQueued {
            outcome: "queued".to_owned(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::ACCEPTED, built)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_paths_name_their_ids_and_tail() {
        let i = ids("/api/v1/projects/7/webhooks").expect("collection");
        assert_eq!(
            (i.project, i.webhook, i.tail),
            (ProjectId::new(7), None, Tail::Collection)
        );
        let i = ids("/api/v1/projects/7/webhooks/event-types").expect("event types");
        assert_eq!((i.webhook, i.tail), (None, Tail::EventTypes));
        let i = ids("/api/v1/projects/7/webhooks/3").expect("one");
        assert_eq!((i.webhook, i.tail), (Some(WebhookId::new(3)), Tail::One));
        let i = ids("/api/v1/projects/7/webhooks/3/secret").expect("secret");
        assert_eq!(i.tail, Tail::Secret);
        let i = ids("/api/v1/projects/7/webhooks/3/deliveries").expect("deliveries");
        assert_eq!(i.tail, Tail::Deliveries);
        let i = ids("/api/v1/projects/7/webhooks/3/deliveries/9/redeliver").expect("redeliver");
        assert_eq!(
            (i.webhook, i.delivery, i.tail),
            (
                Some(WebhookId::new(3)),
                Some(WebhookDeliveryId::new(9)),
                Tail::Redeliver
            )
        );
        for bad in [
            "/api/v1/projects/0/webhooks",
            "/api/v1/projects/7/webhooksx",
            "/api/v1/projects/7/webhooks/x",
            "/api/v1/projects/7/webhooks/3/other",
            "/api/v1/projects/7/webhooks/3/deliveries/x/redeliver",
        ] {
            assert!(ids(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_page_query_is_strict() {
        assert_eq!(page_query(None).expect("default"), (None, 20));
        assert_eq!(
            page_query(Some("before=9&limit=5")).expect("page"),
            (Some(WebhookDeliveryId::new(9)), 5)
        );
        for bad in ["limit=0", "limit=101", "before=x", "x=1", "limit=1&limit=2"] {
            assert!(page_query(Some(bad)).is_err(), "{bad}");
        }
    }
}
