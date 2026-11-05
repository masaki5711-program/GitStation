//! Project webhooks and their delivery history (M3 I2; ADR-0016 §8 and
//! its Amendment I2; migration 20261120200000).
//!
//! - **Managing** ([`create_decided`], [`update_decided`],
//!   [`delete_decided`], [`rotate_secret_decided`], [`redeliver_decided`]):
//!   decided inside the transaction like every project write — the project
//!   row `FOR SHARE` (a purge, deletion or visibility change waits), the
//!   caller's roles and account `FOR SHARE`, then `decide` (forge-integ:
//!   `Authorized::reconfirm`), then a per-project advisory lock (the
//!   20-webhook count, [`MAX_WEBHOOKS`]) or the webhook's row `FOR UPDATE`.
//!   One `webhook.*` audit row on the project each; a webhook of another
//!   project (or none) is `FG-REPO-0005 resource=webhook` after the
//!   decision. The URL and event types arrive checked by forge-core
//!   (`webhook::TargetPolicy`, `webhook_event_types`), the secret sealed
//!   ([`SealedSecret`]): forge-db never sees a plain secret.
//! - **Dispatching** ([`enabled_on`], [`material`], [`record_delivery`]):
//!   the `webhook-dispatch` subscriber's reads and writes in its step's
//!   transaction ([`DerivedTx`]): one delivery row per (webhook, event)
//!   (`webhook_deliveries_once`) and its first `webhook.deliver` job.
//! - **Delivering** ([`delivery_task`], [`record_attempt`]): what the job
//!   reads (the webhook's URL, sealed secrets, the owner's standing, the
//!   event's body) and the one transaction that records an attempt: the
//!   delivery's state, the webhook's failure count and, at
//!   [`DISABLE_AFTER_FAILURES`] failed attempts in a row or
//!   [`DISABLE_AFTER_FAILING`] of failures, the automatic disable (the
//!   system's `webhook.disabled` audit row and event, `FG-INT-0011` for the
//!   caller to log), and the next attempt's job after the backoff.
//! - **Reading** ([`list`], [`get`], [`deliveries`]) on the read pool, which
//!   never sees a ciphertext; [`purge_deliveries`] is the retention.

use forge_core::ids::{ProjectId, UserId, WebhookDeliveryId, WebhookId};
use forge_core::webhook::SealedSecret;
use forge_core::DisableReason;
use forge_errors::{ErrorCode, ForgeError};

pub use forge_core::webhook::{
    DISABLE_AFTER_FAILING, DISABLE_AFTER_FAILURES, MAX_ATTEMPTS,
    MAX_WEBHOOKS_PER_PROJECT as MAX_WEBHOOKS,
};

mod deliver;
mod dispatch;
mod manage;
mod read;
mod retention;
#[cfg(test)]
mod tests;

pub use deliver::{
    delivery_task, fail_delivery, record_attempt, sweep_stuck, Attempt, AttemptOutcome,
    DeliveryEvent, DeliveryTask, Disabled, EventActor, EventWorkItem, Recorded, RecordedAttempt,
};
pub use dispatch::{enabled_on, material, record_delivery, DispatchTarget};
pub use manage::{
    create_decided, delete_decided, redeliver_decided, rotate_secret_decided, update_decided,
    NewWebhook, Rotated, UpdateOutcome, WebhookPatch,
};
pub use read::{deliveries, get, health, list, WebhooksHealth, MAX_DELIVERIES_LISTED};
pub use retention::{purge_deliveries, RETENTION_BATCH};

