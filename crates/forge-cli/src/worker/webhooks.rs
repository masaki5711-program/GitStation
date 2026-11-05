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
//! The `webhook_delivery_retention` task (every hour, at once again after
//! a full batch) deletes finished deliveries older than
//! `webhook.delivery_retention_days` (it runs with or without the lane:
//! history recorded before the keys were removed still ages out).

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::WebhookConfig;
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::webhooks::{purge_deliveries, RETENTION_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;
use forge_integ::webhooks::{DeliverHandler, Transport};

use super::supervise::Next;

/// The lane's runner task name.
pub(crate) const RUNNER_NAME: &str = "webhook_runner";
/// The retention task's name.
pub(crate) const RETENTION_NAME: &str = "webhook_delivery_retention";
/// How often the retention runs when nothing is left.
pub(crate) const RETENTION_PERIOD: Duration = Duration::from_secs(3600);
/// The pause after a full batch.
const BATCH_PAUSE: Duration = Duration::from_millis(500);

/// The lane's own job runner (`webhook.deliver` only); `None` without
/// `webhook.secret_keys_file`.
pub(crate) fn lane(db: &Db, cfg: &WebhookConfig) -> Result<Option<Arc<JobRunner>>, ForgeError> {
    let Some(keys) = cfg.secret_keys()? else {
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
    Ok(Some(Arc::new(runner)))
}

/// One retention run: one batch; a full one asks to run again shortly.
pub(crate) async fn run_retention(db: &Db, days: u32) -> Result<Next, ForgeError> {
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
            .expect("lane")
            .is_none());
        assert_eq!(
            run_retention(&t.db, 30).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
