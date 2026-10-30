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
    // b's job dies: stuck at the real threshold; a ready one is not reported.
    kill_job(&t, b.repository_id).await;
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
            project_id: p.project_id
        }
    );
    kill_job(&t, repo).await;
    let queued = reprovision(&t.db, &ctx("req-rp-2"), repo)
        .await
        .expect("queued");
    let Reprovisioned::Queued { job, .. } = queued else {
        panic!("{queued:?}");
    };
    let mut su = t.superuser().await;
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'repository.reprovision_requested'"
        )
        .await,
        1
    );
    let (origin, state): (String, String) = sqlx::query_as(&format!(
        "SELECT origin_request_id, state FROM forge.jobs WHERE id = {}",
        job.get()
    ))
    .fetch_one(&mut su)
    .await
    .expect("job");
    assert_eq!((origin.as_str(), state.as_str()), ("req-rp-2", "queued"));
    let _ = mark_ready(&t.db, &ctx("req-rp-3"), repo)
        .await
        .expect("ready");
    let err = reprovision(&t.db, &ctx("req-rp-4"), repo)
        .await
        .expect_err("ready");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "repository_ready");
    drop(su);
    t.drop_now().await;
}
