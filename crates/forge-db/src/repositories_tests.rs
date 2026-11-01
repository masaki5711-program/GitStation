//! Repository provisioning against a migrated database (step D4 MR3b):
//! `mark_ready`, the provisioning report, `ready_ids` and `reprovision`.
//! Inspection uses a superuser connection.

use forge_core::RequestId;
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;

use super::*;
use crate::change::Actor;
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

/// resource_type, resource_id, project_id, request_id, details.
type AuditRow = (String, i64, Option<i64>, String, serde_json::Value);
/// aggregate_type, aggregate_id, project_id, dedup_key, payload.
type EventRow = (String, i64, Option<i64>, Option<String>, serde_json::Value);

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn count(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// A project (and its owner `name`) with its repository in `provisioning`.
async fn project(t: &FreshDb, name: &str) -> CreatedProject {
    let user = users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user");
    projects::create(
        &t.db,
        &ctx(&format!("req-project-{name}")),
        &NewProject::new("app", user.namespace_id, "App", user.user_id),
    )
    .await
    .expect("project")
}

/// Let the provisioning job of `repo` die, as the runner would.
async fn kill_job(t: &FreshDb, repo: RepoId) {
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-REPO-0004' WHERE dedup_key = 'repository_provision:{}'",
        repo.get()
    ))
    .await;
}

