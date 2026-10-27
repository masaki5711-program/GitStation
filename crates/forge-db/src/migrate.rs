//! Schema migrations (ADR-0014): forward-only, applied explicitly by
//! `forge db migrate`, checked by `forge serve` before it starts.
//!
//! - [`status`] is read-only (read pool): it never creates `_sqlx_migrations`.
//! - [`check_schema`] fails with `FG-DB-0004` when the database does not
//!   match the binary (pending, db_ahead, checksum_mismatch, dirty).
//! - [`migrate`] applies pending migrations on the migration connection
//!   ([`crate::MigratorConn`], running as `forge_owner`, ADR-0018) with
//!   `database.migrate_statement_timeout_ms`; sqlx serializes concurrent
//!   runs with an advisory lock and runs each migration in one transaction.
//!   A failure is `FG-DB-0003` with the version; nothing is half-applied
//!   (except `-- no-transaction` files: one idempotent statement each).
//!   The whole run has a client-side deadline; status is re-read only to
//!   report, never to decide success.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use forge_errors::{ErrorCode, ForgeError};
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::PgConnection;

use crate::{query_error, read_query_error, Db, MigratorConn};

/// Client-side bound for the status re-read after a successful run.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// The migrations built into this binary (`crates/forge-db/migrations`).
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// How the database relates to the binary's migrations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaStatus {
    /// Applied successfully and known to this binary.
    pub applied: Vec<i64>,
    /// Known to this binary, not applied yet.
    pub pending: Vec<i64>,
    /// Applied, but not known to this binary (the database is newer).
    pub unknown: Vec<i64>,
    /// Applied, but the file in this binary differs (an edited migration).
    pub modified: Vec<i64>,
    /// Recorded as unsuccessful by an earlier run.
    pub dirty: Vec<i64>,
}

impl SchemaStatus {
    /// The first reason the database cannot be used as is, most serious
    /// first, as `FG-DB-0004`.
    pub fn incompatibility(&self) -> Option<ForgeError> {
        [
            ("dirty", &self.dirty),
            ("checksum_mismatch", &self.modified),
            ("db_ahead", &self.unknown),
            ("pending", &self.pending),
        ]
        .into_iter()
        .find(|(_, versions)| !versions.is_empty())
        .map(|(reason, versions)| incompatible(reason, versions))
    }
}

fn incompatible(reason: &'static str, versions: &[i64]) -> ForgeError {
    ForgeError::new(ErrorCode::SchemaIncompatible)
        .with_context("reason", reason)
        .with_context("versions", join(versions))
}

