//! Object guards: the bootstrap version, owners, the exact guard triggers
//! and event triggers, the guard functions' owners, bodies and privileges.

use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::{guard_missing, BOOTSTRAP_VERSION, FILES};
use crate::read_query_error;

pub(super) const LOG_TABLES: &[&str] = &[
    "bootstrap_log",
    "ddl_audit",
    "ddl_audit_ack",
    "guarded_objects",
];

/// `pg_trigger.tgtype` bits: ROW 1, BEFORE 2, DELETE 8, UPDATE 16,
/// TRUNCATE 32.
const APPEND_ONLY_TYPE: i16 = 1 | 2 | 8 | 16;
const NO_TRUNCATE_TYPE: i16 = 2 | 32;

/// Event triggers and the event each fires on.
const EVENT_TRIGGERS: &[(&str, &str)] = &[
    ("forge_ddl_audit_end", "ddl_command_end"),
    ("forge_ddl_audit_drop", "sql_drop"),
];

/// Functions of the bootstrap, compared with the shipped body, and whether
/// a superuser owns them (otherwise forge_audit does).
pub(super) const FUNCTIONS: &[(&str, bool)] = &[
    ("reject_log_mutation", false),
    ("ddl_audit_record", false),
    ("cluster_identity", true),
    ("ddl_audit_status", false),
];

pub(super) async fn check_version(conn: &mut PgConnection) -> Result<i32, ForgeError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('forge_audit.bootstrap_log') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error("bootstrap.log_exists", e))?;
    if !exists {
        return Err(guard_missing("bootstrap_version").with_context("found", "none"));
    }
    let found: Option<i32> =
        sqlx::query_scalar("SELECT max(bootstrap_version) FROM forge_audit.bootstrap_log")
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

