//! What a change must satisfy before anything is written (FG-DB-0010), and
//! the cleaning of the personal, client-supplied context fields.

use forge_errors::{ErrorCode, ForgeError};

use super::{Actor, ChangeContext, ChangeRecord, Outcome, MAX_EVENTS};

/// Largest integer every reader of the sealed audit JSON represents exactly
/// (IEEE-754 doubles, RFC 8785 — ADR-0018 §4).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// Serialized-size bounds (the table CHECKs measure the stored size; these
/// reject before the database does, with a reason).
const MAX_DETAILS_BYTES: usize = 8192;
const MAX_PAYLOAD_BYTES: usize = 65536;

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

/// Every number in `value` is an integer within ±2^53−1 or a float.
fn numbers_are_safe(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                u <= MAX_SAFE_INTEGER
            } else if let Some(i) = n.as_i64() {
                i.unsigned_abs() <= MAX_SAFE_INTEGER
            } else {
                true
            }
        }
        serde_json::Value::Array(items) => items.iter().all(numbers_are_safe),
        serde_json::Value::Object(map) => map.values().all(numbers_are_safe),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => true,
    }
}

/// The client-supplied personal fields, cut to their column's length on a
/// character boundary and without control characters: a long or odd value
/// must never stop its own audit row (a denied request still gets audited).
pub(super) fn sanitized(ctx: &ChangeContext) -> ChangeContext {
    let clean = |value: &Option<String>, max: usize| {
        value
            .as_deref()
            .map(|v| v.chars().filter(|c| !c.is_control()).take(max).collect())
    };
    ChangeContext {
        actor_label: clean(&ctx.actor_label, 64),
        user_agent: clean(&ctx.user_agent, 256),
        ..ctx.clone() // clone: the rest is copied unchanged into the transaction
    }
}

/// Everything that can be checked before touching the database.
pub(super) fn validate(ctx: &ChangeContext, record: &ChangeRecord) -> Result<(), ForgeError> {
    let action = record.audit.action;
    let events = &record.events;
    if events.len() > MAX_EVENTS {
        return Err(rejected("too_many_events", action).with_context("count", events.len()));
    }
    if !events.is_empty() && ctx.actor == Actor::Anonymous {
        return Err(rejected("anonymous_event", action));
    }
    // Events say what happened: only a change that succeeded publishes any.
    if !events.is_empty() && record.audit.outcome != Outcome::Succeeded {
        return Err(rejected("events_on_unsuccessful", action));
    }
    if let Some(e) = events.iter().find(|e| !valid_event_type(e.event_type)) {
        return Err(rejected("invalid_event_type", action).with_context("event_type", e.event_type));
    }
    // A change audited for one project publishes events of that project only.
    if let Some(project) = record.audit.project_id {
        if events.iter().any(|e| e.project_id != Some(project)) {
            return Err(rejected("event_scope_mismatch", action));
        }
    }
    if let Actor::UserViaServiceAccount {
        credential_id,
        act_as_claimed,
        via_assertion_id,
        ..
    } = &ctx.actor
    {
        if ctx.credential_id.is_some_and(|c| c != *credential_id) {
            return Err(rejected("credential_mismatch", action));
        }
        if act_as_claimed.len() > 128 || via_assertion_id.as_deref().map_or(0, str::len) > 128 {
            return Err(rejected("field_too_long", action).with_context("field", "act_as"));
        }
    }
    if ctx.effective_scope.len() > 32 || ctx.effective_scope.iter().any(|s| s.len() > 64) {
        return Err(rejected("field_too_long", action).with_context("field", "effective_scope"));
    }
    let details = record.audit.details.to_string();
    if details.len() > MAX_DETAILS_BYTES {
        return Err(rejected("field_too_long", action).with_context("field", "details"));
    }
    if !numbers_are_safe(&record.audit.details) {
        return Err(rejected("unsafe_number", action).with_context("field", "details"));
    }
    for e in events {
        if e.dedup_key.as_deref().map_or(0, str::len) > 200 {
            return Err(rejected("field_too_long", action).with_context("field", "dedup_key"));
        }
        if e.payload.to_string().len() > MAX_PAYLOAD_BYTES {
            return Err(rejected("field_too_long", action).with_context("field", "payload"));
        }
        if !numbers_are_safe(&e.payload) {
            return Err(rejected("unsafe_number", action).with_context("field", "payload"));
        }
    }
    Ok(())
}
