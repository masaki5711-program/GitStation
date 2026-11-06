//! The `webhook-dispatch` subscriber (ADR-0016 §7 "independent", §8): for
//! each published event of a project, each enabled webhook of that project
//! whose owner may still see the event's class (`prove_webhook_delivery`:
//! Maintainer, the admin class never) and whose allowlist admits the type
//! gets one delivery row and its first `webhook.deliver` job, in the step's
//! transaction (a retried step adds nothing: `webhook_deliveries_once`).
//! The network call is the job's, never the step's.
//!
//! An event without a project (instance-wide) and an admin-class event
//! never reach a webhook. A webhook whose owner lost the project records
//! nothing and logs `FG-INT-0010 reason=owner_lost` (warn) with the event's
//! request id. A type this build does not know is passed over with a
//! warning (an independent subscriber goes on, never silently).

use forge_auth::prove_webhook_delivery;
use forge_core::{telemetry, EventType, RequestId, VisibilityClass};
use forge_db::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use forge_db::webhooks as db;
use forge_errors::{ErrorCode, ForgeError};

/// The subscriber's name (its row in forge.event_subscribers).
pub const DISPATCH: &str = "webhook-dispatch";

/// The subscriber.
#[derive(Debug, Clone, Copy, Default)]
pub struct WebhookDispatch;

impl WebhookDispatch {
    async fn on_event(
        &self,
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
        event_type: EventType,
    ) -> Result<(), ForgeError> {
        let Some(project) = event.project_id else {
            return Ok(());
        };
        if event_type.visibility_class() == VisibilityClass::Admin {
            return Ok(());
        }
        for target in db::enabled_on(tx, project).await? {
            let Some(material) = db::material(tx, target.webhook).await? else {
                continue;
            };
            match prove_webhook_delivery(&material) {
                Ok(set) => {
                    if set.admits(event_type) {
                        db::record_delivery(tx, target.webhook, event).await?;
                    }
                }
                Err(e) if e.code() == ErrorCode::WebhookTargetForbidden => {
                    let request_id = event.request_id().unwrap_or_else(RequestId::generate);
                    telemetry::log_failure(
                        &e.with_context("subscriber", DISPATCH)
                            .with_context("source_seq", event.seq),
                        &request_id,
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Subscriber for WebhookDispatch {
    fn name(&self) -> &'static str {
        DISPATCH
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(event_type) = EventType::parse(&event.event_type) else {
                tracing::warn!(
                    target: "forge_integ",
                    event = "forge.subscribers.unknown_event_type",
                    subscriber = DISPATCH,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            self.on_event(event, tx, event_type).await
        })
    }
}