#[tokio::test]
async fn mark_ready_twice_publishes_one_repository_created() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "mira").await;
    let repo = p.repository_id;
    let target = provision_target(&t.db, repo).await.expect("target");
    assert_eq!(target.project_id, p.project_id);
    assert_eq!(target.state, RepositoryState::Provisioning);
    assert!(!format!("{target:?}").contains(&target.token.to_string()));
    let mut su = t.superuser().await;
    let stored: String = sqlx::query_scalar(&format!(
        "SELECT provision_token::text FROM forge.repositories WHERE id = {}",
        repo.get()
    ))
    .fetch_one(&mut su)
    .await
    .expect("token");
    assert_eq!(target.token.to_string(), stored);

    let first = mark_ready(&t.db, &ctx("req-job-1"), repo)
        .await
        .expect("mark ready");
    assert_eq!(
        first,
        MarkReady::Marked {
            project_id: p.project_id
        }
    );
    let second = mark_ready(&t.db, &ctx("req-job-2"), repo)
        .await
        .expect("again");
    assert_eq!(
        second,
        MarkReady::AlreadyReady {
            project_id: p.project_id
        }
    );
    let (state, ready): (String, bool) = sqlx::query_as(&format!(
        "SELECT state, ready_at IS NOT NULL FROM forge.repositories WHERE id = {}",
        repo.get()
    ))
    .fetch_one(&mut su)
    .await
    .expect("row");
    assert_eq!((state.as_str(), ready), ("ready", true));
    // One audit row on the project, one event of the project.
    let audit: Vec<AuditRow> = sqlx::query_as(
        "SELECT resource_type, resource_id, project_id, request_id, details \
         FROM forge.audit_log WHERE action = 'repository.created'",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit");
    let pid = p.project_id.get();
    let details =
        serde_json::json!({"project_id": pid, "repository_id": repo.get(), "kind": "system"});
    assert_eq!(
        audit,
        vec![(
            "project".to_owned(),
            pid,
            Some(pid),
            "req-job-1".to_owned(),
            details.clone()
        )]
    );
    let events: Vec<EventRow> = sqlx::query_as(
        "SELECT aggregate_type, aggregate_id, project_id, dedup_key, payload \
             FROM forge.events WHERE event_type = 'repository.created'",
    )
    .fetch_all(&mut su)
    .await
    .expect("events");
    assert_eq!(
        events,
        vec![(
            "repository".to_owned(),
            repo.get(),
            Some(pid),
            Some(format!("repository_created:{}", repo.get())),
            details
        )]
    );
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_repository_is_not_found() {
    let t = FreshDb::migrated().await;
    let missing = RepoId::new(4242);
    for err in [
        provision_target(&t.db, missing).await.expect_err("target"),
        mark_ready(&t.db, &ctx("req-nf-1"), missing)
            .await
            .expect_err("mark"),
        reprovision(&t.db, &ctx("req-nf-2"), missing)
            .await
            .expect_err("reprovision"),
    ] {
        assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
        assert!(err.to_string().contains("resource=repository"), "{err}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_report_finds_old_and_jobless_provisioning_repositories() {
    let t = FreshDb::migrated().await;
    let a = project(&t, "ana").await;
    let b = project(&t, "ben").await;
    // Fresh, both with a queued job: nothing stuck at the real threshold.
    let report = provisioning_report(&t.db, PROVISIONING_STUCK_AFTER)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(report.provisioning, 2);
    assert!(report.stuck.is_empty(), "{report:?}");
    report.check().expect("nothing stuck");
    // A threshold of zero: both are older than it, still queued.
    let report = provisioning_report(&t.db, Duration::ZERO)
        .await
        .expect("report")
        .expect("migrated");
    let ids: Vec<_> = report.stuck.iter().map(|s| (s.repo_id, s.job)).collect();
    assert_eq!(
        ids,
        vec![
            (a.repository_id, ProvisionJobState::Queued),
            (b.repository_id, ProvisionJobState::Queued)
        ]
    );
    assert!(report
        .check()
        .expect_err("too old")
        .to_string()
        .contains("reason=too_old"));
    assert_eq!((report.job_not_live, report.too_old), (0, 2));
    // b's job dies: job_not_live comes first, whatever its age.
    kill_job(&t, b.repository_id).await;
    let report = provisioning_report(&t.db, Duration::ZERO)
        .await
        .expect("report")
        .expect("migrated");
    let order: Vec<_> = report.stuck.iter().map(|s| (s.repo_id, s.reason)).collect();
    assert_eq!(
        order,
        vec![
            (b.repository_id, StuckReason::JobNotLive),
            (a.repository_id, StuckReason::TooOld)
        ]
    );
    assert_eq!((report.job_not_live, report.too_old), (1, 1));
    // At the real threshold only b is stuck; a ready one is not reported.
    let _ = mark_ready(&t.db, &ctx("req-ready-a"), a.repository_id)
        .await
        .expect("ready");
    let report = provisioning_report(&t.db, PROVISIONING_STUCK_AFTER)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(report.provisioning, 1);
    assert_eq!(report.stuck.len(), 1);
    assert_eq!(report.stuck[0].repo_id, b.repository_id);
    assert_eq!(report.stuck[0].job, ProvisionJobState::None);
    let err = report.check().expect_err("stuck");
    assert_eq!(err.code(), ErrorCode::RepositoryProvisioningStuck);
    assert!(err.to_string().contains("reason=job_not_live"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn ready_ids_pages_through_ready_repositories_only() {
    let t = FreshDb::migrated().await;
    let mut ready = Vec::new();
    for name in ["cai", "dov", "eli"] {
        let p = project(&t, name).await;
        if name != "dov" {
            let _ = mark_ready(&t.db, &ctx(&format!("req-r-{name}")), p.repository_id)
                .await
                .expect("ready");
            ready.push(p.repository_id);
        }
    }
    let first = ready_ids(&t.db, None, 1).await.expect("page 1");
    assert_eq!(first, vec![ready[0]]);
    let rest = ready_ids(&t.db, first.last().copied(), 10)
        .await
        .expect("page 2");
    assert_eq!(rest, vec![ready[1]]);
    assert!(ready_ids(&t.db, Some(ready[1]), 10)
        .await
        .expect("end")
        .is_empty());
    // The page size is clamped, never unbounded or zero.
    assert_eq!(ready_ids(&t.db, None, 0).await.expect("min").len(), 1);
    t.drop_now().await;
}

/// The provisioning jobs of `repo` (base key and the `:after:` chain):
/// `(id, state, dedup_key)` in id order.
async fn provision_jobs(su: &mut PgConnection, repo: RepoId) -> Vec<(i64, String, String)> {
    sqlx::query_as(
        "SELECT id, state, dedup_key FROM forge.jobs WHERE kind = 'repository.provision' \
         AND (dedup_key = $1 OR dedup_key LIKE $1 || ':after:%') ORDER BY id",
    )
    .bind(format!("repository_provision:{}", repo.get()))
    .fetch_all(su)
    .await
    .expect("jobs")
}

/// Let job `id` succeed (the state a restore or a manual change can leave
/// behind with the repository still provisioning).
async fn succeed_job(t: &FreshDb, id: i64) {
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET state = 'succeeded', finished_at = now() WHERE id = {id}"
    ))
    .await;
}

async fn reprovision_audits(su: &mut PgConnection) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT details FROM forge.audit_log \
         WHERE action = 'repository.reprovision_requested' ORDER BY id",
    )
    .fetch_all(su)
    .await
    .expect("audit")
}

#[tokio::test]
async fn reprovision_refuses_a_ready_repository_and_a_live_job() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "fay").await;
    let repo = p.repository_id;
    // The creating job is still queued: nothing to do, nothing written.
    let live = reprovision(&t.db, &ctx("req-rp-1"), repo)
        .await
        .expect("live");
    assert_eq!(
        live,
        Reprovisioned::AlreadyLive {
            project_id: p.project_id,
            job: p.provision_job,
            state: ProvisionJobState::Queued,
        }
    );
    let _ = mark_ready(&t.db, &ctx("req-rp-3"), repo)
        .await
        .expect("ready");
    let err = reprovision(&t.db, &ctx("req-rp-4"), repo)
        .await
        .expect_err("ready");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "repository_ready");
    let mut su = t.superuser().await;
    assert!(reprovision_audits(&mut su).await.is_empty());
    assert_eq!(provision_jobs(&mut su, repo).await.len(), 1);
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn a_dead_holder_is_reenqueued_with_the_original_key() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "gus").await;
    let repo = p.repository_id;
    kill_job(&t, repo).await;
    let queued = reprovision(&t.db, &ctx("req-dead-1"), repo)
        .await
        .expect("queued");
    let Reprovisioned::Queued { job, after, .. } = queued else {
        panic!("{queued:?}");
    };
    assert_eq!(after, None);
    let mut su = t.superuser().await;
    let jobs = provision_jobs(&mut su, repo).await;
    let key = format!("repository_provision:{}", repo.get());
    assert_eq!(
        jobs,
        vec![
            (p.provision_job.get(), "dead".to_owned(), key.clone()),
            (job.get(), "queued".to_owned(), key)
        ]
    );
    let (origin,): (String,) = sqlx::query_as(&format!(
        "SELECT origin_request_id FROM forge.jobs WHERE id = {}",
        job.get()
    ))
    .fetch_one(&mut su)
    .await
    .expect("origin");
    assert_eq!(origin, "req-dead-1");
    assert_eq!(
        reprovision_audits(&mut su).await,
        vec![serde_json::json!({
            "repository_id": repo.get(),
            "job_id": job.get(),
            "holder_job_id": null,
            "holder_state": "none",
        })]
    );
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn a_succeeded_holder_is_followed_by_a_job_keyed_after_it() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "hal").await;
    let repo = p.repository_id;
    // A restore left the repository provisioning behind a succeeded job:
    // doctor names it, and the global dedup still holds the base key.
    succeed_job(&t, p.provision_job.get()).await;
    let report = provisioning_report(&t.db, PROVISIONING_STUCK_AFTER)
        .await
        .expect("report")
        .expect("migrated");
    assert_eq!(report.stuck.len(), 1);
    assert_eq!(report.stuck[0].job, ProvisionJobState::Succeeded);
    assert_eq!(report.stuck[0].reason, StuckReason::JobNotLive);

    let queued = reprovision(&t.db, &ctx("req-succ-1"), repo)
        .await
        .expect("queued");
    let Reprovisioned::Queued { job, after, .. } = queued else {
        panic!("{queued:?}");
    };
    assert_eq!(after, Some(p.provision_job));
    let after_key = format!(
        "repository_provision:{}:after:{}",
        repo.get(),
        p.provision_job.get()
    );
    let mut su = t.superuser().await;
    let jobs = provision_jobs(&mut su, repo).await;
    assert_eq!(
        jobs.last(),
        Some(&(job.get(), "queued".to_owned(), after_key.clone()))
    );
    assert_eq!(
        reprovision_audits(&mut su).await,
        vec![serde_json::json!({
            "repository_id": repo.get(),
            "job_id": job.get(),
            "holder_job_id": p.provision_job.get(),
            "holder_state": "succeeded",
        })]
    );
    // The new job is the end of the chain: live, so doctor is quiet and a
    // second reprovision changes nothing.
    let report = provisioning_report(&t.db, PROVISIONING_STUCK_AFTER)
        .await
        .expect("report")
        .expect("migrated");
    assert!(report.stuck.is_empty(), "{report:?}");
    let again = reprovision(&t.db, &ctx("req-succ-2"), repo)
        .await
        .expect("live");
    assert_eq!(
        again,
        Reprovisioned::AlreadyLive {
            project_id: p.project_id,
            job,
            state: ProvisionJobState::Queued,
        }
    );
    // That one succeeds too without the repository becoming ready: the
    // chain goes one link further.
    succeed_job(&t, job.get()).await;
    let third = reprovision(&t.db, &ctx("req-succ-3"), repo)
        .await
        .expect("queued");
    assert!(
        matches!(third, Reprovisioned::Queued { after: Some(a), .. } if a == job),
        "{third:?}"
    );
    assert_eq!(provision_jobs(&mut su, repo).await.len(), 3);
    assert_eq!(reprovision_audits(&mut su).await.len(), 2);
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn two_concurrent_reprovisions_enqueue_once() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "ida").await;
    let repo = p.repository_id;
    succeed_job(&t, p.provision_job.get()).await;
    let (c1, c2) = (ctx("req-cc-1"), ctx("req-cc-2"));
    let (a, b) = tokio::join!(reprovision(&t.db, &c1, repo), reprovision(&t.db, &c2, repo));
    let (a, b) = (a.expect("first"), b.expect("second"));
    let queued = [a, b]
        .iter()
        .filter(|r| matches!(r, Reprovisioned::Queued { .. }))
        .count();
    let live = [a, b]
        .iter()
        .filter(|r| matches!(r, Reprovisioned::AlreadyLive { .. }))
        .count();
    assert_eq!((queued, live), (1, 1), "{a:?} {b:?}");
    let mut su = t.superuser().await;
    assert_eq!(provision_jobs(&mut su, repo).await.len(), 2);
    assert_eq!(reprovision_audits(&mut su).await.len(), 1);
    drop(su);
    t.drop_now().await;
}

