//! Group member changes decided inside their transaction (M1 phase 8b
//! MR8): [`add_member_decided`], [`change_member_decided`] and
//! [`remove_member_decided`] — the commands of `groups::members` with the
//! authorization taken where the write happens.
//!
//! Lock order (that of `groups::members` and `groups::update_decided`): the
//! group row `FOR UPDATE`, the caller's membership rows in the group's
//! chain `FOR SHARE` (`roles::lock_roles`), the caller's account row `FOR
//! SHARE`, then — under the group lock — the member's **direct** role (its
//! row `FOR UPDATE`) and,
//! when the change would take the Owner role from a member of a top-level
//! group, the other direct active Owners' user rows `FOR SHARE`. `decide`
//! (forge-hosting: `Authorized::reconfirm`) sees the member's current role
//! (the `from` of the decision, never the request's) and whether the
//! change would leave a top-level group without an Owner (a subgroup may
//! rely on its ancestors' Owners). Every exit that does not commit rolls
//! back explicitly (`ChangeTx::settle`). Group member changes stay
//! audit-only (no event, step D plan Q8).

use forge_core::ids::{GroupId, UserId};
use forge_core::{AuditDetails, Role};
use forge_errors::ForgeError;

use super::members::{direct_role_for_update, lock_group, owners_remain};
use super::{member_candidate, membership_conflict, MemberChange, RESOURCE};
use crate::access::{lock_account, Account};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::roles::{lock_roles, RoleAnchor};
use crate::{query_error, Db};

/// What a group member command's `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupMemberFacts {
    pub account: Account,
    pub group: GroupId,
    /// The caller's effective role on the group, re-read under the locks.
    pub role: Option<Role>,
    /// The member's direct role in the group (`None` = not a direct
    /// member), read under the group lock.
    pub member_role: Option<Role>,
    /// The change would leave a top-level group without a direct active
    /// Owner.
    pub removes_last_owner: bool,
}

#[derive(Debug, Clone, Copy)]
enum Ask {
    Add(Role),
    Change(Role),
    Remove,
}

impl Ask {
    fn drops_owner(self, member_role: Option<Role>) -> bool {
        member_role == Some(Role::Owner)
            && match self {
                Ask::Add(_) => false,
                Ask::Change(to) => to != Role::Owner,
                Ask::Remove => true,
            }
    }

    fn action(self) -> &'static str {
        match self {
            Ask::Add(_) => "member.added",
            Ask::Change(_) => "member.role_changed",
            Ask::Remove => "member.removed",
        }
    }
}

async fn member_facts(
    tx: &mut ChangeTx,
    group: GroupId,
    viewer: UserId,
    member: UserId,
    ask: Ask,
) -> Result<GroupMemberFacts, ForgeError> {
    let top_level = tx.step(|c| lock_group(c, group)).await?;
    let roles = tx
        .step(|c| lock_roles(c, RoleAnchor::Group(group), viewer))
        .await?;
    let account = tx.step(|c| lock_account(c, viewer)).await?;
    let member_role = tx
        .step(|c| direct_role_for_update(c, group, member))
        .await?;
    let removes_last_owner = if top_level && ask.drops_owner(member_role) {
        !tx.step(|c| owners_remain(c, group, member)).await?
    } else {
        false
    };
    Ok(GroupMemberFacts {
        account,
        group,
        role: roles.native,
        member_role,
        removes_last_owner,
    })
}

