//! What one `webhook.deliver` job reads ([`delivery_task`]) and the one
//! transaction that records its attempt ([`record_attempt`]): the
//! delivery's state, the webhook's failure count, the automatic disable and
//! the next attempt after the backoff.

use std::time::Duration;

use forge_core::ids::{ProjectId, WebhookDeliveryId, WebhookId};
use forge_core::webhook::{retry_delay, SealedSecret, WebhookChange};
use forge_core::{AuditDetails, DisableReason, RequestId, WebhookMaterial};
use forge_errors::{ErrorCode, ForgeError};

use super::dispatch::material_on;
use super::manage::{disable_dedup, new_job, record};
use super::{
    parse_uuid, sealed, utc, DeliveryState, DISABLE_AFTER_FAILING, DISABLE_AFTER_FAILURES,
    MAX_ATTEMPTS,
};
use crate::change::{Actor, ChangeContext, ChangeTx, Settle};
use crate::jobs::{enqueue_after_on, Enqueued};
use crate::{query_error, Db};

/// A person as an event's body shows them (read now).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventActor {
    pub id: i64,
    pub username: String,
    pub display_name: String,
}

/// A work item the event concerns (`event_work_items`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWorkItem {
    pub provider: String,
    pub project_id: String,
    pub reference: String,
}

/// The event a delivery carries, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryEvent {
    pub seq: i64,
    pub event_id: uuid::Uuid,
    pub event_type: String,
    pub schema_version: i16,
    /// RFC 3339, UTC, milliseconds.
    pub occurred_at: String,
    pub project_id: Option<i64>,
    pub actor_kind: String,
    pub actor: Option<EventActor>,
    pub via: Option<EventActor>,
    pub work_items: Vec<EventWorkItem>,
    pub payload: serde_json::Value,
}

/// Everything one attempt needs, read on the write pool (the sealed
/// secrets are not the read pool's).
#[derive(Debug, Clone)]
pub struct DeliveryTask {
    pub delivery: WebhookDeliveryId,
    /// The `Forge-Delivery` value.
    pub delivery_uid: uuid::Uuid,
    pub state: DeliveryState,
    /// `redeliveries`: the round the stored row is in.
    pub round: i32,
    pub webhook: WebhookId,
    pub project: ProjectId,
    /// The row's `uid` (the secrets' associated data).
    pub uid: uuid::Uuid,
    pub url: String,
    pub secret: SealedSecret,
    pub secret_version: i32,
    /// The previous secret while it still signs.
    pub previous_secret: Option<SealedSecret>,
    /// The owner's standing, read now (forge-auth decides on it).
    pub material: WebhookMaterial,
    /// `None`: the event was removed by its retention.
    pub event: Option<DeliveryEvent>,
}

type TaskRow = (
    i64,
    String,
    String,
    i32,
    i64,
    i64,
    i64,
    String,
    String,
    i16,
    Vec<u8>,
    i32,
    Option<i16>,
    Option<Vec<u8>>,
);

type EventRow = (
    i64,
    String,
    String,
    i16,
    String,
    Option<i64>,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    String,
);

fn actor(id: Option<i64>, username: Option<String>, display: Option<String>) -> Option<EventActor> {
    match (id, username, display) {
        (Some(id), Some(username), Some(display_name)) => Some(EventActor {
            id,
            username,
            display_name,
        }),
        _ => None,
    }
}