fn join(versions: &[i64]) -> String {
    versions
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Compare the database with `migrator` without changing anything (on the
/// read pool).
pub async fn status(db: &Db, migrator: &Migrator) -> Result<SchemaStatus, ForgeError> {
    let mut conn = db.acquire().await?;
    status_on(&mut conn, migrator, read_query_error).await
}

/// The bookkeeping lives in `public` (search_path of forge_migrator, set
/// by the bootstrap); the reads name it explicitly.
async fn status_on(
    conn: &mut PgConnection,
    migrator: &Migrator,
    map: fn(&'static str, sqlx::Error) -> ForgeError,
) -> Result<SchemaStatus, ForgeError> {
    let table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations')::text")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| map("migrate.table_exists", e))?;
    let recorded: Vec<(i64, Vec<u8>, bool)> = match table {
        Some(_) => sqlx::query_as(
            "SELECT version, checksum, success FROM public._sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| map("migrate.list_applied", e))?,
        None => Vec::new(),
    };
    let known: BTreeMap<i64, &[u8]> = migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| (m.version, m.checksum.as_ref()))
        .collect();
    let mut st = SchemaStatus::default();
    for (version, checksum, success) in &recorded {
        if !success {
            st.dirty.push(*version);
            continue;
        }
        match known.get(version) {
            None => st.unknown.push(*version),
            Some(file) if *file != checksum.as_slice() => st.modified.push(*version),
            Some(_) => st.applied.push(*version),
        }
    }
    st.pending = known
        .keys()
        .filter(|v| !recorded.iter().any(|(r, _, _)| r == *v))
        .copied()
        .collect();
    Ok(st)
}

/// `forge serve`: refuse to run against a schema that does not match.
/// Runs on the read pool.
pub async fn check_schema(db: &Db, migrator: &Migrator) -> Result<SchemaStatus, ForgeError> {
    let st = status(db, migrator).await?;
    match st.incompatibility() {
        Some(err) => Err(err.with_context("target", db.target())),
        None => Ok(st),
    }
}

/// Apply the pending migrations; returns the versions applied by this call.
/// Refuses (FG-DB-0004) when the database is dirty, edited or ahead.
pub async fn migrate(
    mc: &mut MigratorConn,
    migrator: &Migrator,
    statement_timeout: Duration,
) -> Result<Vec<i64>, ForgeError> {
    let target = mc.target().to_owned(); // owned: mc.conn is borrowed mutably below
    let before = status_on(&mut mc.conn, migrator, query_error).await?;
    for (reason, versions) in [
        ("dirty", &before.dirty),
        ("checksum_mismatch", &before.modified),
        ("db_ahead", &before.unknown),
    ] {
        if !versions.is_empty() {
            return Err(incompatible(reason, versions).with_context("target", &target));
        }
    }
    if before.pending.is_empty() {
        return Ok(Vec::new());
    }
    // The migration connection is not pooled: the longer statement timeout
    // ends with it.
    sqlx::query(&format!(
        "SET statement_timeout = {}",
        statement_timeout.as_millis()
    ))
    .execute(&mut mc.conn)
    .await
    .map_err(|e| query_error("migrate.set_statement_timeout", e))?;
    // Client-side deadline for the whole run: the server's statement and
    // lock timeouts would not fire on a half-dead link. One statement
    // timeout per pending migration, plus one for the bookkeeping.
    let factor = u32::try_from(before.pending.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let deadline = statement_timeout.saturating_mul(factor);
    let started = Instant::now();
    let result = tokio::time::timeout(deadline, migrator.run_direct(&mut mc.conn)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(migrate_error(e).with_context("target", &target)),
        // The caller closes the connection (its state is unknown now).
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::MigrationFailed)
                .with_context("reason", "timeout")
                .with_context("timeout_ms", deadline.as_millis())
                .with_context("target", &target))
        }
    }
    // Committed. Record that first: a failing re-read below must not turn a
    // successful migration into a reported failure.
    tracing::info!(
        target: "forge_db",
        db_target = %target,
        pending_before = %join(&before.pending),
        duration_ms = started.elapsed().as_millis(),
        "forge.db.migrate.applied"
    );
    // A concurrent run may have applied some of them first; the versions
    // reported are those pending before and applied now (by either run).
    let verify = match tokio::time::timeout(
        VERIFY_TIMEOUT,
        status_on(&mut mc.conn, migrator, query_error),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::DbUnavailable)
            .with_context("reason", "verify_timeout")
            .with_context("timeout_ms", VERIFY_TIMEOUT.as_millis())),
    };
    match verify {
        Ok(after) => Ok(before
            .pending
            .iter()
            .filter(|v| after.applied.contains(v))
            .copied()
            .collect()),
        Err(e) => {
            tracing::warn!(
                target: "forge_db",
                db_target = %target,
                error = %e,
                "migrations committed; the verification read failed — check with forge db status"
            );
            Ok(before.pending)
        }
    }
}

/// MigrateError is non-exhaustive: matched with `if let`, the rest is
/// `driver`. Row data never reaches the context (`query_error` sanitizes).
fn migrate_error(e: MigrateError) -> ForgeError {
    let failed = |reason: &'static str| {
        ForgeError::new(ErrorCode::MigrationFailed).with_context("reason", reason)
    };
    if let MigrateError::ExecuteMigration(inner, version) = e {
        return failed("sql_failed")
            .with_context("version", version)
            .with_source(query_error("migrate.apply", inner));
    }
    if let MigrateError::Execute(inner) = e {
        // The advisory lock wait is bounded by lock_timeout (55P03): another
        // run holds it.
        let state = inner
            .as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned());
        let reason = if state.as_deref() == Some("55P03") {
            "locked"
        } else {
            "driver"
        };
        return failed(reason).with_source(query_error("migrate.run", inner));
    }
    if let MigrateError::Dirty(v) = e {
        return incompatible("dirty", &[v]);
    }
    if let MigrateError::VersionMismatch(v) = e {
        return incompatible("checksum_mismatch", &[v]);
    }
    if let MigrateError::VersionMissing(v) = e {
        return incompatible("db_ahead", &[v]);
    }
    if let MigrateError::Source(_) = e {
        return failed("source");
    }
    failed("driver")
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
