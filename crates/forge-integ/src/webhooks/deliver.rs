//! The `webhook.deliver` job (one attempt of one delivery; module docs of
//! `webhooks`). The job's own retries (`forge.jobs`, 3) are for internal
//! failures (the database); the delivery's attempts are its own, each a
//! job queued after the backoff by `forge_db::webhooks::record_attempt`.
//! The handler is the logging boundary of an attempt: `FG-INT-0020` (info)
//! for a failed attempt that will be retried, `FG-INT-0004` (warn) for the
//! last one, `FG-INT-0011` (warn) for the automatic disable, `FG-INT-0010`
//! (warn) for a refused target or a lost owner — each once, with the
//! delivery's request id.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use forge_auth::prove_webhook_delivery;
use forge_core::webhook::{
    signature, signature_header, WebhookSecretKeys, DELIVERY_HEADER, EVENT_HEADER,
    SIGNATURE_HEADER, SIGNATURE_KID_HEADER, WEBHOOK_HEADER,
};
use forge_core::{telemetry, EventType, RequestId, WebhookDeliveryId};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::webhooks::{
    self as db, Attempt, AttemptOutcome, DeliveryState, DeliveryTask, Recorded, RecordedAttempt,
};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::body::delivery_body;
use super::client::{Sent, Transport};

/// The job kind.
pub const DELIVER_JOB: &str = forge_db::webhooks::DELIVER_JOB;

/// The handler of `webhook.deliver`.
pub struct DeliverHandler {
    inner: Arc<Inner>,
    kind: JobKind,
}

struct Inner {
    db: Db,
    keys: Arc<WebhookSecretKeys>,
    transport: Transport,
}

impl DeliverHandler {
    pub fn new(
        db: &Db,
        keys: Arc<WebhookSecretKeys>,
        transport: Transport,
    ) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(DELIVER_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "webhook_job_kind")
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                db: db.clone(), // clone: a pool handle
                keys,
                transport,
            }),
            kind,
        })
    }
}

impl JobHandler for DeliverHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let request_id = job.request_id();
            let (ids, result) = match payload(&job) {
                Ok((delivery, round, attempt)) => (
                    Some((delivery, round)),
                    inner.attempt(&request_id, delivery, round, attempt).await,
                ),
                Err(e) => (None, Err(e)),
            };
            let Err(e) = result else {
                return Ok(());
            };
            // I2 review: a job that will not run again (a payload this
            // build cannot read, its last run failing) ends its delivery,
            // so the row never stays pending (redelivery, retention and
            // doctor see it).
            let unreadable = ids.is_none();
            if unreadable || job.lease.attempt >= job.max_attempts {
                let (delivery, round) = match ids {
                    Some((d, r)) => (Some(d), Some(r)),
                    None => (delivery_of(&job), None),
                };
                if let Some(delivery) = delivery {
                    inner.give_up(&request_id, delivery, round, &e).await;
                }
            }
            Err(if unreadable {
                JobFailure::permanent(e)
            } else {
                JobFailure::retry(e)
            })
        })
    }
}

fn payload(job: &ClaimedJob) -> Result<(WebhookDeliveryId, i32, i32), ForgeError> {
    let field = |name: &'static str| {
        job.payload
            .get(name)
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "invalid_job_payload")
                    .with_context("field", name)
            })
    };
    let small = |name: &'static str| {
        field(name).and_then(|v| {
            i32::try_from(v).map_err(|_| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "invalid_job_payload")
                    .with_context("field", name)
            })
        })
    };
    Ok((
        WebhookDeliveryId::new(field("delivery_id")?),
        small("round")?,
        small("attempt")?,
    ))
}

/// The delivery id of a payload whatever else is wrong with it.
fn delivery_of(job: &ClaimedJob) -> Option<WebhookDeliveryId> {
    job.payload
        .get("delivery_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .map(WebhookDeliveryId::new)
}

/// Seconds since the Unix epoch; a clock before it is an error, never a
/// signature with `t=0` (I2 review).
fn unix_now() -> Result<u64, ForgeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "clock_before_epoch")
        })
}

fn reason_of(e: &ForgeError) -> &str {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or("none", |(_, v)| v.as_str())
}

/// The webhook's origin for logs (never the path or query).
fn origin(task: &DeliveryTask) -> String {
    forge_core::webhook::WebhookUrl::parse(&task.url, true)
        .map_or_else(|_| "invalid".to_owned(), |u| u.origin())
}

