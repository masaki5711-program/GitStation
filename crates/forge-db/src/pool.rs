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
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions, PgSslMode};
use sqlx::Postgres;
use sqlx::{ConnectOptions as _, Connection as _};

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
    /// Client-side bound for the readiness round trip.
    ping_timeout: Duration,
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
        reject_libpq_env(std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()))?;
        let target = url.to_string();
        check_unverified(cfg.tls_mode, cfg.tls_allow_unverified, url.host(), &target)?;
        let options = connect_options(cfg, url);
        // One direct connection first: the pool retries refused / starting
        // servers until acquire_timeout and would report every cause as a
        // timeout. The probe reports the real one and checks the version.
        // connect_timeout bounds the whole probe — connect, version query and
        // close — so a server that accepts and then goes silent cannot hang
        // startup (the server-side statement_timeout would not fire).
        let probe = async {
            let conn = options
                .connect()
                .await
                .map_err(|e| connect_error(&target, e))?;
            server_version(conn, &target, min_version).await
        };
        let version = match tokio::time::timeout(cfg.connect_timeout, probe).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(unavailable(&target, "connect_timeout")
                    .with_context("timeout_ms", cfg.connect_timeout.as_millis())
                    .with_context("timeout_key", "database.connect_timeout_ms"))
            }
        };
        // The pool opens its connections on demand (min_connections in the
        // background); a later failure surfaces on acquire as FG-DB-0001.
        let pool = PgPoolOptions::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            .acquire_timeout(cfg.acquire_timeout)
            .connect_lazy_with(options);
        tracing::info!(
            target: "forge_db",
            db_target = %target,
            tls_mode = cfg.tls_mode.as_str(),
            tls_verified = cfg.tls_mode == TlsMode::VerifyFull,
            server_version_num = version,
            "forge.db.connected"
        );
        Ok(Self {
            pool,
            target,
            ping_timeout: cfg.acquire_timeout,
        })
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
        self.pool.acquire().await.map_err(|e| {
            let timed_out = matches!(e, sqlx::Error::PoolTimedOut);
            let err = connect_error(&self.target, e);
            if timed_out {
                return err.with_context("timeout_key", "database.acquire_timeout_ms");
            }
            err
        })
    }

    /// One round trip (readiness), bounded on the client by
    /// `acquire_timeout` (a half-dead link would never answer). Every failure
    /// means the database cannot be used: always `FG-DB-0001`.
    pub async fn ping(&self) -> Result<(), ForgeError> {
        let mut conn = self.acquire().await?;
        let round_trip = sqlx::query("SELECT 1").execute(&mut *conn);
        match tokio::time::timeout(self.ping_timeout, round_trip).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(connect_error(&self.target, e)),
            Err(_) => Err(unavailable(&self.target, "ping_timeout")
                .with_context("timeout_ms", self.ping_timeout.as_millis())
                .with_context("timeout_key", "database.acquire_timeout_ms")),
        }
    }
}

/// `GET /readyz` and `forge doctor`: one bounded round trip (`Db::ping`).
impl forge_core::readiness::ReadinessProbe for Db {
    fn name(&self) -> &'static str {
        "database"
    }

    fn check(&self) -> forge_core::readiness::ProbeFuture<'_> {
        Box::pin(self.ping())
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
    // Case-insensitive: Windows environment names are.
    match names
        .into_iter()
        .find(|n| LIBPQ_ENV.iter().any(|v| v.eq_ignore_ascii_case(n)))
    {
        Some(name) => Err(ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", name)
            .with_context("reason", "libpq_env")),
        None => Ok(()),
    }
}

/// `require` / `disable` to a non-loopback host needs
/// `database.tls_allow_unverified = true` (FG-SYS-0002 `unverified_remote`
/// otherwise); when allowed it is still logged on every start.
pub(crate) fn check_unverified(
    mode: TlsMode,
    allowed: bool,
    host: &str,
    target: &str,
) -> Result<(), ForgeError> {
    let unverified = match mode {
        TlsMode::VerifyFull => false,
        TlsMode::Require | TlsMode::Disable => true,
    };
    if !unverified || is_loopback(host) {
        return Ok(());
    }
    if !allowed {
        return Err(ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "database.tls_mode")
            .with_context("reason", "unverified_remote")
            .with_context("tls_mode", mode.as_str())
            .with_context("target", target));
    }
    tracing::warn!(
        target: "forge_db",
        db_target = %target,
        tls_mode = mode.as_str(),
        "database server identity is not verified for a non-loopback host (allowed by database.tls_allow_unverified)"
    );
    Ok(())
}
/// Check the version on the probe connection, then close it. The query is
/// bounded by the session's statement_timeout.
async fn server_version(
    mut probe: PgConnection,
    target: &str,
    min: i32,
) -> Result<i32, ForgeError> {
    let text: Result<String, _> = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(&mut probe)
        .await;
    if let Err(e) = probe.close().await {
        // The probe is dropped either way; a failed goodbye is not a fault.
        tracing::debug!(target: "forge_db", db_target = %target, error = %e, "probe close failed");
    }
    let text = text.map_err(|e| connect_error(target, e))?;
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
