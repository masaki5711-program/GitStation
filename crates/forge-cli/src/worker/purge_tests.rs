//! The purge task against a migrated database and a temporary storage
//! root (D5 MR4): an expired project's directory and rows go, a project
//! not expired stays, a crash between the phases resumes, a missing
//! directory is fine, a storage failure is `FG-REPO-0020` (logged once)
//! and retried on the next run, and a purging repository is never
//! provisioned.

use std::sync::Arc;
use std::time::Duration;

use forge_core::ids::{ProjectId, UserId};
use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{JobRunner, Ran, WorkerId};
use forge_db::projects::{self, purge as db_purge, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use serde_json::Value;

use super::*;
use crate::worker::loop_tests::Captured;
use crate::worker::provision::tests::storage;
use crate::worker::provision::{ProvisionHandler, Storage};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// A new user `name` and their project `app` (repository `provisioning`,
/// its job queued).
pub(crate) async fn project(t: &FreshDb, name: &str) -> (UserId, CreatedProject) {
    let user = users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user");
    let made = projects::create(
        &t.db,
        &ctx(&format!("req-project-{name}")),
        &NewProject::new("app", user.namespace_id, "App", user.user_id),
    )
    .await
    .expect("project");
    (user.user_id, made)
}

/// Run the queued provisioning job: the bare directory exists afterwards.
pub(crate) async fn provision(t: &FreshDb, storage: &Storage) {
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    let handler = ProvisionHandler::new(&t.db, storage.clone()).expect("handler");
    runner.register(Arc::new(handler)).expect("register");
    let ran = runner.run_once().await.expect("claim").expect("a job ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
}

/// `project` deleted by `by`, its grace period running for a day. A
/// fixture write (forge-hosting alone calls the `*_decided` commands,
/// `forge-hosting/tests/decided_callers.rs`).
pub(crate) async fn delete(t: &FreshDb, project: ProjectId, by: UserId) {
    t.commit_audited(
        ("project", project.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now(), \
             purge_after = now() + interval '1 day', deleted_by = {} WHERE id = {}",
            by.get(),
            project.get()
        )],
    )
    .await;
}

/// The grace period of `project` is over.
pub(crate) async fn expire(t: &FreshDb, project: ProjectId) {
    t.commit_audited(
        ("project", project.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now() - interval '2 days', \
             purge_after = now() - interval '1 second' WHERE id = {}",
            project.get()
        )],
    )
    .await;
}

fn purger(t: &FreshDb, storage: &Storage) -> Purger {
    Purger::new(&t.db, storage.root.clone()) // clone: the purger's own handle
}

/// One batch, capturing the logs.
async fn batch(purger: &Purger) -> (Vec<Purged>, Vec<Value>) {
    let logs = Captured::default();
    let guard = logs.install();
    let done = purge_batch(purger, BATCH).await.expect("batch");
    drop(guard);
    (done, logs.events())
}

fn named<'a>(events: &'a [Value], event: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["fields"]["event"] == event)
        .collect()
}

/// `SELECT count(*) AS n …` through the superuser connection.
async fn count(t: &FreshDb, sql: &str) -> i64 {
    let rows = t.json_rows(sql).await;
    rows.first()
        .and_then(|r| r["n"].as_i64())
        .unwrap_or_else(|| panic!("{sql}: {rows:?}"))
}

/// Whether the project row is there, and its repository's state.
async fn rows_of(t: &FreshDb, project: ProjectId) -> (i64, Option<String>) {
    let id = project.get();
    let projects = count(
        t,
        &format!("SELECT count(*) AS n FROM forge.projects WHERE id = {id}"),
    )
    .await;
    let state = t
        .json_rows(&format!(
            "SELECT state FROM forge.repositories WHERE project_id = {id}"
        ))
        .await
        .first()
        .and_then(|r| r["state"].as_str().map(str::to_owned));
    (projects, state)
}

