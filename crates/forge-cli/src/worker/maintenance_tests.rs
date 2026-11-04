//! M3 O3: the maintenance handler, its own runner and the settings checks,
//! against a migrated database and real git (synthetic content).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use forge_core::config::MaintenanceConfig;
use forge_core::ids::RepoId;
use forge_core::RequestId;
use forge_db::jobs::{JobRunner, Ran, WorkerId, LEASE_SLACK};
use forge_db::maintenance::{
    self as ledger, Outcome, MAINTAIN_JOB, MAINTAIN_JOB_TIMEOUT, RUN_RESERVE,
};
use forge_db::review::merges::lock_merges;
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_git::{Git, GitConfig};

use super::*;
use crate::worker::jobs;
use crate::worker::loop_tests::Captured;
use crate::worker::provision::tests::{project, storage};
use crate::worker::provision::ProvisionHandler;

const BUDGET: Duration = Duration::from_secs(600);

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

/// A ready repository of a new user `name`, created on disk by the real
/// provisioning handler.
async fn ready_repo(t: &FreshDb, storage: &Storage, name: &str) -> RepoId {
    let p = project(t, name).await;
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    runner
        .register(Arc::new(
            ProvisionHandler::new(&t.db, storage.clone()).expect("handler"), // clone: test handles
        ))
        .expect("register");
    let ran = runner.run_once().await.expect("claim").expect("a job ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    p.repository_id
}

/// Plumbing on the bare repository (test setup only).
fn plumb(repo: &Path, args: &[&str], input: Option<&str>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input.as_bytes()).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A branch `main` with one commit; returns its blob id.
fn with_history(repo: &Path) -> String {
    let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some("content\n"));
    let tree = plumb(
        repo,
        &["mktree"],
        Some(&format!("100644 blob {blob}\tREADME\n")),
    );
    let commit = plumb(repo, &["commit-tree", &tree], Some("synthetic\n"));
    plumb(repo, &["update-ref", "refs/heads/main", &commit], None);
    blob
}

fn handler(t: &FreshDb, storage: &Storage) -> MaintainHandler {
    MaintainHandler::new(&t.db, storage, &MaintenanceConfig::default()).expect("handler")
}

fn events_named<'a>(events: &'a [serde_json::Value], name: &str) -> Vec<&'a serde_json::Value> {
    events
        .iter()
        .filter(|e| e["fields"]["event"] == name)
        .collect()
}

/// The prune grace must be longer than the merge reconciliation window
/// plus the merge job's whole retry budget (plan 11 §5 O3, Q2), and at
/// least 24 h; the defaults pass.
#[test]
fn prune_grace_must_exceed_reconcile_window_and_retry_budget() {
    let needed = merge_window_and_budget();
    // 5 leases of (300 s + 30 s), the backoff 10 + 20 + 40 + 80 s, then
    // the reconciliation's executing bound (600 + 30 s) and idle (600 s).
    assert_eq!(needed, Duration::from_secs(5 * 330 + 150 + 630 + 600));
    check_settings(&MaintenanceConfig::default()).expect("the defaults pass");
    let day = Duration::from_secs(24 * 3600);
    check_prune_grace(day, needed).expect("24 h is enough today");
    let err = check_prune_grace(day, day).expect_err("not longer than the window");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    let text = err.to_string();
    assert!(
        text.contains("maintenance.prune_grace_hours") && text.contains("below_merge_window"),
        "{text}"
    );
    let err = check_prune_grace(Duration::from_secs(3600), Duration::ZERO).expect_err("1 h");
    assert!(err.to_string().contains("below_min"), "{err}");
}

