//! The operator CLI's namespace moves (step D5 MR5; `forge admin project
//! move`, `forge admin group move`, `forge admin user rename`): the host
//! operator is the trust root, outside `authorize()` like the other mutating
//! operator commands (ADR-0019 §8), in production an approved runbook step
//! (runbook namespace-redirects). They run the same forge-db move as the
//! API — every subtree rule, the path checks, the Owner invariant, the
//! tombstones, the audit rows and the move event — as actor `system` and
//! **exempt from the rate limit** (`RateLimit::Exempt`, step D5 plan: only
//! the operator CLI). They live here because only forge-hosting may call
//! forge-db's `*_decided` commands (test `decided_callers`): the decision is
//! "the operator ran it", recorded by the change context's actor.
//!
//! The commands lock the caller's roles on the new parent; the operator is
//! no user, so they lock those of [`NO_USER`] (user ids start at 1: no rows)
//! and the facts are not used.

use forge_core::{GroupId, ProjectId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::namespaces::{Destination, MoveOptions, MoveOutcome, RateLimit};
use forge_db::{groups, projects, users};
use forge_errors::{ErrorCode, ForgeError};

use super::Hosting;

/// The viewer the operator's moves lock roles for: nobody (no user has id
/// 0), so no membership row is locked.
const NO_USER: UserId = UserId::new(0);

/// `FG-SYS-0001 reason=operator_not_system` unless the change is the
/// operator's (actor `system`): the rate limit exemption is for the CLI only.
fn operator(ctx: &ChangeContext) -> Result<MoveOptions, ForgeError> {
    if ctx.actor != Actor::System {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "operator_not_system")
        );
    }
    Ok(MoveOptions {
        reclaim: false,
        rate_limit: RateLimit::Exempt,
    })
}

impl Hosting {
    /// `forge admin project move`: the project below `to.parent` as
    /// `to.segment`.
    pub async fn operator_move_project(
        &self,
        ctx: &ChangeContext,
        project: ProjectId,
        to: &Destination,
        reclaim: bool,
    ) -> Result<MoveOutcome, ForgeError> {
        let options = MoveOptions {
            reclaim,
            ..operator(ctx)?
        };
        let (moved, ()) =
            projects::move_decided(&self.db, ctx, project, NO_USER, to, options, |_| Ok(()))
                .await?;
        Ok(moved)
    }

    /// `forge admin group move`: the group below `to.parent` (`None` = the
    /// top level) as `to.segment`.
    pub async fn operator_move_group(
        &self,
        ctx: &ChangeContext,
        group: GroupId,
        to: &Destination,
        reclaim: bool,
    ) -> Result<MoveOutcome, ForgeError> {
        let options = MoveOptions {
            reclaim,
            ..operator(ctx)?
        };
        let (moved, ()) =
            groups::move_decided(&self.db, ctx, group, NO_USER, to, options, |_| Ok(())).await?;
        Ok(moved)
    }

    /// `forge admin user rename`: `user`'s username and personal namespace
    /// become `username`.
    pub async fn operator_rename_user(
        &self,
        ctx: &ChangeContext,
        user: UserId,
        username: &str,
        reclaim: bool,
    ) -> Result<MoveOutcome, ForgeError> {
        let options = MoveOptions {
            reclaim,
            ..operator(ctx)?
        };
        let (moved, ()) =
            users::rename_decided(&self.db, ctx, user, NO_USER, username, options, |_| Ok(()))
                .await?;
        Ok(moved)
    }
}
