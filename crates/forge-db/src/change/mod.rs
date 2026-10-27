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
// The first production callers are the typed commands (step D, users and
// projects); until then only the tests use ChangeTx.
#![cfg_attr(not(test), allow(dead_code))]

use std::net::IpAddr;

use forge_core::ids::{ProjectId, UserId};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

mod tx;

#[cfg_attr(not(test), allow(unused_imports))]
// first production callers: typed commands (step D)
pub(crate) use tx::ChangeTx;

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

    fn user(&self) -> Option<i64> {
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
    /// Lowercase dotted name, e.g. `repository.pushed`.
    pub event_type: &'static str,
    pub schema_version: i16,
    pub aggregate_type: &'static str,
    pub aggregate_id: i64,
    /// None = instance-wide (never shown to integrations or webhooks).
    pub project_id: Option<ProjectId>,
    pub payload: serde_json::Value,
    /// Idempotency of the change (e.g. `push:<repo>:<hash>`): a second
    /// change with the same key is [`Committed::Deduped`] and rolled back.
    pub dedup_key: Option<String>,
}

/// Everything a change records besides its state writes.
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    pub audit: AuditEntry,
    pub events: Vec<DomainEvent>,
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

fn rejected(reason: &'static str, action: &str) -> ForgeError {
    ForgeError::new(ErrorCode::ChangeRejected)
        .with_context("reason", reason)
        .with_context("action", action)
}

/// `^[a-z][a-z_]*(\.[a-z][a-z_]*){1,3}$` (the events.event_type check).
fn valid_event_type(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            p.starts_with(|c: char| c.is_ascii_lowercase())
                && p.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        })
}

/// Everything that can be checked before touching the database.
fn validate(ctx: &ChangeContext, record: &ChangeRecord) -> Result<(), ForgeError> {
    let action = record.audit.action;
    if record.events.len() > MAX_EVENTS {
        return Err(rejected("too_many_events", action).with_context("count", record.events.len()));
    }
    if !record.events.is_empty() && ctx.actor == Actor::Anonymous {
        return Err(rejected("anonymous_event", action));
    }
    if let Some(e) = record
        .events
        .iter()
        .find(|e| !valid_event_type(e.event_type))
    {
        return Err(rejected("invalid_event_type", action).with_context("event_type", e.event_type));
    }
    let too_long = [
        (
            "actor_label",
            ctx.actor_label.as_deref().map_or(0, str::len),
            64,
        ),
        (
            "user_agent",
            ctx.user_agent.as_deref().map_or(0, str::len),
            256,
        ),
    ]
    .into_iter()
    .chain(
        record
            .events
            .iter()
            .map(|e| ("dedup_key", e.dedup_key.as_deref().map_or(0, str::len), 200)),
    )
    .find(|(_, len, max)| len > max);
    if let Some((field, _, _)) = too_long {
        return Err(rejected("field_too_long", action).with_context("field", field));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
