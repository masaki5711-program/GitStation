//! Loading a user's membership roles (step D2, ADR-0010): the one place a
//! role is computed from the membership tables. `authorize()` stays pure and
//! decides with what these functions return; W1 builds `native_role()` on
//! [`MemberRoles`] (ADR-0017 §4.0). Projects joined in step D4
//! ([`load_project_roles`], ADR-0010 Amendment D4).
//!
//! Reads run on the read pool (`forge_ro`, the primary — not a replica)
//! under [`Db::within`]; one statement, so one snapshot.

use forge_core::ids::{GroupId, NamespaceId, ProjectId, UserId};
use forge_core::namespace_path::MAX_DEPTH;
use forge_core::Role;
use forge_errors::{ErrorCode, ForgeError};

use sqlx::postgres::PgConnection;

use crate::{query_error, read_query_error, Db};

/// A user's roles on one group or project namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemberRoles {
    /// The highest role over the namespace's own membership and its
    /// ancestors: the ancestor groups' direct roles and, for a project in a
    /// personal namespace, Owner for that namespace's owner; `None` = no
    /// role there.
    pub native: Option<Role>,
    /// The direct role in this group or project only; `None` = not a direct
    /// member.
    pub direct: Option<Role>,
}

/// The cap on the parent chain, as the SQL parameter. Groups are created at
/// most `MAX_DEPTH` deep (`groups::create`, CHECK `namespaces_depth`), so
/// only corrupt data reaches it.
pub(crate) const CAP: i32 = {
    assert!(MAX_DEPTH <= i32::MAX as usize);
    MAX_DEPTH as i32
};

/// The namespace chain from the anchor (step 1) up to the top level, capped
/// at `CAP` steps ($3), as a recursive CTE named `chain` (columns `id`,
/// `parent_id`, `kind`, `owner_id`, `segment`, `step`, `anchor_kind`). The
/// anchor is a `WHERE` clause on `forge.namespaces` matching at most one
/// row. Shared by the role loaders and the namespace resolver
/// (`crate::resolve`), so both walk the same chain.
macro_rules! chain_cte {
    ($anchor:literal) => {
        concat!(
            "chain (id, parent_id, kind, owner_id, segment, step, anchor_kind) AS ( \
               SELECT id, parent_id, kind, owner_id, segment, 1, kind \
               FROM forge.namespaces WHERE ",
            $anchor,
            " UNION ALL \
               SELECT n.id, n.parent_id, n.kind, n.owner_id, n.segment, c.step + 1, \
                 c.anchor_kind \
               FROM forge.namespaces n JOIN chain c ON n.id = c.parent_id \
               WHERE c.step < $3)"
        )
    };
}
pub(crate) use chain_cte;

/// The columns of one `chain c` row the fold needs ([`fold_chain`]):
/// `step`, `truncated` (the chain was cut by the cap: a cycle or a corrupt
/// depth) and the user's ($2; NULL = anonymous, no role anywhere) `role` at
/// that step: the direct role in a `group` (group_members) or a `project`
/// (project_members), and Owner on a `user` namespace the user owns — only
/// when that namespace is the direct parent (step 2) of a `project` anchor,
/// so a personal namespace gives its owner a role on the projects directly
/// in it, never on itself (ownership of the namespace is
/// `Resource::Namespace`'s) and never on anything a later move (D5) might
/// place deeper below it. Only active users hold a role, and a bot never
/// gets the personal-namespace Owner (bots are never members, ADR-0017;
/// `projects::create` also refuses a bot's namespace as a parent).
macro_rules! chain_role_columns {
    () => {
        "c.step, c.parent_id IS NOT NULL AND c.step >= $3 AS truncated, \
           CASE c.kind \
             WHEN 'group' THEN (SELECT m.role FROM forge.group_members m \
               JOIN forge.users u ON u.id = m.user_id \
               WHERE m.group_id = c.owner_id AND m.user_id = $2 \
                 AND u.state = 'active') \
             WHEN 'project' THEN (SELECT m.role FROM forge.project_members m \
               JOIN forge.users u ON u.id = m.user_id \
               WHERE m.project_id = c.owner_id AND m.user_id = $2 \
                 AND u.state = 'active') \
             WHEN 'user' THEN (SELECT 'owner' FROM forge.users u \
               WHERE c.step = 2 AND c.anchor_kind = 'project' \
                 AND c.owner_id = $2 AND u.id = $2 \
                 AND u.state = 'active' AND u.kind <> 'bot') \
           END AS role"
    };
}
pub(crate) use chain_role_columns;

