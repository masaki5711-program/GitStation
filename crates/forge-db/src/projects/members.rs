//! Direct project memberships (step D4 MR4): add, change and remove one
//! member. The pattern of `groups::members`, with two differences decided
//! in the D4 plan (answers 2 and 3):
//!
//! - **Inherited Owners count.** The last direct Owner may be removed or
//!   demoted while an inherited Owner exists — an Owner of an ancestor
//!   group, or the owner of the personal namespace the project is in (the
//!   semantics of `roles::load_project_roles`; active users only, a bot
//!   never inherits the personal namespace). Without one, `FG-REPO-0010`
//!   `last_owner`.
//! - **Each change is an event.** `member.added` / `member.role_changed` /
//!   `member.removed` of a project publish an event of the maintainer class
//!   (ADR-0016 §5) besides the audit row on (`project`, id); group member
//!   changes stay audit-only.
//!
//! Lock order (as for groups and the disable guard, ADR-0019 §11): the
//! project row `FOR UPDATE`, then user rows `FOR SHARE` (the candidate, or
//! the remaining Owners in id order) — never a user row first and then a
//! project row. `users::disable` locks only the user row and never a
//! project row, so the two cannot deadlock, and a disable of the Owner a
//! removal relies on is serialized by that user row.

use forge_core::ids::{ProjectId, UserId};
use forge_core::namespace_path::MAX_DEPTH;
use forge_core::{AuditDetails, Role};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{evented_record, not_found, RESOURCE};
use crate::change::{ChangeContext, ChangeTx};
use crate::groups::MemberChange;
use crate::{query_error, Db};

/// The cap on the namespace chain, as the SQL parameter (as in `roles`).
const CAP: i32 = {
    assert!(MAX_DEPTH <= i32::MAX as usize);
    MAX_DEPTH as i32
};

/// `FG-REPO-0010` with its `reason`.
pub(super) fn membership_conflict(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::MembershipConflict).with_context("reason", reason)
}

/// Lock the project row. `FG-REPO-0005` (`resource=project`) when it does
/// not exist; a project without its namespace is a broken invariant
/// (`FG-SYS-0001` `project_without_namespace`), not a 404.
pub(crate) async fn lock_project(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<(), ForgeError> {
    let row: Option<bool> = sqlx::query_scalar(
        "SELECT n.id IS NOT NULL FROM forge.projects p \
         LEFT JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = p.id \
         WHERE p.id = $1 FOR UPDATE OF p",
    )
    .bind(project.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("projects.lock", e))?;
    match row {
        None => Err(not_found("project")),
        Some(false) => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "project_without_namespace")
            .with_context("project_id", project.get())),
        Some(true) => Ok(()),
    }
}

/// The user who may become a member: present (`FG-REPO-0005`), not a bot
/// (`FG-REPO-0010` `bot_member`). Read `FOR SHARE`, after the project lock.
pub(super) async fn member_candidate(
    conn: &mut PgConnection,
    user: UserId,
) -> Result<(), ForgeError> {
    let kind: Option<String> =
        sqlx::query_scalar("SELECT kind FROM forge.users WHERE id = $1 FOR SHARE")
            .bind(user.get())
            .fetch_optional(conn)
            .await
            .map_err(|e| query_error("projects.member_candidate", e))?;
    match kind.as_deref() {
        None => Err(not_found("user")),
        Some("bot") => Err(membership_conflict("bot_member")),
        Some(_) => Ok(()),
    }
}

/// The user's direct role in the project, `None` = not a member. A stored
/// role outside the five is `FG-DB-0018` `unknown_role`.
pub(super) async fn direct_role(
    conn: &mut PgConnection,
    project: ProjectId,
    user: UserId,
) -> Result<Option<Role>, ForgeError> {
    role_of(
        conn,
        "SELECT role FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
        project,
        user,
    )
    .await
}

/// [`direct_role`] with the membership row locked `FOR UPDATE` (M1 phase
/// 8b MR8): the decided member commands act on the role they read, so a
/// write to that row from anywhere waits for them or is seen by them.
pub(super) async fn direct_role_for_update(
    conn: &mut PgConnection,
    project: ProjectId,
    user: UserId,
) -> Result<Option<Role>, ForgeError> {
    role_of(
        conn,
        "SELECT role FROM forge.project_members WHERE project_id = $1 AND user_id = $2 FOR UPDATE",
        project,
        user,
    )
    .await
}

