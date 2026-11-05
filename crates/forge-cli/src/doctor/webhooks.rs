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
//! - `webhook.deliveries` (database): `FG-INT-0011` when the worker
//!   disabled a webhook in the last 24 hours, `FG-INT-0004` when a delivery
//!   ended `failed` then; the oldest pending delivery's age is shown.

use forge_core::config::Config;
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

/// `webhook.deliveries`.
pub(super) async fn deliveries_check(db: &Db) -> Check {
    match forge_db::webhooks::health(db).await {
        Ok(h) if h.disabled_24h > 0 => fail(
            DELIVERIES,
            ForgeError::new(ErrorCode::WebhookDisabled)
                .with_context("reason", "disabled_recently")
                .with_context("webhooks_disabled_24h", h.disabled_24h)
                .with_context("deliveries_failed_24h", h.failed_24h),
        ),
        Ok(h) if h.failed_24h > 0 => fail(
            DELIVERIES,
            ForgeError::new(ErrorCode::WebhookDeadLettered)
                .with_context("reason", "failed_recently")
                .with_context("deliveries_failed_24h", h.failed_24h),
        ),
        Ok(h) => ok(
            DELIVERIES,
            &format!(
                "{} webhook(s); no delivery failed and none was disabled in 24 h; oldest pending {}",
                h.webhooks,
                h.oldest_pending_s
                    .map_or_else(|| "none".to_owned(), |s| format!("{s} s"))
            ),
        ),
        Err(e) => fail(DELIVERIES, e),
    }
}
