//! Creating, changing, deleting a webhook, rotating its secret and asking
//! for a delivery again — decided inside the transaction (module docs).

use std::time::Duration;

use forge_core::ids::{ProjectId, UserId, WebhookDeliveryId, WebhookId};
use forge_core::webhook::{SealedSecret, WebhookChange, WebhookField, SECRET_OVERLAP};
use forge_core::{AuditDetails, DedupKey, DedupPart, DisableReason, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use super::{not_found, utc, DeliveryState, MAX_WEBHOOKS, RESOURCE};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::jobs::{enqueue_on, DedupKey as JobDedupKey, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::{query_error, Db};

/// A webhook to create: its URL and event types checked by forge-core, its
/// row's `uid` and its secret sealed for that `uid` (forge-integ).
#[derive(Debug, Clone)]
pub struct NewWebhook {
    pub uid: uuid::Uuid,
    /// As `WebhookUrl::as_str` (passed the target policy).
    pub url: String,
    /// As `WebhookUrl::origin` (the audit row's).
    pub origin: String,
    /// As `webhook_event_types` returned them.
    pub event_types: Vec<String>,
    pub secret: SealedSecret,
}

/// The fields an update may change (`None`: unchanged). `url` is the
/// checked URL and its origin.
#[derive(Debug, Clone, Default)]
pub struct WebhookPatch {
    pub url: Option<(String, String)>,
    pub event_types: Option<Vec<String>>,
    pub enabled: Option<bool>,
}

/// What [`update_decided`] did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated(Vec<WebhookField>),
    Unchanged,
}

/// What [`rotate_secret_decided`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotated {
    pub secret_version: i32,
    /// RFC 3339, UTC: the previous secret signs until then.
    pub previous_valid_until: String,
}

/// The change's audit row on the project and, for `webhook.disabled`, its
/// event (`dedup`).
pub(super) fn record(
    details: &AuditDetails,
    project: ProjectId,
    dedup: Option<DedupKey>,
) -> Result<ChangeRecord, ForgeError> {
    let value = details.to_value();
    let events = match details.event_type() {
        Some(event_type) => vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: RESOURCE,
            aggregate_id: project.get(),
            project_id: Some(project),
            payload: value.clone(), // clone: the audit row keeps the same JSON
            dedup_key: dedup,
        }],
        None => Vec::new(),
    };
    Ok(ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value,
        },
        affected: Vec::new(),
        events,
    })
}

/// Lock the project row (`FOR SHARE`) and read the caller's facts.
async fn decided_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    caller: UserId,
) -> Result<ProjectUpdateFacts, ForgeError> {
    let locked = lock_shared(tx, project).await?;
    locked_access_facts(tx, project, caller, &locked).await
}

/// The webhook row as locked.
struct LockedHook {
    uid: uuid::Uuid,
    url: String,
    origin: String,
    owner: UserId,
    event_types: Vec<String>,
    enabled: bool,
    secret_version: i32,
}

type LockedRow = (String, String, String, i64, Vec<String>, bool, i32);

/// The webhook row `FOR UPDATE` (of `project`). Absent or another
/// project's: `FG-REPO-0005 resource=webhook`.
async fn lock_webhook(
    tx: &mut ChangeTx,
    project: ProjectId,
    webhook: WebhookId,
) -> Result<LockedHook, ForgeError> {
    let row: Option<LockedRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT uid::text, url, origin, owner_user_id, event_types, enabled, \
                 secret_version FROM forge.webhooks WHERE id = $1 AND project_id = $2 FOR UPDATE",
            )
            .bind(webhook.get())
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("webhooks.lock", e))
        })
        .await?;
    let (uid, url, origin, owner, mut event_types, enabled, secret_version) =
        row.ok_or_else(|| not_found("webhook"))?;
    event_types.sort();
    Ok(LockedHook {
        uid: super::parse_uuid(&uid, "uid")?,
        url,
        origin,
        owner: UserId::new(owner),
        event_types,
        enabled,
        secret_version,
    })
}

