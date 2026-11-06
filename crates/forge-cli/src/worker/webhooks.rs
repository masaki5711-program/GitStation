//! The webhook lane of `forge worker` (M3 I2, ADR-0016 §8, runbook
//! webhook-delivery): with `webhook.secret_keys_file` configured, the
//! `webhook-dispatch` subscriber joins the others and `webhook.deliver`
//! runs on its OWN job runner (like the maintenance lane): a slow receiver
//! holds an attempt for up to 13 s (resolution 3 s + 10 s in all) and must
//! not delay merge or provisioning jobs on the one-job-at-a-time main
//! runner. The keys and `webhook.ca_file` are read at the worker's start
//! (`FG-SYS-0002` on a bad one: the worker does not start). Without the
//! keys nothing is dispatched or delivered (doctor `webhook.secret_keys`).
//!
//! The `webhook_delivery_retention` task (every 5 minutes, at once again
//! after a full batch) first fails stuck deliveries (pending without a live
//! job for 10 minutes: their job died; `FG-INT-0004 reason=stuck` per
//! delivery, I2 review), then deletes finished deliveries older than
//! `webhook.delivery_retention_days`. It runs with or without the lane.
//!
//! Without the keys while webhooks exist, the start logs
//! `forge.worker.webhook_lane_off` at warn with `error_code=FG-SYS-0002`
//! (doctor `webhook.deliveries` warns too). With the lane, the dispatch
//! subscriber's first registration starts at the head: earlier events are
//! never delivered; the start logs where it is (`last_seq`).

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::WebhookConfig;
use forge_core::{telemetry, RequestId};
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::subscribers::{self, Start};
use forge_db::webhooks::{purge_deliveries, sweep_stuck, RETENTION_BATCH};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::webhooks::{DeliverHandler, Transport, WebhookDispatch, DISPATCH};

use super::supervise::Next;

/// The lane's runner task name.
pub(crate) const RUNNER_NAME: &str = "webhook_runner";
/// The retention task's name.
pub(crate) const RETENTION_NAME: &str = "webhook_delivery_retention";
/// How often the retention runs when nothing is left.
pub(crate) const RETENTION_PERIOD: Duration = Duration::from_secs(300);
/// Stuck deliveries failed per run.
const SWEEP_BATCH: i64 = 100;
/// The pause after a full batch.
const BATCH_PAUSE: Duration = Duration::from_millis(500);

/// The lane's own job runner (`webhook.deliver` only); `None` without
/// `webhook.secret_keys_file`.
pub(crate) async fn lane(
    db: &Db,
    cfg: &WebhookConfig,
) -> Result<Option<Arc<JobRunner>>, ForgeError> {
    let Some(keys) = cfg.secret_keys()? else {
        let health = forge_db::webhooks::health(db).await?;
        if health.webhooks > 0 {
            tracing::warn!(
                target: "forge_cli",
                event = "forge.worker.webhook_lane_off",
                error_code = ErrorCode::ConfigInvalid.id(),
                config_key = forge_core::webhook::SECRET_KEYS_KEY,
                reason = "missing",
                webhooks = health.webhooks,
                pending = health.oldest_pending_s.is_some(),
                "webhooks exist but webhook.secret_keys_file is not set: nothing is delivered"
            );
        }
        return Ok(None);
    };
    let ca = cfg.ca_pem()?;
    let transport = Transport::new(cfg.policy.clone(), ca.as_deref())?; // clone: the lane owns its policy
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    runner.register(Arc::new(DeliverHandler::new(
        db,
        Arc::new(keys),
        transport,
    )?))?;
    // Registered here (a no-op when it exists) to log where dispatch
    // starts: a first registration is at the head (I2 review).
    subscribers::register(db, &WebhookDispatch, Start::Head).await?;
    let last_seq = subscribers::states(db)
        .await?
        .and_then(|all| all.into_iter().find(|s| s.name == DISPATCH))
        .map(|s| s.last_seq);
    tracing::info!(
        target: "forge_cli",
        event = "forge.worker.webhook_dispatch_from",
        subscriber = DISPATCH,
        last_seq = last_seq.unwrap_or(0),
        "webhook dispatch continues after this event seq (earlier events are not delivered)"
    );
    Ok(Some(Arc::new(runner)))
}

/// One retention run: one batch; a full one asks to run again shortly.
pub(crate) async fn run_retention(db: &Db, days: u32) -> Result<Next, ForgeError> {
    let stuck = sweep_stuck(db, SWEEP_BATCH).await?;
    if !stuck.is_empty() {
        let request_id = RequestId::generate();
        for delivery in &stuck {
            telemetry::log_failure(
                &ForgeError::new(ErrorCode::WebhookDeadLettered)
                    .with_context("reason", "stuck")
                    .with_context("task", RETENTION_NAME)
                    .with_context("delivery_row_id", delivery.get()),
                &request_id,
            );
        }
    }
    let deleted = purge_deliveries(db, days, RETENTION_BATCH).await?;
    if deleted > 0 {
        tracing::info!(
            target: "forge_cli",
            task = RETENTION_NAME,
            deleted,
            older_than_days = days,
            "forge.worker.webhook_deliveries_expired"
        );
    }
    if i64::try_from(deleted).unwrap_or(i64::MAX) >= RETENTION_BATCH {
        tokio::time::sleep(BATCH_PAUSE).await;
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    #[tokio::test]
    async fn without_secret_keys_there_is_no_lane_and_retention_waits() {
        let t = FreshDb::migrated().await;
        assert!(lane(&t.db, &WebhookConfig::default())
            .await
            .expect("lane")
            .is_none());
        assert_eq!(
            run_retention(&t.db, 30).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
