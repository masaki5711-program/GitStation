//! The connection pool. Every way of waiting on the database is bounded:
//! connecting (`connect_timeout`), getting a pooled connection
//! (`acquire_timeout`), and — set on every session — statements
//! (`statement_timeout`), lock waits (`lock_timeout`) and idle transactions
//! (`idle_in_transaction_session_timeout`).

use std::str::FromStr as _;
use std::time::Duration;

use forge_core::config::{DatabaseConfig, TlsMode};
use forge_errors::ForgeError;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use sqlx::Postgres;

use crate::error_map::{connect_error, query_error, unavailable};

/// PostgreSQL 16 or newer (ADR-0002, ADR-0011).
const MIN_SERVER_VERSION_NUM: i32 = 160_000;

/// A connected pool and the target it was opened against (for logs).
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
    target: String,
}

impl Db {
    /// Connect, verify the server version, and return the pool.
    /// Every failure is `FG-DB-0001` with `reason` and `target`.
    pub async fn connect(cfg: &DatabaseConfig) -> Result<Self, ForgeError> {
        let url = cfg.url()?;
        let target = url.to_string();
        let options = connect_options(cfg, url.as_str(), &target)?;
        if cfg.tls_mode == TlsMode::Disable && !is_loopback(url.host()) {
            tracing::warn!(
                target: "forge_db",
                db_target = %target,
                "database TLS is disabled for a non-loopback host"
            );
        }
        let pool_options = PgPoolOptions::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            .acquire_timeout(cfg.acquire_timeout);
        let pool =
            match tokio::time::timeout(cfg.connect_timeout, pool_options.connect_with(options))
                .await
            {
                Ok(Ok(pool)) => pool,
                Ok(Err(e)) => return Err(connect_error(&target, e)),
                Err(_) => {
                    return Err(unavailable(&target, "connect_timeout")
                        .with_context("timeout_ms", cfg.connect_timeout.as_millis()))
                }
            };
        let version: String = sqlx::query_scalar("SHOW server_version_num")
            .fetch_one(&pool)
            .await
            .map_err(|e| connect_error(&target, e))?;
        let version = check_version(&version).map_err(|e| e.with_context("target", &target))?;
        tracing::info!(
            target: "forge_db",
            db_target = %target,
            tls_mode = cfg.tls_mode.as_str(),
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

    /// A pooled connection; waiting longer than `acquire_timeout` is
    /// `FG-DB-0001 reason=acquire_timeout`.
    pub async fn acquire(&self) -> Result<PoolConnection<Postgres>, ForgeError> {
        self.pool
            .acquire()
            .await
            .map_err(|e| query_error("pool.acquire", e).with_context("target", &self.target))
    }

    /// One round trip (readiness).
    pub async fn ping(&self) -> Result<(), ForgeError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| query_error("db.ping", e).with_context("target", &self.target))
    }
}

fn connect_options(
    cfg: &DatabaseConfig,
    url: &str,
    target: &str,
) -> Result<PgConnectOptions, ForgeError> {
    // The URL was validated by the configuration; a rejection here is a
    // driver/config mismatch, reported with the target only.
    let mut options = PgConnectOptions::from_str(url)
        .map_err(|e| unavailable(target, "driver").with_source(e))?
        .application_name("forge")
        .ssl_mode(match cfg.tls_mode {
            TlsMode::VerifyFull => PgSslMode::VerifyFull,
            TlsMode::VerifyCa => PgSslMode::VerifyCa,
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
    Ok(options)
}

fn millis(d: Duration) -> String {
    d.as_millis().to_string()
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Parse `server_version_num` and require PostgreSQL 16+.
pub(crate) fn check_version(text: &str) -> Result<i32, ForgeError> {
    let reject = || {
        forge_errors::ForgeError::new(forge_errors::ErrorCode::DbUnavailable)
            .with_context("reason", "version")
            .with_context("server_version_num", text)
            .with_context("required", MIN_SERVER_VERSION_NUM)
    };
    match text.trim().parse::<i32>() {
        Ok(n) if n >= MIN_SERVER_VERSION_NUM => Ok(n),
        Ok(_) | Err(_) => Err(reject()),
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
