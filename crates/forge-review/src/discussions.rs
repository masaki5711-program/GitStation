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
//! needs git): see [`crate::position::line_changed`].

use std::collections::HashMap;

use forge_auth::{Action, Authorized, DiscussionCtx, LoadedDiscussion, MergeRequestCtx};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    DiscussionId, DiscussionSide, MergeRequestId, ObjectId, ProjectId, ProjectRepository, RepoId,
};
use forge_db::review::{
    self, DiscussionFacts, DiscussionPosition, NewDiscussion, ResolveOutcome, StoredDiscussion,
    StoredVersion,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, Diff};

use crate::fields::{check_note_body, check_position_shape};
use crate::position::{check_position, line_changed};
use crate::{
    absent, account, caller, change_context, loaded, locked, ready, require, shape_mismatch,
    Review, ReviewRequest, StoredMergeRequest,
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

/// One discussion as read, with whether a later version changed its line
/// (`false` for a general one and for one on the current version).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionView {
    pub discussion: StoredDiscussion,
    pub outdated: bool,
}

/// The discussions of a merge request, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionList {
    pub discussions: Vec<DiscussionView>,
    /// A cap cut the list (`forge_db::review::MAX_DISCUSSIONS` / `MAX_NOTES`).
    pub truncated: bool,
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

/// The commit a side of a version shows: the head (`new`), or the merge
/// base (`old`; the target's tip when there was no common history; `None`
/// before the version job recorded it).
fn side_commit(v: &StoredVersion, side: DiscussionSide) -> Option<&ObjectId> {
    match side {
        DiscussionSide::New => Some(&v.head_sha),
        DiscussionSide::Old => v.merge_base_sha.as_ref().or(v.base_sha.as_ref()),
    }
}

impl Review {
    /// The diff source of the inline positions and `outdated` (M3 R3;
    /// `forge serve` sets the browse API).
    #[must_use]
    pub fn with_diffs(mut self, diffs: std::sync::Arc<dyn DiffProbe>) -> Self {
        self.diffs = Some(diffs);
        self
    }

    fn diffs(&self) -> Result<&dyn DiffProbe, ForgeError> {
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
        let ctx = change_context(request, proof.principal(), user);
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
        let ctx = change_context(request, proof.principal(), user);
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
        let ctx = change_context(request, proof.principal(), user);
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

    /// The discussions of `mr` with their notes and `outdated` (the caller
    /// decided `MergeRequestRead` on it). Runs one diff per distinct pair
    /// of commits an inline discussion needs compared (none when every one
    /// sits on the current version); a diff that fails fails the read
    /// (never "not outdated" on an error).
    pub async fn discussions(
        &self,
        mr: &StoredMergeRequest,
        repo: ProjectRepository,
        key: &str,
    ) -> Result<DiscussionList, ForgeError> {
        let page = review::discussions(&self.db, mr.id).await?;
        Ok(DiscussionList {
            discussions: self.annotate(mr, repo, key, page.discussions).await?,
            truncated: page.truncated,
        })
    }

    /// One discussion (as a write returned it) with `outdated`, as the
    /// list shows it.
    pub async fn view(
        &self,
        mr: &StoredMergeRequest,
        repo: ProjectRepository,
        key: &str,
        discussion: StoredDiscussion,
    ) -> Result<DiscussionView, ForgeError> {
        let mut one = self.annotate(mr, repo, key, vec![discussion]).await?;
        one.pop().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "discussion_view_empty")
        })
    }

    /// `outdated` for each of `discussions` (one read of the versions, one
    /// diff per distinct pair of commits).
    async fn annotate(
        &self,
        mr: &StoredMergeRequest,
        repo: ProjectRepository,
        key: &str,
        discussions: Vec<StoredDiscussion>,
    ) -> Result<Vec<DiscussionView>, ForgeError> {
        let needs_git = discussions.iter().any(|d| d.position.is_some());
        let versions = if needs_git {
            review::read_versions(&self.db, mr.id).await?
        } else {
            Vec::new()
        };
        let current = versions
            .iter()
            .find(|v| mr.head_sha.as_deref() == Some(v.head_sha.as_str()));
        let mut cache: HashMap<(String, String), Diff> = HashMap::new();
        let mut out = Vec::with_capacity(discussions.len());
        for d in discussions {
            let outdated = match (&d.position, current) {
                (None, _) | (Some(_), None) => false,
                (Some(p), Some(current)) => {
                    self.outdated(mr, repo, key, &versions, current, p, &mut cache)
                        .await?
                }
            };
            out.push(DiscussionView {
                discussion: d,
                outdated,
            });
        }
        Ok(out)
    }

    /// Whether `p` sits on a line the current version changed (module
    /// docs of `position`). Unknown commits (a purged version, bases not
    /// recorded) count as changed.
    #[allow(clippy::too_many_arguments)] // the read's context and its per-call cache
    async fn outdated(
        &self,
        mr: &StoredMergeRequest,
        repo: ProjectRepository,
        key: &str,
        versions: &[StoredVersion],
        current: &StoredVersion,
        p: &DiscussionPosition,
        cache: &mut HashMap<(String, String), Diff>,
    ) -> Result<bool, ForgeError> {
        if p.version == current.version_no {
            return Ok(false);
        }
        let Some(v) = versions.iter().find(|v| v.version_no == p.version) else {
            return Ok(true);
        };
        if v.refs_purged || current.refs_purged {
            return Ok(true);
        }
        let (Some(from), Some(to)) = (side_commit(v, p.side), side_commit(current, p.side)) else {
            return Ok(true);
        };
        if from == to {
            return Ok(false);
        }
        let pair = (from.as_str().to_owned(), to.as_str().to_owned());
        if !cache.contains_key(&pair) {
            let repo_id = ready(mr.project_id, repo)?;
            let diff = self
                .diffs()?
                .merge_request_diff(repo_id, mr.iid, &pair.0, &pair.1, key)
                .await?;
            cache.insert(pair.clone(), diff); // clone: the key is looked up again below
        }
        Ok(cache
            .get(&pair)
            .is_some_and(|diff| line_changed(diff, &p.path, p.line)))
    }
}
