//! Merge request discussions (M3 R3, ADR-0024 §6, §12): start one
//! (general, or inline on a line of a version's diff), reply, resolve /
//! unresolve, and read them with whether a later version changed their
//! line (`outdated`).
//!
//! Commenting is decided as `MergeRequestComment` on the merge request
//! (`Authorized<MergeRequestCtx>`), resolving as `DiscussionResolve` on the
//! discussion (`Authorized<DiscussionCtx>`); each is decided again inside
//! its transaction on the facts read under the locks (the module docs of
//! the crate).
//!
//! An inline position is checked before the transaction against the
//! version's diff (`merge_base..head`, the M2 browse machinery through
//! [`DiffProbe`], reachable only from the merge request's own refs, a
//! branch or a tag): FG-REVIEW-0004 with the reason of
//! [`crate::position::check_position`]. `outdated` is computed when the
//! threads are read (never stored: every push would rewrite it and it
//! needs git): see `outdated.rs` and [`crate::position::line_changed`].

use forge_auth::{Action, Authorized, DiscussionCtx, LoadedDiscussion, MergeRequestCtx};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    DiscussionId, DiscussionSide, MergeRequestId, ProjectId, ProjectRepository, RepoId,
};
use forge_db::review::{
    self, DiscussionFacts, DiscussionPosition, NewDiscussion, ResolveOutcome, StoredDiscussion,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, Diff};

use crate::fields::{check_note_body, check_position_shape};
use crate::outdated::side_commit;
use crate::position::check_position;
use crate::{
    absent, account, caller, change_context, loaded, locked, ready, require, shape_mismatch,
    Review, ReviewRequest,
};

/// The diff of two commits of a merge request's repository, in the view
/// of merge request `iid` (both ends reachable from its version refs, a
/// branch or a tag; otherwise `FG-REPO-0023 reason=ref_not_found`).
pub trait DiffProbe: Send + Sync {
    /// `caller` is the caller's key (`user:<id>`) for the browse limits.
    fn merge_request_diff<'a>(
        &'a self,
        repo: RepoId,
        iid: i64,
        from: &'a str,
        to: &'a str,
        caller: &'a str,
    ) -> LookupFuture<'a, Diff>;
}

/// The browse API's merge request comparison (`merge_request_compare`).
impl DiffProbe for Browser {
    fn merge_request_diff<'a>(
        &'a self,
        repo: RepoId,
        iid: i64,
        from: &'a str,
        to: &'a str,
        caller: &'a str,
    ) -> LookupFuture<'a, Diff> {
        Box::pin(async move {
            Ok(self
                .merge_request_compare(repo, iid, from, to, caller)
                .await?
                .diff)
        })
    }
}

/// `FG-REVIEW-0004` with its reason and the position's safe facts (never
/// the path or the body).
fn position_invalid(
    reason: &'static str,
    project: ProjectId,
    iid: i64,
    position: &DiscussionPosition,
) -> ForgeError {
    ForgeError::new(ErrorCode::DiscussionPositionInvalid)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("iid", iid)
        .with_context("version", position.version)
        .with_context("side", position.side.as_str())
        .with_context("line", position.line)
}

/// `FG-SYS-0001 reason=diffs_unconfigured`: the server was built without
/// a diff source (`forge serve` always sets one).
fn unconfigured() -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", "diffs_unconfigured")
}

/// `FG-REPO-0005 resource=discussion`.
fn discussion_absent() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "discussion")
        .with_context("reason", "absent")
}

impl Review {
    /// The diff source of the inline positions and `outdated` (M3 R3;
    /// `forge serve` sets the browse API).
    #[must_use]
    pub fn with_diffs(mut self, diffs: std::sync::Arc<dyn DiffProbe>) -> Self {
        self.diffs = Some(diffs);
        self
    }

    /// The time one read may spend on `outdated` diffs (tests shorten it;
    /// `forge serve` keeps [`crate::OUTDATED_BUDGET`]).
    #[must_use]
    pub fn with_outdated_budget(mut self, budget: std::time::Duration) -> Self {
        self.outdated_budget = budget;
        self
    }

    pub(crate) fn outdated_budget(&self) -> std::time::Duration {
        self.outdated_budget
    }

    pub(crate) fn diffs(&self) -> Result<&dyn DiffProbe, ForgeError> {
        self.diffs.as_deref().ok_or_else(unconfigured)
    }