// ── concurrency, pinned (the style of projects/tests/race.rs) ──

/// The row lock both commands take first (a fragment of `lock_repository`).
const ROW_LOCK_WAIT: &str =
    "SELECT project_id, kind, state FROM forge.repositories WHERE id = $1 FOR UPDATE";

/// Wait (bounded) until `n` backends of this test's database wait on a lock
/// in a statement containing `statement`.
async fn until_waiting(t: &FreshDb, statement: &str, n: i64) {
    let mut su = t.superuser().await;
    for _ in 0..500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
             AND datname = current_database() AND strpos(query, $1) > 0",
        )
        .bind(statement)
        .fetch_one(&mut su)
        .await
        .expect("pg_stat_activity");
        if waiting >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("fewer than {n} statements containing {statement:?} waited on a lock within 10 s");
}

/// An open transaction holding `repo`'s row: the commands queue behind it
/// in the order they arrive, then run one after the other.
async fn hold_row(t: &FreshDb, repo: RepoId) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut holder = t.db.write_pool().begin().await.expect("holder");
    sqlx::query("SELECT 1 FROM forge.repositories WHERE id = $1 FOR UPDATE")
        .bind(repo.get())
        .execute(&mut *holder)
        .await
        .expect("lock the row");
    holder
}

async fn created_rows(su: &mut PgConnection, repo: RepoId) -> (i64, i64) {
    let audits = count(
        su,
        "SELECT count(*) FROM forge.audit_log WHERE action = 'repository.created'",
    )
    .await;
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.events WHERE event_type = 'repository.created' \
         AND aggregate_id = $1",
    )
    .bind(repo.get())
    .fetch_one(su)
    .await
    .expect("events");
    (audits, events)
}

