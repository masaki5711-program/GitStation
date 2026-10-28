//! [`DerivedTx`] (ADR-0016 §6): what a subscriber may write while it
//! handles one source event — derived tables through [`DerivedTx::conn`]
//! and derived events through [`DerivedTx::emit`]. It is the step's own
//! transaction (inside a savepoint), so everything commits together with the
//! subscriber's progress past the source event, or not at all.
//!
//! Derived events need no audit row: their lineage (`caused_by_seq`,
//! `root_seq`, `causation_depth`) is set here from the source event, the
//! actor is `system` and the request id is the source event's. The limits
//! are checked before writing (the triggers check them again):
//! [`MAX_DEPTH`] and [`MAX_DERIVED_PER_ROOT`] — reaching either is
//! `FG-DB-0008 reason=fanout_limit` and is remembered ([`DerivedTx::limit`])
//! even if the handler swallows the error.

use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{SourceEvent, MAX_DEPTH, MAX_DERIVED_PER_ROOT};
use crate::change::{
    numbers_are_safe, valid_event_type, DomainEvent, MAX_DEDUP_KEY_CHARS, MAX_PAYLOAD_BYTES,
};
use crate::query_error;

/// Which derivation limit a handler reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The source event is at [`MAX_DEPTH`].
    Depth,
    /// The root already has [`MAX_DERIVED_PER_ROOT`] derived events.
    PerRoot,
}

impl Limit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Depth => "depth",
            Self::PerRoot => "per_root",
        }
    }
}

pub struct DerivedTx<'c> {
    conn: &'c mut PgConnection,
    seq: i64,
    root: i64,
    depth: i16,
    request_id: String,
    /// Derived events of the root before this step (read on first emit).
    existing: Option<i64>,
    emitted: i64,
    limit: Option<Limit>,
}

impl<'c> DerivedTx<'c> {
    pub(crate) fn new(conn: &'c mut PgConnection, source: &SourceEvent) -> Self {
        Self {
            conn,
            seq: source.seq,
            root: source.root_seq.unwrap_or(source.seq),
            depth: source.causation_depth,
            request_id: source.request_id.clone(), // clone: stamped on every derived event
            existing: None,
            emitted: 0,
            limit: None,
        }
    }

    /// The connection for writes to derived tables (tables.toml
    /// `kind = "derived"`): never audited state, never forge.events.
    pub fn conn(&mut self) -> &mut PgConnection {
        self.conn
    }

    /// Publish a derived event of the source event. `Ok(None)`: its
    /// `dedup_key` is already recorded (nothing written). `FG-DB-0008
    /// reason=fanout_limit` (with `limit`) at a derivation limit;
    /// `FG-DB-0010` for an event forge refuses to store.
    pub async fn emit(&mut self, event: DomainEvent) -> Result<Option<uuid::Uuid>, ForgeError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.reached(Limit::Depth));
        }
        let existing = match self.existing {
            Some(n) => n,
            None => {
                let n: i64 = sqlx::query_scalar(
                    "SELECT pg_catalog.count(*) FROM (SELECT 1 FROM ONLY forge.events \
                     WHERE root_seq = $1 LIMIT $2) AS w",
                )
                .bind(self.root)
                .bind(MAX_DERIVED_PER_ROOT)
                .fetch_one(&mut *self.conn)
                .await
                .map_err(|e| query_error("subscribers.derived_count", e))?;
                self.existing = Some(n);
                n
            }
        };
        if existing.saturating_add(self.emitted) >= MAX_DERIVED_PER_ROOT {
            return Err(self.reached(Limit::PerRoot));
        }
        let payload = event.payload.to_string();
        check(&event, &payload, &mut *self.conn).await?;
        let id = uuid::Uuid::now_v7();
        let inserted: Option<String> = sqlx::query_scalar(
            "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
             aggregate_id, project_id, actor_kind, request_id, caused_by_seq, root_seq, \
             causation_depth, dedup_key, payload) VALUES ($1::uuid, $2, $3, $4, $5, $6, \
             'system', $7, $8, $9, $10, $11, $12::jsonb) \
             ON CONFLICT (dedup_key) DO NOTHING RETURNING event_id::text",
        )
        .bind(id.to_string())
        .bind(event.event_type)
        .bind(event.schema_version)
        .bind(event.aggregate_type)
        .bind(event.aggregate_id)
        .bind(event.project_id.map(|p| p.get()))
        .bind(&self.request_id)
        .bind(self.seq)
        .bind(self.root)
        .bind(self.depth + 1)
        .bind(event.dedup_key.as_deref())
        .bind(&payload)
        .fetch_optional(&mut *self.conn)
        .await
        .map_err(|e| query_error("subscribers.derived_event", e))?;
        if inserted.is_some() {
            self.emitted += 1;
        }
        Ok(inserted.map(|_| id))
    }

    /// The limit this step reached, if any.
    pub fn limit(&self) -> Option<Limit> {
        self.limit
    }

    /// Derived events written in this step.
    pub fn emitted(&self) -> i64 {
        self.emitted
    }

    fn reached(&mut self, limit: Limit) -> ForgeError {
        self.limit = Some(limit);
        ForgeError::new(ErrorCode::SubscriberLag)
            .with_context("reason", "fanout_limit")
            .with_context("limit", limit.as_str())
            .with_context("root_seq", self.root)
    }
}

fn rejected(reason: &'static str, event: &DomainEvent) -> ForgeError {
    ForgeError::new(ErrorCode::ChangeRejected)
        .with_context("reason", reason)
        .with_context("action", "subscribers.derived_event")
        .with_context("event_type", event.event_type)
}

/// The checks ChangeTx makes on an event (FG-DB-0010), payload size as the
/// table CHECK measures it.
async fn check(
    event: &DomainEvent,
    payload: &str,
    conn: &mut PgConnection,
) -> Result<(), ForgeError> {
    if !valid_event_type(event.event_type) {
        return Err(rejected("invalid_event_type", event));
    }
    if !numbers_are_safe(&event.payload) {
        return Err(rejected("unsafe_number", event).with_context("field", "payload"));
    }
    if event
        .dedup_key
        .as_deref()
        .is_some_and(|k| k.chars().count() > MAX_DEDUP_KEY_CHARS)
    {
        return Err(rejected("field_too_long", event).with_context("field", "dedup_key"));
    }
    let bytes: i32 = sqlx::query_scalar("SELECT pg_catalog.pg_column_size($1::jsonb)")
        .bind(payload)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| query_error("subscribers.derived_size", e))?;
    if bytes > MAX_PAYLOAD_BYTES {
        return Err(rejected("field_too_long", event).with_context("field", "payload"));
    }
    Ok(())
}
