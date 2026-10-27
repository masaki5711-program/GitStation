//! The migration connection (ADR-0018 §2): one connection — not a pool —
//! logged in as `forge_migrator` (`database.migrate_url`,
//! `application_name = forge-migrate`), switched to `forge_owner` with
//! `SET ROLE`. Every object a migration creates is then owned by
//! `forge_owner`, and the DDL log can tell migrations from other changes.

use forge_core::config::DatabaseConfig;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;
use sqlx::{ConnectOptions as _, Connection as _};

use crate::error_map::connect_error;
use crate::options::{connect_options, preflight, server_version, Login, MIN_SERVER_VERSION_NUM};
use crate::pool::bounded;
use crate::Db;

/// Bound for closing the connection.
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// An open migration connection running as `forge_owner`.
#[derive(Debug)]
pub struct MigratorConn {
    pub(crate) conn: PgConnection,
    target: String,
}

impl MigratorConn {
    /// `host:port/dbname` — safe to log.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Close the connection, bounded; a failed goodbye is not a fault.
    pub async fn close(self) {
        match tokio::time::timeout(CLOSE_TIMEOUT, self.conn.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(target: "forge_db", error = %e, "migration connection close failed")
            }
            Err(_) => tracing::debug!(target: "forge_db", "migration connection close timed out"),
        }
    }
}

impl Db {
    /// Open the migration connection. A missing `database.migrate_url` is
    /// `FG-SYS-0002`; a login that cannot become `forge_owner`, or is a
    /// superuser (outside the trust boundary of ADR-0018), is `FG-SYS-0002`
    /// `reason=wrong_role` with `role_check`; connection failures are
    /// `FG-DB-0001`.
    pub async fn connect_migrator(cfg: &DatabaseConfig) -> Result<MigratorConn, ForgeError> {
        let url = cfg.migrate_url()?;
        let target = preflight(cfg, url)?;
        let options = connect_options(
            cfg,
            &Login {
                url,
                password: cfg.migrate_password.as_ref(),
                application_name: "forge-migrate",
                settings: &[],
            },
        );
        let open = async {
            let mut conn = options
                .connect()
                .await
                .map_err(|e| connect_error(&target, e))?;
            server_version(&mut conn, &target, MIN_SERVER_VERSION_NUM).await?;
            become_owner(&mut conn, &target).await?;
            Ok(conn)
        };
        let conn = bounded(cfg, &target, open).await?;
        Ok(MigratorConn { conn, target })
    }
}

async fn become_owner(conn: &mut PgConnection, target: &str) -> Result<(), ForgeError> {
    if let Err(e) = sqlx::query("SET ROLE forge_owner")
        .execute(&mut *conn)
        .await
    {
        let state = e.as_database_error().and_then(|d| d.code());
        // 42501: not a member; 22023: the role does not exist (no bootstrap).
        return Err(match state.as_deref() {
            Some("42501") => wrong_role(target, "not_member"),
            Some("22023") => wrong_role(target, "owner_missing"),
            Some(_) | None => connect_error(target, e),
        });
    }
    let (current, superuser): (String, bool) = sqlx::query_as(
        "SELECT current_user::text, \
         (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = session_user)",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| connect_error(target, e))?;
    if superuser {
        return Err(wrong_role(target, "superuser"));
    }
    if current != "forge_owner" {
        return Err(wrong_role(target, "not_owner"));
    }
    Ok(())
}

fn wrong_role(target: &str, check: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", "database.migrate_url")
        .with_context("reason", "wrong_role")
        .with_context("role_check", check)
        .with_context("target", target)
}
