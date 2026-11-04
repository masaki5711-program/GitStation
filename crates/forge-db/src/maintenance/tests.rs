//! M3 O3: the maintenance ledger and schedule against a migrated database.

use std::time::Duration;

use forge_core::ids::RepoId;
use forge_core::RequestId;
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::jobs::Enqueued;
use crate::projects::{self, CreatedProject, NewProject};
use crate::repositories::mark_ready;
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn rid() -> RequestId {
    RequestId::parse("req-maint-test").expect("id")
}

/// `n` ready repositories of one user, in creation order.
async fn ready_repos(t: &FreshDb, n: usize) -> Vec<CreatedProject> {
    let user = users::create(
        &t.db,
        &ctx("req-user-maint"),
        &NewUser::human("maint", "maint", "maint@example.test"),
    )
    .await
    .expect("user");
    let mut made = Vec::new();
    for i in 0..n {
        let p = projects::create(
            &t.db,
            &ctx(&format!("req-project-{i}")),
            &NewProject::new(&format!("app{i}"), user.namespace_id, "App", user.user_id),
        )
        .await
        .expect("project");
        let marked = mark_ready(&t.db, &ctx(&format!("req-ready-{i}")), p.repository_id)
            .await
            .expect("ready");
        assert!(matches!(
            marked,
            crate::repositories::MarkReady::Marked { .. }
        ));
        made.push(p);
    }
    made
}

/// A ledger row of `repo` due `hours_ago` (the ledger is operational
/// state: no audit row needed).
async fn due_since(t: &FreshDb, repo: RepoId, hours_ago: i64) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.repository_maintenance (repo_id, project_id, next_due_at) \
         SELECT id, project_id, now() - interval '{hours_ago} hours' \
         FROM forge.repositories WHERE id = {}",
        repo.get()
    ))
    .await;
}

async fn maintain_jobs(t: &FreshDb) -> Vec<(String, String)> {
    let mut su = t.superuser().await;
    sqlx::query_as(
        "SELECT dedup_key, payload->>'repo_id' FROM forge.jobs \
         WHERE kind = 'repository.maintain' ORDER BY id",
    )
    .fetch_all(&mut su)
    .await
    .expect("jobs")
}

