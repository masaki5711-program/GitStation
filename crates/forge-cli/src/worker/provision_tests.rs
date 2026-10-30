//! The `repository.provision` handler against a migrated database and a
//! temporary storage root, run by the real job runner: a first run creates
//! and marks ready, a retry after a crash adopts its own directory, and a
//! directory that is not its own is dead-lettered with `FG-REPO-0004`.

use std::sync::Arc;

use forge_core::ids::RepoId;
use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{self, JobRunner, Ran, WorkerId};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use serde_json::Value;

use super::{ProvisionHandler, Storage};
use crate::worker::loop_tests::Captured;

/// A storage root in a temporary directory (kept alive by the guard).
pub(crate) fn storage() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().expect("storage root");
    let storage = Storage::open(dir.path()).expect("open root");
    (dir, storage)
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// A project of a new user `name`: its repository waits in `provisioning`
/// with its queued provisioning job.
pub(crate) async fn project(t: &FreshDb, name: &str) -> CreatedProject {
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

fn runner(t: &FreshDb, storage: &Storage) -> JobRunner {
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    let handler = ProvisionHandler::new(&t.db, storage.clone()).expect("handler");
    runner.register(Arc::new(handler)).expect("register");
    runner
}

/// Run the one ready job, capturing the logs.
async fn run_once(runner: &JobRunner) -> (Ran, Vec<Value>) {
    let logs = Captured::default();
    let guard = logs.install();
    let ran = runner.run_once().await.expect("claim").expect("a job ran");
    drop(guard);
    (ran, logs.events())
}

async fn state(t: &FreshDb, repo: RepoId) -> &'static str {
    repositories::provision_target(&t.db, repo)
        .await
        .expect("target")
        .state
        .as_str()
}

/// The one `forge.repo.provisioned` event's fields.
fn provisioned(events: &[Value]) -> Value {
    let found: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["event"] == "forge.repo.provisioned")
        .collect();
    assert_eq!(found.len(), 1, "{events:?}");
    found[0]["fields"].clone()
}

#[tokio::test]
async fn provisioning_job_initializes_and_marks_ready() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let p = project(&t, "ivo").await;
    let repo = p.repository_id;
    let (ran, events) = run_once(&runner(&t, &storage)).await;
    assert!(
        matches!(ran, Ran::Succeeded(id) if id == p.provision_job),
        "{ran:?}"
    );
    assert_eq!(state(&t, repo).await, "ready");
    let path = storage.root.existing(repo).expect("the repository exists");
    assert!(path.join("HEAD").is_file(), "{}", path.display());
    let fields = provisioned(&events);
    assert_eq!(fields["outcome"], "created");
    assert_eq!(fields["repo_id"], repo.get());
    assert_eq!(fields["project_id"], p.project_id.get());
    assert_eq!(fields["request_id"], "req-project-ivo");
    assert!(fields["duration_ms"].is_u64(), "{fields}");
    // The token is never logged.
    let token = repositories::provision_target(&t.db, repo)
        .await
        .expect("target")
        .token
        .to_string();
    assert!(!format!("{events:?}").contains(&token));
    t.drop_now().await;
}

#[tokio::test]
async fn a_crash_after_rename_is_adopted_on_retry() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let p = project(&t, "jun").await;
    let repo = p.repository_id;
    // A first attempt created and renamed the repository, then the worker
    // died before mark_ready: the directory carries this row's token.
    let token = repositories::provision_target(&t.db, repo)
        .await
        .expect("target")
        .token;
    storage
        .root
        .init_bare(&storage.git, repo, token, None)
        .await
        .expect("the crashed attempt's repository");
    let (ran, events) = run_once(&runner(&t, &storage)).await;
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    assert_eq!(provisioned(&events)["outcome"], "adopted");
    assert_eq!(state(&t, repo).await, "ready");
    t.drop_now().await;
}

