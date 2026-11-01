//! Renaming a user, decided inside its transaction (D5 MR2, ADR-0015
//! `UserRename`): the username and the personal namespace's path change
//! together (`namespaces::move_subtree` on the user's namespace and the
//! projects in it), with one `user.renamed` audit row and one
//! `project.moved` row per project (audit-only).

use forge_core::ids::{NamespaceId, UserId};
use forge_core::{AuditDetails, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use super::{conflict, not_found, RESOURCE};
use crate::access::{lock_account, Account};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::namespaces::{lock_subtree, lock_target, move_subtree, MoveOptions, MoveOutcome};
use crate::{query_error, Db};

/// What [`rename_decided`]'s `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserRenameFacts {
    /// The caller's account (`UserRename` is for administrators).
    pub account: Account,
    /// The user being renamed, as locked.
    pub target_user: UserId,
    pub kind: UserKind,
    pub active: bool,
}

/// Rename `user` to `username` for `viewer`: the user row is locked
/// `FOR UPDATE`, then the personal namespace and its projects, then the
/// caller's account row; `decide` (forge-hosting: `Resource::UserRename`)
/// sees those facts before anything is written. The namespace moves to
/// the top-level path `username` (a reserved word is `FG-REPO-0008`), the
/// username changes with it, one `user.renamed` audit row
/// (`AuditDetails::UserRenamed`), one `project.moved` row per project and
/// one `moved` tombstone per node. Every exit that does not commit rolls
/// back explicitly.
///
/// Errors: `FG-REPO-0005 resource=user`, `FG-REPO-0015` (`same_location`,
/// `subtree_too_large`, `path_too_long`), `FG-REPO-0018`, `FG-REPO-0007` /
/// `0008` / `0009`, and whatever `decide` returns.
pub async fn rename_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    viewer: UserId,
    username: &str,
    options: MoveOptions,
    decide: impl FnOnce(&UserRenameFacts) -> Result<P, ForgeError>,
) -> Result<(MoveOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<(String, String, Option<i64>)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT u.kind, u.state, n.id FROM forge.users u \
                     LEFT JOIN forge.namespaces n ON n.kind = 'user' AND n.owner_id = u.id \
                     WHERE u.id = $1 FOR UPDATE OF u",
                )
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("users.rename.lock", e))
            })
            .await?;
        let Some((kind, state, namespace)) = row else {
            return Err(not_found());
        };
        let kind = UserKind::parse(&kind).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unknown_user_kind")
                .with_context("table", "users")
        })?;
        let Some(namespace) = namespace.map(NamespaceId::new) else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "user_without_namespace")
                .with_context("user_id", user.get()));
        };
        let subtree = lock_subtree(&mut tx, namespace).await?;
        let target = lock_target(&mut tx, None, viewer).await?;
        let account = tx.step(|c| lock_account(c, viewer)).await?;
        let decided = decide(&UserRenameFacts {
            account,
            target_user: user,
            kind,
            active: state == "active",
        })?;
        let moved = move_subtree(&mut tx, ctx, &subtree, &target, username, options).await?;
        let new_name = moved.outcome.new_path.as_str();
        tx.step(|c| async move {
            sqlx::query("UPDATE forge.users SET username = $2, updated_at = now() WHERE id = $1")
                .bind(user.get())
                .bind(new_name)
                .execute(c)
                .await
                .map_err(|e| conflict("users.rename.username", e))
        })
        .await?;
        let details = AuditDetails::UserRenamed {
            user_id: user,
            summary: moved.outcome.summary.clone(), // clone: the outcome keeps it too
        };
        let record = moved.record(&details, RESOURCE, user.get());
        Ok(Settle::Commit(record, (moved.outcome, decided)))
    }
    .await;
    tx.settle("user.renamed", outcome).await
}
