//! The webhook checks of `forge doctor` (M3 I2, ADR-0016 §8, §11, runbook
//! webhook-delivery):
//!
//! - `webhook.allowed_targets`: how many entries the allowlist has; empty
//!   is the default and means no webhook can be created or delivered.
//! - `webhook.allow_http`: warn when `http` URLs are accepted (development
//!   only).
//! - `webhook.secret_keys`: `webhook.secret_keys_file` read and parsed
//!   (without it webhooks cannot be created and the worker does not
//!   deliver).
//! - `webhook.deliveries` (database): `FG-INT-0004 reason=secret_keys_missing`
//!   when webhooks exist but `webhook.secret_keys_file` is not set (nothing
//!   is delivered), `FG-INT-0011` when the worker disabled a webhook in the
//!   last 24 hours, `FG-INT-0004 reason=pending_too_long` for a pending
//!   delivery without a live job (stuck) or older than 24 hours,
//!   `reason=failed_recently` when a delivery ended `failed` in 24 hours.

use forge_core::config::{Config, WebhookConfig};
use forge_db::webhooks::WebhooksHealth;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, skip, warn, Check};

const TARGETS: &str = "webhook.allowed_targets";
const ALLOW_HTTP: &str = "webhook.allow_http";
const SECRET_KEYS: &str = "webhook.secret_keys";
/// The database check's name (for the database-unavailable skips).
pub(super) const DELIVERIES: &str = "webhook.deliveries";

/// The checks of the configuration alone.
pub(super) fn config_checks(cfg: &Config) -> [Check; 3] {
    let policy = &cfg.webhook.policy;
    let targets = if policy.is_empty() {
        ok(
            TARGETS,
            "empty (the default): no webhook can be created or delivered until the operator lists the receivers",
        )
    } else {
        ok(
            TARGETS,
            &format!(
                "{} entr(ies); loopback, link-local and metadata addresses are refused regardless",
                policy.rules().len()
            ),
        )
    };
    let http = if policy.allow_http() {
        warn(
            ALLOW_HTTP,
            "true: webhooks may deliver over plain http (development only; set it to false in production)",
        )
    } else {
        ok(ALLOW_HTTP, "false: https only")
    };
    let keys = match cfg.webhook.secret_keys() {
        Ok(Some(keys)) => ok(
            SECRET_KEYS,
            &format!("loaded; new secrets are sealed with key_id {}", keys.sealing_key_id()),
        ),
        Ok(None) => skip(
            SECRET_KEYS,
            "webhook.secret_keys_file is not configured: webhooks cannot be created and forge worker does not deliver",
        ),
        Err(e) => fail(SECRET_KEYS, e),
    };
    [targets, http, keys]
}

/// A pending delivery older than this is a warning (a delivery's own
/// attempts end within about 16 hours).
const PENDING_WARN_S: i64 = 24 * 3600;

/// `webhook.deliveries`.
pub(super) async fn deliveries_check(db: &Db, cfg: &WebhookConfig) -> Check {
    match forge_db::webhooks::health(db).await {
        Ok(h) => deliveries_line(&h, cfg.secret_keys_file.is_some()),
        Err(e) => fail(DELIVERIES, e),
    }
}

/// The check's outcome for `h` (`keyed`: `webhook.secret_keys_file` is set).
fn deliveries_line(h: &WebhooksHealth, keyed: bool) -> Check {
    if !keyed && h.webhooks > 0 {
        // I2 review: an existing workload the worker does not deliver.
        return fail(
            DELIVERIES,
            ForgeError::new(ErrorCode::WebhookDeadLettered)
                .with_context("reason", "secret_keys_missing")
                .with_context("config_key", forge_core::webhook::SECRET_KEYS_KEY)
                .with_context("webhooks", h.webhooks),
        );
    }
    if h.disabled_24h > 0 {
        return fail(
            DELIVERIES,
            ForgeError::new(ErrorCode::WebhookDisabled)
                .with_context("reason", "disabled_recently")
                .with_context("webhooks_disabled_24h", h.disabled_24h)
                .with_context("deliveries_failed_24h", h.failed_24h),
        );
    }
    if h.stuck_pending > 0 || h.oldest_pending_s.is_some_and(|s| s > PENDING_WARN_S) {
        return fail(
            DELIVERIES,
            ForgeError::new(ErrorCode::WebhookDeadLettered)
                .with_context("reason", "pending_too_long")
                .with_context("stuck_pending", h.stuck_pending)
                .with_context(
                    "oldest_pending_s",
                    h.oldest_pending_s
                        .map_or_else(|| "none".to_owned(), |s| s.to_string()),
                ),
        );
    }
    if h.failed_24h > 0 {
        return fail(
            DELIVERIES,
            ForgeError::new(ErrorCode::WebhookDeadLettered)
                .with_context("reason", "failed_recently")
                .with_context("deliveries_failed_24h", h.failed_24h),
        );
    }
    ok(
        DELIVERIES,
        &format!(
            "{} webhook(s); no delivery failed and none was disabled in 24 h; oldest pending {}",
            h.webhooks,
            h.oldest_pending_s
                .map_or_else(|| "none".to_owned(), |s| format!("{s} s"))
        ),
    )
}

#[cfg(test)]
mod tests {
    use forge_core::config::WebhookConfig;
    use forge_db::test_support::FreshDb;

    use super::*;

    fn health(
        webhooks: i64,
        stuck: i64,
        oldest: Option<i64>,
    ) -> forge_db::webhooks::WebhooksHealth {
        forge_db::webhooks::WebhooksHealth {
            webhooks,
            failed_24h: 0,
            disabled_24h: 0,
            oldest_pending_s: oldest,
            stuck_pending: stuck,
        }
    }

    fn is_warn(c: &Check) -> bool {
        matches!(&c.outcome, super::super::Outcome::Warn(_))
            || matches!(&c.outcome, super::super::Outcome::Fail(e) if e.code().severity() == forge_errors::Severity::Warn)
    }

    /// I2 review: a stuck pending delivery, a pending one older than a
    /// day, and webhooks without secret keys are each a warning.
    #[test]
    fn stuck_old_or_unkeyed_work_is_a_warning() {
        let keyed = true;
        assert!(!is_warn(&deliveries_line(&health(2, 0, Some(60)), keyed)));
        assert!(is_warn(&deliveries_line(&health(2, 1, Some(60)), keyed)));
        assert!(is_warn(&deliveries_line(
            &health(2, 0, Some(25 * 3600)),
            keyed
        )));
        assert!(is_warn(&deliveries_line(&health(2, 0, None), false)));
        assert!(!is_warn(&deliveries_line(&health(0, 0, None), false)));
    }

    #[tokio::test]
    async fn an_empty_database_is_ok() {
        let t = FreshDb::migrated().await;
        let c = deliveries_check(&t.db, &WebhookConfig::default()).await;
        assert!(
            matches!(c.outcome, super::super::Outcome::Ok(_)),
            "{:?}",
            c.name
        );
        t.drop_now().await;
    }
}
