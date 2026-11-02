//! sqlx errors → registered codes.
//!
//! - **FG-DB-0001 DbUnavailable** (alert, 503): no usable connection — the
//!   server refused or did not answer, authentication or TLS failed, the
//!   pool was exhausted or closed, the connection was lost, or the server is
//!   shutting down / out of connection slots. This holds at startup *and*
//!   for every later statement, so an outage always alerts.
//! - **FG-DB-0002 DbQueryFailed** (warn, 500): the statement itself failed on
//!   a working connection (timeout, lock, conflict, constraint, …). A write
//!   on the read pool (`write_on_read_pool`, 25006 there) and a missing
//!   privilege (`permission_denied`, 42501) are bugs: never retried.
//! - **FG-DB-0009 AppendOnlyViolation** (alert): a guard trigger rejected a
//!   change to an append-only table (XF001).
//! - **FG-DB-0013 AuditMissing** (alert): an event committed without an audit
//!   row in its transaction (XF002, deferred trigger).
//!
//! The context carries `target` / `query` (static names), `reason`,
//! `sqlstate` and — for constraint errors — `constraint` / `table`. Server
//! messages are kept only for SQLSTATE classes whose text names schema
//! objects, never row values (classes 22 / 23 quote the offending data).
//! SQL text and bound parameters never enter the context.

use std::io::ErrorKind;

use forge_errors::{ErrorCode, ForgeError};

/// Where a statement or connection attempt failed.
enum Class {
    Unavailable(&'static str),
    Query(&'static str),
    AppendOnly,
    AuditMissing,
}

/// Which pool a statement ran on: 25006 means "standby" on the write pool
/// but "a write was sent to the read pool" on the read pool.
#[derive(Clone, Copy, PartialEq, Eq)]
enum On {
    Write,
    Read,
}

/// The reason for an error without a SQLSTATE (transport and driver).
fn transport(e: &sqlx::Error) -> Class {
    if let sqlx::Error::Io(io) = e {
        // io::ErrorKind is non-exhaustive (no exhaustive match possible):
        // two kinds have their own reason; every other one is a lost link.
        let kind = io.kind();
        return Class::Unavailable(if kind == ErrorKind::ConnectionRefused {
            "connect_refused"
        } else if kind == ErrorKind::TimedOut {
            "connect_timeout"
        } else {
            "connection_lost"
        });
    }
    if let sqlx::Error::Tls(_) = e {
        return Class::Unavailable("tls");
    }
    if let sqlx::Error::PoolTimedOut = e {
        return Class::Unavailable("acquire_timeout");
    }
    if let sqlx::Error::PoolClosed = e {
        return Class::Unavailable("pool_closed");
    }
    if let sqlx::Error::Protocol(_) | sqlx::Error::WorkerCrashed = e {
        return Class::Unavailable("connection_lost");
    }
    if let sqlx::Error::RowNotFound = e {
        // Callers expecting "maybe none" use fetch_optional; reaching here
        // means a row that must exist does not.
        return Class::Query("row_not_found");
    }
    Class::Query("client_error")
}

/// Transport and pool failures carry no row data; their text helps the
/// diagnosis and is kept as `source`. Decode / encode errors quote the value
/// (`invalid value "…" for enum …`) and are reduced to their kind.
fn source_is_safe(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Protocol(_)
            | sqlx::Error::WorkerCrashed
    )
}

/// The kind of a driver-side error (sqlx::Error is non-exhaustive).
fn driver_kind(e: &sqlx::Error) -> &'static str {
    if let sqlx::Error::ColumnDecode { .. } = e {
        return "column_decode";
    }
    if let sqlx::Error::Decode(_) = e {
        return "decode";
    }
    if let sqlx::Error::Encode(_) = e {
        return "encode";
    }
    if let sqlx::Error::ColumnNotFound(_) | sqlx::Error::ColumnIndexOutOfBounds { .. } = e {
        return "column_not_found";
    }
    if let sqlx::Error::TypeNotFound { .. } = e {
        return "type_not_found";
    }
    if let sqlx::Error::RowNotFound = e {
        return "row_not_found";
    }
    if let sqlx::Error::Configuration(_) = e {
        return "configuration";
    }
    if let sqlx::Error::Migrate(_) = e {
        return "migrate";
    }
    "other"
}

/// The reason for a server error, by SQLSTATE (PostgreSQL appendix A).
fn by_sqlstate(state: &str, on: On) -> Class {
    match state {
        "25006" if on == On::Read => Class::Query("write_on_read_pool"),
        "42501" => Class::Query("permission_denied"),
        "XF001" => Class::AppendOnly,
        "XF002" => Class::AuditMissing,
        "57014" => Class::Query("statement_timeout"),
        "55P03" => Class::Query("lock_timeout"),
        "40001" => Class::Query("serialization"),
        "40P01" => Class::Query("deadlock"),
        "57P01" | "57P02" | "57P03" => Class::Unavailable("server_shutdown"),
        "53300" => Class::Unavailable("too_many_connections"),
        // The server cannot accept writes (standby, recovery).
        "25006" => Class::Unavailable("read_only"),
        // The caller left a transaction idle past the limit; the server
        // ended the session. A caller bug, not an outage: no alert.
        "25P03" => Class::Query("idle_in_transaction_timeout"),
        s if s.starts_with("08") => Class::Unavailable("connection_lost"),
        s if s.starts_with("28") => Class::Unavailable("auth_failed"),
        // Disk full, out of memory, configuration limits.
        s if s.starts_with("53") => Class::Unavailable("resource_exhausted"),
        // System errors outside PostgreSQL (I/O), internal errors (XX000
        // "could not read block", XX001 data corrupted).
        s if s.starts_with("58") || s.starts_with("XX") => Class::Unavailable("server_fault"),
        s if s.starts_with("23") => Class::Query("constraint"),
        _ => Class::Query("server_error"),
    }
}

