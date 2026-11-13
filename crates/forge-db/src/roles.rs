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
use forge_core::{Role, Visibility};
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

// ── Plan 15 MR 2: the chain's visibility and what lies below ──

/// What one chain read knows about a user and a group, project or
/// namespace (plan 15 MR 2): the roles of the role loaders, the effective
/// visibility and whether the user holds a role strictly below. Facts
/// only — `authorize()` decides with them from plan 15 MR 3; nothing does
/// yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainAccess {
    /// The same answer as [`load_group_roles`] / [`load_namespace_roles`] /
    /// [`load_project_roles`] on the same anchor.
    pub roles: MemberRoles,
    /// The narrowest visibility on the chain from the anchor to the top
    /// level: `groups.visibility` of each group and, for a project anchor,
    /// `projects.visibility` (`private` < `internal`). `None` only when the
    /// chain holds no group and no project (a personal namespace anchor:
    /// it has no visibility, `Resource::Namespace` decides).
    pub visibility: Option<Visibility>,
    /// The user (active) holds a direct role on a group or on a not-deleted
    /// project strictly below the anchor (any depth up to the cap). With an
    /// effective `private` anchor this is exactly "something below is
    /// visible to the user": everything below is private too, so only a
    /// role there makes it visible (a role above the anchor is a role on
    /// it, in `roles`). The pass-through view of plan 15 §3.2.
    pub has_visible_descendant: bool,
}

/// The visibility of one chain row (with its `kind`): the group's or the
/// project's stored value, NULL on a personal namespace (and on a group or
/// project namespace whose row is missing — refused by the fold).
macro_rules! chain_visibility_columns {
    () => {
        "c.kind, CASE c.kind \
           WHEN 'group' THEN (SELECT g.visibility FROM forge.groups g WHERE g.id = c.owner_id) \
           WHEN 'project' THEN (SELECT p.visibility FROM forge.projects p \
             WHERE p.id = c.owner_id) \
         END AS visibility"
    };
}

/// The access loaders' statement: the role loaders' chain and columns, the
/// visibility of each row, and (the same on every row: an uncorrelated
/// subquery, evaluated once) whether a direct membership of the user ($2)
/// — active user; a group, or a project not deleted — has the anchor as a
/// strict ancestor. Their chains are walked upwards (`above`, capped at $3
/// steps like `chain`) from the memberships, which the user indexes
/// (`group_members_user`, `project_members_user`) find; the anchor's
/// subtree is never scanned.
macro_rules! access_sql {
    ($anchor:literal) => {
        concat!(
            "WITH RECURSIVE ",
            chain_cte!($anchor),
            ", member_ns (id) AS ( \
               SELECT n.id FROM forge.group_members m \
                 JOIN forge.users u ON u.id = m.user_id AND u.state = 'active' \
                 JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = m.group_id \
               WHERE m.user_id = $2 \
               UNION ALL \
               SELECT n.id FROM forge.project_members m \
                 JOIN forge.users u ON u.id = m.user_id AND u.state = 'active' \
                 JOIN forge.projects p ON p.id = m.project_id AND p.deleted_at IS NULL \
                 JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = m.project_id \
               WHERE m.user_id = $2), \
             above (id, parent_id, step) AS ( \
               SELECT n.id, n.parent_id, 1 FROM forge.namespaces n \
               WHERE n.id IN (SELECT id FROM member_ns) \
               UNION ALL \
               SELECT n.id, n.parent_id, a.step + 1 \
               FROM forge.namespaces n JOIN above a ON n.id = a.parent_id \
               WHERE a.step < $3) \
             SELECT ",
            chain_role_columns!(),
            ", ",
            chain_visibility_columns!(),
            ", EXISTS (SELECT 1 FROM above a \
               WHERE a.parent_id = (SELECT id FROM chain WHERE step = 1)) \
               AS has_visible_descendant \
             FROM chain c ORDER BY c.step"
        )
    };
}

const ACCESS_BY_GROUP: &str = access_sql!("kind = 'group' AND owner_id = $1");
const ACCESS_BY_NAMESPACE: &str = access_sql!("id = $1");
const ACCESS_BY_PROJECT: &str = access_sql!("kind = 'project' AND owner_id = $1");

/// One access row: `step`, `truncated`, `role`, `kind`, `visibility`,
/// `has_visible_descendant`.
type AccessRow = (i32, bool, Option<String>, String, Option<String>, bool);

/// [`ChainAccess`] of `user` on `group`, in one statement on the read pool.
/// Errors as [`load_group_roles`], and `FG-DB-0018`
/// `reason=unknown_visibility` (`raw`) for a stored visibility outside
/// `private` / `internal` (or a group / project row missing under its
/// namespace) anywhere on the chain.
pub async fn load_group_access(
    db: &Db,
    group: GroupId,
    user: UserId,
) -> Result<ChainAccess, ForgeError> {
    load_access(db, ACCESS_BY_GROUP, group.get(), user, "group").await
}