impl Inner {
    /// End a delivery its job gave up on: `failed` and `FG-INT-0004
    /// reason=job_failed` (with the job's error code). A failure to record
    /// it is logged too; the sweep (`webhook_delivery_retention`) fails the
    /// row later.
    async fn give_up(
        &self,
        request_id: &RequestId,
        delivery: WebhookDeliveryId,
        round: Option<i32>,
        cause: &ForgeError,
    ) {
        match db::fail_delivery(&self.db, delivery, round, cause.code()).await {
            Ok(true) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::WebhookDeadLettered)
                    .with_context("reason", "job_failed")
                    .with_context("delivery_row_id", delivery.get())
                    .with_context("cause", cause.code().id()),
                request_id,
            ),
            Ok(false) => {}
            Err(e) => telemetry::log_failure(
                &e.with_context("operation", "webhooks.give_up")
                    .with_context("delivery_row_id", delivery.get()),
                request_id,
            ),
        }
    }

    async fn attempt(
        &self,
        request_id: &RequestId,
        delivery: WebhookDeliveryId,
        round: i32,
        attempt: i32,
    ) -> Result<(), ForgeError> {
        let Some(task) = db::delivery_task(&self.db, delivery).await? else {
            // The webhook was deleted with its history: nothing to deliver.
            return Ok(());
        };
        if task.state != DeliveryState::Pending || task.round != round {
            // Another round began, or this one ended (a rerun of the job).
            return Ok(());
        }
        let (outcome, head) = self.outcome(request_id, &task, attempt).await?;
        let recorded = match db::record_attempt(
            &self.db,
            request_id,
            &Attempt {
                delivery,
                round,
                attempt,
                outcome,
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                // I2 review: the job runs again and may send this delivery
                // a second time (the receiver deduplicates on Forge-Delivery).
                let sent = !matches!(outcome, AttemptOutcome::Skipped { .. });
                tracing::warn!(
                    target: "forge_integ",
                    event = "forge.webhooks.attempt_unrecorded",
                    request_id = %request_id,
                    delivery_id = %task.delivery_uid,
                    attempt,
                    resend = sent,
                    error_code = e.code().id(),
                    "an attempt's outcome was not recorded; its job runs again"
                );
                return Err(e);
            }
        };
        report(request_id, &task, (attempt, outcome, head), &recorded);
        Ok(())
    }

    /// Decide, sign and send (or skip): the outcome, and the bounded,
    /// redacted head of a non-2xx answer for the attempt's log line.
    async fn outcome(
        &self,
        request_id: &RequestId,
        task: &DeliveryTask,
        attempt: i32,
    ) -> Result<(AttemptOutcome, Option<String>), ForgeError> {
        let skipped = |reason| Ok((AttemptOutcome::Skipped { reason }, None));
        if !task.material.enabled {
            return skipped("webhook_disabled");
        }
        let set = match prove_webhook_delivery(&task.material) {
            Ok(set) => set,
            Err(e) if e.code() == ErrorCode::WebhookTargetForbidden => {
                telemetry::log_failure(
                    &e.with_context("delivery_id", task.delivery_uid)
                        .with_context("attempt", attempt),
                    request_id,
                );
                return skipped("owner_lost");
            }
            Err(e) => return Err(e),
        };
        let (Some(event), Some(body)) = (task.event.as_ref(), delivery_body(task)) else {
            return skipped("event_gone");
        };
        if !EventType::parse(&event.event_type).is_some_and(|t| set.admits(t)) {
            // The allowlist or the owner's classes narrowed since dispatch.
            return skipped("not_admitted");
        }
        let body = body.to_string().into_bytes();
        let delivery_id = task.delivery_uid.to_string();
        let t = unix_now()?;
        let mut signatures = Vec::new();
        let mut kids = Vec::new();
        let current = self
            .keys
            .open(&task.uid, task.secret.key_id, &task.secret.ciphertext)?;
        signatures.push(signature(current.expose(), t, &delivery_id, &body)?);
        kids.push(task.secret_version.to_string());
        if let Some(previous) = &task.previous_secret {
            let previous = self
                .keys
                .open(&task.uid, previous.key_id, &previous.ciphertext)?;
            signatures.push(signature(previous.expose(), t, &delivery_id, &body)?);
            kids.push((task.secret_version - 1).to_string());
        }
        let headers = [
            (EVENT_HEADER, event.event_type.clone()), // clone: the header owns its value
            (DELIVERY_HEADER, delivery_id.clone()),   // clone: as above
            (WEBHOOK_HEADER, task.webhook.get().to_string()),
            (SIGNATURE_HEADER, signature_header(t, &signatures)),
            (SIGNATURE_KID_HEADER, kids.join(",")),
        ];
        Ok(match self.transport.post(&task.url, &headers, body).await {
            Sent::Answered {
                status,
                duration_ms,
                ..
            } if (200..300).contains(&status) => (
                AttemptOutcome::Succeeded {
                    status,
                    duration_ms,
                },
                None,
            ),
            Sent::Answered {
                status,
                head,
                duration_ms,
            } => {
                let reason = if (300..400).contains(&status) {
                    "redirect"
                } else {
                    "status"
                };
                tracing::debug!(
                    target: "forge_integ",
                    request_id = %request_id,
                    delivery_id = %task.delivery_uid,
                    status,
                    response_head = %head,
                    "forge.webhooks.answer"
                );
                (
                    AttemptOutcome::Failed {
                        status: Some(status),
                        reason,
                        error_code: ErrorCode::WebhookAttemptFailed,
                        duration_ms,
                    },
                    Some(head),
                )
            }
            Sent::Refused(e) => {
                let reason = if reason_of(&e).starts_with("resolved") {
                    "resolved_forbidden"
                } else {
                    "target_forbidden"
                };
                telemetry::log_failure(
                    &e.with_context("webhook_id", task.webhook.get())
                        .with_context("delivery_id", task.delivery_uid)
                        .with_context("attempt", attempt),
                    request_id,
                );
                (
                    AttemptOutcome::Failed {
                        status: None,
                        reason,
                        error_code: ErrorCode::WebhookTargetForbidden,
                        duration_ms: 0,
                    },
                    None,
                )
            }
            Sent::Failed {
                reason,
                duration_ms,
            } => (
                AttemptOutcome::Failed {
                    status: None,
                    reason,
                    error_code: ErrorCode::WebhookAttemptFailed,
                    duration_ms,
                },
                None,
            ),
        })
    }
}

