//! Privilege guards: the ACLs of the database, the schemas, the log tables
//! and the migration bookkeeping, and the default privileges of the owner
//! roles, compared exactly with what step 3 grants. The DDL log cannot say
//! what a GRANT or REVOKE changed (ADR-0018 amendment), so this is the only
//! detector for privilege drift — forge_owner can change the ACLs of what
//! it owns (schema forge, forge._sqlx_migrations) and its own defaults.

use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::guard_missing;
use crate::read_query_error;

/// `grantee:privilege` (`*` when grantable; PUBLIC for grantee 0) of every
/// grant to someone other than the owner.
const GRANT_ROW: &str = "coalesce(g.rolname::text, 'PUBLIC') || ':' || a.privilege_type \
     || CASE WHEN a.is_grantable THEN '*' ELSE '' END";

/// Every grant on a table and its columns (`role:PRIV(column)`), but the
/// owner's.
pub(super) fn table_grants_sql(name: &str) -> String {
    format!(
        "SELECT {GRANT_ROW} FROM pg_catalog.pg_class c, \
         pg_catalog.aclexplode(coalesce(c.relacl, pg_catalog.acldefault('r', c.relowner))) a \
         LEFT JOIN pg_catalog.pg_roles g ON g.oid = a.grantee \
         WHERE c.oid = '{name}'::pg_catalog.regclass AND a.grantee <> c.relowner \
         UNION ALL \
         SELECT {GRANT_ROW} || '(' || t.attname::text || ')' FROM pg_catalog.pg_attribute t, \
         pg_catalog.aclexplode(t.attacl) a \
         LEFT JOIN pg_catalog.pg_roles g ON g.oid = a.grantee \
         WHERE t.attrelid = '{name}'::pg_catalog.regclass AND t.attacl IS NOT NULL \
           AND a.grantee <> (SELECT relowner FROM pg_catalog.pg_class \
                             WHERE oid = '{name}'::pg_catalog.regclass)"
    )
}

/// The grants `sql` finds equal `expected` (`guard=privileges`, `object`).
pub(super) async fn compare(
    conn: &mut PgConnection,
    object: &str,
    sql: &str,
    expected: &[String],
) -> Result<(), ForgeError> {
    let mut found: Vec<String> = sqlx::query_scalar(sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.privileges", e))?;
    found.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    if found != expected {
        return Err(guard_missing("privileges")
            .with_context("object", object)
            .with_context("grants", found.join(",")));
    }
    Ok(())
}

/// (object label, query returning one row per grant, expected rows).
fn expectations() -> Vec<(&'static str, String, Vec<&'static str>)> {
    let on_schema = |name: &str| {
        format!(
            "SELECT {GRANT_ROW} FROM pg_catalog.pg_namespace n, \
             pg_catalog.aclexplode(coalesce(n.nspacl, pg_catalog.acldefault('n', n.nspowner))) a \
             LEFT JOIN pg_catalog.pg_roles g ON g.oid = a.grantee \
             WHERE n.nspname = '{name}' AND a.grantee <> n.nspowner"
        )
    };
    let on_table = table_grants_sql;
    let read_log = vec![
        "forge_app:SELECT",
        "forge_maint:SELECT",
        "forge_owner:SELECT",
        "forge_ro:SELECT",
    ];
    vec![
        (
            "database",
            format!(
                "SELECT {GRANT_ROW} FROM pg_catalog.pg_database d, \
                 pg_catalog.aclexplode(coalesce(d.datacl, pg_catalog.acldefault('d', d.datdba))) a \
                 LEFT JOIN pg_catalog.pg_roles g ON g.oid = a.grantee \
                 WHERE d.datname = pg_catalog.current_database() AND a.grantee <> d.datdba"
            ),
            vec![
                "forge_app:CONNECT",
                "forge_maint:CONNECT",
                "forge_migrator:CONNECT",
            ],
        ),
        ("schema public", on_schema("public"), vec!["PUBLIC:USAGE"]),
        (
            "schema forge",
            on_schema("forge"),
            vec![
                "forge_app:USAGE",
                "forge_maint:USAGE",
                "forge_purge:CREATE",
                "forge_purge:USAGE",
                "forge_ro:USAGE",
                "forge_seq:CREATE",
                "forge_seq:USAGE",
            ],
        ),
        (
            "schema forge_audit",
            on_schema("forge_audit"),
            vec![
                "forge_app:USAGE",
                "forge_maint:USAGE",
                "forge_owner:USAGE",
                "forge_purge:USAGE",
                "forge_ro:USAGE",
                "forge_seq:USAGE",
            ],
        ),
        (
            "forge_audit.bootstrap_log",
            on_table("forge_audit.bootstrap_log"),
            read_log.clone(), // clone: the same grants on several tables
        ),
        (
            "forge_audit.ddl_audit",
            on_table("forge_audit.ddl_audit"),
            read_log.clone(), // clone: the same grants on several tables
        ),
        (
            "forge_audit.ddl_audit_ack",
            on_table("forge_audit.ddl_audit_ack"),
            [
                read_log.as_slice(),
                &[
                    "forge_maint:INSERT(ddl_audit_id)",
                    "forge_maint:INSERT(note)",
                ],
            ]
            .concat(),
        ),
        (
            "forge_audit.guarded_objects",
            on_table("forge_audit.guarded_objects"),
            [
                read_log.as_slice(),
                &[
                    "forge_owner:INSERT(object_identity)",
                    "forge_owner:INSERT(object_type)",
                ],
            ]
            .concat(),
        ),
        (
            "forge._sqlx_migrations",
            on_table("forge._sqlx_migrations"),
            vec!["forge_app:SELECT", "forge_ro:SELECT"],
        ),
        (
            // Default privileges of the owner roles: only the revoked
            // PUBLIC EXECUTE on functions (the owner's own entry is not a
            // grant to someone else).
            "default privileges",
            format!(
                "SELECT r.rolname::text || '/' || d.defaclobjtype::text || ':' || {GRANT_ROW} \
                 FROM pg_catalog.pg_default_acl d \
                 JOIN pg_catalog.pg_roles r ON r.oid = d.defaclrole, \
                 pg_catalog.aclexplode(d.defaclacl) a \
                 LEFT JOIN pg_catalog.pg_roles g ON g.oid = a.grantee \
                 WHERE r.rolname IN ('forge_owner', 'forge_seq', 'forge_purge') \
                   AND (a.grantee <> d.defaclrole OR d.defaclnamespace <> 0) \
                 UNION ALL \
                 SELECT r.rolname::text || '/f:missing' FROM pg_catalog.pg_roles r \
                 WHERE r.rolname IN ('forge_owner', 'forge_seq', 'forge_purge') \
                   AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_default_acl d \
                                   WHERE d.defaclrole = r.oid AND d.defaclobjtype = 'f' \
                                     AND d.defaclnamespace = 0)"
            ),
            vec![],
        ),
    ]
}

/// Every ACL equal to the shipped one (`guard=privileges`, `object`).
pub(super) async fn check_privileges(conn: &mut PgConnection) -> Result<(), ForgeError> {
    for (object, sql, expected) in expectations() {
        let expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
        compare(conn, object, &sql, &expected).await?;
    }
    Ok(())
}