/// [`ChainAccess`] of `user` on `namespace` (a project namespace answers as
/// [`load_project_access`]). Errors as [`load_group_access`].
pub async fn load_namespace_access(
    db: &Db,
    namespace: NamespaceId,
    user: UserId,
) -> Result<ChainAccess, ForgeError> {
    load_access(db, ACCESS_BY_NAMESPACE, namespace.get(), user, "namespace").await
}

/// [`ChainAccess`] of `user` on `project`: the visibility includes the
/// project's own. Nothing here looks at the deletion of the project itself
/// (`resolve::project_access`). Errors as [`load_group_access`].
pub async fn load_project_access(
    db: &Db,
    project: ProjectId,
    user: UserId,
) -> Result<ChainAccess, ForgeError> {
    load_access(db, ACCESS_BY_PROJECT, project.get(), user, "project").await
}

/// Run one access query and fold it.
async fn load_access(
    db: &Db,
    sql: &'static str,
    anchor: i64,
    user: UserId,
    resource: &'static str,
) -> Result<ChainAccess, ForgeError> {
    let rows: Vec<AccessRow> = db
        .within("roles.load_access", async {
            sqlx::query_as(sql)
                .bind(anchor)
                .bind(user.get())
                .bind(CAP)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error("roles.load_access", e))
        })
        .await?;
    if rows.is_empty() {
        return Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", resource)
            .with_context("reason", "absent"));
    }
    fold_access(rows, resource, anchor, Some(user))
}

/// Fold access rows: the roles by [`fold_chain`] (so a truncated chain or
/// an unknown role fails first, as in the role loaders), then the
/// narrowest visibility over the group and project rows.
fn fold_access(
    rows: Vec<AccessRow>,
    resource: &'static str,
    id: i64,
    user: Option<UserId>,
) -> Result<ChainAccess, ForgeError> {
    let has_visible_descendant = rows.iter().any(|row| row.5);
    let roles = fold_chain(
        rows.iter()
            .map(|(step, truncated, role, ..)| (*step, *truncated, role.clone())),
        resource,
        id,
        user,
    )?;
    let mut visibility: Option<Visibility> = None;
    for (_, _, _, kind, raw, _) in &rows {
        if kind != "group" && kind != "project" {
            continue;
        }
        let Some(v) = raw.as_deref().and_then(Visibility::parse) else {
            // Bounded: the column is unchecked text once the CHECK is gone.
            let raw: String = raw
                .as_deref()
                .unwrap_or("<missing>")
                .chars()
                .take(32)
                .collect();
            return Err(invalid("unknown_visibility", resource, id, user).with_context("raw", raw));
        };
        visibility = Some(match (visibility, v) {
            (Some(Visibility::Private), _) | (_, Visibility::Private) => Visibility::Private,
            _ => Visibility::Internal,
        });
    }
    Ok(ChainAccess {
        roles,
        visibility,
        has_visible_descendant,
    })
}

// ── end plan 15 MR 2 ──

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
/// order of `projects::members`). A lock wait is bounded by the write
/// pool's session `lock_timeout` (`pool.rs`): `FG-DB-0002
/// reason=lock_timeout`. Errors as the read-pool loaders.
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

/// The native role of each of `users` on `project`, read on `conn`
/// without locking, in one statement (M3 R5: the approvers' roles at
/// evaluation time — the approval rows are read under the merge request's
/// lock; the memberships are facts of that moment). The chain and the fold
/// are the role loaders' (the same `chain_role_columns`, with the user
/// taken from the list). A user without a role (or inactive) maps to `None`.
pub(crate) async fn project_roles_on(
    conn: &mut PgConnection,
    project: ProjectId,
    users: &[UserId],
) -> Result<std::collections::BTreeMap<UserId, Option<Role>>, ForgeError> {
    const SQL: &str = concat!(
        "WITH RECURSIVE ",
        chain_cte!("kind = 'project' AND owner_id = $1"),
        " SELECT x.user_id, ",
        chain_role_columns!(),
        " FROM chain c CROSS JOIN unnest($2::bigint[]) AS x(user_id) ORDER BY x.user_id, c.step"
    );
    let mut out = std::collections::BTreeMap::new();
    if users.is_empty() {
        return Ok(out);
    }
    // The role columns name the user as `$2`; here it is each listed id.
    let sql = SQL.replace("= $2", "= x.user_id");
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    let rows: Vec<(i64, i32, bool, Option<String>)> = sqlx::query_as(&sql)
        .bind(project.get())
        .bind(&ids)
        .bind(CAP)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error("roles.project_roles", e))?;
    if rows.is_empty() {
        return Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", "project")
            .with_context("reason", "absent"));
    }
    let mut by_user: std::collections::BTreeMap<i64, Vec<(i32, bool, Option<String>)>> =
        std::collections::BTreeMap::new();
    for (user, step, truncated, role) in rows {
        by_user
            .entry(user)
            .or_default()
            .push((step, truncated, role));
    }
    for user in users {
        let chain = by_user.remove(&user.get()).unwrap_or_default();
        let roles = fold_chain(chain, "project", project.get(), Some(*user))?;
        out.insert(*user, roles.native);
    }
    Ok(out)
}

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
#[cfg(test)]
#[path = "roles_visibility_tests.rs"]
mod visibility_tests;
