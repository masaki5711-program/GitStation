//! The repository checks of `forge doctor` (step D4 MR3b):
//!
//! - `repositories.provisioning` — `FG-REPO-0012` (alert, runbook
//!   repository-provisioning) when a repository has waited in
//!   `provisioning` longer than `PROVISIONING_STUCK_AFTER` (10 min) or has
//!   no queued / running provisioning job at the end of its key chain (dead,
//!   missing, or succeeded without the repository becoming ready);
//!   classified in SQL, with per-reason counts (`job_not_live`, `too_old`).
//! - `repositories.storage` — `FG-REPO-0002` when a `ready` repository has
//!   no directory under the storage root (any other rejection of its path
//!   keeps its own code, e.g. `FG-REPO-0001` for a symlink); the failure
//!   carries `missing` and `other` counts. At most [`STORAGE_CHECK_CAP`]
//!   ready repositories are checked per run, in id order; beyond that the
//!   check says `truncated` (warn without a code, or in the failure's
//!   context). The walk is bounded by [`STORAGE_CHECK_TIMEOUT`] as a whole
//!   (the stat calls run on the blocking pool): out of time it fails with
//!   `FG-REPO-0003 reason=check_timeout`. This check is the only detector
//!   of a ready repository whose directory vanished.
//!
//! Both read the database on the read pool; the storage check only stats
//! the file system (`StorageRoot::existing`), never creates anything.

use std::time::{Duration, Instant};

use forge_core::ids::RepoId;
use forge_db::repositories::{self, READY_PAGE_MAX};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::StorageRoot;

use super::{fail, ok, skip, warn, Check};

const PROVISIONING: &str = "repositories.provisioning";
const STORAGE: &str = "repositories.storage";
/// At most this many ready repositories are checked for their directory.
pub(super) const STORAGE_CHECK_CAP: i64 = 10_000;
/// The whole storage walk (database pages and stat calls) takes at most this.
pub(super) const STORAGE_CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// `repositories.provisioning` and `repositories.storage`. `root` is the
/// opened storage root (`None`: the `storage` check fails, so this one is
/// skipped).
pub(super) async fn repository_checks(
    db: &Db,
    root: Option<&StorageRoot>,
    stuck_after: Duration,
) -> [Check; 2] {
    let provisioning = match repositories::provisioning_report(db, stuck_after).await {
        Ok(Some(report)) => match report.check() {
            Ok(()) => ok(PROVISIONING, &report.summary()),
            Err(e) => fail(PROVISIONING, e),
        },
        Ok(None) => {
            return [
                skip(PROVISIONING, "schema not migrated"),
                skip(STORAGE, "schema not migrated"),
            ]
        }
        Err(e) => fail(PROVISIONING, e),
    };
    let storage = match root {
        Some(root) => storage_check(db, root, STORAGE_CHECK_CAP).await,
        None => skip(STORAGE, "storage root unavailable"),
    };
    [provisioning, storage]
}

/// What the walk over ready repositories found.
#[derive(Default)]
struct Walk {
    checked: i64,
    missing: i64,
    /// Other rejections of a path (symlink, not a directory, I/O).
    other: i64,
    /// The first missing directory (`FG-REPO-0002`).
    first_missing: Option<ForgeError>,
    /// The first other rejection.
    first_other: Option<ForgeError>,
    truncated: bool,
}

impl Walk {
    fn record(&mut self, result: Result<(), ForgeError>) {
        self.checked += 1;
        match result {
            Ok(()) => {}
            Err(e) if e.code() == ErrorCode::StorageMissing => {
                self.missing += 1;
                self.first_missing.get_or_insert(e);
            }
            Err(e) => {
                self.other += 1;
                self.first_other.get_or_insert(e);
            }
        }
    }
}

/// `repositories.storage` over at most `cap` ready repositories, within
/// [`STORAGE_CHECK_TIMEOUT`].
pub(super) async fn storage_check(db: &Db, root: &StorageRoot, cap: i64) -> Check {
    storage_check_within(db, root, cap, STORAGE_CHECK_TIMEOUT).await
}

pub(super) async fn storage_check_within(
    db: &Db,
    root: &StorageRoot,
    cap: i64,
    timeout: Duration,
) -> Check {
    let walk = match walk(db, root, cap, Instant::now() + timeout).await {
        Ok(walk) => walk,
        Err(e) => return fail(STORAGE, e),
    };
    let with_counts = |e: ForgeError| {
        e.with_context("missing", walk.missing)
            .with_context("other", walk.other)
            .with_context("checked", walk.checked)
            .with_context("truncated", walk.truncated)
    };
    // A missing directory first (its runbook is the one to follow), then
    // any other rejection; both carry both counts.
    if let Some(e) = walk.first_missing {
        return fail(STORAGE, with_counts(e));
    }
    if let Some(e) = walk.first_other {
        return fail(STORAGE, with_counts(e));
    }
    if walk.truncated {
        return warn(
            STORAGE,
            &format!(
                "{} ready repositories have their directory; more exist and were not checked \
                 (truncated at {cap})",
                walk.checked
            ),
        );
    }
    ok(
        STORAGE,
        &format!("{} ready repositories have their directory", walk.checked),
    )
}

/// Stat one page of repositories on the blocking pool (`std::fs`; a hung
/// volume must not stall the runtime), bounded by `deadline`.
async fn stat_page(
    root: &StorageRoot,
    page: Vec<RepoId>,
    deadline: Instant,
    walk: &Walk,
) -> Result<Vec<Result<(), ForgeError>>, ForgeError> {
    let out_of_time = || {
        ForgeError::new(ErrorCode::StorageIoFailed)
            .with_context("operation", "doctor_storage")
            .with_context("reason", "check_timeout")
            .with_context("checked", walk.checked)
            .with_context("missing", walk.missing)
            .with_context("other", walk.other)
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(out_of_time());
    }
    let root = root.clone(); // clone: owned by the blocking task
    let task = tokio::task::spawn_blocking(move || {
        page.into_iter()
            .map(|id| root.existing(id).map(|_| ()))
            .collect::<Vec<_>>()
    });
    match tokio::time::timeout(left, task).await {
        Ok(Ok(results)) => Ok(results),
        Ok(Err(join)) => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "doctor_storage")
            .with_context("reason", "blocking_task_failed")
            .with_source(join)),
        // The stat calls may still finish; nothing waits for them.
        Err(_) => Err(out_of_time()),
    }
}

async fn walk(
    db: &Db,
    root: &StorageRoot,
    cap: i64,
    deadline: Instant,
) -> Result<Walk, ForgeError> {
    let mut walk = Walk::default();
    let mut after: Option<RepoId> = None;
    loop {
        let left = cap.saturating_sub(walk.checked);
        if left <= 0 {
            // Anything beyond the cap is reported, never silently skipped.
            walk.truncated = !repositories::ready_ids(db, after, 1).await?.is_empty();
            return Ok(walk);
        }
        let page = repositories::ready_ids(db, after, left.min(READY_PAGE_MAX)).await?;
        let Some(last) = page.last().copied() else {
            return Ok(walk);
        };
        for result in stat_page(root, page, deadline, &walk).await? {
            walk.record(result);
        }
        after = Some(last);
    }
}

#[cfg(test)]
#[path = "repositories_tests.rs"]
mod tests;
