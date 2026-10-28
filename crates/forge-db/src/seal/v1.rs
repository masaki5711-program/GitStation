//! Hash version 1 of the audit seal (ADR-0018 §4) — the ONE place that
//! fixes which columns of `forge.audit_log` are sealed, how they are read,
//! and how a row becomes the bytes that are hashed. Changing anything here
//! breaks every existing seal: add a `v2` instead and keep this one for the
//! ranges it sealed (test `audit_seal_v1_golden_vector`).
//!
//! A row is a JSON object keyed by column name, canonicalized by RFC 8785
//! (JCS): `bigint` as a decimal string; `smallint` / `integer` as numbers;
//! `timestamptz` as UTC ISO 8601 with microseconds (`…T…​.ffffffZ`, by the
//! database's `to_char`); `text` as strings; `text[]` as an array in stored
//! order; `jsonb` (details) re-read from its text and canonicalized, where
//! only integers within ±2^53−1 stay numbers and every other number —
//! fractions, exponents, larger integers, `-0` — becomes the string
//! PostgreSQL prints for it (exact for any stored value: a jsonb numeric
//! can hold more digits than a double, and a rounded value would let a
//! change go unseen); NULL as `null`. The personal columns
//! (`actor_label`, `source_ip`, `user_agent`) are not sealed: redacting them
//! keeps a seal valid.

use forge_errors::{ErrorCode, ForgeError};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use sqlx::postgres::PgRow;
use sqlx::Row as _;

/// The value stored in `audit_seal.hash_version`.
pub const VERSION: i16 = 1;

/// The sealed columns, in the order of [`SELECT`].
pub const COLUMNS: [&str; 19] = [
    "seq",
    "occurred_at",
    "actor_kind",
    "actor_user_id",
    "actor_via_id",
    "credential_id",
    "act_as_claimed",
    "act_as_key_id",
    "via_assertion_id",
    "effective_scope",
    "action",
    "outcome",
    "resource_type",
    "resource_id",
    "project_id",
    "request_id",
    "error_code",
    "occurrences",
    "details",
];

/// The audit columns that are never sealed (redaction may clear them).
pub const UNSEALED: [&str; 3] = ["actor_label", "source_ip", "user_agent"];

/// The sealed rows with `$1 <= seq <= $2`, in seq order. Every name is
/// qualified: it runs under `search_path = pg_catalog, pg_temp`.
pub(crate) const SELECT: &str = "SELECT a.seq, \
     pg_catalog.to_char(a.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') \
         AS occurred_at, \
     a.actor_kind, a.actor_user_id, a.actor_via_id, a.credential_id, a.act_as_claimed, \
     a.act_as_key_id, a.via_assertion_id, a.effective_scope, a.action, a.outcome, \
     a.resource_type, a.resource_id, a.project_id, a.request_id, a.error_code, a.occurrences, \
     a.details::text AS details \
     FROM ONLY forge.audit_log AS a \
     WHERE a.seq BETWEEN $1 AND $2 \
     ORDER BY a.seq";

/// Largest integer every reader of the sealed JSON represents exactly.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// The sealed columns of one audit row, as read by [`SELECT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub seq: i64,
    /// Already in the sealed form (UTC, microseconds).
    pub occurred_at: String,
    pub actor_kind: String,
    pub actor_user_id: Option<i64>,
    pub actor_via_id: Option<i64>,
    pub credential_id: Option<i64>,
    pub act_as_claimed: Option<String>,
    pub act_as_key_id: Option<i16>,
    pub via_assertion_id: Option<String>,
    pub effective_scope: Option<Vec<Option<String>>>,
    pub action: String,
    pub outcome: String,
    pub resource_type: String,
    pub resource_id: Option<i64>,
    pub project_id: Option<i64>,
    pub request_id: String,
    pub error_code: Option<String>,
    pub occurrences: i32,
    /// The jsonb's text.
    pub details: String,
}

