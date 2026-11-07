//! The mirror lane against a migrated database, a provisioned repository and
//! a local bare repository as the GitLab copy (plan 11 §7).

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use forge_core::ids::{RepoId, UserId};
use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{JobRunner, Ran, WorkerId};
use forge_db::mirror::{self as mdb, MirrorSetting};
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;

use super::*;
use crate::worker::loop_tests::{context, Captured};
use crate::worker::provision::tests::storage;
use crate::worker::provision::ProvisionHandler;
use crate::worker::purge::tests::project;

const T: Duration = Duration::from_secs(60);

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct World {
    t: FreshDb,
    _root: tempfile::TempDir,
    storage: Storage,
    dest_dir: tempfile::TempDir,
    repo: RepoId,
    project: forge_core::ids::ProjectId,
    owner: UserId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let (root, storage) = storage();
    let (owner, p) = project(&t, "mira").await;
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    runner
        .register(Arc::new(
            ProvisionHandler::new(&t.db, storage.clone()).expect("handler"), // clone: test handles
        ))
        .expect("register");
    let ran = runner.run_once().await.expect("claim").expect("a job ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let path = storage.root.path_for(p.repository_id);
    let tree = git(&path, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(&path)
        .args(["commit-tree", &tree, "-m", "synthetic"])
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .output()
        .expect("commit");
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    git(&path, &["update-ref", "refs/heads/main", &commit]);
    git(
        &path,
        &["update-ref", "refs/merge-requests/1/head", &commit],
    );
    let dest_dir = tempfile::tempdir().expect("dest");
    let out = Command::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(dest_dir.path().join("gitlab.git"))
        .output()
        .expect("init");
    assert!(out.status.success());
    World {
        t,
        _root: root,
        storage,
        dest_dir,
        repo: p.repository_id,
        project: p.project_id,
        owner,
    }
}

impl World {
    fn dest(&self) -> std::path::PathBuf {
        self.dest_dir.path().join("gitlab.git")
    }

    async fn set_mirror(&self) {
        mdb::set_decided(
            &self.t.db,
            &ChangeContext::new(Actor::User(self.owner), rid("req-mirror-set")),
            self.project,
            self.owner,
            &MirrorSetting {
                url: "https://gitlab.example.invalid/ymma/app.git".to_owned(),
                origin: "https://gitlab.example.invalid:443".to_owned(),
                credential_name: "gitlab".to_owned(),
                enabled: true,
            },
            |_| Ok(()),
        )
        .await
        .expect("set");
    }

    fn handler(&self) -> MirrorHandler {
        MirrorHandler::new(
            &self.t.db,
            self.storage.root.clone(), // clone: the handler's own root handle
            forge_git::Git::new(self.storage.git.config().clone()), // clone: own slots
            Resolver::Local(self.dest()),
            T,
        )
        .expect("handler")
    }

    async fn status(&self) -> mdb::MirrorStatus {
        mdb::get(&self.t.db, self.project)
            .await
            .expect("get")
            .expect("mirror")
            .status
    }
}

#[tokio::test]
async fn a_push_job_brings_the_mirror_up_to_date_without_merge_request_refs() {
    let w = world().await;
    w.set_mirror().await;
    let h = w.handler();
    h.inner
        .mirror(w.repo, &rid("req-job"), 1, false)
        .await
        .expect("pushed");
    let refs = git(&w.dest(), &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(refs, "refs/heads/main");
    let s = w.status().await;
    assert!(s.pending_since.is_none(), "{s:?}");
    assert!(s.last_success_at.is_some());
    assert_eq!(s.failures, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_refused_push_is_int_0013_on_the_last_attempt() {
    let w = world().await;
    w.set_mirror().await;
    let hook = w.dest().join("hooks").join("pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let h = w.handler();
    let logs = Captured::default();
    let guard = logs.install();
    // An earlier attempt: warn, retried, no alert.
    let e = h
        .inner
        .mirror(w.repo, &rid("req-job"), 1, false)
        .await
        .expect_err("retried");
    assert_eq!(e.error.code(), ErrorCode::PushMirrorFailed);
    let alerts = |logs: &Captured| -> Vec<serde_json::Value> {
        logs.failures()
            .into_iter()
            .filter(|f| f["level"] == "ERROR")
            .collect()
    };
    assert!(alerts(&logs).is_empty(), "no alert before the last attempt");
    let warned: Vec<_> = logs
        .events()
        .into_iter()
        .filter(|e| e["fields"]["event"] == "forge.mirror.push_failed")
        .collect();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(warned[0]["level"], "WARN");
    // The last attempt: FG-INT-0013 once, and the job ends.
    h.inner
        .mirror(w.repo, &rid("req-job"), 5, true)
        .await
        .expect("the job ends after logging");
    drop(guard);
    let failures = alerts(&logs);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["fields"]["error_code"], "FG-INT-0013");
    assert_eq!(context(&failures[0], "reason").as_deref(), Some("rejected"));
    assert!(context(&failures[0], "origin").is_some());
    let s = w.status().await;
    assert_eq!(s.failures, 2);
    assert_eq!(s.last_error_code.as_deref(), Some("FG-INT-0013"));
    assert_eq!(s.last_reason.as_deref(), Some("rejected"));
    assert!(s.pending_since.is_some(), "still behind");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_removed_mirror_makes_the_job_end_without_pushing() {
    let w = world().await;
    let h = w.handler();
    h.inner
        .mirror(w.repo, &rid("req-job"), 1, false)
        .await
        .expect("nothing to push");
    assert_eq!(git(&w.dest(), &["for-each-ref", "--format=%(refname)"]), "");
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_lag_sweep_logs_int_0014_once_an_hour() {
    let w = world().await;
    w.set_mirror().await;
    w.t.exec_superuser(
        "UPDATE forge.push_mirror_status SET pending_since = now() - interval '30 minutes'",
    )
    .await;
    let logs = Captured::default();
    let guard = logs.install();
    run_lag(&w.t.db, Duration::from_secs(900))
        .await
        .expect("sweep");
    run_lag(&w.t.db, Duration::from_secs(900))
        .await
        .expect("sweep again");
    drop(guard);
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["fields"]["error_code"], "FG-INT-0014");
    assert_eq!(
        context(&failures[0], "repo_id").as_deref(),
        Some(w.repo.get().to_string().as_str())
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn without_a_credentials_dir_there_is_no_lane_and_the_start_says_so() {
    let w = world().await;
    w.set_mirror().await;
    let logs = Captured::default();
    let guard = logs.install();
    let lane = lane(
        &w.t.db,
        &w.storage,
        &forge_core::config::MirrorConfig::default(),
    )
    .await
    .expect("lane");
    drop(guard);
    assert!(lane.is_none());
    let off: Vec<_> = logs
        .events()
        .into_iter()
        .filter(|e| e["fields"]["event"] == "forge.worker.mirror_lane_off")
        .collect();
    assert_eq!(off.len(), 1, "{off:?}");
    w.t.drop_now().await;
}