/// The audit resource of every webhook change (tables.toml).
const RESOURCE: &str = "project";
/// The job kind of one delivery attempt (forge-integ's handler).
pub const DELIVER_JOB: &str = "webhook.deliver";
/// How often one job may be re-run by the job runner for an internal
/// failure (the delivery's own attempts are counted separately).
pub const JOB_MAX_ATTEMPTS: u8 = 3;
/// A pending delivery without a live `webhook.deliver` job for this long
/// is stuck (its job died: a database failure, a lost lease, a broken
/// payload): the sweep fails it (`FG-INT-0004 reason=stuck`) and doctor's
/// `webhook.deliveries` warns until then.
pub const STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(600);
/// The condition on `d` (a delivery) of a stuck one; `$1` = [`STUCK_AFTER`]
/// in seconds.
pub(crate) const STUCK_WHERE: &str = "d.state = 'pending'      AND coalesce(d.last_attempt_at, d.created_at) < now() - make_interval(secs => $1)      AND NOT EXISTS (SELECT 1 FROM forge.jobs j WHERE j.kind = 'webhook.deliver'          AND j.state IN ('queued', 'running')          AND j.dedup_key LIKE 'delivery:' || d.id || ':' || d.redeliveries || ':%')";
/// One attempt's job may run this long (resolution 3 s + 10 s in all +
/// database steps).
pub const JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A webhook as its project's Maintainers see it (never a secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    pub id: WebhookId,
    pub project: ProjectId,
    pub owner: UserId,
    /// `scheme://host:port` of the URL (the full URL is write-only: its
    /// path or query may hold a receiver's token; I2 review).
    pub origin: String,
    /// Sorted, without duplicates.
    pub event_types: Vec<String>,
    pub enabled: bool,
    pub disabled_reason: Option<DisableReason>,
    /// The version of the current secret (1, then one more per rotation).
    pub secret_version: i32,
    /// The previous secret signs too until then (RFC 3339, UTC), if it
    /// still does.
    pub previous_secret_until: Option<String>,
    pub consecutive_failures: i32,
    pub failing_since: Option<String>,
    pub last_delivery_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A delivery's state (`webhook_deliveries.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// An attempt is queued or running.
    Pending,
    Succeeded,
    /// The last attempt failed (`FG-INT-0004`).
    Failed,
    /// Not sent: `last_reason` says why (`webhook_disabled`, `owner_lost`,
    /// `not_admitted`, `event_gone`).
    Skipped,
}

impl DeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, ForgeError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            other => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unknown_delivery_state")
                .with_context("raw", other.chars().take(16).collect::<String>())),
        }
    }
}

/// One delivery of the history (newest first in [`deliveries`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub id: WebhookDeliveryId,
    /// The `Forge-Delivery` value.
    pub delivery_uid: uuid::Uuid,
    pub webhook: WebhookId,
    pub event_seq: i64,
    pub event_id: uuid::Uuid,
    pub event_type: String,
    pub state: DeliveryState,
    pub attempts: i32,
    pub last_status: Option<i32>,
    pub last_reason: Option<String>,
    pub last_error_code: Option<String>,
    pub last_duration_ms: Option<i32>,
    pub redeliveries: i32,
    pub created_at: String,
    pub last_attempt_at: Option<String>,
    pub finished_at: Option<String>,
}

/// `to_char(<column> AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')`.
macro_rules! utc {
    ($col:literal) => {
        concat!(
            "to_char(",
            $col,
            " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
    };
}
pub(crate) use utc;

fn not_found(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

fn disable_reason(id: i64, raw: Option<&str>) -> Result<Option<DisableReason>, ForgeError> {
    raw.map(|r| {
        DisableReason::parse(r).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unknown_disable_reason")
                .with_context("webhook_id", id)
        })
    })
    .transpose()
}

fn parse_uuid(raw: &str, what: &'static str) -> Result<uuid::Uuid, ForgeError> {
    uuid::Uuid::parse_str(raw).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "stored_uuid_invalid")
            .with_context("column", what)
    })
}

/// A sealed secret as stored (both columns or neither).
fn sealed(key_id: Option<i16>, ciphertext: Option<Vec<u8>>) -> Option<SealedSecret> {
    match (key_id, ciphertext) {
        (Some(key_id), Some(ciphertext)) => Some(SealedSecret { key_id, ciphertext }),
        _ => None,
    }
}
