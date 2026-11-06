//! The `webhook-dispatch` subscriber's reads and writes, in its step's
//! transaction ([`DerivedTx`]): the enabled webhooks of the event's
//! project, the material of each one's decision (forge-auth decides), and
//! one delivery row per (webhook, event) with its first job.

use forge_core::ids::{ProjectId, UserId, WebhookDeliveryId, WebhookId};
use forge_core::{RequestId, UserKind, WebhookMaterial};
use forge_errors::{ErrorCode, ForgeError};

use super::manage::first_job;
use super::MAX_WEBHOOKS;
use crate::jobs::{enqueue_on, Enqueued};
use crate::query_error;
use crate::resolve::{interpret_project, ProjectAccessRow, PROJECT_ACCESS_SQL};
use crate::roles::CAP;
use crate::subscribers::{DerivedTx, SourceEvent};

/// An enabled webhook of the event's project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchTarget {
    pub webhook: WebhookId,
}

/// The enabled webhooks of `project`, by id (at most 20).
pub async fn enabled_on(
    tx: &mut DerivedTx<'_>,
    project: ProjectId,
) -> Result<Vec<DispatchTarget>, ForgeError> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM forge.webhooks WHERE project_id = $1 AND enabled ORDER BY id LIMIT $2",
    )
    .bind(project.get())
    .bind(MAX_WEBHOOKS)
    .fetch_all(&mut *tx.conn())
    .await
    .map_err(|e| query_error("webhooks.enabled_on", e))?;
    Ok(ids
        .into_iter()
        .map(|id| DispatchTarget {
            webhook: WebhookId::new(id),
        })
        .collect())
}

type MaterialRow = (i64, i64, bool, Vec<String>, i64, bool, bool, String);

/// What forge-auth decides a webhook's deliveries on, read now on `conn`:
/// the webhook as stored and its owner's account and access facts on the
/// project. `None`: the webhook is gone.
pub(crate) async fn material_on(
    conn: &mut sqlx::postgres::PgConnection,
    webhook: WebhookId,
) -> Result<Option<WebhookMaterial>, ForgeError> {
    let row: Option<MaterialRow> = sqlx::query_as(
        "SELECT w.id, w.project_id, w.enabled, w.event_types, w.owner_user_id, \
         u.state = 'active', u.is_admin, u.kind FROM forge.webhooks w \
         JOIN forge.users u ON u.id = w.owner_user_id WHERE w.id = $1",
    )
    .bind(webhook.get())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("webhooks.material", e))?;
    let Some((id, project, enabled, event_types, owner, active, admin, kind)) = row else {
        return Ok(None);
    };
    let owner_kind = UserKind::parse(&kind).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_user_kind")
            .with_context("webhook_id", id)
    })?;
    let access: ProjectAccessRow = sqlx::query_as(PROJECT_ACCESS_SQL)
        .bind(project)
        .bind(owner)
        .bind(CAP)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| query_error("webhooks.material_access", e))?;
    let project = ProjectId::new(project);
    let owner = UserId::new(owner);
    let access = interpret_project(access, project, Some(owner))?.map(|p| p.access);
    Ok(Some(WebhookMaterial {
        webhook: WebhookId::new(id),
        project,
        enabled,
        event_types,
        owner,
        owner_active: active,
        owner_admin: admin,
        owner_kind,
        access,
    }))
}

/// [`material_on`] in the subscriber's step.
pub async fn material(
    tx: &mut DerivedTx<'_>,
    webhook: WebhookId,
) -> Result<Option<WebhookMaterial>, ForgeError> {
    material_on(tx.conn(), webhook).await
}

/// Record the delivery of `event` to `webhook` and queue its first attempt
/// (`webhook.deliver`, the event's request id). `Ok(None)`: this webhook
/// already has this event (a retried step) — nothing written.
pub async fn record_delivery(
    tx: &mut DerivedTx<'_>,
    webhook: WebhookId,
    event: &SourceEvent,
) -> Result<Option<WebhookDeliveryId>, ForgeError> {
    let project = event.project_id.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "webhook_event_without_project")
            .with_context("seq", event.seq)
    })?;
    let request_id = event.request_id().unwrap_or_else(RequestId::generate);
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO forge.webhook_deliveries (delivery_uid, webhook_id, project_id, event_seq, \
         event_id, event_type, request_id) VALUES ($1::uuid, $2, $3, $4, $5::uuid, $6, $7) \
         ON CONFLICT (webhook_id, event_seq) DO NOTHING RETURNING id",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(webhook.get())
    .bind(project.get())
    .bind(event.seq)
    .bind(event.event_id.to_string())
    .bind(&event.event_type)
    .bind(request_id.as_str())
    .fetch_optional(&mut *tx.conn())
    .await
    .map_err(|e| query_error("webhooks.record_delivery", e))?;
    let Some(id) = id else {
        return Ok(None);
    };
    let job = first_job(id, 0, &request_id, event.seq)?;
    match enqueue_on(tx.conn(), &job).await? {
        Enqueued::Queued(_) => {}
        // A key of a dead job is free again; a live one cannot exist for a
        // delivery row this statement just made.
        Enqueued::Deduped => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "webhook_job_deduped")
                .with_context("delivery_id", id))
        }
    }
    Ok(Some(WebhookDeliveryId::new(id)))
}
