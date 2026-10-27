//! Read-only checks of what `ops/db/bootstrap/` set up (ADR-0018 §1, §3),
//! for `forge doctor` (`db.bootstrap`, `db.connections`). Every query runs
//! on the read pool; every finding is `FG-DB-0009 AppendOnlyViolation`
//! with `reason` and `guard` (or `FG-SYS-0002` for the connection budget).
//!
//! The bootstrap files are embedded: the function bodies in the database
//! must equal the shipped ones, and [`BOOTSTRAP_VERSION`] is the version
//! the last file records.

use forge_core::config::DatabaseConfig;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::{read_query_error, Db};

/// The version `03-database-objects.sql` writes to `forge.bootstrap_log`.
pub const BOOTSTRAP_VERSION: i32 = 1;

/// The bootstrap files, in order (run by psql in production; by the test
/// support in tests).
pub const FILES: [(&str, &str); 3] = [
    (
        "01-cluster-roles.sql",
        include_str!("../../../ops/db/bootstrap/01-cluster-roles.sql"),
    ),
    (
        "02-create-database.sql",
        include_str!("../../../ops/db/bootstrap/02-create-database.sql"),
    ),
    (
        "03-database-objects.sql",
        include_str!("../../../ops/db/bootstrap/03-database-objects.sql"),
    ),
];

/// Roles created by step 1 and whether they log in.
const ROLES: &[(&str, bool)] = &[
    ("forge_owner", false),
    ("forge_migrator", true),
    ("forge_seq", false),
    ("forge_purge", false),
    ("forge_app", true),
    ("forge_ro", false),
    ("forge_maint", true),
];

/// The roles that may change guarded objects.
const PRIVILEGED: &[&str] = &["forge_owner", "forge_seq", "forge_purge"];

/// Memberships the bootstrap itself creates (member, role).
const FIXED_MEMBERSHIPS: &[(&str, &str)] = &[
    ("forge_migrator", "forge_owner"),
    ("forge_owner", "forge_seq"),
    ("forge_owner", "forge_purge"),
];

const LOG_TABLES: &[&str] = &[
    "bootstrap_log",
    "ddl_audit",
    "ddl_audit_ack",
    "guarded_objects",
];
const EVENT_TRIGGERS: &[&str] = &["forge_ddl_audit_end", "forge_ddl_audit_drop"];
/// Functions owned by the bootstrap superuser, compared with the shipped body.
const FUNCTIONS: &[&str] = &[
    "reject_log_mutation",
    "ddl_audit_record",
    "cluster_identity",
];

fn guard_missing(guard: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AppendOnlyViolation)
        .with_context("reason", "guard_missing")
        .with_context("guard", guard)
}

/// Every guard of `db.bootstrap`, first failure first. Returns the
/// bootstrap version found.
pub async fn check(db: &Db, privileged_members: &[String]) -> Result<i32, ForgeError> {
    let mut conn = db.acquire().await?;
    let version = check_version(&mut conn).await?;
    check_roles(&mut conn).await?;
    check_memberships(&mut conn, privileged_members).await?;
    check_triggers(&mut conn).await?;
    check_functions(&mut conn).await?;
    check_app_login(&mut conn).await?;
    Ok(version)
}

async fn check_version(conn: &mut PgConnection) -> Result<i32, ForgeError> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('forge.bootstrap_log') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.log_exists", e))?;
    if !exists {
        return Err(guard_missing("bootstrap_version").with_context("found", "none"));
    }
    let found: Option<i32> =
        sqlx::query_scalar("SELECT max(bootstrap_version) FROM forge.bootstrap_log")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error("bootstrap.version", e))?;
    match found {
        Some(v) if v == BOOTSTRAP_VERSION => Ok(v),
        other => Err(guard_missing("bootstrap_version")
            .with_context("found", other.map_or("none".to_owned(), |v| v.to_string()))
            .with_context("expected", BOOTSTRAP_VERSION)),
    }
}

async fn check_roles(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let names: Vec<&str> = ROLES.iter().map(|(n, _)| *n).collect();
    let rows: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT rolname::text, rolcanlogin, \
         rolsuper OR rolcreaterole OR rolcreatedb OR rolbypassrls OR rolreplication \
         FROM pg_catalog.pg_roles WHERE rolname = ANY($1)",
    )
    .bind(&names)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.roles", e))?;
    for (name, login) in ROLES {
        match rows.iter().find(|(n, _, _)| n == name) {
            None => return Err(guard_missing("role_attributes").with_context("role", name)),
            Some((_, can_login, elevated)) if can_login != login || *elevated => {
                return Err(guard_missing("role_attributes").with_context("role", name))
            }
            Some(_) => {}
        }
    }
    // Neither the application nor maintenance login may reach a privileged
    // role, directly or through other roles.
    let reachable: Vec<(String, String)> = sqlx::query_as(
        "SELECT a::text, p::text FROM unnest($1::text[]) AS a, unnest($2::text[]) AS p \
         WHERE pg_catalog.pg_has_role(a, p, 'MEMBER')",
    )
    .bind(["forge_app", "forge_maint"])
    .bind(PRIVILEGED)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.role_separation", e))?;
    if let Some((login, role)) = reachable.first() {
        return Err(guard_missing("role_separation")
            .with_context("role", login)
            .with_context("member_of", role));
    }
    Ok(())
}

