//! The connection pools (ADR-0018 §2). Every way of waiting on the database
//! is bounded: the first connect (`connect_timeout` around the whole
//! attempt), getting a pooled connection — including reconnecting after an
//! outage — (`acquire_timeout`), and, set on every session, statements
//! (`statement_timeout`), lock waits (`lock_timeout`) and idle transactions
//! (`idle_in_transaction_session_timeout`).
//!
//! Two pools, both logged in as `forge_app` (`database.url`):
//! - the **write pool** never leaves this crate (typed commands, ADR-0016);
//! - the **read pool** ([`ReadPool`], returned by [`Db::pool`]) runs every
//!   session as `forge_ro` with `default_transaction_read_only = on`. That
//!   prevents accidents, not bypasses (the process holds the write
//!   credentials; `RESET ROLE` undoes it) — the gates and ADR-0016's
//!   triggers handle bypasses.

use std::time::Duration;

use forge_core::config::{DatabaseConfig, TlsMode};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Postgres;
use sqlx::{ConnectOptions as _, Connection as _};

use crate::error_map::{connect_error, read_query_error, unavailable};
use crate::options::{connect_options, preflight, server_version, Login, MIN_SERVER_VERSION_NUM};

/// Bound for closing the pools.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The role and settings of every read-pool session.
const READ_SETTINGS: &[(&str, &str)] = &[
    ("role", "forge_ro"),
    ("default_transaction_read_only", "on"),
];

/// The read pool: sessions run as `forge_ro`, read-only. Run statements on
/// [`ReadPool::executor`] and map their errors with
/// [`crate::read_query_error`] (a write attempt is `FG-DB-0002`
/// `write_on_read_pool`, a bug — never an outage).
#[derive(Debug, Clone)]
pub struct ReadPool(PgPool);

impl ReadPool {
    pub fn executor(&self) -> &PgPool {
        &self.0
    }
}

/// The pools and the target they were opened against (for logs).
#[derive(Debug, Clone)]
pub struct Db {
    write: PgPool,
    read: ReadPool,
    target: String,
    /// Client-side bound for the readiness round trip.
    ping_timeout: Duration,
    /// Client-side bound for a short multi-statement operation (doctor
    /// checks, schema status): acquire + one statement timeout.
    op_timeout: Duration,
}

impl Db {
    /// Connect, verify the server version and the read role, and return
    /// the pools. A missing URL or a `PG*` variable is `FG-SYS-0002`; so is
    /// a login that is a superuser, bypasses row security or can reach
    /// forge_owner / forge_seq / forge_purge (`reason=wrong_role`: it would
    /// bypass every guard of ADR-0018). Every connection failure is
    /// `FG-DB-0001` with `reason` and `target`.
    pub async fn connect(cfg: &DatabaseConfig) -> Result<Self, ForgeError> {
        Self::connect_requiring(cfg, MIN_SERVER_VERSION_NUM).await
    }

    pub(crate) async fn connect_requiring(
        cfg: &DatabaseConfig,
        min_version: i32,
    ) -> Result<Self, ForgeError> {
        let url = cfg.url()?;
        let target = preflight(cfg, url)?;
        let login = |settings| Login {
            url,
            password: cfg.password.as_ref(),
            application_name: "forge",
            settings,
        };
        let write_options = connect_options(cfg, &login(&[]));
        let read_options = connect_options(cfg, &login(READ_SETTINGS));
        // One direct connection per pool first: the pool retries refused /
        // starting servers until acquire_timeout and would report every
        // cause as a timeout. The probes report the real one, check the
        // version and the read role. connect_timeout bounds the whole probe
        // — connect, queries and close — so a server that accepts and then
        // goes silent cannot hang startup.
        let version = bounded(
            cfg,
            &target,
            probe_write(&write_options, &target, min_version),
        )
        .await?;
        bounded(cfg, &target, probe_read(&read_options, &target)).await?;
        // The pools open their connections on demand (min_connections in
        // the background); a later failure surfaces on acquire as FG-DB-0001.
        let pool = |max, options| {
            PgPoolOptions::new()
                .max_connections(max)
                .min_connections(cfg.min_connections)
                .acquire_timeout(cfg.acquire_timeout)
                .connect_lazy_with(options)
        };
        let write = pool(cfg.write_max_connections, write_options);
        let read = ReadPool(pool(cfg.read_max_connections, read_options));
        tracing::info!(
            target: "forge_db",
            db_target = %target,
            tls_mode = cfg.tls_mode.as_str(),
            tls_verified = cfg.tls_mode == TlsMode::VerifyFull,
            server_version_num = version,
            write_max_connections = cfg.write_max_connections,
            read_max_connections = cfg.read_max_connections,
            "forge.db.connected"
        );
        Ok(Self {
            write,
            read,
            target,
            ping_timeout: cfg.acquire_timeout,
            op_timeout: cfg.acquire_timeout.saturating_add(cfg.statement_timeout),
        })
    }