/// The job's lease (timeout + slack) outlives every step: each step's
/// timeout is cut to the run's deadline (the job's timeout minus the
/// ledger reserve), and the largest configurable step fits the run.
#[test]
fn lease_outlives_step_timeouts() {
    let job = ledger::maintain_job(RepoId::new(1), "repository_maintain:1:0", rid("req-lease"))
        .expect("job");
    assert_eq!(job.timeout.get(), MAINTAIN_JOB_TIMEOUT);
    let lease = job.timeout.get() + LEASE_SLACK;
    let run = job.timeout.get().saturating_sub(RUN_RESERVE);
    let max_step = Duration::from_secs(u64::from(MaintenanceConfig::MAX_STEP_TIMEOUT_SECS));
    assert!(max_step + RUN_RESERVE <= job.timeout.get());
    assert!(run < lease);
    // A step never gets more than the run has left.
    let deadline = RunDeadline::new(Duration::from_millis(200), max_step);
    let first = deadline.step().expect("time left");
    assert!(first <= Duration::from_millis(200), "{first:?}");
    std::thread::sleep(Duration::from_millis(250));
    let err = deadline.step().expect_err("the run is over");
    assert_eq!(err.code(), ErrorCode::GitTimeout);
    assert!(err.to_string().contains("run_deadline"), "{err}");
    // The configuration's own range keeps a step inside the run.
    let mut cfg = MaintenanceConfig {
        step_timeout: max_step,
        ..MaintenanceConfig::default()
    };
    check_settings(&cfg).expect("the largest step fits");
    cfg.step_timeout = MAINTAIN_JOB_TIMEOUT;
    let err = check_settings(&cfg).expect_err("a step as long as the job");
    assert!(err.to_string().contains("above_job_timeout"), "{err}");
}

/// A git that never ends (Unix `sleep`, Windows `ping`). Synthetic.
#[cfg(unix)]
fn hanging_git(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("hang.sh");
    std::fs::write(&path, "#!/bin/sh\nsleep 120\n").expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[cfg(windows)]
fn hanging_git(dir: &Path) -> PathBuf {
    let path = dir.join("hang.cmd");
    std::fs::write(&path, "@echo off\r\nping -n 120 127.0.0.1 >nul\r\n").expect("script");
    path
}

async fn job_state(t: &FreshDb, kind: &str) -> Vec<String> {
    t.json_rows(&format!(
        "SELECT state FROM forge.jobs WHERE kind = '{kind}' ORDER BY id"
    ))
    .await
    .into_iter()
    .map(|r| r["state"].as_str().expect("state").to_owned())
    .collect()
}

/// The main runner never registers `repository.maintain`; the
/// maintenance runner registers only it. While a maintenance job hangs in
/// git, the main runner still provisions a new repository at once.
#[tokio::test]
async fn maintenance_has_its_own_runner_and_never_blocks_provisioning() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "ann").await;
    let main_kinds: Vec<String> = jobs::handlers(&t.db, &storage)
        .expect("handlers")
        .iter()
        .map(|h| h.kind().as_str().to_owned())
        .collect();
    assert!(
        !main_kinds.iter().any(|k| k == MAINTAIN_JOB),
        "{main_kinds:?}"
    );
    let lane = runner(&t.db, &storage, &MaintenanceConfig::default()).expect("lane");
    let lane_kinds: Vec<String> = lane.kinds().iter().map(|k| k.as_str().to_owned()).collect();
    assert_eq!(lane_kinds, [MAINTAIN_JOB]);
    // A maintenance job whose git hangs.
    let tmp = tempfile::tempdir().expect("tmp");
    let mut cfg = GitConfig::new(hanging_git(tmp.path()), storage.git.config().home.clone());
    cfg.timeout_total = Duration::from_secs(120);
    let hanging = MaintainHandler::with_git(
        &t.db,
        storage.root.clone(), // clone: the handler's own root handle
        Git::new(cfg),
        &MaintenanceConfig::default(),
    )
    .expect("handler");
    let mut slow = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    slow.register(Arc::new(hanging)).expect("register");
    ledger::enqueue_now(&t.db, repo, &rid("req-maint-hang"))
        .await
        .expect("enqueue");
    let running = tokio::spawn(async move { slow.run_once().await });
    for _ in 0..200 {
        if job_state(&t, MAINTAIN_JOB).await == ["running"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(job_state(&t, MAINTAIN_JOB).await, ["running"]);
    // A new project's provisioning runs on the main runner meanwhile.
    let other = project(&t, "bob").await;
    let mut main = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    for h in jobs::handlers(&t.db, &storage).expect("handlers") {
        main.register(h).expect("register");
    }
    let ran = tokio::time::timeout(Duration::from_secs(30), main.run_once())
        .await
        .expect("not blocked by the maintenance")
        .expect("claim")
        .expect("a job ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let ready = t
        .json_rows(&format!(
            "SELECT state FROM forge.repositories WHERE id = {}",
            other.repository_id.get()
        ))
        .await;
    assert_eq!(ready[0]["state"], "ready");
    assert_eq!(
        job_state(&t, MAINTAIN_JOB).await,
        ["running"],
        "still in git"
    );
    // Dropping the run kills the hanging git's tree.
    running.abort();
    let _aborted = running.await;
    t.drop_now().await;
}

/// A merge holding `merge:<repo_id>` makes the run leave pack-refs out
/// (the rest runs; the ledger says which step); without it refs are packed.
#[tokio::test]
async fn pack_refs_skips_when_merge_lock_held() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "cid").await;
    let path = storage.root.path_for(repo);
    with_history(&path);
    let inner = handler(&t, &storage).inner();
    let lock = lock_merges(&t.db, repo, Duration::from_secs(5))
        .await
        .expect("a merge holds the lock");
    inner
        .maintain_repo(repo, &rid("req-maint-locked"), 1, BUDGET)
        .await
        .expect("run");
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Succeeded));
    assert_eq!(
        row.skipped_reason.as_deref(),
        Some("pack_refs_merge_lock_held")
    );
    assert!(path.join("refs/heads/main").exists(), "still a loose ref");
    lock.release().await;
    inner
        .maintain_repo(repo, &rid("req-maint-free"), 1, BUDGET)
        .await
        .expect("run");
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.skipped_reason, None);
    assert!(!path.join("refs/heads/main").exists(), "packed");
    let packed = std::fs::read_to_string(path.join("packed-refs")).expect("packed-refs");
    assert!(packed.contains("refs/heads/main"), "{packed}");
    t.drop_now().await;
}