/// Schema forge_audit and the log tables belong to forge_audit (nothing a
/// lower role can become may attach code to what the superuser-owned event
/// triggers write to); no rules or row policies on them.
pub(super) async fn check_owners(conn: &mut PgConnection) -> Result<(), ForgeError> {
    // The database and public belong to a superuser, not forge_owner (the
    // database owner owns public, creates schemas and sets session
    // defaults for every login).
    let (db_super, public_super): (bool, bool) = sqlx::query_as(
        "SELECT (SELECT r.rolsuper FROM pg_catalog.pg_database d \
                 JOIN pg_catalog.pg_roles r ON r.oid = d.datdba \
                 WHERE d.datname = pg_catalog.current_database()), \
                coalesce((SELECT r.rolsuper OR r.rolname = 'pg_database_owner' \
                          FROM pg_catalog.pg_namespace n \
                          JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner \
                          WHERE n.nspname = 'public'), true)",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.database_owner", e))?;
    if !db_super {
        return Err(guard_missing("object_owner").with_context("object", "database"));
    }
    if !public_super {
        return Err(guard_missing("object_owner").with_context("object", "schema public"));
    }
    let schema_owner: Option<String> = sqlx::query_scalar(
        "SELECT pg_catalog.pg_get_userbyid(nspowner)::text FROM pg_catalog.pg_namespace \
         WHERE nspname = 'forge_audit'",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.schema_owner", e))?;
    if schema_owner.as_deref() != Some("forge_audit") {
        return Err(guard_missing("object_owner").with_context("object", "schema forge_audit"));
    }
    let rows: Vec<(String, String, bool, bool)> = sqlx::query_as(
        "SELECT c.relname::text, pg_catalog.pg_get_userbyid(c.relowner)::text, \
         EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite rw WHERE rw.ev_class = c.oid), \
         c.relrowsecurity OR c.relforcerowsecurity \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'forge_audit' AND c.relname = ANY($1)",
    )
    .bind(LOG_TABLES)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.table_owners", e))?;
    for table in LOG_TABLES {
        let ok = rows.iter().any(|(name, owner, rules, rls)| {
            name == table && owner == "forge_audit" && !rules && !rls
        });
        if !ok {
            return Err(guard_missing("object_owner").with_context("object", table));
        }
    }
    Ok(())
}

/// Exactly the two guard triggers per log table, calling the guard
/// function, enabled ALWAYS; the event triggers on their event, calling the
/// DDL-log function, enabled ALWAYS, owned by a superuser, without tag
/// filters.
pub(super) async fn check_triggers(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let rows: Vec<(String, String, String, i16, bool)> = sqlx::query_as(
        "SELECT c.relname::text, t.tgname::text, t.tgenabled::text, t.tgtype, \
         (t.tgfoid = 'forge_audit.reject_log_mutation()'::regprocedure \
          AND t.tgnargs = 0 AND t.tgqual IS NULL \
          AND coalesce(pg_catalog.array_length(t.tgattr::int2[], 1), 0) = 0) \
         FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'forge_audit' AND c.relname = ANY($1) AND NOT t.tgisinternal",
    )
    .bind(LOG_TABLES)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.log_triggers", e))?;
    for table in LOG_TABLES {
        let expected = [
            (format!("{table}_append_only"), APPEND_ONLY_TYPE),
            (format!("{table}_no_truncate"), NO_TRUNCATE_TYPE),
        ];
        for (name, kind) in &expected {
            let ok = rows.iter().any(|(t, n, enabled, tgtype, guard_fn)| {
                t == table && n == name && enabled == "A" && tgtype == kind && *guard_fn
            });
            if !ok {
                return Err(guard_missing("log_triggers").with_context("trigger", name));
            }
        }
    }
    if let Some((_, extra, _, _, _)) = rows.iter().find(|(t, n, _, _, _)| {
        *n != format!("{t}_append_only") && *n != format!("{t}_no_truncate")
    }) {
        return Err(guard_missing("log_triggers").with_context("trigger", extra));
    }
    let events: Vec<(String, String, String, bool, bool, bool)> = sqlx::query_as(
        "SELECT e.evtname::text, e.evtevent::text, e.evtenabled::text, r.rolsuper, \
         e.evtfoid = 'forge_audit.ddl_audit_record()'::regprocedure, e.evttags IS NULL \
         FROM pg_catalog.pg_event_trigger e JOIN pg_catalog.pg_roles r ON r.oid = e.evtowner",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.event_triggers", e))?;
    for (name, event) in EVENT_TRIGGERS {
        let ok = events
            .iter()
            .any(|(n, ev, enabled, superuser, func, all_tags)| {
                n == name && ev == event && enabled == "A" && *superuser && *func && *all_tags
            });
        if !ok {
            return Err(guard_missing("event_triggers").with_context("trigger", name));
        }
    }
    // No other event trigger in this database: it would run on every DDL.
    if let Some((extra, ..)) = events
        .iter()
        .find(|(n, ..)| !EVENT_TRIGGERS.iter().any(|(name, _)| name == n))
    {
        return Err(guard_missing("event_triggers").with_context("trigger", extra));
    }
    Ok(())
}

/// Schema forge_audit holds exactly the shipped functions and no
/// extension (a trusted extension can be installed into a schema without
/// CREATE on it — by forge_owner, the database owner).
pub(super) async fn check_functions(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let names: Vec<&str> = FUNCTIONS.iter().map(|(n, _)| *n).collect();
    // Besides the body: SECURITY DEFINER (not for the trigger function),
    // the pinned search_path and the language — any of them changes what
    // runs with the owner's rights.
    let rows: Vec<(String, String, bool, String, bool)> = sqlx::query_as(
        "SELECT p.proname::text, r.rolname::text, r.rolsuper, p.prosrc, \
         coalesce(p.prosecdef = (p.proname NOT IN ('reject_log_mutation', 'ddl_audit_status')) \
         AND p.proconfig = ARRAY['search_path=pg_catalog, pg_temp'] \
         AND p.prolang = (SELECT l.oid FROM pg_catalog.pg_language l WHERE l.lanname = \
             CASE p.proname WHEN 'cluster_identity' THEN 'sql' ELSE 'plpgsql' END), false) \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_catalog.pg_roles r ON r.oid = p.proowner \
         WHERE n.nspname = 'forge_audit'",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.functions", e))?;
    if let Some((extra, ..)) = rows.iter().find(|(n, ..)| !names.contains(&n.as_str())) {
        return Err(guard_missing("function_owner").with_context("function", extra));
    }
    let extension: Option<String> = sqlx::query_scalar(
        "SELECT e.extname::text FROM pg_catalog.pg_extension e \
         JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
         WHERE n.nspname = 'forge_audit' LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.extensions", e))?;
    if let Some(extension) = extension {
        return Err(guard_missing("object_owner").with_context("object", extension));
    }
    for (name, superuser_owned) in FUNCTIONS {
        let found: Vec<_> = rows.iter().filter(|(n, ..)| n == name).collect();
        // Missing or overloaded.
        let [(_, owner, superuser, body, shape)] = found.as_slice() else {
            return Err(guard_missing("function_owner").with_context("function", name));
        };
        let owner_ok = if *superuser_owned {
            *superuser
        } else {
            owner == "forge_audit"
        };
        if !owner_ok {
            return Err(guard_missing("function_owner").with_context("function", name));
        }
        if !shape || shipped_body(name).as_deref() != Some(body.as_str()) {
            return Err(guard_missing("function_body").with_context("function", name));
        }
    }
    // EXECUTE: cluster_identity() for forge_app and forge_seq,
    // ddl_audit_status() for forge_app, forge_ro and forge_maint; the
    // guard and DDL-log functions for no forge role.
    let grants: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT f::text, r::text, pg_catalog.has_function_privilege(r, \
         'forge_audit.' || f || '()', 'EXECUTE') \
         FROM unnest($1::text[]) AS f, unnest($2::text[]) AS r",
    )
    .bind(&names)
    .bind([
        "forge_app",
        "forge_seq",
        "forge_purge",
        "forge_maint",
        "forge_ro",
        "forge_owner",
        "forge_migrator",
    ])
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.function_privileges", e))?;
    for (function, role, granted) in &grants {
        let expected = match function.as_str() {
            "cluster_identity" => role == "forge_app" || role == "forge_seq",
            "ddl_audit_status" => {
                role == "forge_app" || role == "forge_ro" || role == "forge_maint"
            }
            _ => false,
        };
        if *granted != expected {
            return Err(guard_missing("function_privilege")
                .with_context("function", function)
                .with_context("role", role));
        }
    }
    Ok(())
}

/// The search_path settings of step 3 per login, and nothing else: session
/// defaults (`role`, `default_transaction_read_only`, `statement_timeout`,
/// …) for this database, or role-wide ones on a bootstrap role (a role may
/// change its own), are reported. Step 3 removes them.
pub(super) async fn check_settings(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let roles: Vec<&str> = super::roles::ROLES.iter().map(|(n, _)| *n).collect();
    let rows: Vec<(String, bool, Vec<String>)> = sqlx::query_as(
        "SELECT coalesce(r.rolname::text, '*'), s.setdatabase = 0, s.setconfig::text[] \
         FROM pg_catalog.pg_db_role_setting s \
         LEFT JOIN pg_catalog.pg_roles r ON r.oid = s.setrole \
         WHERE s.setdatabase = (SELECT oid FROM pg_catalog.pg_database \
                                WHERE datname = pg_catalog.current_database()) \
            OR (s.setdatabase = 0 AND r.rolname = ANY($1))",
    )
    .bind(&roles)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.db_settings", e))?;
    for (role, role_wide, settings) in &rows {
        if *role_wide {
            return Err(guard_missing("db_settings")
                .with_context("role", role)
                .with_context("scope", "every_database"));
        }
        let expected = match role.as_str() {
            "forge_app" => Some("search_path=forge, public"),
            "forge_maint" => Some("search_path=pg_catalog, pg_temp"),
            "forge_migrator" => Some("search_path=forge"),
            _ => None,
        };
        let ok = matches!((expected, settings.as_slice()), (Some(e), [only]) if only == e);
        if !ok {
            return Err(guard_missing("db_settings").with_context("role", role));
        }
    }
    Ok(())
}

/// The body (`prosrc`) of `forge_audit.<name>` in the shipped step 3 file.
pub(crate) fn shipped_body(name: &str) -> Option<String> {
    let text = FILES[2].1;
    let start = text.find(&format!("FUNCTION forge_audit.{name}("))?;
    let rest = text.get(start..)?;
    let open = rest.find("AS $$")? + "AS $$".len();
    let body = rest.get(open..)?;
    let close = body.find("$$")?;
    body.get(..close).map(str::to_owned)
}
