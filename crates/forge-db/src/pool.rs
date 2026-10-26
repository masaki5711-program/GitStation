//! The connection pool. Every way of waiting on the database is bounded:
//! the first connect (`connect_timeout` around the whole attempt), getting a
//! pooled connection — including reconnecting after an outage —
//! (`acquire_timeout`), and, set on every session, statements
//! (`statement_timeout`), lock waits (`lock_timeout`) and idle transactions
//! (`idle_in_transaction_session_timeout`).
//!
//! The connection options are built only from the configuration: the
//! driver's libpq-compatible environment (`PGPASSWORD`, `PGSSLROOTCERT`,
//! `PGOPTIONS`, …) and `~/.pgpass` would bypass `database.password_file` and
//! the TLS settings, so a set `PG*` variable is a configuration error.

use std::time::Duration;

use forge_core::config::{DatabaseConfig, DatabaseUrl, TlsMode};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use sqlx::Postgres;

use crate::error_map::{connect_error, unavailable};

/// PostgreSQL 16 or newer (ADR-0002, ADR-0011).
const MIN_SERVER_VERSION_NUM: i32 = 160_000;

/// Environment variables the driver reads in place of explicit options.
const LIBPQ_ENV: &[&str] = &[
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGDATABASE",
    "PGUSER",
    "PGPASSWORD",
    "PGPASSFILE",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGOPTIONS",
    "PGAPPNAME",
    "PGSSLMODE",
    "PGSSLROOTCERT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGSSLCRL",
    "PGSSLNEGOTIATION",
    "PGTARGETSESSIONATTRS",
    "PGCONNECT_TIMEOUT",
    "PGCLIENTENCODING",
];

/// A connected pool and the target it was opened against (for logs).
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
    target: String,
}

impl Db {
    /// Connect, verify the server version, and return the pool.
    /// A missing URL or a `PG*` variable is `FG-SYS-0002`; every connection
    /// failure is `FG-DB-0001` with `reason` and `target`.
    pub async fn connect(cfg: &DatabaseConfig) -> Result<Self, ForgeError> {
        Self::connect_requiring(cfg, MIN_SERVER_VERSION_NUM).await
    }

    pub(crate) async fn connect_requiring(
        cfg: &DatabaseConfig,
        min_version: i32,
    ) -> Result<Self, ForgeError> {
        let url = cfg.url()?;
        reject_libpq_env(std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()))?;
        let target = url.to_string();
        warn_if_unverified(cfg.tls_mode, url, &target);
        let pool_options = PgPoolOptions::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            .acquire_timeout(cfg.acquire_timeout);
        let connecting = pool_options.connect_with(connect_options(cfg, url));
        let pool = match tokio::time::timeout(cfg.connect_timeout, connecting).await {
            Ok(Ok(pool)) => pool,
            // The pool's own wait for the first connection ran out first.
            Ok(Err(sqlx::Error::PoolTimedOut)) => {
                return Err(unavailable(&target, "connect_timeout")
                    .with_context("timeout_ms", cfg.acquire_timeout.as_millis())
                    .with_context("timeout_key", "database.acquire_timeout_ms"))
            }
            Ok(Err(e)) => return Err(connect_error(&target, e)),
            Err(_) => {
                return Err(unavailable(&target, "connect_timeout")
                    .with_context("timeout_ms", cfg.connect_timeout.as_millis())
                    .with_context("timeout_key", "database.connect_timeout_ms"))
            }
        };
        let version = match server_version(&pool, &target, min_version).await {
            Ok(v) => v,
            Err(e) => {
                // Release the connections opened for the check before failing.
                pool.close().await;
                return Err(e);
            }
        };
        tracing::info!(
            target: "forge_db",
            db_target = %target,
            tls_mode = cfg.tls_mode.as_str(),
            tls_verified = cfg.tls_mode == TlsMode::VerifyFull,
            server_version_num = version,
            "forge.db.connected"
        );
        Ok(Self { pool, target })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// `host:port/dbname` — safe to log.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// A pooled connection. Every failure — pool exhausted
    /// (`acquire_timeout`), server down, authentication — is `FG-DB-0001`.
    pub async fn acquire(&self) -> Result<PoolConnection<Postgres>, ForgeError> {
        self.pool
            .acquire()
            .await
            .map_err(|e| connect_error(&self.target, e))
    }

