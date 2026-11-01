//! D5 MR5: the namespace moves of the API — a project's move or rename
//! (`ProjectTransfer`), a group's (`GroupTransfer`), a user's rename
//! (`UserRename`, administrators) and a tombstone's release
//! (`NamespaceTombstoneRelease`) — on forge-db's D5 MR2 commands
//! (`projects::move_decided`, `groups::move_decided`,
//! `users::rename_decided`, `tombstones::release_decided`).
//!
//! As every command of this crate: the registry proved the action on what
//! its loader read; the command's transaction locks the resource, the new
//! parent and the caller's roles, and `decide` calls
//! [`forge_auth::Authorized::reconfirm`] on those facts before anything is
//! written. The proof's id binds the destination: a proof for one parent
//! never moves the resource below another (`reconfirm_target_mismatch`).
//!
//! **A rename keeps the parent**: when the locked parent is the one the
//! move names, the target is `LoadedTransferTarget::CurrentParent` and
//! `authorize()` decides it (ADR-0015 "名前だけの変更は、移動先 = 今の親と
//! して扱う"; D5 MR5 review: the rule lives in forge-auth, not here). If the
//! resource moved between the registry's check and the lock, the target is
//! built from the locked parent and decided on it.
//!
//! **Retry**: a move whose transaction lost an unavoidable deadlock or found
//! its subtree changing while it locked it (`FG-DB-0002 retryable=true`:
//! rolled back, nothing written) is run again, at most [`MOVE_ATTEMPTS`]
//! times in all, each attempt decided again from the start; after that the
//! request fails with `FG-REPO-0022` NamespaceMoveContended (503, warn).

use forge_auth::{
    Authorized, Credential, GroupTransferCtx, LoadedGroup, LoadedTombstone, LoadedTransferTarget,
    LoadedUserNamespace, TombstoneCtx, TombstoneRelease, TransferCtx, UserRenameCtx,
};
use forge_core::{Scope, TombstoneId, UserId};
use forge_db::namespaces::{Destination, MoveOptions, MoveOutcome, TargetAccess};
use forge_db::tombstones::{self, TombstoneAccess};
use forge_db::{groups, projects, users};
use forge_errors::{ErrorCode, ForgeError};

use super::{account, caller, change_context, loaded, require, Hosting, WriteRequest};

pub use forge_db::tombstones::Released;

/// How often one move request runs its transaction at most (the first try
/// and the retries after a retryable conflict).
pub const MOVE_ATTEMPTS: u32 = 3;

/// Where a move goes and how (the body of a transfer route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRequest {
    /// The new last segment (a rename; unchanged = the current one).
    pub segment: String,
    /// A→B→A: release, in the same change, the `moved` tombstones at the
    /// new paths that name the very namespaces moving there.
    pub reclaim: bool,
}

/// The new target as the transaction locked it: the resource's current
/// parent (`rename`: the locked parent is `parent`) is `CurrentParent`, for
/// `authorize()` to decide; any other parent is described as it is.
fn target_of(
    target: TargetAccess,
    rename: bool,
    parent: Option<forge_core::NamespaceId>,
) -> LoadedTransferTarget {
    if rename {
        return LoadedTransferTarget::CurrentParent { namespace: parent };
    }
    match target {
        TargetAccess::TopLevel => LoadedTransferTarget::TopLevel,
        TargetAccess::Group {
            namespace,
            group,
            role,
        } => LoadedTransferTarget::Group {
            namespace,
            group: LoadedGroup { id: group, role },
        },
        TargetAccess::PersonalNamespace { namespace, owner } => {
            LoadedTransferTarget::User(LoadedUserNamespace { namespace, owner })
        }
        // Never a parent: the loader answered it as not visible, so no
        // proof names it (and the id check of `reconfirm` refuses it).
        TargetAccess::Project { .. } => LoadedTransferTarget::NotVisible,
    }
}

/// `FG-DB-0002 retryable=true`: rolled back, safe to run again.
fn retryable(err: &ForgeError) -> bool {
    err.code() == ErrorCode::DbQueryFailed
        && err
            .context()
            .iter()
            .any(|(k, v)| *k == "retryable" && v == "true")
}

