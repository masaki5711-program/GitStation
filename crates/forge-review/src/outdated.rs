//! Reading discussions with `outdated` (M3 R3, ADR-0024 R3 decisions):
//! whether a later version rewrote an inline thread's line, computed when
//! the threads are read (never stored).
//!
//! The computation never fails a read and never fails a write's answer
//! (review round 1): a thread whose answer cannot be computed — the
//! merge request has no current version, a purged version or unrecorded
//! bases, more distinct diffs than [`MAX_DIFFS_PER_READ`], or a diff that
//! failed (busy, timeout, unreachable commits), or the read's time budget
//! ([`OUTDATED_BUDGET`], 8 s: well under the request timeout, so a write's
//! answer after its commit stays inside it) ran out — is
//! [`Outdated::Unknown`]: shown as outdated (look again) and flagged as
//! unknown. Past the deadline no git runs at all; a diff in flight is cut
//! at it. A read with unknown threads logs once,
//! `forge.review.discussion_outdated_unknown`: at warn for a diagnosis
//! failure (`diff_failed` / `versions_unreadable` with `error_code` and
//! `error_reason`; `deadline`, `diff_cap`, `no_current_version`,
//! `version_unknown` without a code), at debug for the routine states
//! (`version_purged`, `version_base_pending`).

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use forge_core::{DiscussionSide, ObjectId, ProjectRepository, RequestId};
use forge_db::review::{self, DiscussionPosition, StoredDiscussion, StoredVersion};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Diff;

use crate::position::line_changed;
use crate::{ready, Review, StoredMergeRequest};

/// The most distinct pairs of commits one read diffs; the threads that
/// would need more are `Unknown`.
pub const MAX_DIFFS_PER_READ: usize = 20;

/// The time one read — or a write's answer — spends on `outdated` diffs
/// at most (`Review::with_outdated_budget`).
pub const OUTDATED_BUDGET: Duration = Duration::from_secs(8);

/// Whether a later version changed a thread's line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outdated {
    /// A general thread, one on the current version, or a line no later
    /// version touched.
    No,
    /// The current version removed or rewrote the line, or moved its file.
    Yes,
    /// Could not be computed (module docs): shown as outdated.
    Unknown,
}

impl Outdated {
    /// The API's `outdated`: `Yes` and `Unknown` both ask to look again.
    pub fn shown(self) -> bool {
        match self {
            Self::No => false,
            Self::Yes | Self::Unknown => true,
        }
    }

    /// The API's `outdated_unknown`.
    pub fn is_unknown(self) -> bool {
        match self {
            Self::Unknown => true,
            Self::No | Self::Yes => false,
        }
    }
}

/// One discussion as read, with `outdated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionView {
    pub discussion: StoredDiscussion,
    pub outdated: Outdated,
}

/// The discussions of a merge request, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionList {
    pub discussions: Vec<DiscussionView>,
    /// A cap cut the list (`forge_db::review::MAX_DISCUSSIONS` / `MAX_NOTES`).
    pub truncated: bool,
}

/// The commit a side of a version shows: the head (`new`), or the merge
/// base (`old`; the target's tip when there was no common history; `None`
/// before the version job recorded it).
pub(crate) fn side_commit(v: &StoredVersion, side: DiscussionSide) -> Option<&ObjectId> {
    match side {
        DiscussionSide::New => Some(&v.head_sha),
        DiscussionSide::Old => v.merge_base_sha.as_ref().or(v.base_sha.as_ref()),
    }
}

/// The first failure behind a diagnosis `Unknown`: its code and its
/// context `reason` (a label the failing code chose, never user data).
#[derive(Clone)]
struct Failure {
    code: ErrorCode,
    reason: Option<String>,
}

impl Failure {
    fn of(e: &ForgeError) -> Self {
        Self {
            code: e.code(),
            reason: e
                .context()
                .iter()
                .find(|(k, _)| *k == "reason")
                .map(|(_, v)| v.clone()), // clone: the log keeps the label
        }
    }
}

/// Routine states: a purged version or bases not recorded yet (debug).
fn is_routine(reason: &str) -> bool {
    matches!(reason, "version_purged" | "version_base_pending")
}

/// What one read did, for its single log line.
struct Tally {
    /// Diff results by (from, to); a failure keeps its code.
    diffs: HashMap<(String, String), Result<Diff, ErrorCode>>,
    /// Unknown threads for a diagnosis reason, and the first such reason.
    unknown: usize,
    reason: Option<&'static str>,
    /// Unknown threads for a routine reason, and the first one.
    routine: usize,
    routine_reason: Option<&'static str>,
    first_error: Option<Failure>,
    /// No diff starts after this; one in flight is cut here.
    deadline: Instant,
}

impl Tally {
    fn new(budget: Duration) -> Self {
        Self {
            diffs: HashMap::new(),
            unknown: 0,
            reason: None,
            routine: 0,
            routine_reason: None,
            first_error: None,
            deadline: Instant::now() + budget,
        }
    }

    fn unknown(&mut self, reason: &'static str) -> Outdated {
        if is_routine(reason) {
            self.routine += 1;
            self.routine_reason.get_or_insert(reason);
        } else {
            self.unknown += 1;
            self.reason.get_or_insert(reason);
        }
        Outdated::Unknown
    }
}

/// The read's context.
struct Read<'a> {
    review: &'a Review,
    mr: &'a StoredMergeRequest,
    repo: ProjectRepository,
    key: &'a str,
    versions: Vec<StoredVersion>,
}

