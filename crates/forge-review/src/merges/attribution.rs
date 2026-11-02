//! Who a merge's own commit is attributed to (M3 R5 review round 5,
//! ADR-0024 §8.3). A merge or squash commit is a commit the merge
//! introduced, so it gets `push_commits` rows like a pushed one — for the
//! requester **and every pusher of the merged range** (`head ^<old target>`).
//! Otherwise a squash would launder the range: a later merge request
//! holding the squash commit (main → release) would see only the requester
//! and count the original pushers' approvals.
//!
//! When the range's pushers cannot be told (a range commit with no row, a
//! range over the cap, a git failure, the lock's time used up) the commit
//! gets **no row at all**: a later range holding it has unknown committers
//! (fail closed), never just the requester. One FG-GIT-0016 warn says why.
//! A fast-forward brings no new commit and needs none.

use forge_core::ids::UserId;
use forge_core::{MergeMethod, ObjectId};
use forge_db::review::merges::{MergeLock, StoredMerge};
use forge_db::review::{self};
use forge_errors::{ErrorCode, ForgeError};

use super::execute::Subject;
use super::Merger;
use crate::approvals::committers::{committers_of, MAX_RANGE_COMMITS};
use crate::approvals::Committers;

impl Merger {
    /// The pushers of the merged range (module docs): `Some` (possibly
    /// empty for a fast-forward), or `None` when unknown — the merge
    /// commit then gets no row.
    pub(super) async fn merge_commit_pushers(
        &self,
        lock: &MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
        old: &ObjectId,
    ) -> Option<Vec<UserId>> {
        if attempt.method == MergeMethod::FastForward {
            return Some(Vec::new());
        }
        let listing = self.timeouts.read.min(lock.left());
        if listing.is_zero() {
            unattributed(attempt, subject, "merge_lock_lost", None);
            return None;
        }
        let range = match self
            .git
            .range_commits(
                &self.storage,
                subject.repo,
                Some(old),
                &attempt.head_sha,
                MAX_RANGE_COMMITS,
                listing,
            )
            .await
        {
            Ok(Some(range)) => range,
            Ok(None) => {
                unattributed(attempt, subject, "range_too_long", None);
                return None;
            }
            Err(e) => {
                unattributed(attempt, subject, "git_failed", Some(e));
                return None;
            }
        };
        let read =
            review::range_pushers(&self.db, attempt.project_id, subject.repo, &range.commits);
        let pushers = match tokio::time::timeout(listing, read).await {
            Ok(Ok(pushers)) => pushers,
            Ok(Err(e)) => {
                unattributed(attempt, subject, "db_failed", Some(e));
                return None;
            }
            Err(_) => {
                unattributed(attempt, subject, "deadline", None);
                return None;
            }
        };
        match committers_of(&range.commits, &pushers) {
            Committers::Known(map) => Some(map.into_keys().collect()),
            Committers::Unknown(reason) => {
                unattributed(attempt, subject, reason, None);
                None
            }
        }
    }
}

/// The one warn of an unattributed merge commit.
fn unattributed(
    attempt: &StoredMerge,
    subject: &Subject,
    reason: &'static str,
    cause: Option<ForgeError>,
) {
    tracing::warn!(
        target: "forge_review",
        event = "forge.review.merge_commit_unattributed",
        error_code = ErrorCode::PushCommitsIncomplete.id(),
        request_id = %attempt.request_id,
        merge_id = attempt.id,
        project_id = attempt.project_id.get(),
        repo_id = subject.repo.get(),
        merge_request_id = attempt.merge_request.get(),
        reason = format!("merge_pushers_unknown:{reason}"),
        cause_code = cause.as_ref().map(|e| e.code().id()),
        cause = cause.as_ref().map(tracing::field::display),
        "the merge commit gets no push_commits row: a later range holding it has unknown committers"
    );
}