/// Run `attempt` until it succeeds, fails for good, or [`MOVE_ATTEMPTS`]
/// retryable conflicts happened (`FG-REPO-0022`, the last conflict's
/// `reason` and the attempts in the context, the conflict as the source).
async fn with_retry<T, F, Fut>(command: &'static str, mut attempt: F) -> Result<T, ForgeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ForgeError>>,
{
    let mut tries = 0;
    loop {
        tries += 1;
        match attempt().await {
            Err(e) if retryable(&e) && tries < MOVE_ATTEMPTS => {
                tracing::debug!(
                    target: "forge_hosting",
                    command,
                    attempt = tries,
                    "namespace move retried after a retryable conflict"
                );
            }
            Err(e) if retryable(&e) => {
                let reason = e
                    .context()
                    .iter()
                    .find(|(k, _)| *k == "reason")
                    .map_or_else(String::new, |(_, v)| v.clone());
                return Err(ForgeError::new(ErrorCode::NamespaceMoveContended)
                    .with_context("command", command)
                    .with_context("attempts", tries)
                    .with_context("last_reason", reason)
                    .with_source(e));
            }
            other => return other,
        }
    }
}

impl Hosting {
    // ── D5 MR5: moves, renames, tombstone releases ──

    /// `POST /api/v1/projects/{id}/transfer` (`ProjectTransfer`): move the
    /// project below the proof's target parent as `to.segment` (a rename
    /// when that is its current parent). Decided again on the locked facts
    /// (module docs). One `project.moved` audit row and event, one `moved`
    /// tombstone at the old path.
    pub async fn move_project(
        &self,
        request: &WriteRequest,
        proof: &Authorized<TransferCtx>,
        to: &MoveRequest,
    ) -> Result<MoveOutcome, ForgeError> {
        require(proof.action(), forge_auth::Action::ProjectTransfer)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (project, parent) = *proof.id();
        let parent = parent.ok_or_else(|| {
            // A project's proof always names a parent (TopLevel is denied).
            ForgeError::new(ErrorCode::Internal).with_context("reason", "transfer_without_parent")
        })?;
        let destination = Destination {
            parent: Some(parent),
            segment: to.segment.clone(), // clone: forge-db owns its request
        };
        let options = MoveOptions {
            reclaim: to.reclaim,
            ..MoveOptions::default()
        };
        with_retry("project_move", || async {
            let (moved, ()) = projects::move_decided(
                &self.db,
                &ctx,
                project,
                user,
                &destination,
                options,
                |facts| {
                    let rename = facts.source_parent == Some(parent);
                    let target = target_of(facts.target, rename, Some(parent));
                    let fresh = TransferCtx::new(loaded(&facts.project), target);
                    proof.reconfirm(account(facts.account), fresh).map(drop)
                },
            )
            .await?;
            Ok(moved)
        })
        .await
    }

    /// `POST /api/v1/groups/{id}/transfer` (`GroupTransfer`): move the group
    /// and its subtree below the proof's target (`None` = the top level) as
    /// `to.segment`. One `group.moved` audit row and event, one
    /// `project.moved` audit row per project of the subtree, one `moved`
    /// tombstone per node.
    pub async fn move_group(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupTransferCtx>,
        to: &MoveRequest,
    ) -> Result<MoveOutcome, ForgeError> {
        require(proof.action(), forge_auth::Action::GroupTransfer)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (group, parent) = *proof.id();
        let destination = Destination {
            parent,
            segment: to.segment.clone(), // clone: forge-db owns its request
        };
        let options = MoveOptions {
            reclaim: to.reclaim,
            ..MoveOptions::default()
        };
        with_retry("group_move", || async {
            let (moved, ()) = groups::move_decided(
                &self.db,
                &ctx,
                group,
                user,
                &destination,
                options,
                |facts| {
                    let rename = facts.source_parent == parent;
                    let target = target_of(facts.target, rename, parent);
                    let fresh = GroupTransferCtx::new(
                        LoadedGroup {
                            id: facts.group.group,
                            role: facts.group.role,
                        },
                        target,
                    );
                    proof.reconfirm(account(facts.account), fresh).map(drop)
                },
            )
            .await?;
            Ok(moved)
        })
        .await
    }

    /// `POST /api/v1/users/{id}/rename` (`UserRename`, administrators): the
    /// username and the personal namespace's path become `to.segment`. One
    /// `user.renamed` audit row and event.
    pub async fn rename_user(
        &self,
        request: &WriteRequest,
        proof: &Authorized<UserRenameCtx>,
        to: &MoveRequest,
    ) -> Result<MoveOutcome, ForgeError> {
        require(proof.action(), forge_auth::Action::UserRename)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let target: UserId = *proof.id();
        let options = MoveOptions {
            reclaim: to.reclaim,
            ..MoveOptions::default()
        };
        with_retry("user_rename", || async {
            let (moved, ()) = users::rename_decided(
                &self.db,
                &ctx,
                target,
                user,
                &to.segment,
                options,
                |facts| {
                    let fresh = UserRenameCtx::new(facts.target_user);
                    proof.reconfirm(account(facts.account), fresh).map(drop)
                },
            )
            .await?;
            Ok(moved)
        })
        .await
    }

    /// The tombstone at exactly `path` and what its release would be
    /// decided on for `viewer` (`is_admin`: the caller's administrator rights,
    /// `Principal::admin_rights`), for the
    /// release route's loader; `None` when no tombstone has that path.
    pub async fn tombstone_at(
        &self,
        path: &str,
        viewer: UserId,
        is_admin: bool,
    ) -> Result<Option<LoadedTombstone>, ForgeError> {
        Ok(tombstones::load_by_path(&self.db, path, viewer)
            .await?
            .map(|(id, access)| LoadedTombstone {
                id,
                release: release_of(access, is_admin),
            }))
    }

    /// `POST /api/v1/namespaces/tombstones/release`
    /// (`NamespaceTombstoneRelease`): the path can be used again. One
    /// `namespace_tombstone.released` audit row (audit-only).
    pub async fn release_tombstone(
        &self,
        request: &WriteRequest,
        proof: &Authorized<TombstoneCtx>,
    ) -> Result<Released, ForgeError> {
        require(
            proof.action(),
            forge_auth::Action::NamespaceTombstoneRelease,
        )?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let id: TombstoneId = *proof.id();
        // Administrator rights need the `admin` scope on a token (as
        // `Principal::admin_rights`), on the account flag re-read here.
        let admin_scope = match proof.principal().credential() {
            Credential::Session => true,
            Credential::Token(scopes) => scopes.contains(&Scope::Admin),
        };
        with_retry("tombstone_release", || async {
            let (released, ()) = tombstones::release_decided(&self.db, &ctx, id, user, |facts| {
                let fresh = TombstoneCtx::new(LoadedTombstone {
                    id: facts.tombstone,
                    release: release_of(facts.access, facts.account.is_admin && admin_scope),
                });
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
            Ok(released)
        })
        .await
    }
}

/// forge-auth's release facts. A `moved` tombstone's target counts as
/// visible to its role holders and to administrators only: a caller with
/// no role on the target gets the same 404 as for no tombstone (they could
/// not release it anyway; for an internal project or a group, whose
/// existence they may know, that is a 404 where a 403 would also be true —
/// the conservative answer, never more than they may know).
fn release_of(access: TombstoneAccess, is_admin: bool) -> TombstoneRelease {
    match access {
        TombstoneAccess::Moved { target_role, .. } => TombstoneRelease::Moved {
            target_owner_role: target_role,
            target_visible: target_role.is_some() || is_admin,
        },
        TombstoneAccess::Deleted {
            recorded_owner,
            caller_parent_role,
            has_parent,
        } => TombstoneRelease::Deleted {
            recorded_owner,
            caller_parent_role,
            has_parent,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn conflict(reason: &'static str) -> ForgeError {
        ForgeError::new(ErrorCode::DbQueryFailed)
            .with_context("reason", reason)
            .with_context("retryable", true)
    }

    /// Every attempt lost a retryable conflict: `MOVE_ATTEMPTS` tries, then
    /// FG-REPO-0022 (503) naming the last reason.
    #[tokio::test]
    async fn a_move_that_keeps_losing_conflicts_is_namespace_move_contended() {
        let tries = Cell::new(0);
        let err = with_retry("project_move", || {
            tries.set(tries.get() + 1);
            async { Err::<(), _>(conflict("deadlock")) }
        })
        .await
        .expect_err("contended");
        assert_eq!(tries.get(), MOVE_ATTEMPTS);
        assert_eq!(err.code(), ErrorCode::NamespaceMoveContended);
        assert_eq!(err.code().http_status(), 503);
        let ctx = err.context();
        assert!(
            ctx.contains(&("last_reason", "deadlock".to_owned())),
            "{ctx:?}"
        );
        assert!(
            ctx.contains(&("attempts", MOVE_ATTEMPTS.to_string())),
            "{ctx:?}"
        );
    }

    /// A conflict then a success: the result of the second attempt. Any
    /// other failure is returned at once, never retried.
    #[tokio::test]
    async fn a_retryable_conflict_is_retried_and_other_failures_are_not() {
        let tries = Cell::new(0);
        let ok = with_retry("group_move", || {
            tries.set(tries.get() + 1);
            let first = tries.get() == 1;
            async move {
                if first {
                    Err(conflict("subtree_unstable"))
                } else {
                    Ok(7)
                }
            }
        })
        .await
        .expect("second attempt");
        assert_eq!((ok, tries.get()), (7, 2));
        tries.set(0);
        let err = with_retry("group_move", || {
            tries.set(tries.get() + 1);
            async { Err::<(), _>(ForgeError::new(ErrorCode::RenameRateLimited)) }
        })
        .await
        .expect_err("not retried");
        assert_eq!((err.code(), tries.get()), (ErrorCode::RenameRateLimited, 1));
        // FG-DB-0002 without retryable=true (a constraint, a timeout) is not
        // retried either.
        tries.set(0);
        let err = with_retry("group_move", || {
            tries.set(tries.get() + 1);
            async {
                Err::<(), _>(
                    ForgeError::new(ErrorCode::DbQueryFailed).with_context("reason", "constraint"),
                )
            }
        })
        .await
        .expect_err("not retried");
        assert_eq!((err.code(), tries.get()), (ErrorCode::DbQueryFailed, 1));
    }
}
