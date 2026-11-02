//! The hidden version refs of a merge request (M3 R1b, ADR-0024 §2): the
//! pure planner [`plan_ref_changes`] and [`VersionRefs`], the ref sync under
//! the merge request's refs lock (moved out of `versions.rs` in M3 R2; see
//! that module's documentation for the sync's rules).

use std::collections::BTreeMap;
use std::time::Duration;

use forge_core::ids::MergeRequestId;
use forge_core::{ObjectId, RefName, RepoId, RequestId};
use forge_db::review::{self, RefsLock, VersionsSnapshot};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;
use forge_git::hidden_refs::{
    merge_request_head_ref, merge_request_ref_prefix, merge_request_version_ref,
};
use forge_git::{Git, RefChange, RefUpdateResult, StorageRoot};

/// The longest single git call of a refresh or a sync.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// What a sync must change: the `update-ref` transaction and the refs it
/// takes back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefPlan {
    pub changes: Vec<RefChange>,
    /// `v<n>` refs that held a value no row claims (module docs).
    pub reclaimed: Vec<RefName>,
}

/// The ref changes that make `current` (the refs under
/// `refs/merge-requests/<iid>/`) match `snapshot` (module docs, step 4).
/// Refs under the prefix that no version names (`merge`, reserved for M4)
/// are left alone.
pub fn plan_ref_changes(
    snapshot: &VersionsSnapshot,
    current: &BTreeMap<RefName, ObjectId>,
) -> Result<RefPlan, ForgeError> {
    let mut plan = RefPlan::default();
    let mut want = |name: RefName, value: Option<&ObjectId>, reclaim: bool| {
        match (value, current.get(&name)) {
            (Some(new), None) => plan.changes.push(RefChange::Create {
                name,
                new: new.clone(), // clone: the change owns its value
            }),
            (Some(new), Some(old)) if old != new => {
                if reclaim {
                    plan.reclaimed.push(name.clone()); // clone: listed and changed
                }
                plan.changes.push(RefChange::Update {
                    name,
                    new: new.clone(), // clone: the change owns its value
                    old: old.clone(), // clone: the change owns its value
                });
            }
            (None, Some(old)) => plan.changes.push(RefChange::Delete {
                name,
                old: old.clone(), // clone: the change owns its value
            }),
            (Some(_), Some(_)) | (None, None) => {}
        }
    };
    for v in &snapshot.versions {
        let name = merge_request_version_ref(snapshot.iid, v.version_no)?;
        want(name, (!v.refs_purged).then_some(&v.head_sha), true);
    }
    // `head` follows the merge request's head, unless that version was purged.
    let head = snapshot.head_sha.as_ref().filter(|h| {
        snapshot
            .versions
            .iter()
            .any(|v| v.head_sha == **h && !v.refs_purged)
    });
    want(merge_request_head_ref(snapshot.iid)?, head, false);
    Ok(plan)
}

/// What a sync did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Ref changes applied (0: everything was in place, git did not run).
    pub changes: usize,
    pub reclaimed: usize,
}

/// Where the hidden refs are written: the repositories' storage and git,
/// and (in `forge serve`) the browse API whose caches a write drops.
#[derive(Clone)]
pub struct VersionRefs {
    git: Git,
    storage: StorageRoot,
    browser: Option<Browser>,
}

impl std::fmt::Debug for VersionRefs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionRefs").finish_non_exhaustive()
    }
}

/// `refs/merge-requests/<iid>` (for-each-ref matches whole components).
fn refs_pattern(iid: i64) -> Result<RefName, ForgeError> {
    let prefix = merge_request_ref_prefix(iid)?;
    RefName::parse(prefix.trim_end_matches('/'))
}

impl VersionRefs {
    pub fn new(git: Git, storage: StorageRoot) -> Self {
        Self {
            git,
            storage,
            browser: None,
        }
    }

    /// Drop `browser`'s caches of a repository after every write.
    #[must_use]
    pub fn with_browser(mut self, browser: Browser) -> Self {
        self.browser = Some(browser);
        self
    }

    /// The tip of `branch` in `repo` (`None`: the branch does not exist),
    /// the git call bounded by `deadline` too.
    pub async fn branch_tip(
        &self,
        repo: RepoId,
        branch: &RefName,
        deadline: Deadline,
    ) -> Result<Option<ObjectId>, ForgeError> {
        let timeout = git_budget(deadline, "branch_tip")?;
        self.git
            .read_ref(&self.storage, repo, branch, timeout)
            .await
    }