impl Read<'_> {
    /// The current version: the one whose head is the merge request's
    /// (heads are unique per merge request).
    fn current(&self) -> Option<&StoredVersion> {
        let head = self.mr.head_sha.as_deref()?;
        self.versions.iter().find(|v| v.head_sha.as_str() == head)
    }

    async fn outdated(&self, p: &DiscussionPosition, tally: &mut Tally) -> Outdated {
        let Some(current) = self.current() else {
            return tally.unknown("no_current_version");
        };
        if p.version == current.version_no {
            return Outdated::No;
        }
        let Some(v) = self.versions.iter().find(|v| v.version_no == p.version) else {
            return tally.unknown("version_unknown");
        };
        if v.refs_purged || current.refs_purged {
            return tally.unknown("version_purged");
        }
        let (Some(from), Some(to)) = (side_commit(v, p.side), side_commit(current, p.side)) else {
            return tally.unknown("version_base_pending");
        };
        if from == to {
            return Outdated::No;
        }
        let pair = (from.as_str().to_owned(), to.as_str().to_owned());
        if !tally.diffs.contains_key(&pair) {
            if tally.diffs.len() >= MAX_DIFFS_PER_READ {
                return tally.unknown("diff_cap");
            }
            let left = tally.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return tally.unknown("deadline");
            }
            let Ok(diff) = tokio::time::timeout(left, self.diff(&pair)).await else {
                // Cut at the deadline: not stored, so no later thread waits
                // on it either (they meet the deadline above).
                return tally.unknown("deadline");
            };
            let diff = diff.map_err(|e| {
                tally.first_error.get_or_insert(Failure::of(&e));
                e.code()
            });
            tally.diffs.insert(pair.clone(), diff); // clone: the key is looked up again below
        }
        match tally.diffs.get(&pair) {
            Some(Ok(diff)) if line_changed(diff, &p.path, p.line) => Outdated::Yes,
            Some(Ok(_)) => Outdated::No,
            Some(Err(_)) | None => tally.unknown("diff_failed"),
        }
    }

    async fn diff(&self, (from, to): &(String, String)) -> Result<Diff, ForgeError> {
        let repo = ready(self.mr.project_id, self.repo)?;
        self.review
            .diffs()?
            .merge_request_diff(repo, self.mr.iid, from, to, self.key)
            .await
    }
}

/// One warn line for a read that left threads unknown.
fn log_unknown(request_id: &RequestId, mr: &StoredMergeRequest, tally: &Tally) {
    if tally.unknown > 0 {
        tracing::warn!(
            target: "forge_review",
            event = "forge.review.discussion_outdated_unknown",
            request_id = %request_id,
            merge_request_id = mr.id.get(),
            project_id = mr.project_id.get(),
            iid = mr.iid,
            unknown = tally.unknown,
            routine = tally.routine,
            diffs = tally.diffs.len(),
            reason = tally.reason.unwrap_or("unknown"),
            error_code = tally.first_error.as_ref().map(|f| f.code.id()),
            error_reason = tally.first_error.as_ref().and_then(|f| f.reason.as_deref()),
            "discussion outdated state could not be computed; shown as outdated"
        );
    } else if tally.routine > 0 {
        tracing::debug!(
            target: "forge_review",
            event = "forge.review.discussion_outdated_unknown",
            request_id = %request_id,
            merge_request_id = mr.id.get(),
            routine = tally.routine,
            reason = tally.routine_reason.unwrap_or("unknown"),
            "discussion outdated state not computable for a routine reason"
        );
    }
}

impl Review {
    /// The discussions of `mr` with their notes and `outdated` (the caller
    /// decided `MergeRequestRead` on it). Fails only when the database
    /// cannot be read; git trouble makes threads `Unknown` (module docs).
    pub async fn discussions(
        &self,
        mr: &StoredMergeRequest,
        repo: ProjectRepository,
        key: &str,
        request_id: &RequestId,
    ) -> Result<DiscussionList, ForgeError> {
        let page = review::discussions(&self.db, mr.id).await?;
        let versions = if page.discussions.iter().any(|d| d.position.is_some()) {
            review::read_versions(&self.db, mr.id).await?
        } else {
            Vec::new()
        };
        let read = Read {
            review: self,
            mr,
            repo,
            key,
            versions,
        };
        let mut tally = Tally::new(self.outdated_budget());
        let mut out = Vec::with_capacity(page.discussions.len());
        for d in page.discussions {
            let outdated = match &d.position {
                None => Outdated::No,
                Some(p) => read.outdated(p, &mut tally).await,
            };
            out.push(DiscussionView {
                discussion: d,
                outdated,
            });
        }
        log_unknown(request_id, mr, &tally);
        Ok(DiscussionList {
            discussions: out,
            truncated: page.truncated,
        })
    }

    /// One discussion a write just stored, with `outdated` — never an
    /// error: the write is committed, so a failure to read the versions or
    /// a diff makes it `Unknown` (logged once), and the answer stands.
    pub async fn view(
        &self,
        mr: &StoredMergeRequest,
        repo: ProjectRepository,
        key: &str,
        request_id: &RequestId,
        discussion: StoredDiscussion,
    ) -> DiscussionView {
        let mut tally = Tally::new(self.outdated_budget());
        let outdated = match &discussion.position {
            None => Outdated::No,
            Some(p) => match review::read_versions(&self.db, mr.id).await {
                Ok(versions) => {
                    let read = Read {
                        review: self,
                        mr,
                        repo,
                        key,
                        versions,
                    };
                    read.outdated(p, &mut tally).await
                }
                Err(e) => {
                    tally.first_error = Some(Failure::of(&e));
                    tally.unknown("versions_unreadable")
                }
            },
        };
        log_unknown(request_id, mr, &tally);
        DiscussionView {
            discussion,
            outdated,
        }
    }
}