/// The role loaders' statement: the chain from the anchor ($1) with the
/// user's ($2) role at each step, in step order.
macro_rules! chain_sql {
    ($anchor:literal) => {
        concat!(
            "WITH RECURSIVE ",
            chain_cte!($anchor),
            " SELECT ",
            chain_role_columns!(),
            " FROM chain c ORDER BY c.step"
        )
    };
}

const BY_GROUP: &str = chain_sql!("kind = 'group' AND owner_id = $1");
const BY_NAMESPACE: &str = chain_sql!("id = $1");
const BY_PROJECT: &str = chain_sql!("kind = 'project' AND owner_id = $1");

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
/// namespace it is the project's role, the same answer as
/// [`load_project_roles`]. `FG-REPO-0005` (`resource=namespace`,
/// `reason=absent`) when it does not exist. `user` as in
/// [`load_group_roles`].
pub async fn load_namespace_roles(
    db: &Db,
    namespace: NamespaceId,
    user: UserId,
) -> Result<MemberRoles, ForgeError> {
    load(db, BY_NAMESPACE, namespace.get(), user, "namespace").await
}

/// The roles of `user` on `project` (step D4, ADR-0010 Amendment D4):
/// `direct` is the project_members role; `native` the highest of it, the
/// direct roles in the ancestor groups of the project's namespace and, in a
/// personal namespace, Owner for that namespace's owner. Active users only;
/// the chain is capped as for groups (`FG-DB-0018`). Nothing here looks at
/// visibility or deletion (`resolve::project_access`). `FG-REPO-0005`
/// (`resource=project`, `reason=absent`) when the project has no namespace
/// row. `user` as in [`load_group_roles`].
pub async fn load_project_roles(
    db: &Db,
    project: ProjectId,
    user: UserId,
) -> Result<MemberRoles, ForgeError> {
    load(db, BY_PROJECT, project.get(), user, "project").await
}

// ── M1 phase 8b MR7a: the same roles, read inside a write transaction ──

/// Locks (`FOR SHARE`) the viewer's ($2) direct memberships in the groups
/// of the anchor's chain ($1, capped at $3), in group id order: a removal
/// or role change of one of them (an `UPDATE` / `DELETE` of the row) waits
/// until the write that read them commits, and one committed first is seen
/// by the chain read that follows.
macro_rules! lock_group_members_sql {
    ($anchor:literal) => {
        concat!(
            "WITH RECURSIVE ",
            chain_cte!($anchor),
            " SELECT m.group_id FROM forge.group_members m \
               WHERE m.user_id = $2 \
                 AND m.group_id IN (SELECT c.owner_id FROM chain c WHERE c.kind = 'group') \
               ORDER BY m.group_id FOR SHARE OF m"
        )
    };
}

const LOCK_BY_GROUP: &str = lock_group_members_sql!("kind = 'group' AND owner_id = $1");
const LOCK_BY_NAMESPACE: &str = lock_group_members_sql!("id = $1");
const LOCK_BY_PROJECT: &str = lock_group_members_sql!("kind = 'project' AND owner_id = $1");

/// The viewer's direct membership of the project, locked `FOR SHARE`.
const LOCK_PROJECT_MEMBER: &str = "SELECT role FROM forge.project_members \
     WHERE project_id = $1 AND user_id = $2 FOR SHARE";

/// What a role is read for inside a write transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoleAnchor {
    Group(GroupId),
    Namespace(NamespaceId),
    Project(ProjectId),
}

