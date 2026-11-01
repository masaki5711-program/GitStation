//! Project member changes decided inside their transaction (M1 phase 8b
//! MR8): [`add_member_decided`], [`change_member_decided`] and
//! [`remove_member_decided`] — the commands of `projects::members` with the
//! authorization taken where the write happens.
//!
//! Lock order (that of `projects::members` and `projects::update`): the
//! project row `FOR UPDATE` (with its visibility and deletion), the
//! caller's membership rows `FOR SHARE` (`roles::lock_roles`), the caller's
//! account row `FOR SHARE`, then — under the project lock, so no other
//! member change of the project runs in between — the member's **direct**
//! role (its row `FOR UPDATE`) and, when the change would take an Owner away, the remaining
//! Owners' user rows `FOR SHARE`. `decide` (forge-hosting:
//! `Authorized::reconfirm`) sees all of it: the member's current role (the
//! `from` of the decision, never the request's) and whether the change
//! would leave the project without an active Owner. Only then are the
//! conflicts (`already_member`, `not_member`, `bot_member`) and the write
//! done; every exit that does not commit rolls back explicitly
//! (`ChangeTx::settle`).

use forge_core::ids::{ProjectId, UserId};
use forge_core::{AuditDetails, Role};
use forge_errors::ForgeError;

use super::members::{
    direct_role_for_update, member_candidate, membership_conflict, owners_remain,
};
use super::update::{facts, lock};
use super::{evented_record, ProjectAccessFacts};
use crate::access::Account;
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::groups::MemberChange;
use crate::{query_error, Db};

/// What a member command's `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectMemberFacts {
    pub account: Account,
    /// The project and the caller's role on it, re-read under the locks.
    pub project: ProjectAccessFacts,
    /// The member's direct role in the project (`None` = not a direct
    /// member), read under the project lock.
    pub member_role: Option<Role>,
    /// The change would leave the project without an active Owner (direct
    /// or inherited; `projects::members` semantics).
    pub removes_last_owner: bool,
}

/// What is asked of the membership.
#[derive(Debug, Clone, Copy)]
enum Ask {
    Add(Role),
    Change(Role),
    Remove,
}

impl Ask {
    /// The change takes the Owner role away from a member who has it.
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

/// Lock and read everything `decide` needs (module docs).
async fn member_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    viewer: UserId,
    member: UserId,
    ask: Ask,
) -> Result<ProjectMemberFacts, ForgeError> {
    let locked = lock(tx, project).await?;
    let caller = facts(tx, project, viewer, &locked).await?;
    let member_role = tx
        .step(|c| direct_role_for_update(c, project, member))
        .await?;
    let removes_last_owner = if ask.drops_owner(member_role) {
        !tx.step(|c| owners_remain(c, project, member)).await?
    } else {
        false
    };
    Ok(ProjectMemberFacts {
        account: caller.account,
        project: caller.project,
        member_role,
        removes_last_owner,
    })
}

/// One command: facts → `decide` → `write` (which returns what to commit,
/// or `None` for nothing to write) → settle.
async fn run<T, P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, viewer, member): (ProjectId, UserId, UserId),
    ask: Ask,
    decide: impl FnOnce(&ProjectMemberFacts) -> Result<P, ForgeError>,
    unchanged: T,
    changed: impl FnOnce(Role) -> T,
) -> Result<(T, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = member_facts(&mut tx, project, viewer, member, ask).await?;
        let decided = decide(&facts)?;
        // The backstop of the decision: never commit an ownerless project,
        // whatever `decide` returned.
        if facts.removes_last_owner {
            return Err(membership_conflict("last_owner").with_context("project_id", project.get()));
        }
        let (details, value) = match (ask, facts.member_role) {
            (Ask::Add(_), Some(_)) => return Err(membership_conflict("already_member")),
            (Ask::Change(_) | Ask::Remove, None) => return Err(membership_conflict("not_member")),
            (Ask::Add(role), None) => {
                tx.step(|c| member_candidate(c, member)).await?;
                tx.step(|c| async move {
                    sqlx::query(
                        "INSERT INTO forge.project_members (project_id, user_id, role) \
                         VALUES ($1, $2, $3)",
                    )
                    .bind(project.get())
                    .bind(member.get())
                    .bind(role.as_str())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("projects.add_member", e))
                })
                .await?;
                (
                    AuditDetails::ProjectMemberAdded { user: member, role },
                    changed(role),
                )
            }
            (Ask::Change(to), Some(from)) => {
                if from == to {
                    return Ok(Settle::Discard((unchanged, decided)));
                }
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.project_members SET role = $3, updated_at = now() \
                         WHERE project_id = $1 AND user_id = $2",
                    )
                    .bind(project.get())
                    .bind(member.get())
                    .bind(to.as_str())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("projects.change_member", e))
                })
                .await?;
                (
                    AuditDetails::ProjectMemberRoleChanged {
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
                        "DELETE FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
                    )
                    .bind(project.get())
                    .bind(member.get())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("projects.remove_member", e))
                })
                .await?;
                (
                    AuditDetails::ProjectMemberRemoved { user: member, role },
                    changed(role),
                )
            }
        };
        Ok(Settle::Commit(
            evented_record(&details, project)?,
            (value, decided),
        ))
    }
    .await;
    tx.settle(ask.action(), outcome).await
}

/// [`super::add_member`] decided inside the transaction (module docs):
/// make `member` a direct member with `role`, one `member.added` audit row
/// and event. `decide` sees `member_role` (an existing member is then
/// `FG-REPO-0010 already_member`). Errors after the decision:
/// `FG-REPO-0010` `already_member` / `bot_member`, `FG-REPO-0005
/// resource=user`; before it `FG-REPO-0005 resource=project`; and whatever
/// `decide` returns.
pub async fn add_member_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    member: UserId,
    role: Role,
    decide: impl FnOnce(&ProjectMemberFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let ((), decided) = run(
        db,
        ctx,
        (project, viewer, member),
        Ask::Add(role),
        decide,
        (),
        |_| (),
    )
    .await?;
    Ok(decided)
}

/// [`super::change_member`] decided inside the transaction: give `member`
/// the direct role `to`; `from` is the direct role read under the lock
/// (`decide` sees it as `member_role`). One `member.role_changed` audit row
/// and event, or [`MemberChange::Unchanged`] (nothing written). Errors
/// after the decision: `FG-REPO-0010` `not_member` / `last_owner`.
pub async fn change_member_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    member: UserId,
    to: Role,
    decide: impl FnOnce(&ProjectMemberFacts) -> Result<P, ForgeError>,
) -> Result<(MemberChange, P), ForgeError> {
    run(
        db,
        ctx,
        (project, viewer, member),
        Ask::Change(to),
        decide,
        MemberChange::Unchanged,
        |from| MemberChange::Changed { from },
    )
    .await
}

/// [`super::remove_member`] decided inside the transaction: remove
/// `member`'s direct membership (its role read under the lock, `decide`
/// sees it as `member_role`); one `member.removed` audit row and event.
/// Errors after the decision: `FG-REPO-0010` `not_member` / `last_owner`.
pub async fn remove_member_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    member: UserId,
    decide: impl FnOnce(&ProjectMemberFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let ((), decided) = run(
        db,
        ctx,
        (project, viewer, member),
        Ask::Remove,
        decide,
        (),
        |_| (),
    )
    .await?;
    Ok(decided)
}