fn succeeded(counts: (u64, u64, u64), fsck: (&'static str, bool)) -> Finish {
    Finish {
        outcome: Outcome::Succeeded,
        error_code: None,
        skipped_reason: None,
        counts: Some(counts),
        fsck: Some(fsck),
        next_due_in: Duration::from_secs(7 * 86_400),
    }
}

/// The due repositories are enqueued oldest first (never maintained: by
/// `ready_at`), at most the batch; a second pass while the jobs wait adds
/// nothing (the due time is in the key); a repository maintained
/// meanwhile is not due; a repository that is not ready is never picked.
#[tokio::test]
async fn schedule_picks_due_repos_oldest_first_capped() {
    let t = FreshDb::migrated().await;
    // Never maintained: due since `ready_at` (creation order, 0 oldest);
    // repo 3 has a ledger row due 30 days ago: the oldest of all.
    let repos = ready_repos(&t, 4).await;
    due_since(&t, repos[3].repository_id, 24 * 30).await;
    // One not ready (provisioning) is never due.
    let user = users::create(
        &t.db,
        &ctx("req-user-other"),
        &NewUser::human("other", "other", "other@example.test"),
    )
    .await
    .expect("user");
    let provisioning = projects::create(
        &t.db,
        &ctx("req-project-prov"),
        &NewProject::new("prov", user.namespace_id, "Prov", user.user_id),
    )
    .await
    .expect("project");
    let first = schedule_due(&t.db, 2, &rid()).await.expect("schedule");
    let ids: Vec<RepoId> = first.iter().map(|s| s.repo_id).collect();
    assert_eq!(ids, [repos[3].repository_id, repos[0].repository_id]);
    assert!(first
        .iter()
        .all(|s| matches!(s.enqueued, Enqueued::Queued(_))));
    // The jobs still wait: a second pass dedups them, then takes the next.
    let again = schedule_due(&t.db, 3, &rid()).await.expect("again");
    let kinds: Vec<(RepoId, bool)> = again
        .iter()
        .map(|s| (s.repo_id, matches!(s.enqueued, Enqueued::Deduped)))
        .collect();
    assert_eq!(
        kinds,
        [
            (repos[3].repository_id, true),
            (repos[0].repository_id, true),
            (repos[1].repository_id, false)
        ]
    );
    assert_eq!(maintain_jobs(&t).await.len(), 3);
    // Three live jobs fill a batch of three: nothing new is queued.
    let full = schedule_due(&t.db, 3, &rid()).await.expect("full");
    assert!(full.iter().all(|s| matches!(s.enqueued, Enqueued::Deduped)));
    assert_eq!(maintain_jobs(&t).await.len(), 3, "the live jobs are capped");
    // Maintained: not due for an interval.
    let r0 = repos[0].repository_id;
    start(&t.db, r0).await.expect("start").expect("ready");
    finish(&t.db, r0, &succeeded((3, 1, 2), ("connectivity", true)))
        .await
        .expect("finish");
    let later = schedule_due(&t.db, 10, &rid()).await.expect("later");
    let ids: Vec<RepoId> = later.iter().map(|s| s.repo_id).collect();
    assert!(!ids.contains(&r0), "{ids:?}");
    assert!(!ids.contains(&provisioning.repository_id), "{ids:?}");
    assert!(start(&t.db, provisioning.repository_id)
        .await
        .expect("start")
        .is_none());
    t.drop_now().await;
}

/// start / finish keep the outcome, code, reason, counts and fsck; a
/// failure counts up, a success resets; corrupt marks fsck_ok = false.
#[tokio::test]
async fn ledger_records_outcome_and_counts() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 1).await;
    let r = repos[0].repository_id;
    let s = start(&t.db, r).await.expect("start").expect("ready");
    assert_eq!(
        (s.fsck_ok, s.full_fsck_age_secs, s.failures),
        (None, None, 0)
    );
    finish(&t.db, r, &succeeded((120, 64, 7), ("full", true)))
        .await
        .expect("finish");
    let row1 = row(&t.db, r).await.expect("row").expect("exists");
    assert_eq!(row1.outcome, Some(Outcome::Succeeded));
    assert_eq!(
        (row1.objects, row1.size_pack_kib, row1.refs),
        (Some(120), Some(64), Some(7))
    );
    assert_eq!(row1.fsck_ok, Some(true));
    assert!(row1.full_fsck_done);
    assert!(row1.due_in_secs > 6 * 86_400, "{}", row1.due_in_secs);
    // Two failures in a row count up and keep the code.
    for n in 1..=2 {
        let s = start(&t.db, r).await.expect("start").expect("ready");
        assert_eq!(s.fsck_ok, Some(true));
        assert!(s.full_fsck_age_secs.is_some_and(|a| a < 60));
        let failed = Finish {
            outcome: Outcome::Failed,
            error_code: Some(ErrorCode::RepositoryMaintenanceFailed),
            skipped_reason: None,
            counts: None,
            fsck: None,
            next_due_in: Duration::from_secs(12 * 3600),
        };
        finish(&t.db, r, &failed).await.expect("finish");
        let r_row = super::row(&t.db, r).await.expect("row").expect("exists");
        assert_eq!(r_row.failures, n);
        assert_eq!(r_row.error_code.as_deref(), Some("FG-GIT-0018"));
        assert_eq!(r_row.objects, Some(120), "the last counts stay");
    }
    let rep = super::report(&t.db, Duration::from_secs(7 * 86_400), 5)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(rep.failing, 1);
    assert_eq!(rep.failing_list[0].repo_id, r);
    assert_eq!(rep.refs_over, 1, "7 refs > 5");
    assert_eq!(rep.refs_max, 7);
    // Corrupt: fsck_ok false, counted as a failure, in the corrupt list.
    start(&t.db, r).await.expect("start");
    let corrupt = Finish {
        outcome: Outcome::Corrupt,
        error_code: Some(ErrorCode::RepositoryCorrupt),
        skipped_reason: None,
        counts: None,
        fsck: Some(("connectivity", false)),
        next_due_in: Duration::from_secs(12 * 3600),
    };
    finish(&t.db, r, &corrupt).await.expect("finish");
    let rep = super::report(&t.db, Duration::from_secs(7 * 86_400), 1_000)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(rep.corrupt, 1);
    assert_eq!(
        rep.corrupt_list[0].error_code.as_deref(),
        Some("FG-GIT-0019")
    );
    assert_eq!(rep.refs_over, 0);
    // A skip keeps the failure count; a success resets it.
    let skipped = Finish {
        outcome: Outcome::Skipped,
        error_code: None,
        skipped_reason: Some("incoming_push"),
        counts: None,
        fsck: None,
        next_due_in: Duration::from_secs(3600),
    };
    start(&t.db, r).await.expect("start");
    finish(&t.db, r, &skipped).await.expect("finish");
    let r_row = super::row(&t.db, r).await.expect("row").expect("exists");
    assert_eq!(r_row.outcome, Some(Outcome::Skipped));
    assert_eq!(r_row.skipped_reason.as_deref(), Some("incoming_push"));
    assert_eq!(r_row.failures, 3);
    assert_eq!(r_row.error_code, None);
    start(&t.db, r).await.expect("start");
    finish(&t.db, r, &succeeded((1, 1, 1), ("connectivity", true)))
        .await
        .expect("finish");
    let r_row = super::row(&t.db, r).await.expect("row").expect("exists");
    assert_eq!((r_row.failures, r_row.fsck_ok), (0, Some(true)));
    // A code without a failure (or the reverse) is a caller bug.
    let wrong = Finish {
        error_code: Some(ErrorCode::RepositoryCorrupt),
        ..succeeded((1, 1, 1), ("full", true))
    };
    let err = finish(&t.db, r, &wrong).await.expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Internal);
    t.drop_now().await;
}

