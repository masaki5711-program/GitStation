//! Direct group memberships (step D2): add, change and remove one member.
//! Every command first locks the group row (`FOR UPDATE`), so membership
//! changes of one group run one at a time and "a top-level group keeps at
//! least one direct active Owner" is checked against the committed members.
//! A subgroup may be left without a direct Owner: its ancestors' Owners
//! inherit (user decision, 2026-10-29). Each change writes one `member.*`
//! audit row on the group; no event (Q8).

use forge_core::ids::{GroupId, UserId};
use forge_core::{AuditDetails, Role};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{member_candidate, membership_conflict, not_found, RESOURCE};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx};
use crate::{query_error, Db};

/// What `change_member` did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberChange {
    /// The role changed from `from`: one `member.role_changed` audit row.
    Changed { from: Role },
    /// The member already had the role: nothing was written.
    Unchanged,
}

/// Lock the group row; whether it is a top-level group (its namespace has
/// no parent). `FG-REPO-0005` (`resource=group`) when it does not exist; a
/// group without its namespace is a broken invariant (`FG-SYS-0001`
/// `group_without_namespace`), not a 404.
async fn lock_group(conn: &mut PgConnection, group: GroupId) -> Result<bool, ForgeError> {
    // (has a namespace, the namespace is top-level)
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT n.id IS NOT NULL, n.parent_id IS NULL FROM forge.groups g \
         LEFT JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
         WHERE g.id = $1 FOR UPDATE OF g",
    )
    .bind(group.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("groups.lock", e))?;
    match row {
        None => Err(not_found("group")),
        Some((false, _)) => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "group_without_namespace")
            .with_context("group_id", group.get())),
        Some((true, top_level)) => Ok(top_level),
    }
}

/// The user's direct role in the group, `None` = not a member.
async fn direct_role(
    conn: &mut PgConnection,
    group: GroupId,
    user: UserId,
) -> Result<Option<Role>, ForgeError> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM forge.group_members WHERE group_id = $1 AND user_id = $2",
    )
    .bind(group.get())
    .bind(user.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("groups.member_role", e))?;
    role.map(|r| {
        Role::parse(&r).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unknown_role")
                .with_context("table", "group_members")
        })
    })
    .transpose()
}

/// Refuse to leave a top-level group without a direct active Owner once
/// `leaving` stops being one (`FG-REPO-0010` `last_owner`). The other
/// owners' user rows are locked `FOR SHARE` (in id order): a concurrent
/// `users::disable` of one of them either commits first, and the re-checked
/// row no longer counts, or waits until this change commits. Disabling the
/// sole Owner on its own is not refused yet (step D3).
async fn keep_an_owner(
    conn: &mut PgConnection,
    group: GroupId,
    leaving: UserId,
) -> Result<(), ForgeError> {
    let others: Vec<i64> = sqlx::query_scalar(
        "SELECT u.id FROM forge.group_members m JOIN forge.users u ON u.id = m.user_id \
         WHERE m.group_id = $1 AND m.role = 'owner' AND u.state = 'active' AND m.user_id <> $2 \
         ORDER BY u.id FOR SHARE OF u",
    )
    .bind(group.get())
    .bind(leaving.get())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("groups.owner_count", e))?;
    if others.is_empty() {
        return Err(membership_conflict("last_owner").with_context("group_id", group.get()));
    }
    Ok(())
}

async fn commit(tx: ChangeTx, group: GroupId, details: &AuditDetails) -> Result<(), ForgeError> {
    tx.commit(ChangeRecord::audit_only(details, RESOURCE, group.get()))
        .await?
        .require_recorded(details.action())
}

/// Make `user` a direct member with `role`: one `member.added` audit row.
/// `FG-REPO-0010` `already_member` / `bot_member`; `FG-REPO-0005` for an
/// unknown group or user. A disabled user may be added (they hold no role
/// while disabled).
pub async fn add_member(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    user: UserId,
    role: Role,
) -> Result<(), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(|c| lock_group(c, group)).await?;
    // A disabled user may be added: they hold no role while disabled (the role
    // loader counts active users only), and keep it if re-enabled.
    let _active = tx.step(|c| member_candidate(c, user)).await?;
    if tx.step(|c| direct_role(c, group, user)).await?.is_some() {
        return Err(membership_conflict("already_member"));
    }
    tx.step(|c| async move {
        sqlx::query("INSERT INTO forge.group_members (group_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(group.get())
            .bind(user.get())
            .bind(role.as_str())
            .execute(c)
            .await
            .map_err(|e| query_error("groups.add_member", e))
    })
    .await?;
    commit(tx, group, &AuditDetails::MemberAdded { user, role }).await
}

/// Give a member another role: one `member.role_changed` audit row, or
/// `Unchanged` (nothing written) when the role is the same. Demoting the
/// last direct active Owner of a top-level group is `FG-REPO-0010`
/// `last_owner`; a non-member is `not_member`.
pub async fn change_member(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    user: UserId,
    role: Role,
) -> Result<MemberChange, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let top_level = tx.step(|c| lock_group(c, group)).await?;
    let from = tx
        .step(|c| direct_role(c, group, user))
        .await?
        .ok_or_else(|| membership_conflict("not_member"))?;
    if from == role {
        // Dropping the transaction releases the lock; nothing was written.
        return Ok(MemberChange::Unchanged);
    }
    if top_level && from == Role::Owner {
        tx.step(|c| keep_an_owner(c, group, user)).await?;
    }
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.group_members SET role = $3, updated_at = now() \
             WHERE group_id = $1 AND user_id = $2",
        )
        .bind(group.get())
        .bind(user.get())
        .bind(role.as_str())
        .execute(c)
        .await
        .map_err(|e| query_error("groups.change_member", e))
    })
    .await?;
    let details = AuditDetails::MemberRoleChanged {
        user,
        from,
        to: role,
    };
    commit(tx, group, &details).await?;
    Ok(MemberChange::Changed { from })
}

/// Remove a direct member: one `member.removed` audit row. Removing the
/// last direct active Owner of a top-level group is `FG-REPO-0010`
/// `last_owner`; a non-member is `not_member`.
pub async fn remove_member(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    user: UserId,
) -> Result<(), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let top_level = tx.step(|c| lock_group(c, group)).await?;
    let role = tx
        .step(|c| direct_role(c, group, user))
        .await?
        .ok_or_else(|| membership_conflict("not_member"))?;
    if top_level && role == Role::Owner {
        tx.step(|c| keep_an_owner(c, group, user)).await?;
    }
    tx.step(|c| async move {
        sqlx::query("DELETE FROM forge.group_members WHERE group_id = $1 AND user_id = $2")
            .bind(group.get())
            .bind(user.get())
            .execute(c)
            .await
            .map_err(|e| query_error("groups.remove_member", e))
    })
    .await?;
    commit(tx, group, &AuditDetails::MemberRemoved { user, role }).await
}