/// A whole run: the ledger gets the outcome, the counts and the fsck; the
/// started / completed events carry the job's request id.
#[tokio::test]
async fn a_maintenance_run_records_the_ledger_and_logs_its_events() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "dee").await;
    with_history(&storage.root.path_for(repo));
    let inner = handler(&t, &storage).inner();
    let logs = Captured::default();
    let guard = logs.install();
    inner
        .maintain_repo(repo, &rid("req-maint-run"), 1, BUDGET)
        .await
        .expect("run");
    drop(guard);
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Succeeded));
    assert_eq!((row.objects, row.refs), (Some(3), Some(1)));
    assert_eq!(row.fsck_ok, Some(true));
    assert_eq!(
        row.last_fsck_mode.as_deref(),
        Some("full"),
        "the first fsck is full"
    );
    assert!(row.full_fsck_done);
    assert!(row.due_in_secs > 6 * 86_400);
    let events = logs.events();
    let started = events_named(&events, "forge.git.maintenance_started");
    let completed = events_named(&events, "forge.git.maintenance_completed");
    assert_eq!((started.len(), completed.len()), (1, 1), "{events:?}");
    for e in [started[0], completed[0]] {
        assert_eq!(e["fields"]["request_id"], "req-maint-run");
        assert_eq!(e["fields"]["repo_id"], repo.get());
    }
    assert_eq!(completed[0]["fields"]["outcome"], "succeeded");
    assert_eq!(completed[0]["fields"]["refs"], 1);
    // The next run's fsck is connectivity-only (the full one is fresh).
    inner
        .maintain_repo(repo, &rid("req-maint-run-2"), 1, BUDGET)
        .await
        .expect("run");
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.last_fsck_mode.as_deref(), Some("connectivity"));
    t.drop_now().await;
}