async fn check_memberships(
    conn: &mut PgConnection,
    privileged_members: &[String],
) -> Result<(), ForgeError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.rolname::text, r.rolname::text FROM pg_catalog.pg_auth_members am \
         JOIN pg_catalog.pg_roles r ON r.oid = am.roleid \
         JOIN pg_catalog.pg_roles m ON m.oid = am.member \
         WHERE r.rolname = ANY($1)",
    )
    .bind(PRIVILEGED)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.memberships", e))?;
    let (unexpected, missing) = membership_drift(&rows, privileged_members);
    if unexpected.is_empty() && missing.is_empty() {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::AppendOnlyViolation)
        .with_context("reason", "role_membership_changed")
        .with_context("unexpected", unexpected.join(","))
        .with_context("missing", missing.join(",")))
}

/// Members beyond the fixed memberships that are not configured, and
/// configured members (or fixed memberships) that are absent.
pub(crate) fn membership_drift(
    rows: &[(String, String)],
    privileged_members: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut unexpected: Vec<String> = Vec::new();
    for (member, role) in rows {
        let fixed = FIXED_MEMBERSHIPS
            .iter()
            .any(|(m, r)| m == member && r == role);
        if !fixed && !privileged_members.contains(member) && !unexpected.contains(member) {
            unexpected.push(member.clone()); // clone: collected for the report
        }
    }
    let mut missing: Vec<String> = FIXED_MEMBERSHIPS
        .iter()
        .filter(|(m, r)| !rows.iter().any(|(rm, rr)| rm == m && rr == r))
        .map(|(m, r)| format!("{m}->{r}"))
        .collect();
    missing.extend(
        privileged_members
            .iter()
            .filter(|p| !rows.iter().any(|(m, _)| m == *p))
            .cloned(),
    );
    (unexpected, missing)
}

async fn check_triggers(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.tgname::text, t.tgenabled::text FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'forge' AND c.relname = ANY($1) AND NOT t.tgisinternal",
    )
    .bind(LOG_TABLES)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.log_triggers", e))?;
    for table in LOG_TABLES {
        for suffix in ["_append_only", "_no_truncate"] {
            let name = format!("{table}{suffix}");
            if !rows.iter().any(|(n, enabled)| *n == name && enabled == "A") {
                return Err(guard_missing("log_triggers").with_context("trigger", name));
            }
        }
    }
    let events: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT e.evtname::text, e.evtenabled::text, r.rolsuper \
         FROM pg_catalog.pg_event_trigger e JOIN pg_catalog.pg_roles r ON r.oid = e.evtowner",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.event_triggers", e))?;
    for name in EVENT_TRIGGERS {
        let ok = events
            .iter()
            .any(|(n, enabled, superuser)| n == name && enabled == "A" && *superuser);
        if !ok {
            return Err(guard_missing("event_triggers").with_context("trigger", name));
        }
    }
    Ok(())
}

async fn check_functions(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let rows: Vec<(String, bool, String)> = sqlx::query_as(
        "SELECT p.proname::text, r.rolsuper, p.prosrc FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_catalog.pg_roles r ON r.oid = p.proowner \
         WHERE n.nspname = 'forge' AND p.proname = ANY($1)",
    )
    .bind(FUNCTIONS)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.functions", e))?;
    for name in FUNCTIONS {
        let found: Vec<_> = rows.iter().filter(|(n, _, _)| n == name).collect();
        let [(_, superuser, body)] = found.as_slice() else {
            // Missing or overloaded.
            return Err(guard_missing("function_owner").with_context("function", name));
        };
        if !superuser {
            return Err(guard_missing("function_owner").with_context("function", name));
        }
        if shipped_body(name).as_deref() != Some(body.as_str()) {
            return Err(guard_missing("function_body").with_context("function", name));
        }
    }
    let grants: Vec<(String, bool)> = sqlx::query_as(
        "SELECT r::text, pg_catalog.has_function_privilege(r, 'forge.cluster_identity()', 'EXECUTE') \
         FROM unnest($1::text[]) AS r",
    )
    .bind(["forge_app", "forge_seq", "forge_maint", "forge_ro"])
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.function_privileges", e))?;
    for (role, granted) in &grants {
        let expected = role == "forge_app" || role == "forge_seq";
        if *granted != expected {
            return Err(guard_missing("function_privilege")
                .with_context("function", "cluster_identity")
                .with_context("role", role));
        }
    }
    Ok(())
}

/// The body (`prosrc`) of `forge.<name>` in the shipped step 3 file.
pub(crate) fn shipped_body(name: &str) -> Option<String> {
    let text = FILES[2].1;
    let start = text.find(&format!("FUNCTION forge.{name}("))?;
    let rest = text.get(start..)?;
    let open = rest.find("AS $$")? + "AS $$".len();
    let body = rest.get(open..)?;
    let close = body.find("$$")?;
    body.get(..close).map(str::to_owned)
}

/// The application login must not be a superuser (it would bypass every
/// guard).
async fn check_app_login(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let superuser: bool =
        sqlx::query_scalar("SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = session_user")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error("bootstrap.app_login", e))?;
    if superuser {
        return Err(guard_missing("app_login"));
    }
    Ok(())
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
    let mut conn = db.acquire().await?;
    let usable: i32 = sqlx::query_scalar(
        "SELECT current_setting('max_connections')::int \
         - current_setting('superuser_reserved_connections')::int \
         - coalesce(current_setting('reserved_connections', true)::int, 0)",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.max_connections", e))?;
    let budget = ConnectionBudget::new(cfg, u32::try_from(usable).unwrap_or(0));
    if budget.fits() {
        return Ok(budget);
    }
    Err(ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", "database.write_max_connections")
        .with_context("reason", "connection_budget")
        .with_context("needed", budget.needed)
        .with_context("usable", budget.usable))
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod tests;