/// Exact SQLSTATEs whose server message names objects or limits only —
/// never a quoted token or row value (syntax errors and class 22 / 23 quote
/// the offending input).
const MESSAGE_SAFE: &[&str] = &[
    "08001", "08006", "28000", "28P01", "3D000", "42501", "42P01", "42703", "42883", "53300",
    "55P03", "57014", "57P01", "57P02", "57P03", "XF001", "XF002",
];

fn message_is_safe(state: &str) -> bool {
    MESSAGE_SAFE.contains(&state)
}

fn classify(e: &sqlx::Error, on: On) -> (Class, Option<String>) {
    match e.as_database_error() {
        Some(db) => match db.code() {
            Some(state) => (by_sqlstate(&state, on), Some(state.into_owned())),
            None => (Class::Query("server_error"), None),
        },
        None => (transport(e), None),
    }
}

/// Build the error: code from the class, safe context, and the source only
/// when it cannot contain row values.
fn build(field: &'static str, name: &str, e: sqlx::Error, at_connect: bool, on: On) -> ForgeError {
    let (class, state) = classify(&e, on);
    let (code, reason) = match class {
        Class::Unavailable(r) => (ErrorCode::DbUnavailable, r),
        Class::AppendOnly => (ErrorCode::AppendOnlyViolation, "rejected"),
        Class::AuditMissing => (
            ErrorCode::AuditMissing,
            audit_missing_reason(e.as_database_error().and_then(|d| d.constraint())),
        ),
        // Opening or checking a connection: never a statement error, but a
        // known reason (statement_timeout, serialization, …) is kept; only
        // the generic ones are renamed.
        Class::Query(r) if at_connect => (
            ErrorCode::DbUnavailable,
            match r {
                "client_error" => "driver",
                "server_error" => "server_rejected",
                known => known,
            },
        ),
        Class::Query(r) => (ErrorCode::DbQueryFailed, r),
    };
    let mut err = ForgeError::new(code)
        .with_context(field, name)
        .with_context("reason", reason);
    // A deadlock or serialization failure (40P01 / 40001) rolled the whole
    // transaction back: the request is safe to retry as a whole (D5 MR2 —
    // two crossing moves may deadlock). Callers and clients key on this.
    if code == ErrorCode::DbQueryFailed && matches!(reason, "deadlock" | "serialization") {
        err = err.with_context("retryable", true);
    }
    let Some(db) = e.as_database_error() else {
        if source_is_safe(&e) {
            return err.with_source(e);
        }
        let err = err.with_context("driver_error", driver_kind(&e));
        // The column name is schema, not data.
        if let sqlx::Error::ColumnDecode { index, .. } = &e {
            return err.with_context("column", index);
        }
        return err;
    };
    if let Some(constraint) = db.constraint() {
        err = err.with_context("constraint", constraint);
    }
    if let Some(table) = db.table() {
        err = err.with_context("table", table);
    }
    match state {
        Some(s) if message_is_safe(&s) => err
            .with_context("sqlstate", &s)
            .with_context("db_message", db.message()),
        Some(s) => err.with_context("sqlstate", s),
        None => err,
    }
}

/// XF002 comes from the audit-presence trigger of events (no constraint
/// name), from the audit trigger of a state table (`audited_change`, or
/// `audited_change_config` for a trigger the ledger does not describe;
/// migration 20261028100000) or from a derived-event check, which names
/// itself in CONSTRAINT (migration 20261027140000). Any other name is a
/// check this build does not know (`xf002_unknown`; `constraint` and
/// `db_message` stay in the context).
fn audit_missing_reason(constraint: Option<&str>) -> &'static str {
    match constraint {
        None => "no_audit_row",
        Some("audited_change") => "audited_change",
        Some("audited_change_config") => "audited_change_config",
        Some("derived_lineage") => "derived_lineage",
        Some("derived_depth") => "derived_depth",
        Some("derived_fanout") => "derived_fanout",
        Some("derived_progress") => "derived_progress",
        Some(_) => "xf002_unknown",
    }
}

pub(crate) fn unavailable(target: &str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::DbUnavailable)
        .with_context("target", target)
        .with_context("reason", reason)
}

/// A failure to open or obtain a connection to `target` — always
/// `FG-DB-0001`.
pub(crate) fn connect_error(target: &str, e: sqlx::Error) -> ForgeError {
    build("target", target, e, true, On::Write)
}

/// Map a failed statement on the write pool or a dedicated connection.
/// `query` is a static name such as `"repo.insert"`, never the SQL text.
pub fn query_error(query: &'static str, e: sqlx::Error) -> ForgeError {
    build("query", query, e, false, On::Write)
}

/// Map a failed statement on the [`crate::ReadPool`].
pub fn read_query_error(query: &'static str, e: sqlx::Error) -> ForgeError {
    build("query", query, e, false, On::Read)
}

#[cfg(test)]
#[path = "error_map_tests.rs"]
mod tests;