    /// One round trip (readiness): `FG-DB-0001` when the database cannot be
    /// used, `FG-DB-0002` if the statement itself fails.
    pub async fn ping(&self) -> Result<(), ForgeError> {
        let mut conn = self.acquire().await?;
        sqlx::query("SELECT 1")
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(|e| crate::query_error("db.ping", e).with_context("target", &self.target))
    }
}

/// Explicit options only: `new_without_pgpass` skips `~/.pgpass`, and every
/// field it could have taken from the environment is set here (the
/// environment itself is rejected by `reject_libpq_env`).
fn connect_options(cfg: &DatabaseConfig, url: &DatabaseUrl) -> PgConnectOptions {
    let mut options = PgConnectOptions::new_without_pgpass()
        .host(url.host())
        .port(url.port())
        .username(url.user())
        .database(url.dbname())
        .application_name("forge")
        .ssl_mode(match cfg.tls_mode {
            TlsMode::VerifyFull => PgSslMode::VerifyFull,
            TlsMode::Require => PgSslMode::Require,
            TlsMode::Disable => PgSslMode::Disable,
        })
        .options([
            ("statement_timeout", millis(cfg.statement_timeout)),
            ("lock_timeout", millis(cfg.lock_timeout)),
            (
                "idle_in_transaction_session_timeout",
                millis(cfg.idle_in_transaction_timeout),
            ),
        ]);
    if let Some(password) = &cfg.password {
        options = options.password(password.expose());
    }
    if let Some(ca) = &cfg.tls_root_cert {
        options = options.ssl_root_cert(ca);
    }
    options
}

/// A set libpq variable is `FG-SYS-0002` naming it (`reason=libpq_env`).
pub(crate) fn reject_libpq_env(names: impl IntoIterator<Item = String>) -> Result<(), ForgeError> {
    match names.into_iter().find(|n| LIBPQ_ENV.contains(&n.as_str())) {
        Some(name) => Err(ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", name)
            .with_context("reason", "libpq_env")),
        None => Ok(()),
    }
}

/// An unverified server on a non-loopback host is allowed (it was written
/// down) but always visible in the log.
fn warn_if_unverified(mode: TlsMode, url: &DatabaseUrl, target: &str) {
    let unverified = match mode {
        TlsMode::VerifyFull => false,
        TlsMode::Require | TlsMode::Disable => true,
    };
    if unverified && !is_loopback(url.host()) {
        tracing::warn!(
            target: "forge_db",
            db_target = %target,
            tls_mode = mode.as_str(),
            "database server identity is not verified for a non-loopback host"
        );
    }
}

async fn server_version(pool: &PgPool, target: &str, min: i32) -> Result<i32, ForgeError> {
    let text: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(pool)
        .await
        .map_err(|e| connect_error(target, e))?;
    check_version(&text, min).map_err(|e| e.with_context("target", target))
}

fn millis(d: Duration) -> String {
    d.as_millis().to_string()
}

pub(crate) fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Parse `server_version_num` and require at least `min`.
pub(crate) fn check_version(text: &str, min: i32) -> Result<i32, ForgeError> {
    match text.trim().parse::<i32>() {
        Ok(n) if n >= min => Ok(n),
        Ok(_) | Err(_) => Err(ForgeError::new(ErrorCode::DbUnavailable)
            .with_context("reason", "version")
            .with_context("server_version_num", text)
            .with_context("required", min)),
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
