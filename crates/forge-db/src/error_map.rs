//! sqlx errors → registered codes.
//!
//! - **FG-DB-0001 DbUnavailable** (alert, 503): no usable connection — the
//!   server refused or did not answer, authentication or TLS failed, the
//!   pool was exhausted or closed, the connection was lost, or the server is
//!   shutting down / out of connection slots. This holds at startup *and*
//!   for every later statement, so an outage always alerts.
//! - **FG-DB-0002 DbQueryFailed** (warn, 500): the statement itself failed on
//!   a working connection (timeout, lock, conflict, constraint, …).
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

/// The reason for a server error, by SQLSTATE (PostgreSQL appendix A).
fn by_sqlstate(state: &str) -> Class {
    match state {
        "57014" => Class::Query("statement_timeout"),
        "55P03" => Class::Query("lock_timeout"),
        "40001" => Class::Query("serialization"),
        "40P01" => Class::Query("deadlock"),
        "57P01" | "57P02" | "57P03" => Class::Unavailable("server_shutdown"),
        "53300" => Class::Unavailable("too_many_connections"),
        s if s.starts_with("08") => Class::Unavailable("connection_lost"),
        s if s.starts_with("28") => Class::Unavailable("auth_failed"),
        s if s.starts_with("23") => Class::Query("constraint"),
        _ => Class::Query("server_error"),
    }
}

/// Server messages of these classes name objects, not data.
fn message_is_safe(state: &str) -> bool {
    ["08", "28", "3D", "42", "53", "55", "57"]
        .iter()
        .any(|class| state.starts_with(class))
}

fn classify(e: &sqlx::Error) -> (Class, Option<String>) {
    match e.as_database_error() {
        Some(db) => match db.code() {
            Some(state) => (by_sqlstate(&state), Some(state.into_owned())),
            None => (Class::Query("server_error"), None),
        },
        None => (transport(e), None),
    }
}

/// Build the error: code from the class, safe context, and the source only
/// when it cannot contain row values.
fn build(field: &'static str, name: &str, e: sqlx::Error, at_connect: bool) -> ForgeError {
    let (class, state) = classify(&e);
    let (code, reason) = match class {
        Class::Unavailable(r) => (ErrorCode::DbUnavailable, r),
        // At connect time a server-side rejection is not a statement error.
        Class::Query(r) if at_connect => (
            ErrorCode::DbUnavailable,
            if r == "client_error" {
                "driver"
            } else {
                "server_rejected"
            },
        ),
        Class::Query(r) => (ErrorCode::DbQueryFailed, r),
    };
    let mut err = ForgeError::new(code)
        .with_context(field, name)
        .with_context("reason", reason);
    let Some(db) = e.as_database_error() else {
        return err.with_source(e);
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

pub(crate) fn unavailable(target: &str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::DbUnavailable)
        .with_context("target", target)
        .with_context("reason", reason)
}

/// A failure to open or obtain a connection to `target` — always
/// `FG-DB-0001`.
pub(crate) fn connect_error(target: &str, e: sqlx::Error) -> ForgeError {
    build("target", target, e, true)
}

/// Map a failed statement. `query` is a static name such as
/// `"repo.insert"`, never the SQL text.
pub fn query_error(query: &'static str, e: sqlx::Error) -> ForgeError {
    build("query", query, e, false)
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
            ("28P01", true, "auth_failed"),
        ] {
            let class = by_sqlstate(state);
            let (is_unavailable, r) = match class {
                Class::Unavailable(r) => (true, r),
                Class::Query(r) => (false, r),
            };
            assert_eq!((is_unavailable, r), (unavailable, reason), "{state}");
        }
    }

    #[test]
    fn only_data_free_classes_keep_the_server_message() {
        assert!(message_is_safe("42P01"));
        assert!(message_is_safe("57P01"));
        assert!(
            !message_is_safe("23505"),
            "unique violation detail quotes the key"
        );
        assert!(!message_is_safe("22P02"), "invalid input quotes the value");
    }
}
