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
        Class::AuditMissing => (ErrorCode::AuditMissing, audit_missing_reason(&e)),
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

/// XF002 comes from the audit-presence trigger (no constraint name) or from
/// a derived-event check, which names itself in CONSTRAINT (migration
/// 20261027140000).
fn audit_missing_reason(e: &sqlx::Error) -> &'static str {
    match e.as_database_error().and_then(|d| d.constraint()) {
        Some("derived_lineage") => "derived_lineage",
        Some("derived_depth") => "derived_depth",
        Some("derived_fanout") => "derived_fanout",
        Some("derived_progress") => "derived_progress",
        Some(_) | None => "no_audit_row",
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
mod tests {
    use super::*;

    fn reason_of(err: &ForgeError) -> String {
        err.to_string()
            .split(' ')
            .find_map(|kv| kv.strip_prefix("reason="))
            .unwrap_or_default()
            .trim_end_matches(':')
            .to_owned()
    }

    #[test]
    fn transport_errors_are_unavailable_with_their_reason() {
        for (kind, reason) in [
            (ErrorKind::ConnectionRefused, "connect_refused"),
            (ErrorKind::TimedOut, "connect_timeout"),
            (ErrorKind::ConnectionReset, "connection_lost"),
            (ErrorKind::BrokenPipe, "connection_lost"),
        ] {
            let err = query_error("t.q", sqlx::Error::Io(std::io::Error::from(kind)));
            assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
            assert_eq!(reason_of(&err), reason, "{err}");
        }
        for (e, reason) in [
            (sqlx::Error::PoolTimedOut, "acquire_timeout"),
            (sqlx::Error::PoolClosed, "pool_closed"),
            (sqlx::Error::WorkerCrashed, "connection_lost"),
            (sqlx::Error::Protocol("x".into()), "connection_lost"),
        ] {
            let err = query_error("t.q", e);
            assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
            assert_eq!(reason_of(&err), reason, "{err}");
        }
    }

    #[test]
    fn a_decode_error_never_carries_the_value() {
        let e = sqlx::Error::ColumnDecode {
            index: "\"kind\"".to_owned(),
            source: "invalid value \"tok-5e1f-secret\" for enum Kind".into(),
        };
        let err = query_error("t.q", e);
        assert_eq!(err.code(), ErrorCode::DbQueryFailed);
        let text = err.to_string();
        assert!(!text.contains("tok-5e1f-secret"), "{text}");
        assert!(text.contains("driver_error=column_decode"), "{text}");
        assert!(text.contains("column=\"kind\""), "{text}");
        assert!(std::error::Error::source(&err).is_none());
        let err = query_error(
            "t.q",
            sqlx::Error::Decode("value \"x-9\" out of range".into()),
        );
        assert!(!err.to_string().contains("x-9"), "{err}");
    }

    #[test]
    fn row_not_found_is_a_query_failure_with_its_own_reason() {
        let err = query_error("t.q", sqlx::Error::RowNotFound);
        assert_eq!(err.code(), ErrorCode::DbQueryFailed);
        assert_eq!(reason_of(&err), "row_not_found");
    }

    #[test]
    fn sqlstates_map_to_the_documented_reasons() {
        for (state, unavailable, reason) in [
            ("57014", false, "statement_timeout"),
            ("55P03", false, "lock_timeout"),
            ("40001", false, "serialization"),
            ("40P01", false, "deadlock"),
            ("23505", false, "constraint"),
            ("42P01", false, "server_error"),
            ("57P01", true, "server_shutdown"),
            ("57P03", true, "server_shutdown"),
            ("53300", true, "too_many_connections"),
            ("08006", true, "connection_lost"),
            ("53100", true, "resource_exhausted"),
            ("53200", true, "resource_exhausted"),
            ("58030", true, "server_fault"),
            ("XX000", true, "server_fault"),
            ("25006", true, "read_only"),
            ("25P03", false, "idle_in_transaction_timeout"),
            ("28P01", true, "auth_failed"),
            ("42501", false, "permission_denied"),
            ("XF001", false, "append_only"),
            ("XF002", false, "audit_missing"),
        ] {
            let class = by_sqlstate(state, On::Write);
            let (is_unavailable, r) = match class {
                Class::Unavailable(r) => (true, r),
                Class::Query(r) => (false, r),
                Class::AppendOnly => (false, "append_only"),
                Class::AuditMissing => (false, "audit_missing"),
            };
            assert_eq!((is_unavailable, r), (unavailable, reason), "{state}");
        }
    }

    #[test]
    fn a_read_only_rejection_depends_on_the_pool() {
        assert!(matches!(
            by_sqlstate("25006", On::Read),
            Class::Query("write_on_read_pool")
        ));
        assert!(matches!(
            by_sqlstate("25006", On::Write),
            Class::Unavailable("read_only")
        ));
        assert!(matches!(
            by_sqlstate("42501", On::Read),
            Class::Query("permission_denied")
        ));
    }

    #[test]
    fn only_data_free_classes_keep_the_server_message() {
        assert!(message_is_safe("42P01"));
        assert!(message_is_safe("57P01"));
        assert!(!message_is_safe("42601"), "a syntax error quotes the token");
        assert!(!message_is_safe("55006"), "object in use may name data");
        assert!(
            !message_is_safe("23505"),
            "unique violation detail quotes the key"
        );
        assert!(!message_is_safe("22P02"), "invalid input quotes the value");
    }
}