/// Overdue: a ready repository more than one interval past its due time.
#[tokio::test]
async fn doctor_report_lists_overdue_repositories() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 2).await;
    due_since(&t, repos[0].repository_id, 24 * 8).await;
    let rep = super::report(&t.db, Duration::from_secs(7 * 86_400), 100_000)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(rep.overdue, 1);
    assert_eq!(rep.overdue_list[0].repo_id, repos[0].repository_id);
    assert!(rep.overdue_list[0].value > 86_400);
    t.drop_now().await;
}

/// The admin trigger enqueues a ready repository at once (its own key per
/// request) and refuses a missing one.
#[tokio::test]
async fn an_admin_maintain_enqueues_one_job_per_request() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 1).await;
    let r = repos[0].repository_id;
    let a = RequestId::parse("req-admin-1").expect("id");
    assert!(matches!(
        enqueue_now(&t.db, r, &a).await.expect("enqueue"),
        Enqueued::Queued(_)
    ));
    assert_eq!(
        enqueue_now(&t.db, r, &a).await.expect("again"),
        Enqueued::Deduped
    );
    let jobs = maintain_jobs(&t).await;
    assert_eq!(
        jobs,
        [(
            format!("repository_maintain:{}:admin:req-admin-1", r.get()),
            r.get().to_string()
        )]
    );
    let err = enqueue_now(&t.db, RepoId::new(999_999), &a)
        .await
        .expect_err("missing");
    assert_eq!(err.code(), ErrorCode::NotFound);
    t.drop_now().await;
}