async fn role_of(
    conn: &mut PgConnection,
    sql: &'static str,
    project: ProjectId,
    user: UserId,
) -> Result<Option<Role>, ForgeError> {
    let role: Option<String> = sqlx::query_scalar(sql)
        .bind(project.get())
        .bind(user.get())
        .fetch_optional(conn)
        .await
        .map_err(|e| query_error("projects.member_role", e))?;
    role.map(|raw| {
        Role::parse(&raw).ok_or_else(|| {
            // The role loader's code and context (FG-DB-0018, runbook
            // membership-data). Bounded: the column is unchecked text once
            // the CHECK is gone.
            let raw: String = raw.chars().take(32).collect();
            ForgeError::new(ErrorCode::MembershipDataInvalid)
                .with_context("reason", "unknown_role")
                .with_context("resource", RESOURCE)
                .with_context("id", project.get())
                .with_context("user", user.get())
                .with_context("raw", raw)
        })
    })
    .transpose()
}

/// The active users who stay Owner of the project once `leaving` stops
/// being a direct Owner, locked `FOR SHARE` in id order: the other direct
/// Owners, the direct Owners of the ancestor groups and the owner of the
/// personal namespace the project is directly in (not a bot) — the Owner
/// sources of `roles::load_project_roles`. `leaving` counts when it
/// inherits Owner. The chain is capped at `MAX_DEPTH` steps; a longer
/// (corrupt) chain only finds fewer Owners, which refuses rather than
/// allows.
const REMAINING_OWNERS: &str = "\
    WITH RECURSIVE chain (parent_id, kind, owner_id, step) AS ( \
        SELECT parent_id, kind, owner_id, 1 FROM forge.namespaces \
        WHERE kind = 'project' AND owner_id = $1 \
      UNION ALL \
        SELECT n.parent_id, n.kind, n.owner_id, c.step + 1 \
        FROM forge.namespaces n JOIN chain c ON n.id = c.parent_id WHERE c.step < $3), \
    owners (user_id, personal) AS ( \
        SELECT m.user_id, false FROM forge.project_members m \
        WHERE m.project_id = $1 AND m.role = 'owner' AND m.user_id <> $2 \
      UNION ALL \
        SELECT m.user_id, false FROM chain c JOIN forge.group_members m \
          ON c.kind = 'group' AND m.group_id = c.owner_id AND m.role = 'owner' \
      UNION ALL \
        SELECT c.owner_id, true FROM chain c WHERE c.kind = 'user' AND c.step = 2) \
    SELECT u.id FROM forge.users u \
    WHERE u.state = 'active' AND EXISTS (SELECT 1 FROM owners o \
        WHERE o.user_id = u.id AND (NOT o.personal OR u.kind <> 'bot')) \
    ORDER BY u.id FOR SHARE OF u";

/// Refuse to leave the project without an active Owner once `leaving` stops
/// being a direct one (`FG-REPO-0010` `last_owner`). The remaining Owners'
/// user rows are locked `FOR SHARE`: a concurrent `users::disable` of one of
/// them either commits first, and the re-checked row no longer counts, or
/// waits until this change commits and then sees `leaving` gone.
async fn keep_an_owner(
    conn: &mut PgConnection,
    project: ProjectId,
    leaving: UserId,
) -> Result<(), ForgeError> {
    if !owners_remain(conn, project, leaving).await? {
        return Err(membership_conflict("last_owner").with_context("project_id", project.get()));
    }
    Ok(())
}

/// Whether an active Owner stays once `leaving` stops being a direct one
/// (the remaining Owners' user rows locked `FOR SHARE`, as
/// [`keep_an_owner`]).
pub(super) async fn owners_remain(
    conn: &mut PgConnection,
    project: ProjectId,
    leaving: UserId,
) -> Result<bool, ForgeError> {
    let remaining: Vec<i64> = sqlx::query_scalar(REMAINING_OWNERS)
        .bind(project.get())
        .bind(leaving.get())
        .bind(CAP)
        .fetch_all(conn)
        .await
        .map_err(|e| query_error("projects.owner_count", e))?;
    Ok(!remaining.is_empty())
}

