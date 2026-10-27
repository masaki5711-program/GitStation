//! Connection options, built only from the configuration: the driver's
//! libpq-compatible environment (`PGPASSWORD`, `PGSSLROOTCERT`,
//! `PGOPTIONS`, …) and `~/.pgpass` would bypass the password files and the
//! TLS settings, so a set `PG*` variable is a configuration error.

use std::time::Duration;

use forge_core::config::{DatabaseConfig, DatabaseUrl, Secret, TlsMode};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};

use crate::error_map::connect_error;

/// PostgreSQL 16 or newer (ADR-0002, ADR-0011).
pub(crate) const MIN_SERVER_VERSION_NUM: i32 = 160_000;

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

/// Which login and session settings a connection uses.
pub(crate) struct Login<'a> {
    pub(crate) url: &'a DatabaseUrl,
    pub(crate) password: Option<&'a Secret>,
    pub(crate) application_name: &'static str,
    /// Extra session settings sent in the startup packet.
    pub(crate) settings: &'a [(&'static str, &'static str)],
}

/// The checks every connection goes through before it is opened; returns
/// the loggable target (`host:port/dbname`).
pub(crate) fn preflight(cfg: &DatabaseConfig, url: &DatabaseUrl) -> Result<String, ForgeError> {
    reject_libpq_env(std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()))?;
    let target = url.to_string();
    check_unverified(cfg.tls_mode, cfg.tls_allow_unverified, url.host(), &target)?;
    Ok(target)
}

/// Explicit options only: `new_without_pgpass` skips `~/.pgpass`, and every
/// field it could have taken from the environment is set here (the
/// environment itself is rejected by `reject_libpq_env`).
pub(crate) fn connect_options(cfg: &DatabaseConfig, login: &Login<'_>) -> PgConnectOptions {
    let mut settings = vec![
        ("statement_timeout", millis(cfg.statement_timeout)),
        ("lock_timeout", millis(cfg.lock_timeout)),
        (
            "idle_in_transaction_session_timeout",
            millis(cfg.idle_in_transaction_timeout),
        ),
    ];
    settings.extend(login.settings.iter().map(|(k, v)| (*k, (*v).to_owned())));
    let mut options = PgConnectOptions::new_without_pgpass()
        .host(login.url.host())
        .port(login.url.port())
        .username(login.url.user())
        .database(login.url.dbname())
        .application_name(login.application_name)
        .ssl_mode(match cfg.tls_mode {
            TlsMode::VerifyFull => PgSslMode::VerifyFull,
            TlsMode::Require => PgSslMode::Require,
            TlsMode::Disable => PgSslMode::Disable,
        })
        .options(settings);
    if let Some(password) = login.password {
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

/// Check the server version on an open connection. The query is bounded by
/// the session's statement_timeout (and by the caller's connect timeout).
pub(crate) async fn server_version(
    conn: &mut PgConnection,
    target: &str,
    min: i32,
) -> Result<i32, ForgeError> {
    let text: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| connect_error(target, e))?;
    check_version(&text, min).map_err(|e| e.with_context("target", target))
}

pub(crate) fn millis(d: Duration) -> String {
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
