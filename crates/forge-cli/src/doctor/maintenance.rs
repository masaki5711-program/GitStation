//! The repository maintenance checks of `forge doctor` (M3 O3, ADR-0024
//! "実装で決めたこと（O3）", runbook repo-maintenance), from the ledger
//! `forge.repository_maintenance` on the read pool:
//!
//! - `repositories.maintenance` — `FG-GIT-0018` `reason=failing` when a
//!   repository's maintenance failed twice or more in a row (the alert of
//!   a warn code: one failure is retried the next night), else
//!   `reason=overdue` when a ready repository is more than one
//!   `maintenance.interval_days` past its due time (no worker runs the
//!   maintenance, or its window never comes);
//!   O3 review: also `reason=stale_running` (a row still `running` past
//!   the job lease: the run is gone and not reaped yet) and
//!   `reason=no_recent_success` (no run succeeded within twice the
//!   interval: skipped or failing every night without turning overdue);
//!   the order of precedence is failing, stale_running, no_recent_success,
//!   overdue;
//! - `repositories.integrity` — `FG-GIT-0019` when the last fsck of a
//!   repository failed;
//! - `repositories.stale_locks` — `FG-GIT-0020` (alert) when a ready
//!   repository has a `packed-refs.lock` older than [`STALE_LOCK_AFTER`]
//!   (every push deleting a packed ref fails while it stays); at most
//!   [`LOCK_CHECK_CAP`] repositories per run within [`LOCK_CHECK_TIMEOUT`],
//!   the stat calls on the blocking pool;
//! - `repositories.refs` — a warn (no code: measure only, plan 11 Q5) when
//!   a repository has more refs than `maintenance.refs_warn`.
//!
//! Each failure names at most `REPORT_LIST_MAX` repositories (ids only).

use std::time::{Duration, Instant, SystemTime};

use forge_core::config::MaintenanceConfig;
use forge_core::ids::RepoId;
use forge_db::maintenance::{report, Flagged, MaintenanceReport};
use forge_db::repositories::{ready_ids, READY_PAGE_MAX};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::StorageRoot;

use super::{fail, ok, skip, warn, Check};

pub(super) const MAINTENANCE: &str = "repositories.maintenance";
pub(super) const INTEGRITY: &str = "repositories.integrity";
pub(super) const REFS: &str = "repositories.refs";
pub(super) const STALE_LOCKS: &str = "repositories.stale_locks";
/// A `packed-refs.lock` older than this belongs to no running git (writers
/// hold it for seconds; pack-refs of 100 000 loose refs about 7 s).
pub(super) const STALE_LOCK_AFTER: Duration = Duration::from_secs(600);
/// At most this many ready repositories are looked at per run.
pub(super) const LOCK_CHECK_CAP: i64 = 10_000;
/// The whole walk takes at most this.
pub(super) const LOCK_CHECK_TIMEOUT: Duration = Duration::from_secs(60);

