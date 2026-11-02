//! Moving or renaming a project, decided inside its transaction (D5 MR2,
//! ADR-0015 Amendment D5): `namespaces::move_subtree` on the project's
//! namespace, one `project.moved` audit row and (D5 MR5) one `project.moved`
//! event (integration class, dedup `project_moved:<project_id>:<µs epoch>`).

use forge_core::ids::{NamespaceId, ProjectId, UserId};
use forge_core::AuditDetails;
use forge_errors::ForgeError;

use super::update::{facts, lock};
use super::{ProjectAccessFacts, RESOURCE};
use crate::access::Account;
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::namespaces::{
    lock_subtree, lock_target, move_subtree, Destination, MoveOptions, MoveOutcome, TargetAccess,
};
use crate::{query_error, Db};

/// The prefix of the `project.moved` event's dedup key,
/// `project_moved:<project_id>:<µs epoch>` (D5 MR5).
pub const MOVED_DEDUP_PREFIX: &str = "project_moved";

/// What [`move_decided`]'s `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectMoveFacts {
    pub account: Account,
    /// The project and the caller's native role on it (the source side).
    pub project: ProjectAccessFacts,
    /// The project's current parent (a rename keeps it).
    pub source_parent: Option<NamespaceId>,
    /// The new parent and the caller's role there.
    pub target: TargetAccess,
}

/// Move the project below `to.parent` as `to.segment` (a rename when the
/// parent is unchanged) for `viewer`, deciding inside the transaction: the
/// project row is locked `FOR UPDATE`, then its namespace, then the new
/// parent `FOR SHARE` with `viewer`'s roles there, then `viewer`'s roles on
/// the project and the account row; `decide` (forge-hosting:
/// `Authorized::reconfirm` with `Resource::Transfer`) sees those facts
/// before anything is written. One `project.moved` audit row on the
/// project (`AuditDetails::ProjectMoved`) with its `project.moved` event,
/// and one `moved` tombstone at the
/// old path. Every exit that does not commit rolls back explicitly.
///
/// Errors: `FG-REPO-0005` (project or parent absent), `FG-REPO-0015`,
/// `FG-REPO-0018`, `FG-REPO-0007` / `0009` (the new path), and whatever
/// `decide` returns.
pub async fn move_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    to: &Destination,
    options: MoveOptions,
    decide: impl FnOnce(&ProjectMoveFacts) -> Result<P, ForgeError>,
) -> Result<(MoveOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock(&mut tx, project).await?;
        let namespace: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT id FROM forge.namespaces WHERE kind = 'project' AND owner_id = $1",
                )
                .bind(project.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("projects.move.namespace", e))
            })
            .await?;
        // The target before the subtree (D5 review): a project moving into
        // a group while that group's ancestor moves waits on the target
        // without holding its own namespace, so the two never deadlock.
        let target = lock_target(&mut tx, to.parent, viewer).await?;
        let subtree = lock_subtree(&mut tx, NamespaceId::new(namespace)).await?;
        let seen = facts(&mut tx, project, viewer, &locked).await?;
        let decided = decide(&ProjectMoveFacts {
            account: seen.account,
            project: seen.project,
            source_parent: subtree.root_parent(),
            target: target.access(),
        })?;
        let moved = move_subtree(&mut tx, ctx, &subtree, &target, &to.segment, options).await?;
        let details = AuditDetails::ProjectMoved {
            project_id: project,
            summary: moved.outcome.summary.clone(), // clone: the outcome keeps it too
        };
        let record = moved
            .evented_record(
                &mut tx,
                &details,
                RESOURCE,
                project.get(),
                MOVED_DEDUP_PREFIX,
            )
            .await?;
        Ok(Settle::Commit(record, (moved.outcome, decided)))
    }
    .await;
    tx.settle("project.moved", outcome).await
}