#[tokio::test]
async fn two_concurrent_mark_ready_mark_once() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "jon").await;
    let repo = p.repository_id;
    let holder = hold_row(&t, repo).await;
    let (c1, c2) = (ctx("req-mr-1"), ctx("req-mr-2"));
    let release = async {
        until_waiting(&t, ROW_LOCK_WAIT, 2).await;
        holder.commit().await.expect("release");
    };
    let (a, b, ()) = tokio::join!(
        mark_ready(&t.db, &c1, repo),
        mark_ready(&t.db, &c2, repo),
        release
    );
    let mut outcomes = [a.expect("first"), b.expect("second")];
    outcomes.sort_by_key(|o| matches!(o, MarkReady::AlreadyReady { .. }));
    assert_eq!(
        outcomes,
        [
            MarkReady::Marked {
                project_id: p.project_id
            },
            MarkReady::AlreadyReady {
                project_id: p.project_id
            }
        ]
    );
    let mut su = t.superuser().await;
    assert_eq!(created_rows(&mut su, repo).await, (1, 1));
    drop(su);
    t.drop_now().await;
}

/// Pinned, mark_ready first: reprovision waits on the row, then sees the
/// repository ready and refuses; no job is added.
#[tokio::test]
async fn reprovision_after_a_concurrent_mark_ready_is_refused() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "kim").await;
    let repo = p.repository_id;
    kill_job(&t, repo).await;
    let holder = hold_row(&t, repo).await;
    let (c1, c2) = (ctx("req-mr-first"), ctx("req-rp-second"));
    let marking = mark_ready(&t.db, &c1, repo);
    let reprovisioning = async {
        until_waiting(&t, ROW_LOCK_WAIT, 1).await;
        let r = reprovision(&t.db, &c2, repo);
        let release = async {
            until_waiting(&t, ROW_LOCK_WAIT, 2).await;
            holder.commit().await.expect("release");
        };
        let (r, ()) = tokio::join!(r, release);
        r
    };
    let (marked, reprovisioned) = tokio::join!(marking, reprovisioning);
    assert!(matches!(marked, Ok(MarkReady::Marked { .. })), "{marked:?}");
    let err = reprovisioned.expect_err("ready by then");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "repository_ready");
    let mut su = t.superuser().await;
    assert_eq!(provision_jobs(&mut su, repo).await.len(), 1);
    assert_eq!(created_rows(&mut su, repo).await, (1, 1));
    drop(su);
    t.drop_now().await;
}

