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
//! capped at [`COUNT_CAP`] visible rows (`capped`) over at most
//! [`COUNT_SCAN_PAGES`] pages of candidates; reaching that bound with
//! candidates left is said (`truncated`), never a silent undercount.

use std::collections::HashMap;
use std::sync::Arc;

use forge_auth::{
    prove, Action, Authorized, LoadedProject, MergeRequestCtx, Principal, UserMergeRequestsCtx,
};
use forge_core::{ProjectId, ProjectRecord, ProtectionRules, UserId};
pub use forge_db::review::user_lists::UserListScope;
use forge_db::review::user_lists::{self, UserListRow};
use forge_db::{protected_refs, resolve};
use forge_errors::ForgeError;
use tokio::sync::Mutex;

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
    /// [`COUNT_CAP`] visible rows were found (show "99+").
    pub capped: bool,
    /// The count stopped at its scan bound ([`COUNT_SCAN_PAGES`] pages of
    /// candidates) while more candidates remained: the true count may be
    /// higher (show "n+"). Never set together with `capped`.
    pub truncated: bool,
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

/// What one request reads once per project, shared by the scopes it
/// decides (the three counts run concurrently). A racing first read of a
/// project may happen twice; both read the same facts.
#[derive(Default)]
struct Shared {
    /// `None`: the project is gone or not visible to the caller.
    projects: Mutex<HashMap<ProjectId, Option<Arc<ProjectRecord>>>>,
    /// Read for `attention` reasons only, when a row has a failed check.
    protection: Mutex<HashMap<ProjectId, Arc<ProtectionRules>>>,
}

/// Decides the rows of one scope for one caller.
struct Decider<'a> {
    review: &'a Review,
    principal: &'a Principal,
    user: UserId,
    scope: UserListScope,
    shared: &'a Shared,
}

impl<'a> Decider<'a> {
    fn new(
        review: &'a Review,
        proof: &'a Authorized<UserMergeRequestsCtx>,
        scope: UserListScope,
        shared: &'a Shared,
    ) -> Self {
        Self {
            review,
            principal: proof.principal(),
            user: *proof.id(),
            scope,
            shared,
        }
    }

    /// `rows` the caller may see in this scope, in their order.
    async fn visible(&self, rows: Vec<UserListRow>) -> Result<Vec<UserListItem>, ForgeError> {
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(item) = self.decide(row).await? {
                items.push(item);
            }
        }
        Ok(items)
    }

    /// The project as loaded for the caller (one read per project of the
    /// request, as the inbox).
    async fn project(&self, id: ProjectId) -> Result<Option<Arc<ProjectRecord>>, ForgeError> {
        if let Some(known) = self.shared.projects.lock().await.get(&id) {
            return Ok(known.clone()); // clone: an Arc
        }
        let record = resolve::load_project(&self.review.db, id, Some(self.user))
            .await?
            .map(Arc::new);
        Ok(self
            .shared
            .projects
            .lock()
            .await
            .entry(id)
            .or_insert(record)
            .clone()) // clone: an Arc
    }

    /// The project's protection rules (read once per project of the
    /// request).
    async fn protection(&self, id: ProjectId) -> Result<Arc<ProtectionRules>, ForgeError> {
        if let Some(known) = self.shared.protection.lock().await.get(&id) {
            return Ok(Arc::clone(known));
        }
        let rules = Arc::new(protected_refs::rules_for(&self.review.db, id).await?);
        Ok(Arc::clone(
            self.shared
                .protection
                .lock()
                .await
                .entry(id)
                .or_insert(rules),
        ))
    }

    async fn decide(&self, row: UserListRow) -> Result<Option<UserListItem>, ForgeError> {
        let Some(record) = self.project(row.merge_request.project_id).await? else {
            return Ok(None); // the project is gone or not visible
        };
        let loaded = LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        };
        let ctx = MergeRequestCtx::new(loaded, loaded_merge_request(&row.merge_request));
        // Read first: nothing more is decided about a merge request the
        // caller may not read.
        let Ok(read) = prove(self.principal, Action::MergeRequestRead, ctx) else {
            return Ok(None);
        };
        if self.scope == UserListScope::Approvable
            && !read.ctx().viewer(self.principal, None).can_approve
        {
            return Ok(None);
        }
        let reasons = self.reasons(&row).await?;
        if self.scope == UserListScope::Attention && reasons.is_empty() {
            return Ok(None);
        }
        Ok(Some(UserListItem {
            merge_request: row.merge_request,
            project_path: record.path.full_path(),
            reasons,
        }))
    }

    /// Why `row` waits on its author (the target's merge rule is read only
    /// for a row with a failed check, once per project).
    async fn reasons(&self, row: &UserListRow) -> Result<Vec<AttentionReason>, ForgeError> {
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
        let required = self
            .protection(row.merge_request.project_id)
            .await?
            .merge_rule(&row.merge_request.target_branch)
            .required_checks;
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
        let shared = Shared::default();
        let items = Decider::new(self, proof, query.scope, &shared)
            .visible(rows)
            .await?;
        Ok(UserListPage { items, next_before })
    }

    /// `GET /api/v1/user/merge-requests/counts` (`UserMergeRequestList`):
    /// the visible rows of every scope, each capped at [`COUNT_CAP`]; the
    /// three scopes are counted concurrently and share the project reads.
    pub async fn user_merge_request_counts(
        &self,
        proof: &Authorized<UserMergeRequestsCtx>,
    ) -> Result<UserListCounts, ForgeError> {
        require(proof.action(), Action::UserMergeRequestList)?;
        let shared = Shared::default();
        let (authored, approvable, attention) = tokio::try_join!(
            self.count(proof, UserListScope::Authored, &shared),
            self.count(proof, UserListScope::Approvable, &shared),
            self.count(proof, UserListScope::Attention, &shared),
        )?;
        Ok(UserListCounts {
            authored,
            approvable,
            attention,
        })
    }

    async fn count(
        &self,
        proof: &Authorized<UserMergeRequestsCtx>,
        scope: UserListScope,
        shared: &Shared,
    ) -> Result<UserListCount, ForgeError> {
        let decider = Decider::new(self, proof, scope, shared);
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
                    truncated: false,
                });
            }
            if !full {
                return Ok(UserListCount {
                    count,
                    capped: false,
                    truncated: false,
                });
            }
        }
        // The scan bound was reached on a full page: more candidates may
        // remain, so the count is a lower bound (said, never silently low).
        Ok(UserListCount {
            count,
            capped: false,
            truncated: true,
        })
    }
}