impl Row {
    pub(crate) fn from_pg(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            seq: row.try_get("seq")?,
            occurred_at: row.try_get("occurred_at")?,
            actor_kind: row.try_get("actor_kind")?,
            actor_user_id: row.try_get("actor_user_id")?,
            actor_via_id: row.try_get("actor_via_id")?,
            credential_id: row.try_get("credential_id")?,
            act_as_claimed: row.try_get("act_as_claimed")?,
            act_as_key_id: row.try_get("act_as_key_id")?,
            via_assertion_id: row.try_get("via_assertion_id")?,
            effective_scope: row.try_get("effective_scope")?,
            action: row.try_get("action")?,
            outcome: row.try_get("outcome")?,
            resource_type: row.try_get("resource_type")?,
            resource_id: row.try_get("resource_id")?,
            project_id: row.try_get("project_id")?,
            request_id: row.try_get("request_id")?,
            error_code: row.try_get("error_code")?,
            occurrences: row.try_get("occurrences")?,
            details: row.try_get("details")?,
        })
    }

    /// The RFC 8785 bytes that are hashed.
    pub fn canonical(&self) -> Result<Vec<u8>, ForgeError> {
        let big = |v: Option<i64>| v.map_or(Value::Null, |n| Value::String(n.to_string()));
        let text = |v: Option<&str>| v.map_or(Value::Null, Value::from);
        let details: Value = serde_json::from_str(&quote_unsafe_numbers(&self.details))
            .map_err(|e| unserializable(self.seq, "details_unreadable").with_source(e))?;
        let scope = self.effective_scope.as_ref().map_or(Value::Null, |items| {
            Value::Array(items.iter().map(|i| text(i.as_deref())).collect())
        });
        let fields: [(&str, Value); 19] = [
            ("seq", Value::String(self.seq.to_string())),
            ("occurred_at", Value::from(self.occurred_at.as_str())),
            ("actor_kind", Value::from(self.actor_kind.as_str())),
            ("actor_user_id", big(self.actor_user_id)),
            ("actor_via_id", big(self.actor_via_id)),
            ("credential_id", big(self.credential_id)),
            ("act_as_claimed", text(self.act_as_claimed.as_deref())),
            (
                "act_as_key_id",
                self.act_as_key_id.map_or(Value::Null, Value::from),
            ),
            ("via_assertion_id", text(self.via_assertion_id.as_deref())),
            ("effective_scope", scope),
            ("action", Value::from(self.action.as_str())),
            ("outcome", Value::from(self.outcome.as_str())),
            ("resource_type", Value::from(self.resource_type.as_str())),
            ("resource_id", big(self.resource_id)),
            ("project_id", big(self.project_id)),
            ("request_id", Value::from(self.request_id.as_str())),
            ("error_code", text(self.error_code.as_deref())),
            ("occurrences", Value::from(self.occurrences)),
            ("details", details),
        ];
        let object: Map<String, Value> =
            fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        serde_json_canonicalizer::to_vec(&Value::Object(object))
            .map_err(|e| unserializable(self.seq, "canonicalization").with_source(e))
    }

    /// The row hash: SHA-256 of [`Row::canonical`].
    pub fn sha256(&self) -> Result<[u8; 32], ForgeError> {
        Ok(Sha256::digest(self.canonical()?).into())
    }
}

/// JSON text (as PostgreSQL prints a jsonb) with every number token that is
/// not an integer within ±2^53−1 put in quotes, verbatim. Strings (keys and
/// values) are copied unchanged, escapes included.
pub fn quote_unsafe_numbers(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 8);
    let mut chars = json.char_indices().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some((i, c)) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c != '-' && !c.is_ascii_digit() {
            out.push(c);
            continue;
        }
        let mut end = i + c.len_utf8();
        while let Some(&(j, d)) = chars.peek() {
            if !(d.is_ascii_digit() || matches!(d, '.' | 'e' | 'E' | '+' | '-')) {
                break;
            }
            end = j + d.len_utf8();
            chars.next();
        }
        let token = json.get(i..end).unwrap_or_default();
        if is_safe_integer(token) {
            out.push_str(token);
        } else {
            out.push('"');
            out.push_str(token);
            out.push('"');
        }
    }
    out
}

/// `-?(0|[1-9][0-9]*)` within ±2^53−1, not `-0`.
fn is_safe_integer(token: &str) -> bool {
    let digits = token.strip_prefix('-').unwrap_or(token);
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && token != "-0";
    canonical && digits.parse::<u64>().is_ok_and(|n| n <= MAX_SAFE_INTEGER)
}

/// A stored row that hash version 1 cannot express (a jsonb number beyond
/// the range of a double, for example — only a raw SQL insert can store
/// one). Sealing stops at that row; the runbook names the cause.
fn unserializable(seq: i64, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("operation", "seal.v1")
        .with_context("reason", reason)
        .with_context("seq", seq)
}