#[tokio::test]
async fn an_expired_project_is_removed_from_disk_and_database_once() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let (alice, web) = project(&t, "alice").await;
    provision(&t, &storage).await;
    let path = storage
        .root
        .existing(web.repository_id)
        .expect("provisioned");
    delete(&t, web.project_id, alice).await;
    expire(&t, web.project_id).await;
    let (done, events) = batch(&purger(&t, &storage)).await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].project_id, web.project_id);
    assert_eq!(
        done[0].outcome,
        Outcome::Purged {
            storage_removed: true
        }
    );
    assert!(!path.exists(), "{}", path.display());
    assert_eq!(rows_of(&t, web.project_id).await, (0, None));
    let purged = named(&events, "forge.repo.purged");
    assert_eq!(purged.len(), 1, "{events:?}");
    assert_eq!(purged[0]["fields"]["project_id"], web.project_id.get());
    assert_eq!(purged[0]["fields"]["storage_removed"], true);
    assert_eq!(purged[0]["fields"]["resumed"], false);
    // Nothing left to do: the next run finds nothing, the event exists once.
    let (again, _) = batch(&purger(&t, &storage)).await;
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.events WHERE event_type = 'project.purged'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn projects_not_expired_or_not_deleted_are_untouched() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let (alice, web) = project(&t, "alice").await;
    provision(&t, &storage).await;
    let (_bob, live) = project(&t, "bob").await;
    provision(&t, &storage).await;
    delete(&t, web.project_id, alice).await;
    let (done, events) = batch(&purger(&t, &storage)).await;
    assert!(done.is_empty(), "{done:?}");
    assert!(named(&events, "forge.repo.purged").is_empty());
    for made in [&web, &live] {
        assert!(storage.root.existing(made.repository_id).is_ok());
        assert_eq!(
            rows_of(&t, made.project_id).await,
            (1, Some("ready".to_owned()))
        );
    }
    assert_eq!(
        run_purge(&purger(&t, &storage)).await.expect("run"),
        Next::AfterPeriod
    );
    t.drop_now().await;
}

/// A crash after phase 1 (the repository `purging`, its directory still
/// there): the next run resumes and finishes; it never goes back to ready.
#[tokio::test]
async fn a_crash_between_the_phases_resumes_on_the_next_run() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let (alice, web) = project(&t, "alice").await;
    provision(&t, &storage).await;
    delete(&t, web.project_id, alice).await;
    expire(&t, web.project_id).await;
    let started = db_purge::start(&t.db, &ctx("req-crashed"), web.project_id)
        .await
        .expect("phase 1");
    assert!(matches!(started, db_purge::Started::Started { .. }));
    assert_eq!(
        rows_of(&t, web.project_id).await,
        (1, Some("purging".to_owned()))
    );
    let (done, events) = batch(&purger(&t, &storage)).await;
    assert_eq!(
        done[0].outcome,
        Outcome::Purged {
            storage_removed: true
        }
    );
    let purged = named(&events, "forge.repo.purged");
    assert_eq!(purged[0]["fields"]["resumed"], true);
    assert!(storage.root.existing(web.repository_id).is_err());
    assert_eq!(rows_of(&t, web.project_id).await, (0, None));
    t.drop_now().await;
}

/// A directory already gone (never provisioned, or removed by a run that
/// crashed before phase 3) is not an error.
#[tokio::test]
async fn a_missing_directory_is_purged_idempotently() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let (alice, web) = project(&t, "alice").await;
    delete(&t, web.project_id, alice).await;
    expire(&t, web.project_id).await;
    let (done, _) = batch(&purger(&t, &storage)).await;
    assert_eq!(
        done[0].outcome,
        Outcome::Purged {
            storage_removed: false
        }
    );
    assert_eq!(rows_of(&t, web.project_id).await, (0, None));
    t.drop_now().await;
}

