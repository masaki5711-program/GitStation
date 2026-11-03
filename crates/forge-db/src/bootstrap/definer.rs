//! The SECURITY DEFINER functions of the migrated schema (ADR-0018 §1, §3)
//! exactly as the migrations ship them — one function of each name, owned
//! by its role, SECURITY DEFINER, plpgsql, volatile, `search_path =
//! pg_catalog, forge, pg_temp`, the shipped arguments and body, and EXECUTE
//! for the listed roles only (besides its owner):
//!
//! - `forge.assign_seq` (step C4): owner forge_seq, EXECUTE forge_app;
//! - `forge.events_retention` (M3 I1c): owner forge_purge, EXECUTE
//!   forge_app (the worker's `events.retention` task);
//! - `forge.feed_new_epoch` (M3 I1c): owner forge_seq, EXECUTE forge_maint
//!   (`forge admin events new-epoch`).
//!
//! Each finding is `FG-DB-0009 reason=guard_missing` with `guard`
//! (function_owner / function_body / function_privilege) and `function`.

use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::guard_missing;
use crate::read_query_error;

/// One shipped SECURITY DEFINER function.
struct Definer {
    name: &'static str,
    signature: &'static str,
    arguments: &'static str,
    /// The migration that creates it.
    version: i64,
    owner: &'static str,
    /// Grantees other than the owner, as `role:PRIVILEGE`.
    grants: &'static [&'static str],
}

const DEFINERS: &[Definer] = &[
    Definer {
        name: "assign_seq",
        signature: "forge.assign_seq(integer,boolean)",
        arguments: "max_batch integer, include_audit boolean, OUT from_seq bigint, \
             OUT to_seq bigint, OUT audit_from bigint, OUT audit_to bigint",
        version: 20261027110000,
        owner: "forge_seq",
        grants: &["forge_app:EXECUTE"],
    },
    Definer {
        name: "events_retention",
        signature: "forge.events_retention(integer,text)",
        arguments: "max_rows integer, request_id text, OUT deleted bigint, \
             OUT from_seq bigint, OUT to_seq bigint",
        version: 20261117100000,
        owner: "forge_purge",
        grants: &["forge_app:EXECUTE"],
    },
    Definer {
        name: "feed_new_epoch",
        signature: "forge.feed_new_epoch(text,text)",
        arguments: "reason text, request_id text, OUT old_epoch uuid, OUT new_epoch uuid",
        version: 20261117100000,
        owner: "forge_seq",
        grants: &["forge_maint:EXECUTE"],
    },
];

fn finding(guard: &'static str, name: &'static str) -> ForgeError {
    guard_missing(guard).with_context("function", name)
}

/// Every shipped SECURITY DEFINER function (the name kept for the caller).
pub(super) async fn check_assign_seq(conn: &mut PgConnection) -> Result<(), ForgeError> {
    for definer in DEFINERS {
        check_definer(conn, definer).await?;
    }
    Ok(())
}

async fn check_definer(conn: &mut PgConnection, d: &Definer) -> Result<(), ForgeError> {
    let (applied, defined): (bool, i64) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM forge._sqlx_migrations WHERE version = $1 AND success), \
                (SELECT count(*) FROM pg_catalog.pg_proc p \
                 JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = 'forge' AND p.proname = $2)",
    )
    .bind(d.version)
    .bind(d.name)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.definer_present", e))?;
    if !applied && defined == 0 {
        // Not migrated yet: the schema check reports the pending migration.
        return Ok(());
    }
    // Missing, or an overload next to it (a better-matching call could
    // reach code that is not the shipped one).
    if defined != 1 {
        return Err(finding("function_owner", d.name).with_context("count", defined));
    }
    let found: Option<(String, bool, String)> = sqlx::query_as(
        "SELECT pg_catalog.pg_get_userbyid(p.proowner)::text, \
         p.prosecdef AND p.prokind = 'f' AND p.provolatile = 'v' \
         AND p.proconfig = ARRAY['search_path=pg_catalog, forge, pg_temp'] \
         AND p.prolang = (SELECT l.oid FROM pg_catalog.pg_language l WHERE l.lanname = 'plpgsql') \
         AND pg_catalog.pg_get_function_arguments(p.oid) = $2, \
         p.prosrc \
         FROM pg_catalog.pg_proc p WHERE p.oid = pg_catalog.to_regprocedure($1)",
    )
    .bind(d.signature)
    .bind(d.arguments)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.definer_function", e))?;
    let Some((owner, shape, body)) = found else {
        return Err(finding("function_owner", d.name));
    };
    if owner != d.owner {
        return Err(finding("function_owner", d.name));
    }
    if !shape || migration_body(d.name).as_deref() != Some(body.as_str()) {
        return Err(finding("function_body", d.name));
    }
    check_grants(conn, d).await
}

/// Exactly the function's grants besides the owner (PUBLIC included: a
/// function's default ACL grants PUBLIC EXECUTE).
async fn check_grants(conn: &mut PgConnection, d: &Definer) -> Result<(), ForgeError> {
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT coalesce(g.rolname::text, 'PUBLIC') || ':' || a.privilege_type \
         || CASE WHEN a.is_grantable THEN '*' ELSE '' END \
         FROM pg_catalog.pg_proc p, \
         pg_catalog.aclexplode(coalesce(p.proacl, pg_catalog.acldefault('f', p.proowner))) a \
         LEFT JOIN pg_catalog.pg_roles g ON g.oid = a.grantee \
         WHERE p.oid = pg_catalog.to_regprocedure($1) AND a.grantee <> p.proowner",
    )
    .bind(d.signature)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.definer_privileges", e))?;
    if let Some(extra) = found.iter().find(|g| !d.grants.contains(&g.as_str())) {
        return Err(finding("function_privilege", d.name).with_context("role", role_of(extra)));
    }
    if let Some(missing) = d.grants.iter().find(|g| !found.iter().any(|f| f == *g)) {
        return Err(finding("function_privilege", d.name).with_context("role", role_of(missing)));
    }
    Ok(())
}

fn role_of(grant: &str) -> &str {
    grant.split_once(':').map_or(grant, |(role, _)| role)
}

/// The body of `forge.<name>(…)` in the last migration that defines it
/// (functions with arguments; `schema::migration_body` matches `()` only).
fn migration_body(name: &str) -> Option<String> {
    let marker = format!("FUNCTION forge.{name}(");
    crate::MIGRATOR.iter().rev().find_map(|m| {
        let sql = m.sql.as_ref();
        let start = sql
            .rmatch_indices(marker.as_str())
            .map(|(i, _)| i)
            .find(|&i| {
                let before = sql.get(..i).unwrap_or_default();
                before.ends_with("CREATE ") || before.ends_with("CREATE OR REPLACE ")
            })?;
        let rest = sql.get(start..)?;
        let open = rest.find("AS $$")? + "AS $$".len();
        let body = rest.get(open..)?;
        let close = body.find("$$")?;
        body.get(..close).map(str::to_owned)
    })
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_shipped_body_is_found_in_the_migrations() {
        let body = migration_body("assign_seq").expect("assign_seq is shipped");
        assert!(body.contains("pg_advisory_xact_lock"), "{body}");
        assert!(body.trim_end().ends_with("END"), "{body}");
        assert_eq!(role_of("forge_app:EXECUTE"), "forge_app");
        for d in DEFINERS {
            let body = migration_body(d.name);
            assert!(
                body.as_deref()
                    .is_some_and(|b| b.trim_end().ends_with("END")),
                "{} is shipped",
                d.name
            );
        }
    }
}
