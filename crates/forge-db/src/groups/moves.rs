//! Moving or renaming a group, decided inside its transaction (D5 MR2,
//! ADR-0015 Amendment D5): `namespaces::move_subtree` on the group's
//! namespace and every descendant, one `group.moved` audit row and one
//! `project.moved` row per project below it (audit-only in MR2).

use forge_core::ids::{GroupId, NamespaceId, UserId};
use forge_core::AuditDetails;
use forge_errors::ForgeError;

use super::{not_found, GroupAccess, RESOURCE};
use crate::access::{lock_account, Account};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::namespaces::{
    lock_subtree, lock_target, move_subtree, Destination, MoveOptions, MoveOutcome, TargetAccess,
};
use crate::roles::{lock_roles, RoleAnchor};
use crate::{query_error, Db};

/// What [`move_decided`]'s `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupMoveFacts {
    pub account: Account,
    /// The group and the caller's native role on it (the source side).
    pub group: GroupAccess,
    /// The group's current parent (`None` = top level; a rename keeps it).
    pub source_parent: Option<NamespaceId>,
    /// The new parent and the caller's role there.
    pub target: TargetAccess,
}

/// Move the group (and its subtree) below `to.parent` (`None` = top level)
/// as `to.segment` for `viewer`, deciding inside the transaction: the group
/// row `FOR UPDATE`, its subtree's namespaces, the new parent `FOR SHARE`
/// with `viewer`'s roles there, `viewer`'s roles on the group, then the
/// account row; `decide` (forge-hosting: `Resource::GroupTransfer`) sees
/// those facts before anything is written. One `group.moved` audit row
/// (`AuditDetails::GroupMoved`), one `project.moved` row per project of the
/// subtree and one `moved` tombstone per node. Every exit that does not
/// commit rolls back explicitly.
///
/// Errors: `FG-REPO-0005` (group or parent absent), `FG-REPO-0015`,
/// `FG-REPO-0018`, `FG-REPO-0007` / `0008` / `0009`, and whatever `decide`
/// returns.
pub async fn move_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    viewer: UserId,
    to: &Destination,
    options: MoveOptions,
    decide: impl FnOnce(&GroupMoveFacts) -> Result<P, ForgeError>,
) -> Result<(MoveOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let namespace: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT n.id FROM forge.groups g \
                     JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
                     WHERE g.id = $1 FOR UPDATE OF g",
                )
                .bind(group.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("groups.move.lock", e))
            })
            .await?;
        let Some(namespace) = namespace.map(NamespaceId::new) else {
            return Err(not_found("group"));
        };
        // The target before the subtree (as projects::move_decided).
        let target = lock_target(&mut tx, to.parent, viewer).await?;
        let subtree = lock_subtree(&mut tx, namespace).await?;
        let roles = tx
            .step(|c| lock_roles(c, RoleAnchor::Group(group), viewer))
            .await?;
        let account = tx.step(|c| lock_account(c, viewer)).await?;
        let decided = decide(&GroupMoveFacts {
            account,
            group: GroupAccess {
                group,
                namespace,
                role: roles.native,
            },
            source_parent: subtree.root_parent(),
            target: target.access(),
        })?;
        let moved = move_subtree(&mut tx, ctx, &subtree, &target, &to.segment, options).await?;
        let details = AuditDetails::GroupMoved {
            group_id: group,
            summary: moved.outcome.summary.clone(), // clone: the outcome keeps it too
        };
        let record = moved.record(&details, RESOURCE, group.get());
        Ok(Settle::Commit(record, (moved.outcome, decided)))
    }
    .await;
    tx.settle("group.moved", outcome).await
}