fn ids(list: &[Flagged]) -> String {
    list.iter()
        .map(|f| f.repo_id.get().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The three lines of `report` (pure: the tests feed reports directly).
pub(super) fn maintenance_lines(r: &MaintenanceReport, cfg: &MaintenanceConfig) -> [Check; 3] {
    let maintenance = if r.failing > 0 {
        let worst = r.failing_list.first().map_or(0, |f| f.value);
        fail(
            MAINTENANCE,
            ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("reason", "failing")
                .with_context("repositories", r.failing)
                .with_context("max_consecutive", worst)
                .with_context("repo_ids", ids(&r.failing_list))
                .with_context(
                    "last_error_code",
                    r.failing_list
                        .first()
                        .and_then(|f| f.error_code.as_deref())
                        .unwrap_or(""),
                ),
        )
    } else if r.stale_running > 0 {
        fail(
            MAINTENANCE,
            ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("reason", "stale_running")
                .with_context("repositories", r.stale_running)
                .with_context(
                    "max_running_secs",
                    r.stale_running_list.first().map_or(0, |f| f.value),
                )
                .with_context("repo_ids", ids(&r.stale_running_list)),
        )
    } else if r.no_recent_success > 0 {
        fail(
            MAINTENANCE,
            ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("reason", "no_recent_success")
                .with_context("repositories", r.no_recent_success)
                .with_context(
                    "max_since_success_secs",
                    r.no_recent_success_list.first().map_or(0, |f| f.value),
                )
                .with_context("interval_secs", cfg.interval.as_secs())
                .with_context("repo_ids", ids(&r.no_recent_success_list)),
        )
    } else if r.overdue > 0 {
        fail(
            MAINTENANCE,
            ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("reason", "overdue")
                .with_context("repositories", r.overdue)
                .with_context(
                    "max_overdue_secs",
                    r.overdue_list.first().map_or(0, |f| f.value),
                )
                .with_context("interval_secs", cfg.interval.as_secs())
                .with_context("repo_ids", ids(&r.overdue_list)),
        )
    } else {
        ok(
            MAINTENANCE,
            &format!(
                "no repository failing or overdue (every {} days)",
                cfg.interval.as_secs() / 86_400
            ),
        )
    };
    let integrity = if r.corrupt > 0 {
        fail(
            INTEGRITY,
            ForgeError::new(ErrorCode::RepositoryCorrupt)
                .with_context("reason", "last_fsck_failed")
                .with_context("repositories", r.corrupt)
                .with_context("repo_ids", ids(&r.corrupt_list)),
        )
    } else {
        ok(
            INTEGRITY,
            "the last fsck of every checked repository is clean",
        )
    };
    let refs = if r.refs_over > 0 {
        warn(
            REFS,
            &format!(
                "{} repositories have more than {} refs (largest {}: repository {}); \
                 measure only, pushes are not refused (runbook repo-maintenance)",
                r.refs_over,
                cfg.refs_warn,
                r.refs_max,
                r.refs_list.first().map_or(0, |f| f.repo_id.get())
            ),
        )
    } else {
        ok(
            REFS,
            &format!(
                "largest ref count {} (warn above {})",
                r.refs_max, cfg.refs_warn
            ),
        )
    };
    [maintenance, integrity, refs]
}

/// `repositories.maintenance`, `repositories.integrity`, `repositories.refs`.
pub(super) async fn maintenance_checks(db: &Db, cfg: &MaintenanceConfig) -> [Check; 3] {
    match report(db, cfg.interval, cfg.refs_warn).await {
        Ok(Some(r)) => maintenance_lines(&r, cfg),
        Ok(None) => [MAINTENANCE, INTEGRITY, REFS].map(|n| skip(n, "schema not migrated")),
        Err(e) => [
            fail(MAINTENANCE, e),
            skip(INTEGRITY, "maintenance ledger unavailable"),
            skip(REFS, "maintenance ledger unavailable"),
        ],
    }
}

/// What the lock walk found.
#[derive(Default)]
struct LockWalk {
    checked: i64,
    /// The first error looking at a lock (kept for the detail).
    first_error: Option<String>,
    stale: Vec<(RepoId, u64)>,
    /// Repositories whose lock could not be looked at (their own code is
    /// `repositories.storage`'s).
    unchecked: i64,
    truncated: bool,
}

/// `repositories.stale_locks` (module docs). `root`: the opened storage
/// root (`None`: skipped, the `storage` check fails).
pub(super) async fn stale_locks_check(db: &Db, root: Option<&StorageRoot>) -> Check {
    let Some(root) = root else {
        return skip(STALE_LOCKS, "storage root unavailable");
    };
    stale_locks_within(
        db,
        root,
        LOCK_CHECK_CAP,
        STALE_LOCK_AFTER,
        LOCK_CHECK_TIMEOUT,
    )
    .await
}

pub(super) async fn stale_locks_within(
    db: &Db,
    root: &StorageRoot,
    cap: i64,
    stale_after: Duration,
    timeout: Duration,
) -> Check {
    let walk = match lock_walk(db, root, cap, stale_after, Instant::now() + timeout).await {
        Ok(w) => w,
        Err(e) => return fail(STALE_LOCKS, e),
    };
    if !walk.stale.is_empty() {
        let mut stale = walk.stale;
        stale.sort_by_key(|s| std::cmp::Reverse(s.1));
        let ids: Vec<String> = stale
            .iter()
            .take(20)
            .map(|(id, _)| id.get().to_string())
            .collect();
        return fail(
            STALE_LOCKS,
            ForgeError::new(ErrorCode::RepositoryRefLockStale)
                .with_context("reason", "packed_refs_lock")
                .with_context("repositories", stale.len())
                .with_context("max_age_secs", stale.first().map_or(0, |s| s.1))
                .with_context("repo_ids", ids.join(","))
                .with_context("checked", walk.checked)
                .with_context("unchecked", walk.unchecked)
                .with_context("truncated", walk.truncated),
        );
    }
    let detail = format!(
        "no packed-refs.lock older than {} min in {} ready repositories{}{}",
        stale_after.as_secs() / 60,
        walk.checked,
        if walk.unchecked > 0 {
            format!(
                " ({} could not be looked at, first: {}; see repositories.storage)",
                walk.unchecked,
                walk.first_error.as_deref().unwrap_or("")
            )
        } else {
            String::new()
        },
        if walk.truncated {
            format!("; more exist (truncated at {cap})")
        } else {
            String::new()
        }
    );
    if walk.truncated || walk.unchecked > 0 {
        warn(STALE_LOCKS, &detail)
    } else {
        ok(STALE_LOCKS, &detail)
    }
}

async fn lock_walk(
    db: &Db,
    root: &StorageRoot,
    cap: i64,
    stale_after: Duration,
    deadline: Instant,
) -> Result<LockWalk, ForgeError> {
    let out_of_time = |walk: &LockWalk| {
        ForgeError::new(ErrorCode::StorageIoFailed)
            .with_context("operation", "doctor_stale_locks")
            .with_context("reason", "check_timeout")
            .with_context("checked", walk.checked)
    };
    let mut walk = LockWalk::default();
    let mut after: Option<RepoId> = None;
    loop {
        let left = cap.saturating_sub(walk.checked);
        if left <= 0 {
            walk.truncated = !ready_ids(db, after, 1).await?.is_empty();
            return Ok(walk);
        }
        let page = ready_ids(db, after, left.min(READY_PAGE_MAX)).await?;
        let Some(last) = page.last().copied() else {
            return Ok(walk);
        };
        let time_left = deadline.saturating_duration_since(Instant::now());
        if time_left.is_zero() {
            return Err(out_of_time(&walk));
        }
        let root = root.clone(); // clone: owned by the blocking task
        let task = tokio::task::spawn_blocking(move || {
            let now = SystemTime::now();
            page.into_iter()
                .map(|id| {
                    (
                        id,
                        forge_git::maintenance::packed_refs_lock_age(&root, id, now),
                    )
                })
                .collect::<Vec<_>>()
        });
        let results = match tokio::time::timeout(time_left, task).await {
            Ok(Ok(r)) => r,
            Ok(Err(join)) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("operation", "doctor_stale_locks")
                    .with_context("reason", "blocking_task_failed")
                    .with_source(join))
            }
            // The stat calls may still finish; nothing waits for them.
            Err(_) => return Err(out_of_time(&walk)),
        };
        for (id, age) in results {
            walk.checked += 1;
            match age {
                Ok(Some(age)) if age >= stale_after => walk.stale.push((id, age.as_secs())),
                Ok(_) => {}
                Err(e) => {
                    walk.unchecked += 1;
                    walk.first_error.get_or_insert_with(|| {
                        format!("repository {}: {} {}", id.get(), e.code().id(), e)
                    });
                }
            }
        }
        after = Some(last);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

    use forge_core::ids::RepoId;

    use super::*;
    use crate::doctor::Outcome;

    fn flagged(id: i64, value: i64, code: Option<&str>) -> Flagged {
        Flagged {
            repo_id: RepoId::new(id),
            value,
            error_code: code.map(str::to_owned),
        }
    }

    fn code_of(c: &Check) -> (ErrorCode, String) {
        let Outcome::Fail(e) = &c.outcome else {
            panic!("{}: a failure", c.name);
        };
        (e.code(), e.to_string())
    }

    /// Overdue → FG-GIT-0018 reason=overdue; two failures in a row →
    /// reason=failing (named first); a failed fsck → FG-GIT-0019; many refs
    /// → a warn without a code; a clean ledger → three ok lines.
    #[test]
    fn doctor_maintenance_lines_name_overdue_failing_corrupt_and_refs() {
        let cfg = MaintenanceConfig::default();
        let clean = maintenance_lines(&MaintenanceReport::default(), &cfg);
        assert!(clean.iter().all(|c| matches!(c.outcome, Outcome::Ok(_))));
        let overdue = MaintenanceReport {
            overdue: 2,
            overdue_list: vec![flagged(7, 900_000, None), flagged(9, 700_000, None)],
            ..MaintenanceReport::default()
        };
        let [m, i, r] = maintenance_lines(&overdue, &cfg);
        let (code, text) = code_of(&m);
        assert_eq!(code, ErrorCode::RepositoryMaintenanceFailed);
        assert!(
            text.contains("reason=overdue") && text.contains("repo_ids=7,9"),
            "{text}"
        );
        assert!(matches!(i.outcome, Outcome::Ok(_)));
        assert!(matches!(r.outcome, Outcome::Ok(_)));
        let bad = MaintenanceReport {
            failing: 1,
            failing_list: vec![flagged(3, 2, Some("FG-GIT-0002"))],
            corrupt: 1,
            corrupt_list: vec![flagged(4, 0, Some("FG-GIT-0019"))],
            refs_over: 1,
            refs_list: vec![flagged(5, 200_000, None)],
            refs_max: 200_000,
            ..overdue
        };
        let [m, i, r] = maintenance_lines(&bad, &cfg);
        let (_, text) = code_of(&m);
        assert!(
            text.contains("reason=failing") && text.contains("last_error_code=FG-GIT-0002"),
            "{text}"
        );
        let (code, text) = code_of(&i);
        assert_eq!(code, ErrorCode::RepositoryCorrupt);
        assert!(text.contains("repo_ids=4"), "{text}");
        let Outcome::Warn(detail) = &r.outcome else {
            panic!("refs warn");
        };
        assert!(
            detail.contains("200000") && detail.contains("repository 5"),
            "{detail}"
        );
    }

    /// O3 review: a run that is gone (`stale_running`) and a repository
    /// with no success for twice the interval (`no_recent_success`) are
    /// FG-GIT-0018, after `failing` and before `overdue`.
    #[test]
    fn doctor_maintenance_lines_name_stale_runs_and_no_recent_success() {
        let cfg = MaintenanceConfig::default();
        let gone = MaintenanceReport {
            stale_running: 1,
            stale_running_list: vec![flagged(8, 7200, None)],
            no_recent_success: 1,
            no_recent_success_list: vec![flagged(9, 1_300_000, None)],
            overdue: 1,
            overdue_list: vec![flagged(9, 700_000, None)],
            ..MaintenanceReport::default()
        };
        let [m, _, _] = maintenance_lines(&gone, &cfg);
        let (code, text) = code_of(&m);
        assert_eq!(code, ErrorCode::RepositoryMaintenanceFailed);
        assert!(
            text.contains("reason=stale_running") && text.contains("repo_ids=8"),
            "{text}"
        );
        let skipped = MaintenanceReport {
            stale_running: 0,
            stale_running_list: Vec::new(),
            ..gone
        };
        let [m, _, _] = maintenance_lines(&skipped, &cfg);
        let (_, text) = code_of(&m);
        assert!(
            text.contains("reason=no_recent_success") && text.contains("repo_ids=9"),
            "{text}"
        );
    }

    /// O3 review HIGH: a `packed-refs.lock` older than the threshold in a
    /// ready repository is FG-GIT-0020 (alert); a fresh one (a writer at
    /// work) is not.
    #[tokio::test]
    async fn a_stale_packed_refs_lock_is_reported() {
        use std::sync::Arc;

        use forge_db::jobs::{JobRunner, WorkerId};
        use forge_db::test_support::FreshDb;

        use crate::worker::provision::tests::{project, storage};
        use crate::worker::provision::ProvisionHandler;

        let t = FreshDb::migrated().await;
        let (_root, storage) = storage();
        let p = project(&t, "lou").await;
        let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
        runner
            .register(Arc::new(
                ProvisionHandler::new(&t.db, storage.clone()).expect("handler"), // clone: test handles
            ))
            .expect("register");
        runner
            .run_once()
            .await
            .expect("claim")
            .expect("provisioned");
        let lock = storage
            .root
            .path_for(p.repository_id)
            .join("packed-refs.lock");
        let check = || {
            stale_locks_within(
                &t.db,
                &storage.root,
                LOCK_CHECK_CAP,
                STALE_LOCK_AFTER,
                LOCK_CHECK_TIMEOUT,
            )
        };
        assert!(matches!(check().await.outcome, Outcome::Ok(_)), "no lock");
        std::fs::write(&lock, b"").expect("fresh lock");
        assert!(
            matches!(check().await.outcome, Outcome::Ok(_)),
            "a fresh lock"
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(&lock)
            .and_then(|f| f.set_modified(SystemTime::now() - Duration::from_secs(3600)))
            .expect("age");
        let c = check().await;
        let (code, text) = code_of(&c);
        assert_eq!(code, ErrorCode::RepositoryRefLockStale);
        assert!(
            text.contains(&format!("repo_ids={}", p.repository_id.get()))
                && text.contains("repositories=1"),
            "{text}"
        );
        t.drop_now().await;
    }
}