#[tokio::test]
async fn a_foreign_directory_at_the_path_is_dead_lettered_with_repo_0004() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let p = project(&t, "kai").await;
    let repo = p.repository_id;
    // A repository forge did not mark (e.g. left over from before a
    // database restore): never adopted, never retried.
    let path = storage.root.path_for(repo);
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    storage
        .git
        .command("init")
        .arg("--bare")
        .arg("--quiet")
        .arg("--template=")
        .path(&path)
        .output()
        .await
        .expect("git init --bare");
    let (ran, events) = run_once(&runner(&t, &storage)).await;
    assert!(matches!(ran, Ran::Dead { .. }), "{ran:?}");
    assert!(
        !events
            .iter()
            .any(|e| e["fields"]["event"] == "forge.repo.provisioned"),
        "{events:?}"
    );
    let health = jobs::health(&t.db)
        .await
        .expect("health")
        .expect("migrated");
    let dead = health.newest_dead.expect("a dead job");
    // Dead on the first attempt: an operator must look at the directory.
    assert_eq!(
        (dead.job_id, dead.attempts, dead.last_error_code.as_str()),
        (p.provision_job.get(), 1, "FG-REPO-0004")
    );
    assert_eq!(state(&t, repo).await, "provisioning");
    assert!(path.join("HEAD").is_file(), "the directory is left alone");
    t.drop_now().await;
}

#[tokio::test]
async fn an_already_ready_repository_is_not_touched_again() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let p = project(&t, "lia").await;
    let repo = p.repository_id;
    // Marked ready by an attempt whose lease was lost before completing.
    let _ = repositories::mark_ready(&t.db, &ctx("req-earlier"), repo)
        .await
        .expect("ready");
    let (ran, events) = run_once(&runner(&t, &storage)).await;
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    assert_eq!(provisioned(&events)["outcome"], "already_ready");
    assert!(
        !storage.root.path_for(repo).exists(),
        "no directory is created for a ready repository"
    );
    t.drop_now().await;
}

// ── the retry classification and the run's deadline (review of 198df93) ──

mod classification {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use forge_core::ids::ProjectId;
    use forge_db::jobs::Retry;
    use forge_db::repositories::MarkReady;
    use forge_errors::{ErrorCode, ForgeError};
    use forge_git::Provisioned;

    use super::super::{db_failure, outcome, storage_failure, Deadlines, COMMIT_RESERVE};

    #[test]
    fn transient_storage_failures_retry_and_the_rest_dead_letter() {
        for code in [
            ErrorCode::GitTimeout,
            ErrorCode::GitBusy,
            ErrorCode::GitUnavailable,
            ErrorCode::GitProcessFailed,
            ErrorCode::StorageIoFailed,
        ] {
            assert_eq!(
                storage_failure(ForgeError::new(code)).retry,
                Retry::Backoff,
                "{code:?}"
            );
        }
        for code in [
            ErrorCode::RepoPathOccupied,
            ErrorCode::StoragePathRejected,
            ErrorCode::ConfigInvalid,
            ErrorCode::Internal,
        ] {
            assert_eq!(
                storage_failure(ForgeError::new(code)).retry,
                Retry::Never,
                "{code:?}"
            );
        }
    }

    #[test]
    fn an_unreachable_database_or_a_failed_query_retries_and_the_rest_dead_letter() {
        for code in [ErrorCode::DbUnavailable, ErrorCode::DbQueryFailed] {
            assert_eq!(
                db_failure(ForgeError::new(code)).retry,
                Retry::Backoff,
                "{code:?}"
            );
        }
        for code in [
            ErrorCode::NotFound,
            ErrorCode::ChangeRejected,
            ErrorCode::AuditMissing,
            ErrorCode::Internal,
        ] {
            assert_eq!(
                db_failure(ForgeError::new(code)).retry,
                Retry::Never,
                "{code:?}"
            );
        }
    }

    #[test]
    fn a_concurrent_mark_ready_is_reported_as_already_ready() {
        let project_id = ProjectId::new(1);
        let created = Provisioned::Created(PathBuf::from("/r/1.git"));
        let adopted = Provisioned::Adopted(PathBuf::from("/r/1.git"));
        assert_eq!(
            outcome(&created, MarkReady::Marked { project_id }),
            "created"
        );
        assert_eq!(
            outcome(&adopted, MarkReady::Marked { project_id }),
            "adopted"
        );
        // Another run marked it between this run's read and its mark_ready.
        for made in [&created, &adopted] {
            assert_eq!(
                outcome(made, MarkReady::AlreadyReady { project_id }),
                "already_ready"
            );
        }
    }

