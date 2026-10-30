//! The repository checks of `forge doctor` (step D4 MR3b):
//!
//! - `repositories.provisioning` — `FG-REPO-0012` (alert, runbook
//!   repository-provisioning) when a repository has waited in
//!   `provisioning` longer than `PROVISIONING_STUCK_AFTER` (10 min) or has
//!   no queued / running provisioning job (it died).
//! - `repositories.storage` — `FG-REPO-0002` when a `ready` repository has
//!   no directory under the storage root (any other rejection of its path
//!   keeps its own code, e.g. `FG-REPO-0001` for a symlink). At most
//!   [`STORAGE_CHECK_CAP`] ready repositories are checked per run, in id
//!   order; beyond that the check says `truncated` (warn without a code, or
//!   in the failure's context).
//!
//! Both read the database on the read pool; the storage check only stats
//! the file system (`StorageRoot::existing`), never creates anything.

use std::time::Duration;

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
    /// The first missing directory (`FG-REPO-0002`).
    first_missing: Option<ForgeError>,
    /// The first other rejection of a path (symlink, not a directory, I/O).
    first_other: Option<ForgeError>,
    truncated: bool,
}

/// `repositories.storage` over at most `cap` ready repositories.
pub(super) async fn storage_check(db: &Db, root: &StorageRoot, cap: i64) -> Check {
    let walk = match walk(db, root, cap).await {
        Ok(walk) => walk,
        Err(e) => return fail(STORAGE, e),
    };
    let with_counts = |e: ForgeError| {
        e.with_context("missing", walk.missing)
            .with_context("checked", walk.checked)
            .with_context("truncated", walk.truncated)
    };
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

async fn walk(db: &Db, root: &StorageRoot, cap: i64) -> Result<Walk, ForgeError> {
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
        for id in page {
            walk.checked += 1;
            match root.existing(id) {
                Ok(_) => {}
                Err(e) if e.code() == ErrorCode::StorageMissing => {
                    walk.missing += 1;
                    walk.first_missing.get_or_insert(e);
                }
                Err(e) => {
                    walk.first_other.get_or_insert(e);
                }
            }
        }
        after = Some(last);
    }
}

#[cfg(test)]
#[path = "repositories_tests.rs"]
mod tests;
