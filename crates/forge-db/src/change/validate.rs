//! What a change must satisfy before anything is written (FG-DB-0010), and
//! the cleaning of the client-supplied fields.

use forge_errors::{ErrorCode, ForgeError};

use super::{Actor, ChangeContext, ChangeRecord, Outcome, MAX_EVENTS};

/// Largest integer every reader of the sealed audit JSON represents exactly
/// (IEEE-754 doubles, RFC 8785 — ADR-0018 §4).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// Stored-size bounds of the table CHECKs (`pg_column_size` of the jsonb,
/// measured by the database: the text length can differ either way).
const MAX_DETAILS_BYTES: i32 = 8192;
const MAX_PAYLOAD_BYTES: i32 = 65536;
/// Column lengths in characters (the table CHECKs use `length()`).
const MAX_LABEL_CHARS: usize = 64;
const MAX_USER_AGENT_CHARS: usize = 256;
const MAX_ACT_AS_CHARS: usize = 128;
const MAX_SCOPES: usize = 32;
const MAX_SCOPE_CHARS: usize = 64;
const MAX_DEDUP_KEY_CHARS: usize = 200;

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

/// Control characters plus the invisible ones that change how text displays
/// (bidi overrides and isolates, zero-width, soft hyphen, BOM): an audit
/// view must show who acted as it was recorded.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

fn clean(value: &str, max: usize) -> String {
    value.chars().filter(|c| !hidden(*c)).take(max).collect()
}

/// The client-supplied fields, cut to their column's length on a character
/// boundary and without hidden characters: a long or odd value must never
/// stop its own audit row (a denied request still gets audited).
pub(super) fn sanitized(ctx: &ChangeContext) -> ChangeContext {
    let actor = match &ctx.actor {
        Actor::UserViaServiceAccount {
            user,
            via,
            credential_id,
            act_as_claimed,
            act_as_key_id,
            via_assertion_id,
        } => Actor::UserViaServiceAccount {
            user: *user,
            via: *via,
            credential_id: *credential_id,
            act_as_claimed: clean(act_as_claimed, MAX_ACT_AS_CHARS),
            act_as_key_id: *act_as_key_id,
            via_assertion_id: via_assertion_id
                .as_deref()
                .map(|v| clean(v, MAX_ACT_AS_CHARS)),
        },
        Actor::User(_) | Actor::ServiceAccount(_) | Actor::System | Actor::Anonymous => {
            ctx.actor.clone() // clone: copied unchanged into the transaction
        }
    };
    ChangeContext {
        actor,
        effective_scope: ctx
            .effective_scope
            .iter()
            .take(MAX_SCOPES)
            .map(|s| clean(s, MAX_SCOPE_CHARS))
            .collect(),
        actor_label: ctx
            .actor_label
            .as_deref()
            .map(|v| clean(v, MAX_LABEL_CHARS)),
        user_agent: ctx
            .user_agent
            .as_deref()
            .map(|v| clean(v, MAX_USER_AGENT_CHARS)),
        ..ctx.clone() // clone: the rest is copied unchanged into the transaction
    }
}

/// The audit details of a change that did not succeed may carry values from
/// the refused request; when they do not fit they are replaced by a marker
/// instead of refusing the change, so the refusal is still audited.
fn drop_details(record: &mut ChangeRecord, problem: &'static str) {
    record.audit.details = serde_json::json!({ "details_dropped": problem });
}

/// Before `validate`: unsafe numbers in the details of an unsuccessful change.
pub(super) fn fitted(mut record: ChangeRecord) -> ChangeRecord {
    if record.audit.outcome != Outcome::Succeeded && !numbers_are_safe(&record.audit.details) {
        drop_details(&mut record, "unsafe_number");
    }
    record
}

/// After `validate`, with the stored sizes the database measured.
pub(super) fn fit_sizes(
    mut record: ChangeRecord,
    details_bytes: i32,
    payload_bytes: &[i32],
) -> Result<ChangeRecord, ForgeError> {
    let action = record.audit.action;
    if details_bytes > MAX_DETAILS_BYTES {
        if record.audit.outcome != Outcome::Succeeded {
            drop_details(&mut record, "too_long");
        } else {
            return Err(rejected("field_too_long", action).with_context("field", "details"));
        }
    }
    if payload_bytes.iter().any(|b| *b > MAX_PAYLOAD_BYTES) {
        return Err(rejected("field_too_long", action).with_context("field", "payload"));
    }
    Ok(record)
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
    if let Actor::UserViaServiceAccount { credential_id, .. } = &ctx.actor {
        if ctx.credential_id.is_some_and(|c| c != *credential_id) {
            return Err(rejected("credential_mismatch", action));
        }
    }
    // Only a succeeded change gets here with unsafe details (see `fitted`).
    if !numbers_are_safe(&record.audit.details) {
        return Err(rejected("unsafe_number", action).with_context("field", "details"));
    }
    for e in events {
        if e.dedup_key.as_deref().map_or(0, |k| k.chars().count()) > MAX_DEDUP_KEY_CHARS {
            return Err(rejected("field_too_long", action).with_context("field", "dedup_key"));
        }
        if !numbers_are_safe(&e.payload) {
            return Err(rejected("unsafe_number", action).with_context("field", "payload"));
        }
    }
    Ok(())
}