/// A corrupt repository (a reachable blob lost) is recorded `corrupt` with
/// FG-GIT-0019 at error level, and nothing is pruned or packed.
#[tokio::test]
async fn a_corrupt_repository_is_recorded_and_not_pruned() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "eve").await;
    let path = storage.root.path_for(repo);
    let blob = with_history(&path);
    let (dir, file) = blob.split_at(2);
    let lost = path.join("objects").join(dir).join(file);
    let mut perms = std::fs::metadata(&lost).expect("blob").permissions();
    #[allow(clippy::permissions_set_readonly_false)] // test setup: git writes 0444
    perms.set_readonly(false);
    std::fs::set_permissions(&lost, perms).expect("writable");
    std::fs::remove_file(&lost).expect("lose the blob");
    let inner = handler(&t, &storage).inner();
    let logs = Captured::default();
    let guard = logs.install();
    inner
        .maintain_repo(repo, &rid("req-maint-corrupt"), 1, BUDGET)
        .await
        .expect("recorded, not a job failure");
    drop(guard);
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Corrupt));
    assert_eq!(row.error_code.as_deref(), Some("FG-GIT-0019"));
    assert_eq!((row.fsck_ok, row.failures), (Some(false), 1));
    assert!(path.join("refs/heads/main").exists(), "no pack-refs ran");
    assert!(
        !path
            .join("objects/pack")
            .read_dir()
            .expect("pack dir")
            .any(|e| e
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".pack")),
        "no gc ran"
    );
    let failures: Vec<_> = logs
        .events()
        .into_iter()
        .filter(|e| e["fields"]["error_code"] == "FG-GIT-0019")
        .collect();
    assert_eq!(failures.len(), 1, "logged once");
    assert_eq!(failures[0]["level"], "ERROR");
    assert_eq!(failures[0]["fields"]["request_id"], "req-maint-corrupt");
    t.drop_now().await;
}

/// A repository that is not ready (still provisioning) is left alone: no
/// row, a `not_ready` skip in the log.
#[tokio::test]
async fn a_repository_not_ready_is_skipped() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let p = project(&t, "fay").await;
    let inner = handler(&t, &storage).inner();
    inner
        .maintain_repo(p.repository_id, &rid("req-maint-nr"), 1, BUDGET)
        .await
        .expect("nothing to do");
    assert!(ledger::row(&t.db, p.repository_id)
        .await
        .expect("row")
        .is_none());
    t.drop_now().await;
}

/// A step that fails (here: fsck past its timeout, the hanging git's
/// tree killed) is recorded `failed` with its code and completes the job;
/// the first failure is a warn, the second in a row an error (alert).
#[tokio::test]
async fn a_second_failure_in_a_row_is_logged_as_an_alert() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "gus").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let git = Git::new(GitConfig::new(
        hanging_git(tmp.path()),
        storage.git.config().home.clone(),
    ));
    let cfg = MaintenanceConfig {
        step_timeout: Duration::from_secs(2),
        ..MaintenanceConfig::default()
    };
    let inner = MaintainHandler::with_git(&t.db, storage.root.clone(), git, &cfg)
        .expect("handler")
        .inner();
    let mut levels = Vec::new();
    for n in 1..=2 {
        let logs = Captured::default();
        let guard = logs.install();
        inner
            .maintain_repo(repo, &rid(&format!("req-maint-fail-{n}")), 1, BUDGET)
            .await
            .expect("recorded, not a job failure");
        drop(guard);
        let failures: Vec<_> = logs
            .events()
            .into_iter()
            .filter(|e| e["fields"]["error_code"] == "FG-GIT-0002")
            .collect();
        assert_eq!(failures.len(), 1, "logged once");
        assert_eq!(failures[0]["fields"]["step"], "fsck");
        assert_eq!(failures[0]["fields"]["failures"], n);
        levels.push(failures[0]["level"].as_str().expect("level").to_owned());
        let row = ledger::row(&t.db, repo)
            .await
            .expect("row")
            .expect("exists");
        assert_eq!(row.outcome, Some(Outcome::Failed));
        assert_eq!(row.error_code.as_deref(), Some("FG-GIT-0002"));
        assert_eq!(row.failures, n);
    }
    assert_eq!(levels, ["WARN", "ERROR"]);
    t.drop_now().await;
}