/// The task of delivery `delivery` (`None`: the delivery is gone — its
/// webhook was deleted).
pub async fn delivery_task(
    db: &Db,
    delivery: WebhookDeliveryId,
) -> Result<Option<DeliveryTask>, ForgeError> {
    const QUERY: &str = "webhooks.delivery_task";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("webhooks.acquire", e))?;
        let row: Option<TaskRow> = sqlx::query_as(
            "SELECT d.id, d.delivery_uid::text, d.state, d.redeliveries, d.event_seq, \
             w.id, w.project_id, w.uid::text, w.url, w.secret_key_id, w.secret_ciphertext, \
             w.secret_version, \
             CASE WHEN w.secret_prev_until > now() THEN w.secret_prev_key_id END, \
             CASE WHEN w.secret_prev_until > now() THEN w.secret_prev_ciphertext END \
             FROM forge.webhook_deliveries d JOIN forge.webhooks w ON w.id = d.webhook_id \
             WHERE d.id = $1",
        )
        .bind(delivery.get())
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((
            id,
            delivery_uid,
            state,
            round,
            seq,
            webhook,
            project,
            uid,
            url,
            key_id,
            ciphertext,
            secret_version,
            prev_key,
            prev_ciphertext,
        )) = row
        else {
            return Ok(None);
        };
        let webhook = WebhookId::new(webhook);
        let Some(material) = material_on(&mut conn, webhook).await? else {
            return Ok(None);
        };
        let event: Option<EventRow> = sqlx::query_as(concat!(
            "SELECT e.seq, e.event_id::text, e.event_type, e.schema_version, ",
            "to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), ",
            "e.project_id, e.actor_kind, e.actor_user_id, u.username, u.display_name, ",
            "e.actor_via_id, v.username, v.display_name, e.payload::text ",
            "FROM forge.events e LEFT JOIN forge.users u ON u.id = e.actor_user_id ",
            "LEFT JOIN forge.users v ON v.id = e.actor_via_id WHERE e.seq = $1"
        ))
        .bind(seq)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error("webhooks.delivery_event", e))?;
        let event = match event {
            None => None,
            Some((
                seq,
                event_id,
                event_type,
                schema_version,
                occurred_at,
                project_id,
                actor_kind,
                actor_id,
                actor_name,
                actor_display,
                via_id,
                via_name,
                via_display,
                payload,
            )) => {
                let rows: Vec<(String, String, String)> = sqlx::query_as(
                    "SELECT i.kind, w.external_project_id, w.ref FROM forge.event_work_items ew \
                     JOIN forge.work_items w ON w.id = ew.work_item_id \
                     JOIN forge.integrations i ON i.id = w.integration_id \
                     WHERE ew.seq = $1 ORDER BY w.id LIMIT 100",
                )
                .bind(seq)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| query_error("webhooks.delivery_work_items", e))?;
                let payload = serde_json::from_str(&payload).map_err(|_| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "stored_payload_invalid")
                        .with_context("seq", seq)
                })?;
                Some(DeliveryEvent {
                    seq,
                    event_id: parse_uuid(&event_id, "event_id")?,
                    event_type,
                    schema_version,
                    occurred_at,
                    project_id,
                    actor_kind,
                    actor: actor(actor_id, actor_name, actor_display),
                    via: actor(via_id, via_name, via_display),
                    work_items: rows
                        .into_iter()
                        .map(|(provider, project_id, reference)| EventWorkItem {
                            provider,
                            project_id,
                            reference,
                        })
                        .collect(),
                    payload,
                })
            }
        };
        Ok(Some(DeliveryTask {
            delivery: WebhookDeliveryId::new(id),
            delivery_uid: parse_uuid(&delivery_uid, "delivery_uid")?,
            state: DeliveryState::parse(&state)?,
            round,
            webhook,
            project: ProjectId::new(project),
            uid: parse_uuid(&uid, "uid")?,
            url,
            secret: SealedSecret { key_id, ciphertext },
            secret_version,
            previous_secret: sealed(prev_key, prev_ciphertext),
            material,
            event,
        }))
    })
    .await
}

/// How one attempt ended, as the job saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The receiver answered 2xx.
    Succeeded { status: u16, duration_ms: i32 },
    /// Nothing usable: `status` when the receiver answered, `reason` the
    /// short code (`status`, `redirect`, `connect`, `timeout`, `tls`, …),
    /// `error_code` the FG code of the failure.
    Failed {
        status: Option<u16>,
        reason: &'static str,
        error_code: ErrorCode,
        duration_ms: i32,
    },
    /// Not sent at all (`webhook_disabled`, `owner_lost`, `not_admitted`,
    /// `event_gone`): the delivery ends without counting a failure.
    Skipped { reason: &'static str },
}

/// One attempt to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    pub delivery: WebhookDeliveryId,
    /// The job's round (the row's `redeliveries` when it was queued).
    pub round: i32,
    /// 1-based within the round.
    pub attempt: i32,
    pub outcome: AttemptOutcome,
}