/// The attempt's one log line (module docs).
fn report(
    request_id: &RequestId,
    task: &DeliveryTask,
    (attempt, outcome, head): (i32, AttemptOutcome, Option<String>),
    recorded: &RecordedAttempt,
) {
    if let (AttemptOutcome::Skipped { reason }, Recorded::Skipped) = (outcome, recorded.recorded) {
        // I2 review: why a delivery went nowhere (owner_lost also logs
        // FG-INT-0010).
        tracing::info!(
            target: "forge_integ",
            event = "forge.webhooks.skipped",
            request_id = %request_id,
            webhook_id = task.webhook.get(),
            delivery_id = %task.delivery_uid,
            reason,
            "a webhook delivery was skipped"
        );
    }
    if let AttemptOutcome::Failed { status, reason, .. } = outcome {
        let status = status.map_or_else(|| "none".to_owned(), |s| s.to_string());
        let base = |code: ErrorCode| {
            let mut e = ForgeError::new(code)
                .with_context("webhook_id", task.webhook.get())
                .with_context("delivery_id", task.delivery_uid)
                .with_context(
                    "event_type",
                    task.event
                        .as_ref()
                        .map_or("gone", |e| e.event_type.as_str())
                        .to_owned(),
                )
                .with_context("origin", origin(task))
                .with_context("consecutive_failures", recorded.consecutive_failures);
            if let Some(h) = &head {
                e = e.with_context("response_head", h);
            }
            e
        };
        match recorded.recorded {
            Recorded::Retrying {
                next_attempt,
                retry_in,
            } => telemetry::log_failure(
                &base(ErrorCode::WebhookAttemptFailed)
                    .with_context("attempt", attempt)
                    .with_context("status", &status)
                    .with_context("reason", reason)
                    .with_context("next_attempt", next_attempt)
                    .with_context("retry_in_s", retry_in.as_secs()),
                request_id,
            ),
            Recorded::Failed { attempts } => telemetry::log_failure(
                &base(ErrorCode::WebhookDeadLettered)
                    .with_context(
                        "event_id",
                        task.event
                            .as_ref()
                            .map_or_else(|| "gone".to_owned(), |e| e.event_id.to_string()),
                    )
                    .with_context("attempts", attempts)
                    .with_context("last_status", &status)
                    .with_context("last_reason", reason),
                request_id,
            ),
            Recorded::Succeeded | Recorded::Skipped | Recorded::Stale => {}
        }
    }
    if let Some(disabled) = &recorded.disabled {
        telemetry::log_failure(
            &ForgeError::new(ErrorCode::WebhookDisabled)
                .with_context("webhook_id", task.webhook.get())
                .with_context("project_id", disabled.project.get())
                .with_context("reason", disabled.reason.as_str())
                .with_context("failures", disabled.failures)
                .with_context(
                    "failing_since",
                    disabled
                        .failing_since
                        .as_deref()
                        .unwrap_or("none")
                        .to_owned(),
                ),
            request_id,
        );
    }
}