/// The database knows the configured zone; an unknown one is FG-SYS-0002.
#[tokio::test]
async fn the_window_time_zone_is_resolved_by_the_database() {
    let t = FreshDb::migrated().await;
    let m = local_minute(&t.db, "Asia/Tokyo").await.expect("tokyo");
    assert!(m < 24 * 60);
    let err = local_minute(&t.db, "Mars/Olympus")
        .await
        .expect_err("unknown zone");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{err}");
    assert!(err.to_string().contains("maintenance.window_time_zone"));
    t.drop_now().await;
}

/// Review MEDIUM: a repository's first run keeps the due time its job was
/// keyed with, so a scheduling pass while it runs dedups instead of
/// enqueueing a second job.
#[tokio::test]
async fn a_first_run_keeps_its_due_key_so_the_scheduler_dedups() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 1).await;
    let r = repos[0].repository_id;
    let first = schedule_due(&t.db, 5, &rid()).await.expect("schedule");
    assert!(
        matches!(first[0].enqueued, Enqueued::Queued(_)),
        "{first:?}"
    );
    // The key has whole seconds: start in a later second than `ready_at`.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    start(&t.db, r).await.expect("start").expect("ready");
    let again = schedule_due(&t.db, 5, &rid()).await.expect("again");
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0].enqueued, Enqueued::Deduped);
    assert_eq!(maintain_jobs(&t).await.len(), 1, "one job, not two");
    t.drop_now().await;
}

/// Age the running row of `repo` (operational state: no audit row).
async fn started_hours_ago(t: &FreshDb, repo: RepoId, hours: i64) {
    t.exec_superuser(&format!(
        "UPDATE forge.repository_maintenance SET last_started_at = now() - interval '{hours} hours' \
         WHERE repo_id = {}",
        repo.get()
    ))
    .await;
}

/// Review MEDIUM: a run that crashed and was dead-lettered left its row
/// `running`; the reaper records it as a failure (FG-DB-0007), counts it,
/// and moves its due time, so the scheduler does not enqueue it again
/// every pass. doctor sees it as stale until then.
#[tokio::test]
async fn a_lost_run_is_counted_as_a_failure_and_not_requeued_every_pass() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 1).await;
    let r = repos[0].repository_id;
    start(&t.db, r).await.expect("start").expect("ready");
    started_hours_ago(&t, r, 2).await;
    let rep = super::report(&t.db, Duration::from_secs(7 * 86_400), 100_000)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(rep.stale_running, 1);
    assert_eq!(rep.stale_running_list[0].repo_id, r);
    let reaped = reap_stale_running(&t.db, Duration::from_secs(12 * 3600))
        .await
        .expect("reap");
    assert_eq!(reaped, [(r, 1)], "the failure count comes back");
    let row = super::row(&t.db, r).await.expect("row").expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Failed));
    assert_eq!(row.error_code.as_deref(), Some("FG-DB-0007"));
    assert_eq!(row.failures, 1);
    assert!(row.due_in_secs > 11 * 3600, "{}", row.due_in_secs);
    assert!(
        schedule_due(&t.db, 5, &rid())
            .await
            .expect("schedule")
            .is_empty(),
        "not due again at once"
    );
    assert!(reap_stale_running(&t.db, Duration::from_secs(60))
        .await
        .expect("again")
        .is_empty());
    t.drop_now().await;
}

/// A row still `running` while its job is live is not reaped (a long run
/// whose lease the queue still holds).
#[tokio::test]
async fn a_running_row_with_a_live_job_is_not_reaped() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 1).await;
    let r = repos[0].repository_id;
    enqueue_now(&t.db, r, &rid()).await.expect("enqueue");
    start(&t.db, r).await.expect("start").expect("ready");
    started_hours_ago(&t, r, 2).await;
    assert!(reap_stale_running(&t.db, Duration::from_secs(60))
        .await
        .expect("reap")
        .is_empty());
    t.drop_now().await;
}