    #[test]
    fn the_storage_work_gets_half_the_job_and_mark_ready_the_rest_before_commit() {
        let started = Instant::now();
        let d = Deadlines::new(started, Duration::from_secs(60));
        assert_eq!(d.storage, started + Duration::from_secs(30));
        let mark = d.mark_ready();
        assert!(mark <= Duration::from_secs(60) - COMMIT_RESERVE, "{mark:?}");
        assert!(mark >= Duration::from_secs(47), "{mark:?}");
        // A tiny timeout still leaves mark_ready its minimum.
        let tiny = Deadlines::new(started, Duration::from_millis(100));
        assert_eq!(tiny.mark_ready(), Duration::from_secs(1));
    }
}

/// A repository id no row has: the handler's lookup is FG-REPO-0005, which
/// no retry can fix — dead at once.
#[tokio::test]
async fn a_job_for_an_unknown_repository_is_dead_lettered_with_repo_0005() {
    use forge_db::jobs::{enqueue, JobKind, JobTimeout, MaxAttempts, NewJob};
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let job = NewJob {
        kind: JobKind::parse(repositories::PROVISION_JOB).expect("kind"),
        payload: serde_json::json!({ "repo_id": 987_654 }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(repositories::PROVISION_TIMEOUT).expect("timeout"),
        dedup_key: None,
        origin_request_id: RequestId::parse("req-unknown-repo").expect("id"),
        origin_event_seq: None,
    };
    let _ = enqueue(&t.db, &job).await.expect("enqueue");
    let (ran, _events) = run_once(&runner(&t, &storage)).await;
    assert!(matches!(ran, Ran::Dead { .. }), "{ran:?}");
    let health = jobs::health(&t.db)
        .await
        .expect("health")
        .expect("migrated");
    let dead = health.newest_dead.expect("a dead job");
    assert_eq!(
        (dead.attempts, dead.last_error_code.as_str()),
        (1, "FG-REPO-0005")
    );
    t.drop_now().await;
}

/// A git that never answers is cut at the run's storage deadline (half the
/// job's timeout) with its own code, FG-GIT-0002, logged once by the
/// runner, and the job is retried — not left to the runner's anonymous
/// timeout.
#[cfg(unix)]
#[tokio::test]
async fn a_hanging_git_is_retried_with_its_own_code() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use forge_git::{Git, GitConfig};

    let t = FreshDb::migrated().await;
    let (_root, mut storage) = storage();
    let bin = tempfile::tempdir().expect("bin dir");
    let fake = bin.path().join("git");
    // `exec`: the killed child is the sleep itself (no orphan holding the pipes).
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 30\n").expect("fake git");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    storage.git = Git::new(GitConfig::new(&fake, storage.root.path()));
    let p = project(&t, "max").await;
    // A 2 s job: the storage work gets 1 s.
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET timeout_ms = 2000 WHERE id = {}",
        p.provision_job.get()
    ))
    .await;
    let logs = Captured::default();
    let guard = logs.install();
    let started = Instant::now();
    let ran = runner(&t, &storage)
        .run_once()
        .await
        .expect("claim")
        .expect("a job ran");
    drop(guard);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cut before the job timeout"
    );
    let Ran::Retrying { failure, .. } = &ran else {
        panic!("{ran:?}");
    };
    assert_eq!(
        failure.code(),
        forge_errors::ErrorCode::GitTimeout,
        "{failure}"
    );
    let failures = logs.failures();
    let codes: Vec<_> = failures
        .iter()
        .map(|e| e["fields"]["error_code"].clone())
        .collect();
    assert!(codes.contains(&Value::from("FG-GIT-0002")), "{failures:?}");
    assert_eq!(state(&t, p.repository_id).await, "provisioning");
    assert!(!storage.root.path_for(p.repository_id).exists());
    t.drop_now().await;
}
