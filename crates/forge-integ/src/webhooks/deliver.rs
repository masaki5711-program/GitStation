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
            let (delivery, round, attempt) = payload(&job).map_err(JobFailure::permanent)?;
            inner
                .attempt(&job.request_id(), delivery, round, attempt)
                .await
                .map_err(JobFailure::retry)
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        let recorded = db::record_attempt(
            &self.db,
            request_id,
            &Attempt {
                delivery,
                round,
                attempt,
                outcome,
            },
        )
        .await?;
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
        let t = unix_now();
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
