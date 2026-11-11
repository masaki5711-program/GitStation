//! The merge requests of the caller's own work across projects (plan 15
//! MR 6): `GET /api/v1/user/merge-requests?scope=` and `…/counts`
//! (`UserMergeRequestList` on the caller's own lists; `authorize()` allows
//! nobody else, administrators included).
//!
//! forge-db reads the candidates of a scope (`review::user_lists`); every
//! row is decided again here, for the caller's own principal, by the one
//! decision point — this module has no access rule of its own:
//!
//! - every scope: `MergeRequestRead` on the merge request in its project as
//!   loaded for the caller now (`forge_auth::prove`, as the merge request
//!   routes and forge-notify decide). A merge request the caller cannot
//!   read is neither listed nor counted, and nothing about it is returned.
//! - `approvable`: also the existing approval decision —
//!   [`MergeRequestCtx::viewer`]'s `can_approve`, the trial of
//!   `MergeRequestApprove` the merge request screen shows its button by
//!   (people only, never the author, Developer and above; a token needs
//!   `merge_approve`). forge-db already left out the caller's own merge
//!   requests and those whose current head and target the caller approved.
//! - `attention` (the caller's open merge requests that wait on the
//!   author): the mergeability check found a conflict for the current
//!   head, a conflict with another open merge request is predicted for it,
//!   or a check the target's merge rule requires
//!   ([`forge_core::ProtectionRules::merge_rule`]) has a run on the head
//!   that completed without success. Runs on merge results (M4 C7) are not
//!   read here.
//!
//! A page may hold fewer rows than asked (hidden rows are skipped);
//! `next_before` is set whenever more candidates may follow. A count is
//! capped at [`COUNT_CAP`] visible rows over at most [`COUNT_SCAN_PAGES`]
//! pages of candidates.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use forge_auth::{
    prove, Action, Authorized, LoadedProject, MergeRequestCtx, Principal, UserMergeRequestsCtx,
};
use forge_core::{ProjectId, ProjectRecord, ProtectionRules, UserId};
pub use forge_db::review::user_lists::UserListScope;
use forge_db::review::user_lists::{self, UserListRow};
use forge_db::{protected_refs, resolve};
use forge_errors::ForgeError;

use crate::{loaded_merge_request, require, Review, StoredMergeRequest};

/// The most visible rows a count reports (`capped` at it: show "99+").
pub const COUNT_CAP: i64 = 100;
/// The most pages of candidates one count reads.
pub const COUNT_SCAN_PAGES: usize = 10;
/// The candidates read per page of a count.
const COUNT_PAGE: usize = 100;

/// Why a merge request needs its author (`attention`; also shown on the
/// other scopes' rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttentionReason {
    /// The mergeability check found a conflict for the current head.
    Conflict,
    /// A conflict with another open merge request is predicted.
    ConflictPredicted,
    /// A required check's run on the head completed without success.
    RequiredCheckFailed,
}

impl AttentionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::ConflictPredicted => "conflict_predicted",
            Self::RequiredCheckFailed => "required_check_failed",
        }
    }
}

/// One row the caller may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserListItem {
    pub merge_request: StoredMergeRequest,
    /// The project's path now (`group/project`).
    pub project_path: String,
    /// Empty unless the row needs its author (always non-empty on
    /// `attention`).
    pub reasons: Vec<AttentionReason>,
}

/// A page of one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserListPage {
    pub items: Vec<UserListItem>,
    /// Pass as `before` for the next page; `None`: no more.
    pub next_before: Option<i64>,
}

/// The visible rows of one scope, at most [`COUNT_CAP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserListCount {
    pub count: i64,
    /// [`COUNT_CAP`] visible rows were found.
    pub capped: bool,
}

/// The counts of every scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserListCounts {
    pub authored: UserListCount,
    pub approvable: UserListCount,
    pub attention: UserListCount,
}

/// What one page is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserListQuery {
    pub scope: UserListScope,
    pub before: Option<i64>,
    pub limit: usize,
}

/// The facts read once per project of a page.
struct ProjectFacts {
    record: ProjectRecord,
    /// Read for `attention` only, when a row has a failed check.
    protection: Option<ProtectionRules>,
}

/// Decides the rows of one scope for one caller.
struct Decider<'a> {
    review: &'a Review,
    principal: &'a Principal,
    user: UserId,
    scope: UserListScope,
    /// `None`: the project is gone or not visible to the caller.
    projects: HashMap<ProjectId, Option<ProjectFacts>>,
}

impl<'a> Decider<'a> {
    fn new(
        review: &'a Review,
        proof: &'a Authorized<UserMergeRequestsCtx>,
        scope: UserListScope,
    ) -> Self {
        Self {
            review,
            principal: proof.principal(),
            user: *proof.id(),
            scope,
            projects: HashMap::new(),
        }
    }