/// Pinned, reprovision first: the new job is enqueued, then mark_ready
/// marks the repository ready; the new job will find it ready
/// (`already_ready`, the handler never touches the directory).
#[tokio::test]
async fn mark_ready_after_a_concurrent_reprovision_still_marks() {
    let t = FreshDb::migrated().await;
    let p = project(&t, "lou").await;
    let repo = p.repository_id;
    kill_job(&t, repo).await;
    let holder = hold_row(&t, repo).await;
    let (c1, c2) = (ctx("req-rp-first"), ctx("req-mr-second"));
    let reprovisioning = reprovision(&t.db, &c1, repo);
    let marking = async {
        until_waiting(&t, ROW_LOCK_WAIT, 1).await;
        let m = mark_ready(&t.db, &c2, repo);
        let release = async {
            until_waiting(&t, ROW_LOCK_WAIT, 2).await;
            holder.commit().await.expect("release");
        };
        let (m, ()) = tokio::join!(m, release);
        m
    };
    let (reprovisioned, marked) = tokio::join!(reprovisioning, marking);
    assert!(
        matches!(reprovisioned, Ok(Reprovisioned::Queued { .. })),
        "{reprovisioned:?}"
    );
    assert!(matches!(marked, Ok(MarkReady::Marked { .. })), "{marked:?}");
    let mut su = t.superuser().await;
    assert_eq!(provision_jobs(&mut su, repo).await.len(), 2);
    assert_eq!(created_rows(&mut su, repo).await, (1, 1));
    drop(su);
    t.drop_now().await;
}

/// D5 MR4: a repository being purged is never marked ready or provisioned
/// again; its job's state is readable for the purge (`provision_job_state`).
#[tokio::test]
async fn a_purging_repository_is_never_marked_ready_or_reprovisioned() {
    let t = FreshDb::migrated().await;
    let made = project(&t, "alice").await;
    assert_eq!(
        provision_job_state(&t.db, made.repository_id)
            .await
            .expect("job state"),
        ProvisionJobState::Queued
    );
    let owner: i64 = {
        let mut su = t.superuser().await;
        sqlx::query_scalar("SELECT user_id FROM forge.project_members WHERE project_id = $1")
            .bind(made.project_id.get())
            .fetch_one(&mut su)
            .await
            .expect("owner")
    };
    projects::delete_decided(
        &t.db,
        &ctx("req-delete"),
        made.project_id,
        forge_core::ids::UserId::new(owner),
        Duration::from_secs(86_400),
        |_| Ok(()),
    )
    .await
    .expect("delete");
    t.commit_audited(
        ("project", made.project_id.get()),
        &[format!(
            "UPDATE forge.projects SET purge_after = deleted_at WHERE id = {}",
            made.project_id.get()
        )],
    )
    .await;
    let started = projects::purge::start(&t.db, &ctx("req-start"), made.project_id)
        .await
        .expect("start");
    assert!(
        matches!(started, projects::purge::Started::Started { .. }),
        "{started:?}"
    );
    let marked = mark_ready(&t.db, &ctx("req-ready"), made.repository_id)
        .await
        .expect("mark_ready");
    assert_eq!(
        marked,
        MarkReady::Purging {
            project_id: made.project_id
        }
    );
    let err = reprovision(&t.db, &ctx("req-reprovision"), made.repository_id)
        .await
        .expect_err("purging");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "repository_purging");
    let mut su = t.superuser().await;
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'repository.created'"
        )
        .await,
        0
    );
    t.drop_now().await;
}
