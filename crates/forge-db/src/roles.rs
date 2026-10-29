//! Loading a user's membership roles (step D2, ADR-0010): the one place a
//! role is computed from the membership tables. `authorize()` stays pure and
//! decides with what these functions return; W1 builds `native_role()` on
//! [`MemberRoles`] (ADR-0017 §4.0). Projects join in step D4.
//!
//! Reads run on the read pool (`forge_ro`, the primary — not a replica)
//! under [`Db::within`]; one statement, so one snapshot.

use forge_core::ids::{GroupId, NamespaceId, UserId};
use forge_core::namespace_path::MAX_DEPTH;
use forge_core::Role;
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// A user's roles on one group namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemberRoles {
    /// The highest direct role over the namespace's group and its ancestor
    /// groups; `None` = no role there.
    pub native: Option<Role>,
    /// The direct role in this group only; `None` = not a direct member.
    pub direct: Option<Role>,
}

/// The cap on the parent chain, as the SQL parameter. Groups are created at
/// most `MAX_DEPTH` deep (`groups::create`, CHECK `namespaces_depth`), so
/// only corrupt data reaches it.
const CAP: i32 = {
    assert!(MAX_DEPTH <= i32::MAX as usize);
    MAX_DEPTH as i32
};

/// The namespace chain from the anchor (step 1) up to the top level, capped
/// at `CAP` steps ($3), with the user's ($2) direct role in each group on
/// it. Only active users hold a role; a `user` namespace's `owner_id` is a
/// user id and never joins `group_members`. `truncated` marks a chain cut by
/// the cap (a cycle or a corrupt depth).
macro_rules! chain_sql {
    ($anchor:literal) => {
        concat!(
            "WITH RECURSIVE chain (parent_id, kind, owner_id, step) AS ( \
               SELECT parent_id, kind, owner_id, 1 FROM forge.namespaces WHERE ",
            $anchor,
            " UNION ALL \
               SELECT n.parent_id, n.kind, n.owner_id, c.step + 1 \
               FROM forge.namespaces n JOIN chain c ON n.id = c.parent_id \
               WHERE c.step < $3) \
             SELECT c.step, c.parent_id IS NOT NULL AND c.step >= $3 AS truncated, \
               (SELECT m.role FROM forge.group_members m \
                JOIN forge.users u ON u.id = m.user_id \
                WHERE c.kind = 'group' AND m.group_id = c.owner_id \
                  AND m.user_id = $2 AND u.state = 'active') AS role \
             FROM chain c ORDER BY c.step"
        )
    };
}

const BY_GROUP: &str = chain_sql!("kind = 'group' AND owner_id = $1");
const BY_NAMESPACE: &str = chain_sql!("id = $1");

/// The roles of `user` on `group`. `FG-REPO-0005` (`resource=group`,
/// `reason=absent`) when the group has no namespace row.
///
/// `user` is the authenticated principal: an unknown or disabled user gets
/// no role, the same answer as a non-member.
pub async fn load_group_roles(
    db: &Db,
    group: GroupId,
    user: UserId,
) -> Result<MemberRoles, ForgeError> {
    load(db, BY_GROUP, group.get(), user, "group").await
}

/// The roles of `user` on `namespace`. On a personal namespace both are
/// `None` (ownership is decided from `Resource::Namespace`). On a project
/// namespace `native` covers only the ancestor groups and `direct` is `None`
/// until step D4 adds project members — not a project role yet.
/// `FG-REPO-0005` (`resource=namespace`, `reason=absent`) when it does not
/// exist. `user` as in [`load_group_roles`].
pub async fn load_namespace_roles(
    db: &Db,
    namespace: NamespaceId,
    user: UserId,
) -> Result<MemberRoles, ForgeError> {
    load(db, BY_NAMESPACE, namespace.get(), user, "namespace").await
}

/// `FG-DB-0018` with what the runbook needs to find the rows.
fn invalid(reason: &'static str, resource: &'static str, id: i64, user: UserId) -> ForgeError {
    ForgeError::new(ErrorCode::MembershipDataInvalid)
        .with_context("reason", reason)
        .with_context("resource", resource)
        .with_context("id", id)
        .with_context("user", user.get())
}

/// Run one chain query and fold it. A chain longer than the cap or an
/// unknown stored role is `FG-DB-0018`, never a silently partial role.
async fn load(
    db: &Db,
    sql: &'static str,
    anchor: i64,
    user: UserId,
    resource: &'static str,
) -> Result<MemberRoles, ForgeError> {
    let rows: Vec<(i32, bool, Option<String>)> = db
        .within("roles.load", async {
            sqlx::query_as(sql)
                .bind(anchor)
                .bind(user.get())
                .bind(CAP)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error("roles.load", e))
        })
        .await?;
    if rows.is_empty() {
        return Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", resource)
            .with_context("reason", "absent"));
    }
    let mut roles = MemberRoles::default();
    for (step, truncated, role) in rows {
        if truncated {
            return Err(invalid("namespace_chain_too_deep", resource, anchor, user));
        }
        let Some(raw) = role else { continue };
        let Some(role) = Role::parse(&raw) else {
            // Bounded: the column is unchecked text once the CHECK is gone.
            let raw: String = raw.chars().take(32).collect();
            return Err(invalid("unknown_role", resource, anchor, user).with_context("raw", raw));
        };
        // Step 1 is the anchor itself (ORDER BY step): its group is the
        // direct membership; a personal namespace has no role row there.
        if step == 1 {
            roles.direct = Some(role);
        }
        roles.native = roles.native.max(Some(role));
    }
    Ok(roles)
}

#[cfg(test)]
#[path = "roles_tests.rs"]
mod tests;