/// A push in progress (its quarantine) defers the whole run: nothing runs,
/// the ledger says `skipped incoming_push` and the run is due again within
/// the hour.
#[tokio::test]
async fn an_incoming_push_defers_the_run_and_is_recorded() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "hal").await;
    let path = storage.root.path_for(repo);
    with_history(&path);
    std::fs::create_dir(
        path.join("objects")
            .join(format!("{}x1", forge_git::maintenance::QUARANTINE_PREFIX)),
    )
    .expect("quarantine");
    let inner = handler(&t, &storage).inner();
    let logs = Captured::default();
    let guard = logs.install();
    inner
        .maintain_repo(repo, &rid("req-maint-push"), 1, BUDGET)
        .await
        .expect("deferred");
    drop(guard);
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Skipped));
    assert_eq!(row.skipped_reason.as_deref(), Some("incoming_push"));
    assert!(row.due_in_secs <= 3600, "{}", row.due_in_secs);
    assert_eq!(row.fsck_ok, None, "no fsck ran");
    assert!(path.join("refs/heads/main").exists(), "nothing packed");
    let events = logs.events();
    let skipped = events_named(&events, "forge.git.maintenance_skipped");
    assert_eq!(skipped.len(), 1, "{events:?}");
    assert_eq!(skipped[0]["fields"]["reason"], "incoming_push");
    assert_eq!(skipped[0]["fields"]["request_id"], "req-maint-push");
    t.drop_now().await;
}

/// What the scripted fake's `pack-refs` does.
#[derive(Clone, Copy)]
enum PackRefs {
    /// Takes `packed-refs.lock` (unless one exists) and hangs, ignoring
    /// SIGTERM: it ends only by a kill and leaves its lock.
    HangUntilKilled,
    /// Fails at once (exit 1) while a lock appears — a writer that took it
    /// right after our pack-refs exited on its own.
    ExitWhileAWriterLocks,
    /// Unix: hangs, and on SIGTERM exits while a lock is present (git
    /// removed its own; the one left is a writer's).
    #[cfg(unix)]
    ExitOnTermWhileAWriterLocks,
}

/// A fake git that exits 0 for every subcommand except: `fsck` with
/// `fsck_quarantine` creates an incoming quarantine (a push starting while
/// fsck runs); `pack-refs` does what `pack_refs` says. It runs in the
/// repository.
#[cfg(unix)]
fn scripted_git(dir: &Path, fsck_quarantine: bool, pack_refs: PackRefs) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("scripted-git.sh");
    let fsck = if fsck_quarantine {
        "mkdir -p objects/tmp_objdir-incoming-during-fsck; exit 0"
    } else {
        "exit 0"
    };
    let pack = match pack_refs {
        PackRefs::HangUntilKilled => {
            "trap '' TERM; [ -e packed-refs.lock ] || : > packed-refs.lock; while :; do sleep 1; done"
        }
        PackRefs::ExitWhileAWriterLocks => ": > packed-refs.lock; exit 1",
        PackRefs::ExitOnTermWhileAWriterLocks => {
            "trap ': > packed-refs.lock; exit 143' TERM; while :; do sleep 1 & wait; done"
        }
    };
    let body = format!(
        "#!/bin/sh\ncase \"$*\" in\n*fsck*) {fsck};;\n*pack-refs*) {pack};;\n*) exit 0;;\nesac\n"
    );
    std::fs::write(&path, body).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[cfg(windows)]
fn scripted_git(dir: &Path, fsck_quarantine: bool, pack_refs: PackRefs) -> PathBuf {
    let path = dir.join("scripted-git.cmd");
    let fsck = if fsck_quarantine {
        "mkdir objects\\tmp_objdir-incoming-during-fsck\r\n"
    } else {
        ""
    };
    let pack = match pack_refs {
        PackRefs::HangUntilKilled => {
            "if not exist packed-refs.lock type nul > packed-refs.lock\r\n\
             ping -n 60 127.0.0.1 >nul\r\n"
        }
        PackRefs::ExitWhileAWriterLocks => "type nul > packed-refs.lock\r\nexit /b 1\r\n",
    };
    let body = format!(
        "@echo off\r\n\
         echo %* | findstr /c:\"fsck\" >nul\r\n\
         if not errorlevel 1 (\r\n{fsck}exit /b 0\r\n)\r\n\
         echo %* | findstr /c:\"pack-refs\" >nul\r\n\
         if errorlevel 1 exit /b 0\r\n\
         {pack}"
    );
    std::fs::write(&path, body).expect("script");
    path
}