/// A storage failure (here a file where the repository should be, then a
/// removal cut by its deadline): `FG-REPO-0020` with `step=storage` and the
/// cause's code, logged once per run; the repository stays `purging` and
/// the rows stay; once the cause is gone the next run finishes it.
#[tokio::test]
async fn a_failed_purge_is_repository_purge_stuck_and_retried_next_run() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let (alice, web) = project(&t, "alice").await;
    delete(&t, web.project_id, alice).await;
    expire(&t, web.project_id).await;
    let path = storage.root.path_for(web.repository_id);
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    std::fs::write(&path, b"not a repository").expect("file");
    let (done, events) = batch(&purger(&t, &storage)).await;
    assert_eq!(
        done[0].outcome,
        Outcome::Failed(ErrorCode::RepositoryPurgeStuck)
    );
    let failures: Vec<&Value> = events
        .iter()
        .filter(|e| e["fields"]["error_code"] == "FG-REPO-0020")
        .collect();
    assert_eq!(failures.len(), 1, "logged once: {events:?}");
    let fields = &failures[0]["fields"];
    let context = fields["context"].as_str().expect("context");
    assert!(context.contains("step=storage"), "{context}");
    assert!(context.contains("cause_code=FG-REPO-0001"), "{context}");
    assert_eq!(fields["request_id"], done[0].request_id.as_str());
    assert_eq!(
        rows_of(&t, web.project_id).await,
        (1, Some("purging".to_owned()))
    );
    // A removal slower than its deadline is the same failure, retryable.
    std::fs::remove_file(&path).expect("unlink");
    std::fs::create_dir_all(&path).expect("a directory again");
    let mut slow = purger(&t, &storage);
    slow.storage_timeout = Duration::ZERO;
    let (done, events) = batch(&slow).await;
    assert_eq!(
        done[0].outcome,
        Outcome::Failed(ErrorCode::RepositoryPurgeStuck)
    );
    let text = format!("{events:?}");
    assert!(text.contains("cause_code=FG-REPO-0003"), "{text}");
    assert!(text.contains("reason=deadline"), "{text}");
    assert!(path.exists(), "nothing removed past the deadline");
    // The next run with time to spare finishes it.
    let (done, _) = batch(&purger(&t, &storage)).await;
    assert_eq!(
        done[0].outcome,
        Outcome::Purged {
            storage_removed: true
        }
    );
    assert!(!path.exists());
    assert_eq!(rows_of(&t, web.project_id).await, (0, None));
    t.drop_now().await;
}

/// The provisioning job of a repository whose purge started creates
/// nothing (`outcome=purging`), and the purge then removes the rows.
#[tokio::test]
async fn a_purging_repository_is_never_provisioned() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let (alice, web) = project(&t, "alice").await;
    delete(&t, web.project_id, alice).await;
    expire(&t, web.project_id).await;
    let started = db_purge::start(&t.db, &ctx("req-start"), web.project_id)
        .await
        .expect("phase 1");
    assert!(matches!(started, db_purge::Started::Started { .. }));
    let logs = Captured::default();
    let guard = logs.install();
    provision(&t, &storage).await;
    drop(guard);
    let events = logs.events();
    let provisioned = named(&events, "forge.repo.provisioned");
    assert_eq!(provisioned.len(), 1, "{events:?}");
    assert_eq!(provisioned[0]["fields"]["outcome"], "purging");
    assert!(!storage.root.path_for(web.repository_id).exists());
    let (done, _) = batch(&purger(&t, &storage)).await;
    assert_eq!(
        done[0].outcome,
        Outcome::Purged {
            storage_removed: false
        }
    );
    t.drop_now().await;
}

/// A run starts no project after its budget: the rest waits, untouched,
/// for the next run (`forge.repo.purge_cut`).
#[tokio::test]
async fn a_run_past_its_budget_leaves_the_rest_for_the_next_run() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let (alice, web) = project(&t, "alice").await;
    delete(&t, web.project_id, alice).await;
    expire(&t, web.project_id).await;
    let mut spent = purger(&t, &storage);
    spent.run_budget = Duration::ZERO;
    let (done, events) = batch(&spent).await;
    assert!(done.is_empty(), "{done:?}");
    let cut = named(&events, "forge.repo.purge_cut");
    assert_eq!(cut.len(), 1, "{events:?}");
    assert_eq!(cut[0]["fields"]["left"], 1);
    assert_eq!(rows_of(&t, web.project_id).await.0, 1);
    let (done, _) = batch(&purger(&t, &storage)).await;
    assert_eq!(done.len(), 1);
    t.drop_now().await;
}
