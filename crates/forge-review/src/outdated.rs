//! Reading discussions with `outdated` (M3 R3, ADR-0024 R3 decisions):
//! whether a later version rewrote an inline thread's line, computed when
//! the threads are read (never stored).
//!
//! The computation never fails a read and never fails a write's answer
//! (review round 1): a thread whose answer cannot be computed — the
//! merge request has no current version, a purged version or unrecorded
//! bases, more distinct diffs than [`MAX_DIFFS_PER_READ`], or a diff that
//! failed (busy, timeout, unreachable commits) — is [`Outdated::Unknown`]:
//! shown as outdated (look again) and flagged as unknown. A read with
//! unknown threads logs one warn event,
//! `forge.review.discussion_outdated_unknown`, with the counts, the reason
//! and the first diff failure's `error_code`.

use std::collections::HashMap;

use forge_core::{DiscussionSide, ObjectId, ProjectRepository, RequestId};
use forge_db::review::{self, DiscussionPosition, StoredDiscussion, StoredVersion};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Diff;

use crate::position::line_changed;
use crate::{ready, Review, StoredMergeRequest};

/// The most distinct pairs of commits one read diffs; the threads that
/// would need more are `Unknown`.
pub const MAX_DIFFS_PER_READ: usize = 20;

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

/// What one read did, for its single log line.
#[derive(Default)]
struct Tally {
    /// Diff results by (from, to); a failure keeps its code.
    diffs: HashMap<(String, String), Result<Diff, ErrorCode>>,
    unknown: usize,
    reason: Option<&'static str>,
    first_error: Option<ErrorCode>,
}

impl Tally {
    fn unknown(&mut self, reason: &'static str) -> Outdated {
        self.unknown += 1;
        self.reason.get_or_insert(reason);
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
            let diff = self.diff(&pair).await.map_err(|e| e.code());
            if let Err(code) = &diff {
                tally.first_error.get_or_insert(*code);
            }
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
    if tally.unknown == 0 {
        return;
    }
    tracing::warn!(
        target: "forge_review",
        event = "forge.review.discussion_outdated_unknown",
        request_id = %request_id,
        merge_request_id = mr.id.get(),
        project_id = mr.project_id.get(),
        iid = mr.iid,
        unknown = tally.unknown,
        diffs = tally.diffs.len(),
        reason = tally.reason.unwrap_or("unknown"),
        error_code = tally.first_error.map(ErrorCode::id),
        "discussion outdated state could not be computed; shown as outdated"
    );
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
        let mut tally = Tally::default();
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
        let mut tally = Tally::default();
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
                    tally.first_error = Some(e.code());
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