/// What [`record_attempt`] did with the delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Succeeded,
    /// Another attempt is queued, ready after `retry_in`.
    Retrying {
        next_attempt: i32,
        retry_in: Duration,
    },
    /// The delivery is `failed` (the last attempt, or the webhook is
    /// disabled now): `FG-INT-0004` for the caller to log.
    Failed {
        attempts: i32,
    },
    Skipped,
    /// The delivery is no longer this job's (another round began, it ended,
    /// or it is gone): nothing was written.
    Stale,
}

/// The automatic disable an attempt caused (`FG-INT-0011`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disabled {
    pub project: ProjectId,
    pub reason: DisableReason,
    pub failures: i32,
    /// RFC 3339, UTC.
    pub failing_since: Option<String>,
}

/// [`record_attempt`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedAttempt {
    pub recorded: Recorded,
    pub webhook: Option<WebhookId>,
    /// The webhook's failed attempts in a row after this one.
    pub consecutive_failures: i32,
    pub disabled: Option<Disabled>,
}

/// The job of attempt `attempt` of a delivery round.
pub(super) fn attempt_job(
    delivery: i64,
    round: i32,
    attempt: i32,
    request_id: &RequestId,
) -> Result<crate::jobs::NewJob, ForgeError> {
    new_job(delivery, round, attempt, request_id, None)
}

fn stale() -> RecordedAttempt {
    RecordedAttempt {
        recorded: Recorded::Stale,
        webhook: None,
        consecutive_failures: 0,
        disabled: None,
    }
}