/// Commit with one audit row on (`project`, id) and its `member.*` event.
/// The precondition of `change_member` / `remove_member`: the member's
/// direct role re-read under the lock must still be `from`, the role the
/// caller loaded and authorized against (`authorize()` with
/// `MembershipChange::{Change, Remove}`). Otherwise the membership changed
/// in between (e.g. a promotion to Owner) and the authorization no longer
/// holds: `FG-REPO-0010` `role_changed` with `expected` / `actual`.
pub(super) fn check_from(stored: Role, from: Role) -> Result<(), ForgeError> {
    if stored == from {
        return Ok(());
    }
    Err(membership_conflict("role_changed")
        .with_context("expected", from.as_str())
        .with_context("actual", stored.as_str()))
}

async fn commit(
    tx: ChangeTx,
    project: ProjectId,
    details: &AuditDetails,
) -> Result<(), ForgeError> {
    tx.commit(evented_record(details, project)?)
        .await?
        .require_recorded(details.action())
}

/// Make `user` a direct member with `role`: one `member.added` audit row and
/// event. `FG-REPO-0010` `already_member` / `bot_member`; `FG-REPO-0005`
/// for an unknown project or user. A disabled user may be added (they hold
/// no role while disabled). This command does not authorize: a route
/// decides with `authorize()` (`MembershipChange`) beforehand.
pub async fn add_member(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    user: UserId,
    role: Role,
) -> Result<(), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(|c| lock_project(c, project)).await?;
    tx.step(|c| member_candidate(c, user)).await?;
    if tx.step(|c| direct_role(c, project, user)).await?.is_some() {
        return Err(membership_conflict("already_member"));
    }
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.project_members (project_id, user_id, role) VALUES ($1, $2, $3)",
        )
        .bind(project.get())
        .bind(user.get())
        .bind(role.as_str())
        .execute(c)
        .await
        .map_err(|e| query_error("projects.add_member", e))
    })
    .await?;
    commit(
        tx,
        project,
        &AuditDetails::ProjectMemberAdded { user, role },
    )
    .await
}

/// Give a member the role `to`: one `member.role_changed` audit row and
/// event, or `Unchanged` (nothing written) when `to` is `from`. `from` is
/// the member's direct role as the caller loaded it before `authorize()`;
/// a different stored role is `FG-REPO-0010` `role_changed`.
/// Demoting the last direct Owner without an inherited active Owner is
/// `FG-REPO-0010` `last_owner`; a non-member is `not_member`.
pub async fn change_member(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    user: UserId,
    from: Role,
    to: Role,
) -> Result<MemberChange, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(|c| lock_project(c, project)).await?;
    let stored = tx
        .step(|c| direct_role(c, project, user))
        .await?
        .ok_or_else(|| membership_conflict("not_member"))?;
    check_from(stored, from)?;
    if from == to {
        // Nothing was written; release the lock now, not on drop.
        tx.rollback("member.role_changed").await;
        return Ok(MemberChange::Unchanged);
    }
    if from == Role::Owner {
        tx.step(|c| keep_an_owner(c, project, user)).await?;
    }
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.project_members SET role = $3, updated_at = now() \
             WHERE project_id = $1 AND user_id = $2",
        )
        .bind(project.get())
        .bind(user.get())
        .bind(to.as_str())
        .execute(c)
        .await
        .map_err(|e| query_error("projects.change_member", e))
    })
    .await?;
    let details = AuditDetails::ProjectMemberRoleChanged { user, from, to };
    commit(tx, project, &details).await?;
    Ok(MemberChange::Changed { from })
}

/// Remove a direct member whose direct role is `from` (as loaded before
/// `authorize()`; else `FG-REPO-0010` `role_changed`): one
/// `member.removed` audit row and event.
/// Removing the last direct Owner without an inherited active Owner is
/// `FG-REPO-0010` `last_owner`; a non-member is `not_member`.
pub async fn remove_member(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    user: UserId,
    from: Role,
) -> Result<(), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(|c| lock_project(c, project)).await?;
    let role = tx
        .step(|c| direct_role(c, project, user))
        .await?
        .ok_or_else(|| membership_conflict("not_member"))?;
    check_from(role, from)?;
    if role == Role::Owner {
        tx.step(|c| keep_an_owner(c, project, user)).await?;
    }
    tx.step(|c| async move {
        sqlx::query("DELETE FROM forge.project_members WHERE project_id = $1 AND user_id = $2")
            .bind(project.get())
            .bind(user.get())
            .execute(c)
            .await
            .map_err(|e| query_error("projects.remove_member", e))
    })
    .await?;
    commit(
        tx,
        project,
        &AuditDetails::ProjectMemberRemoved { user, role },
    )
    .await
}