/// The roles of `user` on `anchor`, read on the write transaction's
/// connection **after locking the membership rows they come from**
/// (`FOR SHARE`, see [`lock_group_members_sql`]): the same chain and fold
/// as [`load_group_roles`] / [`load_namespace_roles`] /
/// [`load_project_roles`], so a write decides on the roles it holds until
/// it commits (M1 phase 8b MR7a; no time-of-check / time-of-use gap). The
/// caller locks the target row first and the user row after (the lock
/// order of `projects::members`). Errors as the read-pool loaders.
pub(crate) async fn lock_roles(
    conn: &mut PgConnection,
    anchor: RoleAnchor,
    user: UserId,
) -> Result<MemberRoles, ForgeError> {
    let (lock, read, id, resource) = match anchor {
        RoleAnchor::Group(g) => (LOCK_BY_GROUP, BY_GROUP, g.get(), "group"),
        RoleAnchor::Namespace(n) => (LOCK_BY_NAMESPACE, BY_NAMESPACE, n.get(), "namespace"),
        RoleAnchor::Project(p) => (LOCK_BY_PROJECT, BY_PROJECT, p.get(), "project"),
    };
    sqlx::query_scalar::<_, i64>(lock)
        .bind(id)
        .bind(user.get())
        .bind(CAP)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error("roles.lock", e))?;
    if let RoleAnchor::Project(p) = anchor {
        sqlx::query_scalar::<_, String>(LOCK_PROJECT_MEMBER)
            .bind(p.get())
            .bind(user.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error("roles.lock_project_member", e))?;
    }
    let rows: Vec<(i32, bool, Option<String>)> = sqlx::query_as(read)
        .bind(id)
        .bind(user.get())
        .bind(CAP)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error("roles.load_locked", e))?;
    if rows.is_empty() {
        return Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", resource)
            .with_context("reason", "absent"));
    }
    fold_chain(rows, resource, id, Some(user))
}

// ── end M1 phase 8b MR7a ──

/// `FG-DB-0018` with what the runbook needs to find the rows (`user` only
/// for a signed-in viewer).
fn invalid(
    reason: &'static str,
    resource: &'static str,
    id: i64,
    user: Option<UserId>,
) -> ForgeError {
    let err = ForgeError::new(ErrorCode::MembershipDataInvalid)
        .with_context("reason", reason)
        .with_context("resource", resource)
        .with_context("id", id);
    match user {
        Some(user) => err.with_context("user", user.get()),
        None => err,
    }
}

/// Run one chain query and fold it.
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
    fold_chain(rows, resource, anchor, Some(user))
}

/// Fold the chain rows (`step`, `truncated`, `role`; [`chain_role_columns`])
/// into [`MemberRoles`] — the one role computation, shared with the
/// namespace resolver (`crate::resolve`). A chain longer than the cap or an
/// unknown stored role is `FG-DB-0018`, never a silently partial role.
/// `resource` / `id` name where the load started (for the log).
pub(crate) fn fold_chain(
    rows: impl IntoIterator<Item = (i32, bool, Option<String>)>,
    resource: &'static str,
    id: i64,
    user: Option<UserId>,
) -> Result<MemberRoles, ForgeError> {
    let mut roles = MemberRoles::default();
    for (step, truncated, role) in rows {
        if truncated {
            return Err(invalid("namespace_chain_too_deep", resource, id, user));
        }
        let Some(raw) = role else { continue };
        let Some(role) = Role::parse(&raw) else {
            // Bounded: the column is unchecked text once the CHECK is gone.
            let raw: String = raw.chars().take(32).collect();
            return Err(invalid("unknown_role", resource, id, user).with_context("raw", raw));
        };
        // Step 1 is the anchor itself: its group or project is the direct
        // membership; a personal namespace has no role there.
        if step == 1 {
            roles.direct = Some(role);
        }
        roles.native = roles.native.max(Some(role));
    }
    Ok(roles)
}

#[cfg(test)]
#[path = "roles_project_tests.rs"]
mod project_tests;
#[cfg(test)]
#[path = "roles_tests.rs"]
mod tests;