    /// FG-REVIEW-0004 unless `position` is in its version's diff.
    async fn check_inline(
        &self,
        project: ProjectId,
        iid: i64,
        repo: ProjectRepository,
        key: &str,
        position: &DiscussionPosition,
    ) -> Result<(), ForgeError> {
        let invalid = |reason| position_invalid(reason, project, iid, position);
        let mr = self.merge_request(project, iid).await?.ok_or_else(absent)?;
        let versions = review::read_versions(&self.db, mr.id).await?;
        let v = versions
            .iter()
            .find(|v| v.version_no == position.version)
            .ok_or_else(|| invalid("version_unknown"))?;
        if v.refs_purged {
            return Err(invalid("version_purged"));
        }
        let from =
            side_commit(v, DiscussionSide::Old).ok_or_else(|| invalid("version_base_pending"))?;
        let repo = ready(project, repo)?;
        let diff = self
            .diffs()?
            .merge_request_diff(repo, iid, from.as_str(), v.head_sha.as_str(), key)
            .await?;
        check_position(&diff, &position.path, position.side, position.line).map_err(invalid)
    }

    /// `POST …/merge-requests/{iid}/discussions` (`MergeRequestComment`):
    /// a discussion with its first note `body`, general or at `position`.
    /// The body is checked first (`FG-WEB-0006`), then the position against
    /// its version's diff (FG-REVIEW-0004), then the transaction (the
    /// reconfirmed decision).
    pub async fn start_discussion(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        body: &str,
        position: Option<&DiscussionPosition>,
    ) -> Result<StoredDiscussion, ForgeError> {
        require(proof.action(), Action::MergeRequestComment)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("discussion_start"));
        };
        check_note_body(body)?;
        let user = caller(proof)?;
        if let Some(p) = position {
            check_position_shape(p.version, &p.path, p.line)?;
            let key = format!("user:{}", user.get());
            self.check_inline(project, iid, repo, &key, p).await?;
        }
        let ctx = change_context(request, proof.principal(), user)?;
        let new = NewDiscussion {
            body: body.to_owned(),
            position: position.cloned(), // clone: forge-db owns its request
        };
        let (stored, ()) = review::start_discussion_mr_decided(
            &self.db,
            &ctx,
            project,
            iid,
            user,
            &new,
            |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok(stored)
    }

    /// `POST …/discussions/{discussion}/notes` (`MergeRequestComment`): a
    /// reply. The discussion must belong to the proof's merge request
    /// (`FG-REPO-0005 resource=discussion`, after the decision).
    pub async fn reply(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        discussion: DiscussionId,
        body: &str,
    ) -> Result<StoredDiscussion, ForgeError> {
        require(proof.action(), Action::MergeRequestComment)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("discussion_reply"));
        };
        check_note_body(body)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (stored, ()) = review::add_note_mr_decided(
            &self.db,
            &ctx,
            project,
            iid,
            discussion,
            user,
            body,
            |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok(stored)
    }

    /// `POST …/discussions/{discussion}/resolve` / `…/unresolve`
    /// (`DiscussionResolve`): decided again on the discussion as locked
    /// (who started it, who authored the merge request). The outcome and
    /// the discussion after the transaction.
    pub async fn resolve(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<DiscussionCtx>,
        resolved: bool,
    ) -> Result<(ResolveOutcome, StoredDiscussion), ForgeError> {
        require(proof.action(), Action::DiscussionResolve)?;
        let (project, Some(iid), discussion) = *proof.id() else {
            return Err(shape_mismatch("discussion_resolve"));
        };
        let user = proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_without_user")
                .with_context("action", proof.action().name())
        })?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, stored, ()) = review::resolve_discussion_mr_decided(
            &self.db,
            &ctx,
            project,
            iid,
            discussion,
            user,
            resolved,
            |facts: &DiscussionFacts| {
                let mr = locked(&facts.merge_request)?;
                let d = facts.discussion.as_ref().ok_or_else(discussion_absent)?;
                let fresh = DiscussionCtx::new(
                    loaded(&facts.merge_request.project),
                    mr,
                    LoadedDiscussion {
                        id: d.id,
                        started_by: d.started_by,
                    },
                );
                proof
                    .reconfirm(account(facts.merge_request.account), fresh)
                    .map(drop)
            },
        )
        .await?;
        Ok((outcome, stored))
    }

    /// Discussion `id` of merge request `mr` without its notes (the
    /// resolution route's loader; `None` = absent).
    pub async fn discussion(
        &self,
        mr: MergeRequestId,
        id: DiscussionId,
    ) -> Result<Option<StoredDiscussion>, ForgeError> {
        review::discussion(&self.db, mr, id).await
    }
}