/// Create a webhook of `project` owned by `caller`. Errors (nothing
/// written): `decide`'s, `FG-REPO-0005 resource=project`, `FG-INT-0012
/// reason=webhook_limit` past [`MAX_WEBHOOKS`].
pub async fn create_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    new: &NewWebhook,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(WebhookId, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let key = format!("forge.webhooks:{}", project.get());
        tx.step(|c| async move {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(key)
                .execute(c)
                .await
                .map_err(|e| query_error("webhooks.project_lock", e))
        })
        .await?;
        let count: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar("SELECT count(*) FROM forge.webhooks WHERE project_id = $1")
                    .bind(project.get())
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error("webhooks.count", e))
            })
            .await?;
        if count >= MAX_WEBHOOKS {
            return Err(ForgeError::new(ErrorCode::IntegrationLimitReached)
                .with_context("reason", "webhook_limit")
                .with_context("limit", MAX_WEBHOOKS)
                .with_context("project_id", project.get()));
        }
        let id: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.webhooks (uid, project_id, owner_user_id, url, origin, \
                     event_types, secret_key_id, secret_ciphertext) \
                     VALUES ($1::uuid, $2, $3, $4, $8, $5, $6, $7) RETURNING id",
                )
                .bind(new.uid.to_string())
                .bind(project.get())
                .bind(caller.get())
                .bind(&new.url)
                .bind(&new.event_types)
                .bind(new.secret.key_id)
                .bind(&new.secret.ciphertext)
                .bind(&new.origin)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("webhooks.create", e))
            })
            .await?;
        let webhook = WebhookId::new(id);
        let details = AuditDetails::Webhook(WebhookChange::Created {
            webhook,
            project,
            origin: new.origin.clone(), // clone: the audit row owns it
            event_types: new.event_types.clone(), // clone: as above
            secret_version: 1,
        });
        Ok(Settle::Commit(
            record(&details, project, None)?,
            (webhook, decided),
        ))
    }
    .await;
    tx.settle("webhook.created", outcome).await
}

/// Change a webhook's URL, event types or enabled flag. Enabling it again
/// clears its failure count (and why it was disabled); disabling it by hand
/// records `disabled_reason = manual`. A new URL or event allowlist makes
/// the caller the owner (I2 review: deliveries are decided for whoever last
/// chose where and what they go; `WebhookField::Owner`, the audit row's
/// `owner_from` / `owner_id`). Nothing is written when every given
/// value equals the stored one. Errors: `decide`'s, `FG-REPO-0005`
/// (`resource=project` / `webhook`).
pub async fn update_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, webhook): (ProjectId, WebhookId),
    caller: UserId,
    patch: &WebhookPatch,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(UpdateOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let hook = lock_webhook(&mut tx, project, webhook).await?;
        let (enabled, owner_from) = (hook.enabled, hook.owner);
        let mut fields = Vec::new();
        let (to_url, origin) = match &patch.url {
            Some((u, o)) if *u != hook.url => {
                fields.push(WebhookField::Url);
                (u.clone(), o.clone()) // clone: the update and the audit row own them
            }
            Some(_) | None => (hook.url, hook.origin),
        };
        let to_types = match &patch.event_types {
            Some(t) if *t != hook.event_types => {
                fields.push(WebhookField::EventTypes);
                t.clone() // clone: as above
            }
            Some(_) | None => hook.event_types,
        };
        let retargeted = !fields.is_empty();
        let owner = if retargeted { caller } else { owner_from };
        if owner != owner_from {
            fields.push(WebhookField::Owner);
        }
        let to_enabled = patch.enabled.unwrap_or(enabled);
        if to_enabled != enabled {
            fields.push(WebhookField::Enabled);
        }
        if fields.is_empty() {
            return Ok(Settle::Discard((UpdateOutcome::Unchanged, decided)));
        }
        let reason = (!to_enabled).then_some(DisableReason::Manual.as_str());
        let enabling = to_enabled && !enabled;
        let (u, t, o) = (&to_url, &to_types, &origin);
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.webhooks SET url = $2, event_types = $3, enabled = $4, \
                 origin = $7, owner_user_id = $8, \
                 disabled_reason = CASE WHEN $4 THEN NULL ELSE coalesce(disabled_reason, $5) END, \
                 consecutive_failures = CASE WHEN $6 THEN 0 ELSE consecutive_failures END, \
                 failing_since = CASE WHEN $6 THEN NULL ELSE failing_since END, \
                 updated_at = now() WHERE id = $1",
            )
            .bind(webhook.get())
            .bind(u)
            .bind(t)
            .bind(to_enabled)
            .bind(reason)
            .bind(enabling)
            .bind(o)
            .bind(owner.get())
            .execute(c)
            .await
            .map_err(|e| query_error("webhooks.update", e))
        })
        .await?;
        let details = AuditDetails::Webhook(WebhookChange::Updated {
            webhook,
            project,
            owner_from,
            owner,
            fields: fields.clone(), // clone: the answer keeps them too
            origin,
            enabled: to_enabled,
            event_types: to_types.clone(), // clone: the audit row owns them
        });
        Ok(Settle::Commit(
            record(&details, project, None)?,
            (UpdateOutcome::Updated(fields), decided),
        ))
    }
    .await;
    tx.settle("webhook.updated", outcome).await
}