async fn run<T, P>(
    db: &Db,
    ctx: &ChangeContext,
    (group, viewer, member): (GroupId, UserId, UserId),
    ask: Ask,
    decide: impl FnOnce(&GroupMemberFacts) -> Result<P, ForgeError>,
    unchanged: T,
    changed: impl FnOnce(Role) -> T,
) -> Result<(T, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = member_facts(&mut tx, group, viewer, member, ask).await?;
        let decided = decide(&facts)?;
        // The backstop of the decision: never commit an ownerless
        // top-level group, whatever `decide` returned.
        if facts.removes_last_owner {
            return Err(membership_conflict("last_owner").with_context("group_id", group.get()));
        }
        let (details, value) = match (ask, facts.member_role) {
            (Ask::Add(_), Some(_)) => return Err(membership_conflict("already_member")),
            (Ask::Change(_) | Ask::Remove, None) => return Err(membership_conflict("not_member")),
            (Ask::Add(role), None) => {
                // A disabled user may be added (they hold no role while
                // disabled), as in `groups::add_member`.
                let _active = tx.step(|c| member_candidate(c, member)).await?;
                tx.step(|c| async move {
                    sqlx::query(
                        "INSERT INTO forge.group_members (group_id, user_id, role) \
                         VALUES ($1, $2, $3)",
                    )
                    .bind(group.get())
                    .bind(member.get())
                    .bind(role.as_str())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("groups.add_member", e))
                })
                .await?;
                (
                    AuditDetails::MemberAdded { user: member, role },
                    changed(role),
                )
            }
            (Ask::Change(to), Some(from)) => {
                if from == to {
                    return Ok(Settle::Discard((unchanged, decided)));
                }
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.group_members SET role = $3, updated_at = now() \
                         WHERE group_id = $1 AND user_id = $2",
                    )
                    .bind(group.get())
                    .bind(member.get())
                    .bind(to.as_str())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("groups.change_member", e))
                })
                .await?;
                (
                    AuditDetails::MemberRoleChanged {
                        user: member,
                        from,
                        to,
                    },
                    changed(from),
                )
            }
            (Ask::Remove, Some(role)) => {
                tx.step(|c| async move {
                    sqlx::query(
                        "DELETE FROM forge.group_members WHERE group_id = $1 AND user_id = $2",
                    )
                    .bind(group.get())
                    .bind(member.get())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("groups.remove_member", e))
                })
                .await?;
                (
                    AuditDetails::MemberRemoved { user: member, role },
                    changed(role),
                )
            }
        };
        Ok(Settle::Commit(
            ChangeRecord::audit_only(&details, RESOURCE, group.get()),
            (value, decided),
        ))
    }
    .await;
    tx.settle(ask.action(), outcome).await
}

/// [`super::add_member`] decided inside the transaction (module docs).
/// Errors after the decision: `FG-REPO-0010` `already_member` /
/// `bot_member`, `FG-REPO-0005 resource=user`; before it `FG-REPO-0005
/// resource=group`; and whatever `decide` returns.
pub async fn add_member_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    viewer: UserId,
    member: UserId,
    role: Role,
    decide: impl FnOnce(&GroupMemberFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let ((), decided) = run(
        db,
        ctx,
        (group, viewer, member),
        Ask::Add(role),
        decide,
        (),
        |_| (),
    )
    .await?;
    Ok(decided)
}

/// [`super::change_member`] decided inside the transaction; `from` is the
/// direct role read under the lock. [`MemberChange::Unchanged`] writes
/// nothing. Errors after the decision: `FG-REPO-0010` `not_member` /
/// `last_owner`.
pub async fn change_member_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    viewer: UserId,
    member: UserId,
    to: Role,
    decide: impl FnOnce(&GroupMemberFacts) -> Result<P, ForgeError>,
) -> Result<(MemberChange, P), ForgeError> {
    run(
        db,
        ctx,
        (group, viewer, member),
        Ask::Change(to),
        decide,
        MemberChange::Unchanged,
        |from| MemberChange::Changed { from },
    )
    .await
}

/// [`super::remove_member`] decided inside the transaction. Errors after
/// the decision: `FG-REPO-0010` `not_member` / `last_owner`.
pub async fn remove_member_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    viewer: UserId,
    member: UserId,
    decide: impl FnOnce(&GroupMemberFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let ((), decided) = run(
        db,
        ctx,
        (group, viewer, member),
        Ask::Remove,
        decide,
        (),
        |_| (),
    )
    .await?;
    Ok(decided)
}