    /// The read pool (pages, API reads, schema status).
    pub fn pool(&self) -> &ReadPool {
        &self.read
    }

    /// The write pool: typed commands inside this crate only.
    #[cfg_attr(not(test), allow(dead_code))] // first production caller: ChangeTx (step C3)
    pub(crate) fn write_pool(&self) -> &PgPool {
        &self.write
    }

    /// `host:port/dbname` — safe to log.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Close both pools: waits for checked-out connections to return, at
    /// most `CLOSE_TIMEOUT` (then the process exits with them open; the
    /// server ends them).
    pub async fn close(&self) {
        let both = async { tokio::join!(self.write.close(), self.read.0.close()) };
        if tokio::time::timeout(CLOSE_TIMEOUT, both).await.is_err() {
            tracing::warn!(
                target: "forge_db",
                db_target = %self.target,
                timeout_ms = CLOSE_TIMEOUT.as_millis(),
                "database pools did not close in time; connections are left to the server"
            );
        }
    }

    /// Run a short multi-statement operation with a client-side bound
    /// (acquire + statement timeout): a half-dead link fires no server-side
    /// timeout. Exceeding it is `FG-DB-0001 reason=operation_timeout`.
    pub async fn within<T>(
        &self,
        operation: &'static str,
        op: impl std::future::Future<Output = Result<T, ForgeError>>,
    ) -> Result<T, ForgeError> {
        match tokio::time::timeout(self.op_timeout, op).await {
            Ok(result) => result,
            Err(_) => Err(unavailable(&self.target, "operation_timeout")
                .with_context("operation", operation)
                .with_context("timeout_ms", self.op_timeout.as_millis())
                .with_context("timeout_key", "database.statement_timeout_ms")),
        }
    }

    /// A read-pool connection. Every failure — pool exhausted
    /// (`acquire_timeout`), server down, authentication — is `FG-DB-0001`.
    pub async fn acquire(&self) -> Result<PoolConnection<Postgres>, ForgeError> {
        acquire_from(&self.read.0, &self.target).await
    }

    /// One round trip on each pool (readiness), concurrently, each bounded
    /// on the client by `acquire_timeout` (a half-dead link would never
    /// answer). Every failure means the database cannot be used:
    /// `FG-DB-0001` (with `pool`).
    pub async fn ping(&self) -> Result<(), ForgeError> {
        let (write, read) = tokio::join!(
            self.ping_pool(&self.write, "write"),
            self.ping_pool(&self.read.0, "read")
        );
        write.and(read)
    }

    async fn ping_pool(&self, pool: &PgPool, name: &'static str) -> Result<(), ForgeError> {
        let mut conn = acquire_from(pool, &self.target)
            .await
            .map_err(|e| e.with_context("pool", name))?;
        let round_trip = sqlx::query("SELECT 1").execute(&mut *conn);
        match tokio::time::timeout(self.ping_timeout, round_trip).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(connect_error(&self.target, e).with_context("pool", name)),
            Err(_) => Err(unavailable(&self.target, "ping_timeout")
                .with_context("pool", name)
                .with_context("timeout_ms", self.ping_timeout.as_millis())
                .with_context("timeout_key", "database.acquire_timeout_ms")),
        }
    }
}

