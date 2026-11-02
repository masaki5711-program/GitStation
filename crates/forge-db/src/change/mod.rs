//! Writing changes (ADR-0016 §6): every state change, its audit row and its
//! domain events are written in one transaction by [`ChangeTx`]
//! (crate-private). The typed commands of forge-db build on it; nothing
//! outside this crate writes (root gate db-write-boundary), and an event
//! without an audit row in its transaction cannot commit (deferred trigger,
//! XF002 → FG-DB-0013).
//!
//! The types here describe a change: who made it ([`ChangeContext`]), what
//! the audit log records ([`AuditEntry`]) and which events it publishes
//! ([`DomainEvent`]), together a [`ChangeRecord`].

use std::net::IpAddr;

use forge_core::ids::{ProjectId, UserId};
use forge_core::{DedupKey, EventType, RequestId};
use forge_errors::{ErrorCode, ForgeError};

mod tx;
mod validate;

pub(crate) use tx::{ChangeTx, Settle};
// The event checks DerivedTx shares (subscribers, C5).
pub(crate) use validate::{dedup_key_ok, numbers_are_safe, MAX_PAYLOAD_BYTES};

/// At most this many events per change: a command that needs more emits one
/// aggregate event instead.
pub const MAX_EVENTS: usize = 100;

/// Who made a change. The variants mirror the `actor_kind` check
/// constraints of forge.events and forge.audit_log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    User(UserId),
    ServiceAccount(UserId),
    /// A service account (kanflow) acting for a person (ADR-0017 §4).
    UserViaServiceAccount {
        user: UserId,
        via: UserId,
        credential_id: i64,
        /// Keyed HMAC of the claimed person (the key lives outside the DB).
        act_as_claimed: String,
        act_as_key_id: i16,
        via_assertion_id: Option<String>,
    },
    System,
    /// Audit rows only (a failed login, a denied anonymous request): an
    /// anonymous actor never publishes events.
    Anonymous,
}

impl Actor {
    fn kind(&self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::ServiceAccount(_) => "service_account",
            Self::UserViaServiceAccount { .. } => "user_via_service_account",
            Self::System => "system",
            Self::Anonymous => "anonymous",
        }
    }

    pub(crate) fn user(&self) -> Option<i64> {
        match self {
            Self::User(u)
            | Self::ServiceAccount(u)
            | Self::UserViaServiceAccount { user: u, .. } => Some(u.get()),
            Self::System | Self::Anonymous => None,
        }
    }

    fn via(&self) -> Option<i64> {
        match self {
            Self::UserViaServiceAccount { via, .. } => Some(via.get()),
            Self::User(_) | Self::ServiceAccount(_) | Self::System | Self::Anonymous => None,
        }
    }
}

/// The request a change belongs to.
#[derive(Debug, Clone)]
pub struct ChangeContext {
    pub actor: Actor,
    pub request_id: RequestId,
    /// The token or session used (its id, never its value).
    pub credential_id: Option<i64>,
    pub effective_scope: Vec<String>,
    /// Personal data (outside the audit seal, redacted after 365 days).
    pub actor_label: Option<String>,
    pub source_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

impl ChangeContext {
    pub fn new(actor: Actor, request_id: RequestId) -> Self {
        Self {
            actor,
            request_id,
            credential_id: None,
            effective_scope: Vec::new(),
            actor_label: None,
            source_ip: None,
            user_agent: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Denied,
    Failed,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }
}

/// The audit row of a change.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// `<resource>.<verb>`, e.g. `repository.created`.
    pub action: &'static str,
    pub outcome: Outcome,
    pub resource_type: &'static str,
    pub resource_id: Option<i64>,
    pub project_id: Option<ProjectId>,
    pub error_code: Option<ErrorCode>,
    /// Safe details only (no secrets, no request bodies; ≤ 8 KiB).
    pub details: serde_json::Value,
}

/// A domain event published by a change (ADR-0016 §5).
#[derive(Debug, Clone)]
pub struct DomainEvent {
    /// The registered type (`forge_core::EventType`; its name passes the
    /// table CHECK, test `every_event_type_passes_the_db_check`).
    pub event_type: EventType,
    pub schema_version: i16,
    pub aggregate_type: &'static str,
    pub aggregate_id: i64,
    /// None = instance-wide (never shown to integrations or webhooks).
    pub project_id: Option<ProjectId>,
    pub payload: serde_json::Value,
    /// Idempotency of the change (e.g. `push:<repo>:<hash>`): a second
    /// change with the same key is [`Committed::Deduped`] and rolled back.
    /// Keys are unique instance-wide, so [`DedupKey`] is built from
    /// server-side ids, hashes and fixed words only (ADR-0016 D3 amendment);
    /// the writers check its shape again (`invalid_dedup_key`).
    pub dedup_key: Option<DedupKey>,
}

/// At most this many further audit rows per change ([`ChangeRecord::affected`]):
/// a subtree move audits each project it moves (at most 1000 namespaces,
/// ADR-0015 Amendment D5) and the causes of the tombstones it reclaims.
pub const MAX_AFFECTED: usize = 2000;

/// Everything a change records besides its state writes.
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    /// The change's own audit row (its action names the change).
    pub audit: AuditEntry,
    /// One further audit row per other resource the change changed, with
    /// the same outcome (D5: a subtree move writes one `project` row per
    /// project it moves, so each project's audit trail shows the move;
    /// ADR-0016 Amendment D5). Usually empty.
    pub affected: Vec<AuditEntry>,
    pub events: Vec<DomainEvent>,
}

impl ChangeRecord {
    /// A change that publishes no event, only its succeeded audit row
    /// (`user.*`, `group.*` and `member.*` are audit-only): the action and
    /// JSON come from `details`, the resource from `resource_type` / `id`.
    pub(crate) fn audit_only(
        details: &forge_core::AuditDetails,
        resource_type: &'static str,
        resource_id: i64,
    ) -> Self {
        Self {
            audit: AuditEntry {
                action: details.action(),
                outcome: Outcome::Succeeded,
                resource_type,
                resource_id: Some(resource_id),
                project_id: None,
                error_code: None,
                details: details.to_value(),
            },
            affected: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Every audit row of the change: [`Self::audit`] first, then
    /// [`Self::affected`] in order.
    pub(crate) fn audit_rows(&self) -> impl Iterator<Item = &AuditEntry> {
        std::iter::once(&self.audit).chain(self.affected.iter())
    }
}

/// The result of [`ChangeTx::commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Committed {
    /// Written; the published events' ids (UUIDv7) in order.
    Recorded { event_ids: Vec<uuid::Uuid> },
    /// An event's `dedup_key` was already recorded: nothing was written
    /// (state writes of the change included).
    Deduped,
}

impl Committed {
    /// For a change without events, which is never deduplicated: a
    /// `Deduped` would mean the change was rolled back, so it is not
    /// reported as success (`FG-SYS-0001`, `reason=unexpected_dedup`).
    pub(crate) fn require_recorded(self, action: &'static str) -> Result<(), ForgeError> {
        match self {
            Self::Recorded { .. } => Ok(()),
            Self::Deduped => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unexpected_dedup")
                .with_context("action", action)),
        }
    }
}

#[cfg(test)]
mod tests;