fn scripted_handler(
    t: &FreshDb,
    storage: &Storage,
    tmp: &Path,
    fsck_quarantine: bool,
    pack_refs: PackRefs,
) -> Arc<Inner> {
    let git = Git::new(GitConfig::new(
        scripted_git(tmp, fsck_quarantine, pack_refs),
        storage.git.config().home.clone(),
    ));
    let cfg = MaintenanceConfig {
        step_timeout: Duration::from_secs(2),
        ..MaintenanceConfig::default()
    };
    MaintainHandler::with_git(&t.db, storage.root.clone(), git, &cfg) // clone: own root handle
        .expect("handler")
        .inner()
}

/// Re-review HIGH: a pack-refs that exited on its own removed its own lock;
/// a lock present afterwards is a live writer's and is never touched (no
/// removal, no alert from the run: doctor's stale-lock check covers it).
#[tokio::test]
async fn an_exited_pack_refs_never_touches_a_writers_lock() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "max").await;
    let lock = storage.root.path_for(repo).join("packed-refs.lock");
    let tmp = tempfile::tempdir().expect("tmp");
    let inner = scripted_handler(
        &t,
        &storage,
        tmp.path(),
        false,
        PackRefs::ExitWhileAWriterLocks,
    );
    let logs = Captured::default();
    let guard = logs.install();
    inner
        .maintain_repo(repo, &rid("req-maint-exit-lock"), 1, BUDGET)
        .await
        .expect("recorded");
    drop(guard);
    assert!(lock.exists(), "the writer's lock is left alone");
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.error_code.as_deref(), Some("FG-GIT-0018"));
    let events = logs.events();
    assert!(
        events_named(&events, "forge.git.maintenance_lock_removed").is_empty()
            && events_named(&events, "forge.git.maintenance_lock_left").is_empty(),
        "{events:?}"
    );
    t.drop_now().await;
}

/// Unix: a pack-refs stopped by SIGTERM removed its own lock; a lock
/// present afterwards is a writer's and is kept.
#[cfg(unix)]
#[tokio::test]
async fn a_sigtermed_pack_refs_never_touches_a_writers_lock() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "ned").await;
    let lock = storage.root.path_for(repo).join("packed-refs.lock");
    let tmp = tempfile::tempdir().expect("tmp");
    let inner = scripted_handler(
        &t,
        &storage,
        tmp.path(),
        false,
        PackRefs::ExitOnTermWhileAWriterLocks,
    );
    inner
        .maintain_repo(repo, &rid("req-maint-term-lock"), 1, BUDGET)
        .await
        .expect("recorded");
    assert!(lock.exists(), "the writer's lock is left alone");
    t.drop_now().await;
}

/// Set a file's modification time.
fn age_file(path: &Path, age: Duration) {
    let mut perms = std::fs::metadata(path).expect("file").permissions();
    #[allow(clippy::permissions_set_readonly_false)] // test setup
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).expect("writable");
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(std::time::SystemTime::now() - age))
        .expect("age");
}

