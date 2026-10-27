//! Role guards: attributes, separation of the logins, and every membership
//! that involves a forge role.

use forge_core::config::PrivilegedMember;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::guard_missing;
use crate::read_query_error;

/// Roles created by step 1 and whether they log in.
const ROLES: &[(&str, bool)] = &[
    ("forge_owner", false),
    ("forge_migrator", true),
    ("forge_seq", false),
    ("forge_purge", false),
    ("forge_app", true),
    ("forge_ro", false),
    ("forge_maint", true),
    ("forge_audit", false),
];

/// The roles that may change guarded objects.
const PRIVILEGED: &[&str] = &["forge_owner", "forge_seq", "forge_purge"];

/// The logins: they may hold no role beyond their fixed memberships (no
/// predefined role such as pg_write_all_data either).
const LOGINS: &[&str] = &["forge_app", "forge_maint", "forge_migrator"];

/// Memberships the bootstrap itself creates (member, role).
const FIXED_MEMBERSHIPS: &[(&str, &str)] = &[
    ("forge_migrator", "forge_owner"),
    ("forge_owner", "forge_seq"),
    ("forge_owner", "forge_purge"),
    ("forge_app", "forge_ro"),
];

pub(super) async fn check_roles(conn: &mut PgConnection) -> Result<(), ForgeError> {
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
    // Neither the application nor the maintenance login may reach a
    // privileged role, directly or through other roles.
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

/// One membership row: member, role, whether it carries ADMIN OPTION.
pub(crate) type Membership = (String, String, bool);

pub(super) async fn check_memberships(
    conn: &mut PgConnection,
    privileged_members: &[PrivilegedMember],
) -> Result<(), ForgeError> {
    let names: Vec<&str> = ROLES.iter().map(|(n, _)| *n).collect();
    let rows: Vec<Membership> = sqlx::query_as(
        "SELECT m.rolname::text, r.rolname::text, am.admin_option \
         FROM pg_catalog.pg_auth_members am \
         JOIN pg_catalog.pg_roles r ON r.oid = am.roleid \
         JOIN pg_catalog.pg_roles m ON m.oid = am.member \
         WHERE r.rolname = ANY($1) OR m.rolname = ANY($1)",
    )
    .bind(&names)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.memberships", e))?;
    if let Some((member, role, _)) = rows
        .iter()
        .find(|(m, r, _)| LOGINS.contains(&m.as_str()) && !is_fixed(m, r))
    {
        return Err(guard_missing("role_separation")
            .with_context("role", member)
            .with_context("member_of", role));
    }
    let (unexpected, missing) = membership_drift(&rows, privileged_members);
    if unexpected.is_empty() && missing.is_empty() {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::AppendOnlyViolation)
        .with_context("reason", "role_membership_changed")
        .with_context("unexpected", unexpected.join(","))
        .with_context("missing", missing.join(",")))
}

fn is_fixed(member: &str, role: &str) -> bool {
    FIXED_MEMBERSHIPS
        .iter()
        .any(|(m, r)| *m == member && *r == role)
}

/// Memberships (`member->role`) that are neither fixed nor configured, or
/// carry ADMIN OPTION; and fixed or configured memberships that are absent.
pub(crate) fn membership_drift(
    rows: &[Membership],
    privileged_members: &[PrivilegedMember],
) -> (Vec<String>, Vec<String>) {
    let configured = |member: &str, role: &str| {
        privileged_members
            .iter()
            .any(|p| p.member == member && p.role == role)
    };
    let mut unexpected: Vec<String> = Vec::new();
    for (member, role, admin) in rows {
        let allowed = is_fixed(member, role) || configured(member, role);
        let label = if *admin {
            format!("{member}->{role}(admin)")
        } else {
            format!("{member}->{role}")
        };
        if (!allowed || *admin) && !unexpected.contains(&label) {
            unexpected.push(label);
        }
    }
    let present = |m: &str, r: &str| rows.iter().any(|(rm, rr, _)| rm == m && rr == r);
    let mut missing: Vec<String> = FIXED_MEMBERSHIPS
        .iter()
        .filter(|(m, r)| !present(m, r))
        .map(|(m, r)| format!("{m}->{r}"))
        .collect();
    missing.extend(
        privileged_members
            .iter()
            .filter(|p| !present(&p.member, &p.role))
            .map(|p| format!("{}->{}", p.member, p.role)),
    );
    (unexpected, missing)
}
