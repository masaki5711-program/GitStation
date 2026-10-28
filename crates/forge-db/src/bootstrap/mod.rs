//! Read-only checks of what `ops/db/bootstrap/` set up (ADR-0018 §1, §3 and
//! its 2026-10-27 amendment), for `forge doctor` (`db.bootstrap`,
//! `db.connections`). Every query runs on the read pool under one
//! client-side bound ([`Db::within`]); every finding is
//! `FG-DB-0009 AppendOnlyViolation` with `reason` and `guard` (or
//! `FG-SYS-0002` for the connection budget).
//!
//! The bootstrap files are embedded: the guard function bodies in the
//! database must equal the shipped ones, and [`BOOTSTRAP_VERSION`] is the
//! version the last file records (tested).

use forge_core::config::{DatabaseConfig, PrivilegedMember};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::Connection as _;

use crate::{read_query_error, Db};

mod acl;
pub(crate) mod audited;
mod definer;
mod objects;
mod records;
mod roles;
pub(crate) mod schema;

#[cfg(test)]
use roles::membership_drift;

/// The version `03-database-objects.sql` writes to
/// `forge_audit.bootstrap_log`.
pub const BOOTSTRAP_VERSION: i32 = 8;

/// The bootstrap files, in order (run by psql in production; by the test
/// support in tests).
pub const FILES: [(&str, &str); 3] = [
    (
        "01-cluster-roles.sql",
        include_str!("../../../../ops/db/bootstrap/01-cluster-roles.sql"),
    ),
    (
        "02-create-database.sql",
        include_str!("../../../../ops/db/bootstrap/02-create-database.sql"),
    ),
    (
        "03-database-objects.sql",
        include_str!("../../../../ops/db/bootstrap/03-database-objects.sql"),
    ),
];

pub(crate) fn guard_missing(guard: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AppendOnlyViolation)
        .with_context("reason", "guard_missing")
        .with_context("guard", guard)
}

/// Every guard of `db.bootstrap`, first failure first. Returns the
/// bootstrap version found.
pub async fn check(db: &Db, privileged_members: &[PrivilegedMember]) -> Result<i32, ForgeError> {
    db.within("bootstrap.check", async {
        let mut conn = db.acquire().await?;
        // Names resolve in pg_catalog only: forge (search_path of the read
        // pool) belongs to forge_owner, which could plant a better-matching
        // function or operator that makes a check pass.
        let mut tx = conn
            .begin()
            .await
            .map_err(|e| read_query_error("bootstrap.begin", e))?;
        sqlx::query("SET LOCAL search_path = pg_catalog, pg_temp")
            .execute(&mut *tx)
            .await
            .map_err(|e| read_query_error("bootstrap.search_path", e))?;
        let version = objects::check_version(&mut tx).await?;
        roles::check_roles(&mut tx).await?;
        roles::check_memberships(&mut tx, privileged_members).await?;
        objects::check_owners(&mut tx).await?;
        objects::check_triggers(&mut tx).await?;
        objects::check_functions(&mut tx).await?;
        objects::check_settings(&mut tx).await?;
        acl::check_privileges(&mut tx).await?;
        schema::check_schema_objects(&mut tx).await?;
        definer::check_assign_seq(&mut tx).await?;
        records::check_migration_records(&mut tx).await?;
        records::check_unexpected_ddl(&mut tx).await?;
        // Read-only: nothing to keep; ending it returns the connection clean.
        tx.rollback()
            .await
            .map_err(|e| read_query_error("bootstrap.end", e))?;
        Ok(version)
    })
    .await
}

/// The connection budget of ADR-0018 §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionBudget {
    /// `(serve + worker) × (write + read) + LISTEN + migration + maintenance`.
    pub needed: u32,
    /// `max_connections` minus the reserved slots.
    pub usable: u32,
}

impl ConnectionBudget {
    pub fn new(cfg: &DatabaseConfig, usable: u32) -> Self {
        let per_process = cfg
            .write_max_connections
            .saturating_add(cfg.read_max_connections);
        Self {
            needed: per_process.saturating_mul(2).saturating_add(3),
            usable,
        }
    }

    /// At most 80 % of the usable slots.
    pub fn fits(&self) -> bool {
        u64::from(self.needed) * 5 <= u64::from(self.usable) * 4
    }
}

/// `db.connections`: the configured pools must fit the server.
pub async fn check_connections(
    db: &Db,
    cfg: &DatabaseConfig,
) -> Result<ConnectionBudget, ForgeError> {
    let usable: i32 = db
        .within("bootstrap.max_connections", async {
            let mut conn = db.acquire().await?;
            sqlx::query_scalar(
                "SELECT current_setting('max_connections')::int \
                 - current_setting('superuser_reserved_connections')::int \
                 - coalesce(current_setting('reserved_connections', true)::int, 0)",
            )
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error("bootstrap.max_connections", e))
        })
        .await?;
    // Reserved slots at or above max_connections: nothing is left.
    let Ok(usable) = u32::try_from(usable) else {
        return Err(over_budget(ConnectionBudget::new(cfg, 0)).with_context("server_slots", usable));
    };
    let budget = ConnectionBudget::new(cfg, usable);
    if budget.fits() {
        return Ok(budget);
    }
    Err(over_budget(budget))
}

fn over_budget(budget: ConnectionBudget) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", "database.write_max_connections")
        .with_context("reason", "connection_budget")
        .with_context("needed", budget.needed)
        .with_context("usable", budget.usable)
}

#[cfg(test)]
mod tests;