async fn acquire_from(pool: &PgPool, target: &str) -> Result<PoolConnection<Postgres>, ForgeError> {
    pool.acquire().await.map_err(|e| {
        let timed_out = matches!(e, sqlx::Error::PoolTimedOut);
        let err = connect_error(target, e);
        if timed_out {
            return err.with_context("timeout_key", "database.acquire_timeout_ms");
        }
        err
    })
}

/// `connect_timeout` around a whole probe.
pub(crate) async fn bounded<T>(
    cfg: &DatabaseConfig,
    target: &str,
    probe: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    match tokio::time::timeout(cfg.connect_timeout, probe).await {
        Ok(result) => result,
        Err(_) => Err(unavailable(target, "connect_timeout")
            .with_context("timeout_ms", cfg.connect_timeout.as_millis())
            .with_context("timeout_key", "database.connect_timeout_ms")),
    }
}

async fn probe_write(
    options: &PgConnectOptions,
    target: &str,
    min_version: i32,
) -> Result<i32, ForgeError> {
    let mut conn = options
        .connect()
        .await
        .map_err(|e| connect_error(target, e))?;
    let version = server_version(&mut conn, target, min_version).await;
    close_quietly(conn, target).await;
    version
}

/// The read role is set in the startup packet: a login that is not a
/// member of `forge_ro` fails to connect (42501). Check that the setting
/// took effect anyway — a silently ignored one would make the read pool a
/// write pool.
async fn probe_read(options: &PgConnectOptions, target: &str) -> Result<(), ForgeError> {
    let mut conn = options.connect().await.map_err(|e| {
        let denied = e
            .as_database_error()
            .and_then(|d| d.code())
            .is_some_and(|c| c == "42501");
        if denied {
            return wrong_read_role(target, "not_member");
        }
        connect_error(target, e)
    })?;
    let row: Result<(String, String, bool, bool), _> = sqlx::query_as(
        "SELECT current_user::text, current_setting('transaction_read_only'), \
         r.rolsuper OR r.rolbypassrls, \
         coalesce((SELECT bool_or(pg_catalog.pg_has_role(session_user, p.oid, 'MEMBER')) \
                   FROM pg_catalog.pg_roles p \
                   WHERE p.rolname IN ('forge_owner', 'forge_seq', 'forge_purge')), false) \
         FROM pg_catalog.pg_roles r WHERE r.rolname = session_user",
    )
    .fetch_one(&mut conn)
    .await;
    close_quietly(conn, target).await;
    let (user, read_only, elevated, privileged) =
        row.map_err(|e| read_query_error("db.read_role", e))?;
    if elevated {
        return Err(wrong_read_role(target, "superuser"));
    }
    if privileged {
        return Err(wrong_read_role(target, "privileged"));
    }
    if user != "forge_ro" {
        return Err(wrong_read_role(target, "role_not_applied"));
    }
    if read_only != "on" {
        return Err(wrong_read_role(target, "not_read_only"));
    }
    Ok(())
}

fn wrong_read_role(target: &str, check: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", "database.url")
        .with_context("reason", "wrong_role")
        .with_context("role_check", check)
        .with_context("target", target)
}

pub(crate) async fn close_quietly(conn: sqlx::postgres::PgConnection, target: &str) {
    if let Err(e) = conn.close().await {
        // The probe is dropped either way; a failed goodbye is not a fault.
        tracing::debug!(target: "forge_db", db_target = %target, error = %e, "probe close failed");
    }
}

/// `GET /readyz` and `forge doctor`: one bounded round trip per pool
/// (`Db::ping`).
impl forge_core::readiness::ReadinessProbe for Db {
    fn name(&self) -> &'static str {
        "database"
    }

    fn check(&self) -> forge_core::readiness::ProbeFuture<'_> {
        Box::pin(self.ping())
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