    /// The merge base of `a` and `b` in `repo` (`None`: no common
    /// history), the git call bounded by `deadline` too.
    pub async fn merge_base(
        &self,
        repo: RepoId,
        a: &ObjectId,
        b: &ObjectId,
        deadline: Deadline,
    ) -> Result<Option<ObjectId>, ForgeError> {
        let timeout = git_budget(deadline, "merge_base")?;
        self.git
            .merge_base(&self.storage, repo, a, b, timeout)
            .await
    }

    /// One plan and write under `lock`: every git call gets what is left of
    /// the hold (at most [`GIT_TIMEOUT`]), so a slow git fails as
    /// FG-GIT-0002, and the lock is proven alive right before the write.
    async fn sync_once(
        &self,
        lock: &mut RefsLock,
        repo: RepoId,
        request_id: &RequestId,
    ) -> Result<Option<SyncReport>, ForgeError> {
        let deadline = lock.deadline();
        let Some(snapshot) = review::versions_snapshot(lock).await? else {
            return Ok(None);
        };
        let current = self
            .git
            .read_refs(
                &self.storage,
                repo,
                &refs_pattern(snapshot.iid)?,
                git_budget(deadline, "read_refs")?,
            )
            .await?;
        let plan = plan_ref_changes(&snapshot, &current)?;
        if !plan.changes.is_empty() {
            lock.alive().await?;
        }
        let applied = self
            .git
            .update_refs(
                &self.storage,
                repo,
                &plan.changes,
                git_budget(deadline, "update_refs")?,
            )
            .await?;
        for name in &plan.reclaimed {
            tracing::info!(
                target: "forge_review",
                event = "forge.review.version_ref_reclaimed",
                request_id = %request_id,
                repo_id = repo.get(),
                merge_request_id = snapshot.merge_request.get(),
                r#ref = name.as_str(),
                "a version ref held a value no version claims; taken back"
            );
        }
        let changes = match applied {
            RefUpdateResult::Applied { changes } => changes,
            RefUpdateResult::Nothing => 0,
        };
        if changes > 0 {
            if let Some(browser) = &self.browser {
                browser.invalidate(repo);
            }
        }
        Ok(Some(SyncReport {
            changes,
            reclaimed: plan.reclaimed.len(),
        }))
    }

    /// [`Self::sync`] under a lock the caller holds (the version job holds
    /// it for its whole run). A refused transaction (a ref moved between
    /// the read and the write — by hand, or a run whose lock was lost) is
    /// logged once at warn and planned again from fresh reads; a second
    /// refusal is returned (`FG-GIT-0003`, the job retries).
    pub async fn sync_locked(
        &self,
        lock: &mut RefsLock,
        repo: RepoId,
        request_id: &RequestId,
    ) -> Result<Option<SyncReport>, ForgeError> {
        match self.sync_once(lock, repo, request_id).await {
            Err(e) if e.code() == ErrorCode::GitProcessFailed => {
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.version_refs_retry",
                    request_id = %request_id,
                    error_code = e.code().id(),
                    repo_id = repo.get(),
                    merge_request_id = lock.merge_request().get(),
                    error = %e,
                    "the version ref transaction was refused; planning again"
                );
                self.sync_once(lock, repo, request_id).await
            }
            other => other,
        }
    }

    /// Make the hidden refs of merge request `mr` (in `repo`) match its
    /// committed versions (module docs, step 4), under its lock (taken and
    /// released here). `Ok(None)`: the merge request is gone.
    pub async fn sync(
        &self,
        db: &Db,
        repo: RepoId,
        mr: MergeRequestId,
        request_id: &RequestId,
    ) -> Result<Option<SyncReport>, ForgeError> {
        let mut lock = review::lock_version_refs(db, mr).await?;
        let synced = self.sync_locked(&mut lock, repo, request_id).await;
        lock.release().await;
        synced
    }
}

/// The end of a locked run (`RefsLock::deadline`).
pub type Deadline = tokio::time::Instant;

/// What is left before `deadline`, at most [`GIT_TIMEOUT`];
/// `FG-GIT-0002 operation=version_refs` when nothing is.
fn git_budget(deadline: Deadline, step: &'static str) -> Result<Duration, ForgeError> {
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    if left.is_zero() {
        return Err(ForgeError::new(ErrorCode::GitTimeout)
            .with_context("operation", "version_refs")
            .with_context("step", step)
            .with_context("timeout_ms", review::REFS_LOCK_HOLD.as_millis()));
    }
    Ok(left.min(GIT_TIMEOUT))
}