/// Delete a webhook and its delivery history (its queued jobs find nothing
/// and end). Errors: `decide`'s, `FG-REPO-0005`.
pub async fn delete_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, webhook): (ProjectId, WebhookId),
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let hook = lock_webhook(&mut tx, project, webhook).await?;
        tx.step(|c| async move {
            sqlx::query("DELETE FROM forge.webhook_deliveries WHERE webhook_id = $1")
                .bind(webhook.get())
                .execute(c)
                .await
                .map_err(|e| query_error("webhooks.delete_deliveries", e))
        })
        .await?;
        tx.step(|c| async move {
            sqlx::query("DELETE FROM forge.webhooks WHERE id = $1")
                .bind(webhook.get())
                .execute(c)
                .await
                .map_err(|e| query_error("webhooks.delete", e))
        })
        .await?;
        let details = AuditDetails::Webhook(WebhookChange::Deleted {
            webhook,
            project,
            origin: hook.origin,
        });
        Ok(Settle::Commit(record(&details, project, None)?, decided))
    }
    .await;
    tx.settle("webhook.deleted", outcome).await
}

/// Replace a webhook's secret with the one `seal` seals for the row's
/// `uid` (forge-integ generates it) and make the caller the owner (I2
/// review); the current one becomes the previous
/// and still signs for [`SECRET_OVERLAP`] (a previous one still in its
/// overlap is dropped: at most two secrets sign). Errors: `decide`'s,
/// `seal`'s, `FG-REPO-0005`.
pub async fn rotate_secret_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, webhook): (ProjectId, WebhookId),
    caller: UserId,
    seal: impl FnOnce(&uuid::Uuid) -> Result<SealedSecret, ForgeError>,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(Rotated, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let hook = lock_webhook(&mut tx, project, webhook).await?;
        let (version, owner_from) = (hook.secret_version, hook.owner);
        let sealed = seal(&hook.uid)?;
        let until: String = tx
            .step(|c| async move {
                sqlx::query_scalar(concat!(
                    "UPDATE forge.webhooks SET secret_prev_key_id = secret_key_id, ",
                    "secret_prev_ciphertext = secret_ciphertext, ",
                    "secret_prev_until = now() + make_interval(secs => $4), ",
                    "secret_key_id = $2, secret_ciphertext = $3, ",
                    "secret_version = secret_version + 1, owner_user_id = $5, ",
                    "updated_at = now() WHERE id = $1 ",
                    "RETURNING ",
                    utc!("secret_prev_until")
                ))
                .bind(webhook.get())
                .bind(sealed.key_id)
                .bind(&sealed.ciphertext)
                .bind(SECRET_OVERLAP.as_secs_f64())
                .bind(caller.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("webhooks.rotate", e))
            })
            .await?;
        let rotated = Rotated {
            secret_version: version + 1,
            previous_valid_until: until,
        };
        let details = AuditDetails::Webhook(WebhookChange::SecretRotated {
            webhook,
            project,
            owner_from,
            owner: caller,
            secret_version: rotated.secret_version,
            previous_valid_until: rotated.previous_valid_until.clone(), // clone: the answer keeps it
        });
        Ok(Settle::Commit(
            record(&details, project, None)?,
            (rotated, decided),
        ))
    }
    .await;
    tx.settle("webhook.secret_rotated", outcome).await
}

/// The least time between two attempts a person asks for.
pub const REDELIVERY_INTERVAL: Duration = Duration::from_secs(60);

/// The first job of a delivery round (`round` = its `redeliveries`).
pub(super) fn first_job(
    delivery: i64,
    round: i32,
    request_id: &RequestId,
    event_seq: i64,
) -> Result<NewJob, ForgeError> {
    new_job(delivery, round, 1, request_id, Some(event_seq))
}