/// Review HIGH: a pack-refs killed at its timeout leaves `packed-refs.lock`;
/// the run removes it (it is ours: made during the step, under the merge
/// lock). A lock older than the step is someone else's: it is left and
/// alerted as FG-GIT-0020.
#[tokio::test]
async fn a_killed_pack_refs_removes_its_own_lock_and_keeps_a_foreign_one() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "ivy").await;
    let path = storage.root.path_for(repo);
    let lock = path.join("packed-refs.lock");
    let tmp = tempfile::tempdir().expect("tmp");
    let inner = scripted_handler(&t, &storage, tmp.path(), false, PackRefs::HangUntilKilled);
    let logs = Captured::default();
    let guard = logs.install();
    inner
        .maintain_repo(repo, &rid("req-maint-lock-own"), 1, BUDGET)
        .await
        .expect("recorded");
    drop(guard);
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Failed));
    assert_eq!(row.error_code.as_deref(), Some("FG-GIT-0002"));
    assert!(!lock.exists(), "our own lock is removed");
    let events = logs.events();
    assert_eq!(
        events_named(&events, "forge.git.maintenance_lock_removed").len(),
        1,
        "{events:?}"
    );
    // Someone else's lock, an hour old: kept and alerted.
    std::fs::write(&lock, b"").expect("foreign lock");
    age_file(&lock, Duration::from_secs(3600));
    let logs = Captured::default();
    let guard = logs.install();
    inner
        .maintain_repo(repo, &rid("req-maint-lock-foreign"), 1, BUDGET)
        .await
        .expect("recorded");
    drop(guard);
    assert!(lock.exists(), "someone else's lock is left");
    let alerts: Vec<_> = logs
        .events()
        .into_iter()
        .filter(|e| e["fields"]["error_code"] == "FG-GIT-0020")
        .collect();
    assert_eq!(alerts.len(), 1, "alerted once");
    assert_eq!(alerts[0]["level"], "ERROR");
    assert_eq!(
        alerts[0]["fields"]["event"],
        "forge.git.maintenance_lock_left"
    );
    t.drop_now().await;
}

/// Review MEDIUM: a push that starts while fsck runs defers gc: the
/// quarantine is checked again right before gc.
#[tokio::test]
async fn a_push_starting_during_fsck_defers_gc() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "jon").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let inner = scripted_handler(&t, &storage, tmp.path(), true, PackRefs::HangUntilKilled);
    inner
        .maintain_repo(repo, &rid("req-maint-mid-push"), 1, BUDGET)
        .await
        .expect("deferred");
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Skipped));
    assert_eq!(row.skipped_reason.as_deref(), Some("incoming_push"));
    assert_eq!(row.fsck_ok, Some(true), "the fsck that ran is recorded");
    assert!(
        !storage
            .root
            .path_for(repo)
            .join("packed-refs.lock")
            .exists(),
        "pack-refs never ran"
    );
    t.drop_now().await;
}

/// Review MEDIUM: the schedule records a lost run (crashed, no live job)
/// every pass, outside the window too.
#[tokio::test]
async fn the_schedule_reaps_a_lost_run_outside_the_window() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let repo = ready_repo(&t, &storage, "kim").await;
    ledger::start(&t.db, repo)
        .await
        .expect("start")
        .expect("ready");
    t.exec_superuser(&format!(
        "UPDATE forge.repository_maintenance SET last_started_at = now() - interval '2 hours', \
         failures = 1 WHERE repo_id = {}",
        repo.get()
    ))
    .await;
    let now = ledger::local_minute(&t.db, "UTC").await.expect("minute");
    let shift = |m: u16| (now + m) % (24 * 60);
    let cfg = MaintenanceConfig {
        window: forge_core::config::MaintenanceWindow {
            start_minute: shift(60),
            end_minute: shift(120),
        },
        time_zone: "UTC".to_owned(),
        ..MaintenanceConfig::default()
    };
    let logs = Captured::default();
    let guard = logs.install();
    run_schedule(&t.db, &cfg).await.expect("pass");
    drop(guard);
    let row = ledger::row(&t.db, repo)
        .await
        .expect("row")
        .expect("exists");
    assert_eq!(row.outcome, Some(Outcome::Failed));
    assert_eq!(row.error_code.as_deref(), Some("FG-DB-0007"));
    assert_eq!(row.failures, 2);
    // The second failure in a row: an alert, with the pass's request id.
    let events = logs.events();
    let lost = events_named(&events, "forge.git.maintenance_run_lost");
    assert_eq!(lost.len(), 1, "{events:?}");
    assert_eq!(lost[0]["level"], "ERROR");
    assert!(lost[0]["fields"]["request_id"]
        .as_str()
        .is_some_and(|r| !r.is_empty()));
    assert!(
        job_state(&t, MAINTAIN_JOB).await.is_empty(),
        "nothing enqueued outside the window"
    );
    t.drop_now().await;
}