/// A retried job that starts over a previous run still `running` past the
/// lease counts that lost run as a failure.
#[tokio::test]
async fn start_counts_a_lost_previous_run() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 1).await;
    let r = repos[0].repository_id;
    let s = start(&t.db, r).await.expect("start").expect("ready");
    assert!(!s.lost_previous);
    // A fresh `running` row (a concurrent run) is not a lost one.
    let s = start(&t.db, r).await.expect("start").expect("ready");
    assert!(!s.lost_previous);
    assert_eq!(s.failures, 0);
    started_hours_ago(&t, r, 2).await;
    let s = start(&t.db, r).await.expect("start").expect("ready");
    assert!(s.lost_previous);
    assert_eq!(s.failures, 1);
    t.drop_now().await;
}

/// Review MEDIUM: a repository skipped every night is never failed nor
/// overdue; doctor sees that no run succeeded within twice the interval.
/// A success clears it.
#[tokio::test]
async fn no_recent_success_is_reported_until_a_run_succeeds() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 2).await;
    let r = repos[0].repository_id;
    start(&t.db, r).await.expect("start").expect("ready");
    let skipped = Finish {
        outcome: Outcome::Skipped,
        error_code: None,
        skipped_reason: Some("incoming_push"),
        counts: None,
        fsck: None,
        next_due_in: Duration::from_secs(3600),
    };
    finish(&t.db, r, &skipped).await.expect("finish");
    t.exec_superuser(&format!(
        "UPDATE forge.repository_maintenance SET last_success_at = now() - interval '15 days' \
         WHERE repo_id = {}",
        r.get()
    ))
    .await;
    let week = Duration::from_secs(7 * 86_400);
    let rep = super::report(&t.db, week, 100_000)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!((rep.no_recent_success, rep.overdue, rep.failing), (1, 0, 0));
    assert_eq!(rep.no_recent_success_list[0].repo_id, r);
    start(&t.db, r).await.expect("start").expect("ready");
    finish(&t.db, r, &succeeded((1, 1, 1), ("connectivity", true)))
        .await
        .expect("finish");
    let rep = super::report(&t.db, week, 100_000)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(rep.no_recent_success, 0, "a success clears it");
    t.drop_now().await;
}

/// Re-review LOW: the backfill (20261117100200) gives a row that succeeded
/// before `last_success_at` existed its last finish; others stay empty.
#[tokio::test]
async fn the_success_backfill_fills_rows_that_succeeded_before_the_column() {
    let t = FreshDb::migrated().await;
    let repos = ready_repos(&t, 2).await;
    let (ok, failed) = (repos[0].repository_id, repos[1].repository_id);
    for (r, outcome) in [(ok, "succeeded"), (failed, "skipped")] {
        t.exec_superuser(&format!(
            "INSERT INTO forge.repository_maintenance \
             (repo_id, project_id, outcome, last_finished_at, next_due_at) \
             SELECT id, project_id, '{outcome}', now() - interval '3 days', now() \
             FROM forge.repositories WHERE id = {}",
            r.get()
        ))
        .await;
    }
    let migration =
        include_str!("../../migrations/20261117100200_repository_maintenance_success_backfill.sql");
    let update = migration
        .split(';')
        .find(|s| s.contains("UPDATE forge.repository_maintenance"))
        .expect("the backfill statement");
    t.exec_superuser(update).await;
    let rows = t
        .json_rows(
            "SELECT repo_id, last_success_at IS NOT NULL AS has FROM forge.repository_maintenance \
             ORDER BY repo_id",
        )
        .await;
    let has: Vec<(i64, bool)> = rows
        .iter()
        .map(|r| {
            (
                r["repo_id"].as_i64().expect("id"),
                r["has"].as_bool().expect("has"),
            )
        })
        .collect();
    assert_eq!(has, [(ok.get(), true), (failed.get(), false)]);
    t.drop_now().await;
}
