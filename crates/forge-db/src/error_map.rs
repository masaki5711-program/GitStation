//! sqlx errors → registered codes. Connection failures are `FG-DB-0001`
//! (with `reason` and `target`); failures of a statement are `FG-DB-0002`
//! (with the static `query` name and the SQLSTATE). SQL text and bound
//! parameters never enter the context.

use std::io::ErrorKind;

use forge_errors::{ErrorCode, ForgeError};

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
}

pub(crate) fn unavailable(target: &str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::DbUnavailable)
        .with_context("target", target)
        .with_context("reason", reason)
}

/// A failure to open a connection to `target` (host:port/dbname).
pub(crate) fn connect_error(target: &str, e: sqlx::Error) -> ForgeError {
    let state = sqlstate(&e);
    let reason = match state.as_deref() {
        // invalid_password / invalid_authorization_specification
        Some("28P01" | "28000") => "auth_failed",
        Some(_) => "server_rejected",
        None => transport_reason(&e),
    };
    let err = unavailable(target, reason);
    let err = match state {
        Some(s) => err.with_context("sqlstate", s),
        None => err,
    };
    err.with_source(e)
}

fn transport_reason(e: &sqlx::Error) -> &'static str {
    if let sqlx::Error::Io(io) = e {
        // io::ErrorKind is non-exhaustive (no exhaustive match possible):
        // two kinds have their own reason, every other one is plain I/O.
        let kind = io.kind();
        return if kind == ErrorKind::ConnectionRefused {
            "connect_refused"
        } else if kind == ErrorKind::TimedOut {
            "connect_timeout"
        } else {
            "io"
        };
    }
    if let sqlx::Error::Tls(_) = e {
        return "tls";
    }
    if let sqlx::Error::PoolTimedOut = e {
        return "connect_timeout";
    }
    "driver"
}

/// Map a failed statement. `query` is a static name such as
/// `"repo.insert"`, never the SQL text.
pub fn query_error(query: &'static str, e: sqlx::Error) -> ForgeError {
    if let sqlx::Error::PoolTimedOut = e {
        return ForgeError::new(ErrorCode::DbUnavailable)
            .with_context("query", query)
            .with_context("reason", "acquire_timeout")
            .with_source(e);
    }
    if let sqlx::Error::PoolClosed = e {
        return ForgeError::new(ErrorCode::DbUnavailable)
            .with_context("query", query)
            .with_context("reason", "pool_closed")
            .with_source(e);
    }
    let state = sqlstate(&e);
    let reason = match state.as_deref() {
        Some("57014") => "statement_timeout",
        Some("55P03") => "lock_timeout",
        Some(_) => "server_error",
        None if matches!(e, sqlx::Error::Io(_) | sqlx::Error::Protocol(_)) => "connection_lost",
        None => "client_error",
    };
    let err = ForgeError::new(ErrorCode::DbQueryFailed)
        .with_context("query", query)
        .with_context("reason", reason);
    let err = match state {
        Some(s) => err.with_context("sqlstate", s),
        None => err,
    };
    err.with_source(e)
}