/// Record one attempt in one transaction (as the system, `request_id`
/// the delivery's): see the module docs. A delivery that is no longer
/// pending in the job's round is [`Recorded::Stale`] (nothing written).
pub async fn record_attempt(
    db: &Db,
    request_id: &RequestId,
    attempt: &Attempt,
) -> Result<RecordedAttempt, ForgeError> {
    let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    let mut tx = ChangeTx::begin(db, &ctx).await?;
    let delivery = attempt.delivery;
    let outcome = async {
        let row: Option<(String, i32, i64)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT state, redeliveries, webhook_id FROM forge.webhook_deliveries \
                     WHERE id = $1 FOR UPDATE",
                )
                .bind(delivery.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("webhooks.attempt_delivery", e))
            })
            .await?;
        let Some((state, round, webhook)) = row else {
            return Ok(Settle::Discard(stale()));
        };
        if DeliveryState::parse(&state)? != DeliveryState::Pending || round != attempt.round {
            return Ok(Settle::Discard(stale()));
        }
        let hook: Option<(i64, bool, i32, bool)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT project_id, enabled, consecutive_failures, \
                     coalesce(failing_since <= now() - make_interval(secs => $2), false) \
                     FROM forge.webhooks WHERE id = $1 FOR UPDATE",
                )
                .bind(webhook)
                .bind(DISABLE_AFTER_FAILING.as_secs_f64())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("webhooks.attempt_webhook", e))
            })
            .await?;
        let Some((project, enabled, failures, failing_long)) = hook else {
            return Ok(Settle::Discard(stale()));
        };
        let project = ProjectId::new(project);
        let webhook_id = WebhookId::new(webhook);
        match attempt.outcome {
            AttemptOutcome::Skipped { reason } => {
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.webhook_deliveries SET state = 'skipped', last_reason = $2, \
                         last_error_code = NULL, finished_at = now() WHERE id = $1",
                    )
                    .bind(delivery.get())
                    .bind(reason)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("webhooks.attempt_skipped", e))
                })
                .await?;
                Ok(Settle::CommitDerived(RecordedAttempt {
                    recorded: Recorded::Skipped,
                    webhook: Some(webhook_id),
                    consecutive_failures: failures,
                    disabled: None,
                }))
            }
            AttemptOutcome::Succeeded {
                status,
                duration_ms,
            } => {
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.webhook_deliveries SET state = 'succeeded', \
                         attempts = attempts + 1, last_status = $2, last_reason = NULL, \
                         last_error_code = NULL, last_duration_ms = $3, last_attempt_at = now(), \
                         finished_at = now() WHERE id = $1",
                    )
                    .bind(delivery.get())
                    .bind(i32::from(status))
                    .bind(duration_ms.max(0))
                    .execute(c)
                    .await
                    .map_err(|e| query_error("webhooks.attempt_succeeded", e))
                })
                .await?;
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.webhooks SET consecutive_failures = 0, failing_since = NULL, \
                         last_delivery_at = now(), updated_at = now() WHERE id = $1",
                    )
                    .bind(webhook)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("webhooks.attempt_reset", e))
                })
                .await?;
                Ok(Settle::CommitDerived(RecordedAttempt {
                    recorded: Recorded::Succeeded,
                    webhook: Some(webhook_id),
                    consecutive_failures: 0,
                    disabled: None,
                }))
            }
            AttemptOutcome::Failed {
                status,
                reason,
                error_code,
                duration_ms,
            } => {
                let now_failures = failures.saturating_add(1);
                let disable = if !enabled {
                    None
                } else if now_failures >= DISABLE_AFTER_FAILURES {
                    Some(DisableReason::Failures)
                } else if failing_long {
                    Some(DisableReason::Failing24h)
                } else {
                    None
                };
                let last = attempt.attempt >= MAX_ATTEMPTS || disable.is_some() || !enabled;
                let code = error_code.id();
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.webhook_deliveries SET attempts = attempts + 1, \
                         last_status = $2, last_reason = $3, last_error_code = $4, \
                         last_duration_ms = $5, last_attempt_at = now(), \
                         state = CASE WHEN $6 THEN 'failed' ELSE 'pending' END, \
                         finished_at = CASE WHEN $6 THEN now() END WHERE id = $1",
                    )
                    .bind(delivery.get())
                    .bind(status.map(i32::from))
                    .bind(reason)
                    .bind(code)
                    .bind(duration_ms.max(0))
                    .bind(last)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("webhooks.attempt_failed", e))
                })
                .await?;
                let reason_word = disable.map(DisableReason::as_str);
                let since: Option<String> = tx
                    .step(|c| async move {
                        sqlx::query_scalar(concat!(
                            "UPDATE forge.webhooks SET consecutive_failures = $2, ",
                            "failing_since = coalesce(failing_since, now()), ",
                            "last_delivery_at = now(), updated_at = now(), ",
                            "enabled = CASE WHEN $3::text IS NULL THEN enabled ELSE false END, ",
                            "disabled_reason = coalesce($3, disabled_reason) WHERE id = $1 ",
                            "RETURNING ",
                            utc!("failing_since")
                        ))
                        .bind(webhook)
                        .bind(now_failures)
                        .bind(reason_word)
                        .fetch_one(c)
                        .await
                        .map_err(|e| query_error("webhooks.attempt_count", e))
                    })
                    .await?;
                let recorded = if last {
                    Recorded::Failed {
                        attempts: attempt.attempt,
                    }
                } else {
                    let next = attempt.attempt + 1;
                    let retry_in = retry_delay(attempt.attempt);
                    let job = attempt_job(delivery.get(), attempt.round, next, request_id)?;
                    let queued = tx
                        .step(|c| async move { enqueue_after_on(c, &job, retry_in).await })
                        .await?;
                    if queued == Enqueued::Deduped {
                        // The next attempt is already queued (a rerun of
                        // this job after its result was lost): nothing new.
                        tracing::debug!(
                            target: "forge_db",
                            delivery_id = delivery.get(),
                            attempt = next,
                            "forge.webhooks.next_attempt_already_queued"
                        );
                    }
                    Recorded::Retrying {
                        next_attempt: next,
                        retry_in,
                    }
                };
                let answer = RecordedAttempt {
                    recorded,
                    webhook: Some(webhook_id),
                    consecutive_failures: now_failures,
                    disabled: disable.map(|reason| Disabled {
                        project,
                        reason,
                        failures: now_failures,
                        failing_since: since.clone(), // clone: the audit row keeps it too
                    }),
                };
                match disable {
                    Some(reason) => {
                        let details = AuditDetails::Webhook(WebhookChange::Disabled {
                            webhook: webhook_id,
                            project,
                            reason,
                            consecutive_failures: now_failures,
                            failing_since: since,
                        });
                        let dedup =
                            disable_dedup(webhook, delivery.get(), attempt.round, attempt.attempt)?;
                        Ok(Settle::Commit(
                            record(&details, project, Some(dedup))?,
                            answer,
                        ))
                    }
                    None => Ok(Settle::CommitDerived(answer)),
                }
            }
        }
    }
    .await;
    tx.settle("webhook.attempt", outcome).await
}