    /// `rows` the caller may see in this scope, in their order.
    async fn visible(&mut self, rows: Vec<UserListRow>) -> Result<Vec<UserListItem>, ForgeError> {
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(item) = self.decide(row).await? {
                items.push(item);
            }
        }
        Ok(items)
    }

    async fn decide(&mut self, row: UserListRow) -> Result<Option<UserListItem>, ForgeError> {
        let project = row.merge_request.project_id;
        if let Entry::Vacant(slot) = self.projects.entry(project) {
            // One access read per project of the page (as the inbox).
            let record = resolve::load_project(&self.review.db, project, Some(self.user)).await?;
            slot.insert(record.map(|record| ProjectFacts {
                record,
                protection: None,
            }));
        }
        let Some(Some(facts)) = self.projects.get(&project) else {
            return Ok(None); // the project is gone or not visible
        };
        let loaded = LoadedProject {
            id: facts.record.id,
            visibility: facts.record.access.visibility,
            role: facts.record.access.role,
            pending_deletion: facts.record.access.pending_deletion,
        };
        let ctx = MergeRequestCtx::new(loaded, loaded_merge_request(&row.merge_request));
        if self.scope == UserListScope::Approvable && !ctx.viewer(self.principal, None).can_approve
        {
            return Ok(None);
        }
        if prove(self.principal, Action::MergeRequestRead, ctx).is_err() {
            return Ok(None);
        }
        let project_path = facts.record.path.full_path();
        let reasons = self.reasons(&row).await?;
        if self.scope == UserListScope::Attention && reasons.is_empty() {
            return Ok(None);
        }
        Ok(Some(UserListItem {
            merge_request: row.merge_request,
            project_path,
            reasons,
        }))
    }

    /// Why `row` waits on its author (the target's merge rule is read only
    /// for a row with a failed check, once per project).
    async fn reasons(&mut self, row: &UserListRow) -> Result<Vec<AttentionReason>, ForgeError> {
        let mut reasons = Vec::new();
        if row.conflict {
            reasons.push(AttentionReason::Conflict);
        }
        if row.conflict_predicted {
            reasons.push(AttentionReason::ConflictPredicted);
        }
        if row.failed_checks.is_empty() {
            return Ok(reasons);
        }
        let project = row.merge_request.project_id;
        let Some(Some(facts)) = self.projects.get_mut(&project) else {
            return Ok(reasons);
        };
        if facts.protection.is_none() {
            facts.protection = Some(protected_refs::rules_for(&self.review.db, project).await?);
        }
        let required = facts
            .protection
            .as_ref()
            .map(|p| {
                p.merge_rule(&row.merge_request.target_branch)
                    .required_checks
            })
            .unwrap_or_default();
        if required
            .iter()
            .any(|name| row.failed_checks.iter().any(|f| f == name.as_str()))
        {
            reasons.push(AttentionReason::RequiredCheckFailed);
        }
        Ok(reasons)
    }
}

impl Review {
    /// `GET /api/v1/user/merge-requests?scope=` (`UserMergeRequestList`):
    /// one page of the caller's `scope`, newest first.
    pub async fn user_merge_requests(
        &self,
        proof: &Authorized<UserMergeRequestsCtx>,
        query: UserListQuery,
    ) -> Result<UserListPage, ForgeError> {
        require(proof.action(), Action::UserMergeRequestList)?;
        let rows = user_lists::user_merge_requests(
            &self.db,
            *proof.id(),
            query.scope,
            query.before,
            query.limit,
        )
        .await?;
        let full = rows.len() >= query.limit;
        let next_before = full
            .then(|| rows.last().map(|r| r.merge_request.id.get()))
            .flatten();
        let items = Decider::new(self, proof, query.scope).visible(rows).await?;
        Ok(UserListPage { items, next_before })
    }

    /// `GET /api/v1/user/merge-requests/counts` (`UserMergeRequestList`):
    /// the visible rows of every scope, each capped at [`COUNT_CAP`].
    pub async fn user_merge_request_counts(
        &self,
        proof: &Authorized<UserMergeRequestsCtx>,
    ) -> Result<UserListCounts, ForgeError> {
        require(proof.action(), Action::UserMergeRequestList)?;
        Ok(UserListCounts {
            authored: self.count(proof, UserListScope::Authored).await?,
            approvable: self.count(proof, UserListScope::Approvable).await?,
            attention: self.count(proof, UserListScope::Attention).await?,
        })
    }

    async fn count(
        &self,
        proof: &Authorized<UserMergeRequestsCtx>,
        scope: UserListScope,
    ) -> Result<UserListCount, ForgeError> {
        let mut decider = Decider::new(self, proof, scope);
        let mut count: i64 = 0;
        let mut before = None;
        // Bounded: at most COUNT_SCAN_PAGES pages, stop at COUNT_CAP visible.
        for _ in 0..COUNT_SCAN_PAGES {
            let rows =
                user_lists::user_merge_requests(&self.db, *proof.id(), scope, before, COUNT_PAGE)
                    .await?;
            let full = rows.len() >= COUNT_PAGE;
            before = rows.last().map(|r| r.merge_request.id.get());
            let seen = decider.visible(rows).await?.len();
            count = count.saturating_add(i64::try_from(seen).unwrap_or(i64::MAX));
            if count >= COUNT_CAP {
                return Ok(UserListCount {
                    count: COUNT_CAP,
                    capped: true,
                });
            }
            if !full {
                break;
            }
        }
        Ok(UserListCount {
            count,
            capped: false,
        })
    }
}