fn job_parts() -> Result<(JobKind, MaxAttempts, JobTimeout), ForgeError> {
    let bad =
        |reason: &'static str| ForgeError::new(ErrorCode::Internal).with_context("reason", reason);
    Ok((
        JobKind::parse(super::DELIVER_JOB).ok_or_else(|| bad("webhook_job_kind"))?,
        MaxAttempts::new(super::JOB_MAX_ATTEMPTS).ok_or_else(|| bad("webhook_job_attempts"))?,
        JobTimeout::new(super::JOB_TIMEOUT).ok_or_else(|| bad("webhook_job_timeout"))?,
    ))
}

pub(super) fn new_job(
    delivery: i64,
    round: i32,
    attempt: i32,
    request_id: &RequestId,
    event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let (kind, max_attempts, timeout) = job_parts()?;
    let dedup =
        JobDedupKey::parse(&format!("delivery:{delivery}:{round}:{attempt}")).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "webhook_job_dedup")
        })?;
    Ok(NewJob {
        kind,
        payload: serde_json::json!({ "delivery_id": delivery, "round": round, "attempt": attempt }),
        max_attempts,
        timeout,
        dedup_key: Some(dedup),
        origin_request_id: request_id.clone(), // clone: the job owns its origin
        origin_event_seq: event_seq,
    })
}

/// Ask for delivery `delivery` of a webhook again: a new round of attempts
/// (`redeliveries + 1`, the same `Forge-Delivery` id). Refused while one is
/// pending or within [`REDELIVERY_INTERVAL`] of the last attempt
/// (`FG-WEB-0010`), for a disabled webhook (`FG-WEB-0006
/// rule=webhook_disabled`). Errors: `decide`'s, `FG-REPO-0005`
/// (`resource=project` / `webhook` / `webhook_delivery`).
pub async fn redeliver_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, webhook, delivery): (ProjectId, WebhookId, WebhookDeliveryId),
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let request_id = ctx.request_id.clone(); // clone: the job carries it
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let enabled = lock_webhook(&mut tx, project, webhook).await?.enabled;
        let row: Option<(String, i32, i64, bool)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT state, redeliveries, event_seq, \
                     coalesce(last_attempt_at > now() - make_interval(secs => $3), false) \
                     FROM forge.webhook_deliveries WHERE id = $1 AND webhook_id = $2 FOR UPDATE",
                )
                .bind(delivery.get())
                .bind(webhook.get())
                .bind(REDELIVERY_INTERVAL.as_secs_f64())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("webhooks.redeliver_lock", e))
            })
            .await?;
        let (state, round, event_seq, recent) = row.ok_or_else(|| not_found("webhook_delivery"))?;
        if !enabled {
            return Err(ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "field_invalid")
                .with_context("rule", "webhook_disabled")
                .with_context("webhook_id", webhook.get()));
        }
        let limited = |reason: &'static str| {
            ForgeError::new(ErrorCode::RateLimited)
                .with_context("route", "webhook_redeliver")
                .with_context("reason", reason)
                .with_context("retry_after_s", REDELIVERY_INTERVAL.as_secs())
        };
        if DeliveryState::parse(&state)? == DeliveryState::Pending {
            return Err(limited("delivery_pending"));
        }
        if recent {
            return Err(limited("redelivery_interval"));
        }
        let next = round + 1;
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.webhook_deliveries SET state = 'pending', finished_at = NULL, \
                 redeliveries = $2 WHERE id = $1",
            )
            .bind(delivery.get())
            .bind(next)
            .execute(c)
            .await
            .map_err(|e| query_error("webhooks.redeliver", e))
        })
        .await?;
        let job = first_job(delivery.get(), next, &request_id, event_seq)?;
        tx.step(|c| async move { enqueue_on(c, &job).await.map(drop) })
            .await?;
        let details = AuditDetails::Webhook(WebhookChange::Redelivered {
            webhook,
            project,
            delivery,
        });
        Ok(Settle::Commit(record(&details, project, None)?, decided))
    }
    .await;
    tx.settle("webhook.redelivered", outcome).await
}

/// The dedup key of the automatic disable's event: one per (webhook,
/// delivery, round, attempt) — the attempt that disabled it.
pub(super) fn disable_dedup(
    webhook: i64,
    delivery: i64,
    round: i32,
    attempt: i32,
) -> Result<DedupKey, ForgeError> {
    DedupKey::new(
        "webhook_disabled",
        &[
            DedupPart::Id(webhook),
            DedupPart::Id(delivery),
            DedupPart::Id(i64::from(round)),
            DedupPart::Id(i64::from(attempt)),
        ],
    )
}
