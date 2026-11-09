#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The mergeability job against a real bare repository and the development
//! database (M3 R2, ADR-0024 §3): what it stores when it cannot decide —
//! no version, a git timeout (stored, then returned for the retry) — and
//! that a job that failed for good ends its request as `job_failed`.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::*;
use forge_core::{CheckReason, MergeabilityStatus, RequestId};
use forge_db::repositories;
use forge_db::review::{mergeability_written, request_check, MergeabilityRequest};
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::mergeability::MergeTreeLimits;
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{check, Mergeability, NewFields, Review, StoredMergeRequest};

fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn commit(repo: &Path, content: &str, parent: Option<&str>) -> String {
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
    );
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tREADME.md\n").as_bytes()),
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: PathBuf,
    git: Git,
    storage: StorageRoot,
    mr: StoredMergeRequest,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = forge_db::users::create(
        &t.db,
        &system("req-user-olga"),
        &forge_db::users::NewUser::human("olga", "olga", "olga@example.test"),
    )
    .await
    .expect("user")
    .user_id;
    let web = project(&t, owner, &[]).await;
    let _marked = repositories::mark_ready(&t.db, &system("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let base = commit(&path, "base\n", None);
    let head = commit(&path, "head\n", Some(&base));
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let review = Review::new(t.db.clone(), Arc::new(browser));
    let proof = open_proof(
        &t,
        &session(owner),
        web.project_id,
        ("feature", "main"),
        false,
    )
    .await
    .expect("open proof");
    let mr = review
        .open(
            &request("req-open"),
            &proof,
            ready(web.repository_id),
            &NewFields {
                title: "Feature".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("open");
    World {
        t,
        _home: home,
        _root: root,
        path,
        git,
        storage,
        mr,
    }
}

impl World {
    async fn request(&self) -> MergeabilityRequest {
        let seq = request_check(
            &self.t.db,
            self.mr.id,
            RequestId::parse("req-q").expect("id"),
        )
        .await
        .expect("request")
        .expect("open");
        MergeabilityRequest {
            merge_request: self.mr.id,
            check_seq: seq,
        }
    }

    fn checker(&self, limits: MergeTreeLimits) -> Mergeability {
        // clone: the checker owns its handles
        Mergeability::new(self.git.clone(), self.storage.clone()).with_limits(limits)
    }

    async fn status(&self) -> (MergeabilityStatus, Option<CheckReason>) {
        let s = mergeability_written(&self.t.db, self.mr.id)
            .await
            .expect("read")
            .expect("row");
        (s.status, s.reason)
    }
}

/// Review (pr-test): an undecidable check is stored with its reason —
/// `no_version` before the first version; a git timeout is stored as
/// `git_timeout` AND returned (the worker retries), never "mergeable".
#[tokio::test]
async fn check_maps_git_timeout_and_no_version_to_check_failed() {
    let w = world().await;
    let rid = RequestId::parse("req-check").expect("id");
    let report = check(
        &w.t.db,
        &w.checker(MergeTreeLimits::default()),
        w.request().await,
        &rid,
    )
    .await
    .expect("no version");
    assert_eq!(report.reason, Some(CheckReason::NoVersion));
    assert_eq!(
        w.status().await,
        (
            MergeabilityStatus::CheckFailed,
            Some(CheckReason::NoVersion)
        )
    );
    // Version 1 (the version job's row, as the worker writes it).
    let head = plumb(&w.path, &["rev-parse", "refs/heads/feature"], None);
    forge_db::review::record_mr_version(
        &w.t.db,
        &system("req-v1"),
        w.mr.id,
        &forge_core::ObjectId::parse(&head).expect("oid"),
        None,
    )
    .await
    .expect("v1");
    // A git that never answers `merge-tree`: the timeout is certain (a
    // near-zero timeout is not: a fast git can win the race).
    let fake = tempfile::tempdir().expect("fake git dir");
    let stalled = forge_git::test_git::stalled_git(w._home.path(), fake.path(), Some("merge-tree"));
    let slow = Mergeability::new(stalled, w.storage.clone()).with_limits(MergeTreeLimits {
        timeout: forge_git::test_git::STALL,
        ..MergeTreeLimits::default()
    });
    let err = check(&w.t.db, &slow, w.request().await, &rid)
        .await
        .expect_err("timeout");
    assert_eq!(err.code(), ErrorCode::GitTimeout, "{err}");
    assert_eq!(
        w.status().await,
        (
            MergeabilityStatus::CheckFailed,
            Some(CheckReason::GitTimeout)
        )
    );
    // A normal run decides.
    let report = check(
        &w.t.db,
        &w.checker(MergeTreeLimits::default()),
        w.request().await,
        &rid,
    )
    .await
    .expect("decides");
    assert_eq!(report.status, Some(MergeabilityStatus::CanBeMerged));
    w.t.drop_now().await;
}

/// Review (silent-failure HIGH): a job that failed for good (here: a
/// repository not ready on its last attempt) ends its request as
/// `check_failed reason=job_failed`.
#[tokio::test]
async fn a_job_failed_for_good_stores_job_failed() {
    let w = world().await;
    let request = w.request().await;
    let begun = forge_db::review::begin_check(&w.t.db, w.mr.id, request.check_seq)
        .await
        .expect("started");
    assert!(matches!(begun, forge_db::review::CheckStart::Check(_)));
    let cause = ForgeError::new(ErrorCode::RepositoryNotReady);
    forge_review::mergeability::fail(
        &w.t.db,
        request,
        &RequestId::parse("req-dead").expect("id"),
        &cause,
    )
    .await;
    assert_eq!(
        w.status().await,
        (
            MergeabilityStatus::CheckFailed,
            Some(CheckReason::JobFailed)
        )
    );
    w.t.drop_now().await;
}
