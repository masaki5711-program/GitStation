#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The version job and the purge end to end (M3 R1b, ADR-0024 §2): a real
//! bare repository (synthetic content) under a temporary storage root, the
//! development database, the real browse API as the branch probe.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::*;
use forge_auth::{Action, Credential, MergeRequestEdit, Principal};
use forge_core::{ObjectId, RepoId, RequestId, UserId};
use forge_db::repositories;
use forge_db::review::{list_versions, RefreshRequest};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{refresh, NewFields, Review, VersionRefs};

/// Plumbing on the bare repository (test setup only; synthetic content).
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

fn commit(repo: &Path, content: &str) -> String {
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
    plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None)
}

fn set_branch(repo: &Path, name: &str, sha: &str) {
    plumb(
        repo,
        &["update-ref", &format!("refs/heads/{name}"), sha],
        None,
    );
}

/// The hidden refs of merge request 1 (`name → sha`).
fn hidden(repo: &Path) -> Vec<(String, String)> {
    let out = plumb(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/merge-requests/1",
        ],
        None,
    );
    out.lines()
        .map(|l| {
            let (n, s) = l.split_once(' ').expect("line");
            (n.to_owned(), s.to_owned())
        })
        .collect()
}

fn pair(name: &str, sha: &str) -> (String, String) {
    (format!("refs/merge-requests/1/{name}"), sha.to_owned())
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: PathBuf,
    repo: RepoId,
    project: forge_core::ProjectId,
    owner: UserId,
    review: Review,
    refs: VersionRefs,
    browser: Browser,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    // An administrator (the purge is theirs) who owns the project.
    let owner = forge_db::users::create(
        &t.db,
        &system("req-user-olga"),
        &forge_db::users::NewUser {
            is_admin: true,
            ..forge_db::users::NewUser::human("olga", "olga", "olga@example.test")
        },
    )
    .await
    .expect("admin")
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
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let refs = VersionRefs::new(git, storage).with_browser(browser.clone());
    let review =
        Review::new(t.db.clone(), Arc::new(browser.clone())).with_version_refs(refs.clone());
    World {
        t,
        _home: home,
        _root: root,
        path,
        repo: web.repository_id,
        project: web.project_id,
        owner,
        review,
        refs,
        browser,
    }
}

impl World {
    async fn refresh(&self, mr: forge_core::ids::MergeRequestId, request: &str) -> &'static str {
        refresh(
            &self.t.db,
            &self.refs,
            RefreshRequest {
                merge_request: mr,
                pushed_by: None,
            },
            &RequestId::parse(request).expect("id"),
        )
        .await
        .expect("refresh")
        .outcome
    }

    fn admin(&self) -> Principal {
        Principal::authenticated(self.owner, true, true, Credential::Session)
    }

    async fn proof(&self, action: Action) -> forge_auth::Authorized<forge_auth::MergeRequestCtx> {
        stored_proof(
            &self.t,
            &self.review,
            &self.admin(),
            action,
            self.project,
            1,
            MergeRequestEdit::default(),
        )
        .await
        .expect("proof")
    }
}

/// A push to the source adds a version with its `v<n>` and `head` refs; a
/// rerun changes nothing; a force push adds `v2` while `v1` keeps the
/// replaced commit through `git gc --prune=now`.
#[tokio::test]
async fn a_push_adds_a_version_and_its_hidden_refs_which_survive_a_force_push() {
    let w = world().await;
    let base = commit(&w.path, "base\n");
    let c1 = commit(&w.path, "reviewed\n");
    set_branch(&w.path, "main", &base);
    set_branch(&w.path, "feature", &c1);
    let proof = open_proof(
        &w.t,
        &session(w.owner),
        w.project,
        ("feature", "main"),
        false,
    )
    .await
    .expect("open proof");
    let mr = w
        .review
        .open(
            &request("req-open"),
            &proof,
            ready(w.repo),
            &NewFields {
                title: "Feature".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("open");
    assert_eq!(w.refresh(mr.id, "req-r1").await, "added");
    assert_eq!(hidden(&w.path), [pair("head", &c1), pair("v1", &c1)]);
    // The job ran again: no new version, no event, no ref change.
    assert_eq!(w.refresh(mr.id, "req-r1-again").await, "already_head");
    let pushed = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.pushed'",
    )
    .await;
    assert_eq!(pushed, 1);

    // Force push: an unrelated commit replaces the reviewed one.
    let c2 = commit(&w.path, "rewritten\n");
    set_branch(&w.path, "feature", &c2);
    assert_eq!(w.refresh(mr.id, "req-r2").await, "added");
    assert_eq!(
        hidden(&w.path),
        [pair("head", &c2), pair("v1", &c1), pair("v2", &c2)]
    );
    plumb(
        &w.path,
        &["reflog", "expire", "--expire=now", "--all"],
        None,
    );
    plumb(&w.path, &["gc", "--prune=now", "--quiet"], None);
    assert_eq!(plumb(&w.path, &["cat-file", "-t", &c1], None), "commit");
    let stored = list_versions(&w.t.db, mr.id).await.expect("versions");
    assert_eq!(stored.len(), 2);
    w.t.drop_now().await;
}

/// ADR-0024 §2 and closing table #42, #43: the purge refuses an open merge
/// request's head (FG-REVIEW-0002), deletes the refs of the versions it
/// marks, and the version job never writes them again; a purged merge
/// request is reopened only away from the purged head.
#[tokio::test]
async fn purged_version_refs_are_never_recreated() {
    let w = world().await;
    let base = commit(&w.path, "base\n");
    let c1 = commit(&w.path, "synthetic secret\n");
    set_branch(&w.path, "main", &base);
    set_branch(&w.path, "feature", &c1);
    let proof = open_proof(
        &w.t,
        &session(w.owner),
        w.project,
        ("feature", "main"),
        false,
    )
    .await
    .expect("open proof");
    let mr = w
        .review
        .open(
            &request("req-open"),
            &proof,
            ready(w.repo),
            &NewFields {
                title: "Feature".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("open");
    w.refresh(mr.id, "req-r1").await;
    let c2 = commit(&w.path, "clean\n");
    set_branch(&w.path, "feature", &c2);
    w.refresh(mr.id, "req-r2").await;

    let purge = w.proof(Action::MergeRequestVersionRefsPurge).await;
    let err = w
        .review
        .purge_version_refs(&request("req-p0"), &purge, ready(w.repo), None)
        .await
        .expect_err("the open head");
    assert_eq!(err.code(), ErrorCode::PurgePreconditionFailed);
    assert_reason(
        &err,
        ErrorCode::PurgePreconditionFailed,
        "purge_requires_closed_or_rewritten",
    );
    assert_eq!(hidden(&w.path).len(), 3, "nothing touched");

    let done = w
        .review
        .purge_version_refs(&request("req-p1"), &purge, ready(w.repo), Some(&[1]))
        .await
        .expect("purge v1");
    assert_eq!(done.purged, [1]);
    assert_eq!(hidden(&w.path), [pair("head", &c2), pair("v2", &c2)]);
    // The job runs again (any later push): v1 stays gone.
    assert_eq!(w.refresh(mr.id, "req-r3").await, "already_head");
    assert_eq!(hidden(&w.path), [pair("head", &c2), pair("v2", &c2)]);
    // Someone recreates v1 by hand: the next sync deletes it again.
    plumb(
        &w.path,
        &["update-ref", "refs/merge-requests/1/v1", &c1],
        None,
    );
    w.refresh(mr.id, "req-r4").await;
    assert_eq!(hidden(&w.path), [pair("head", &c2), pair("v2", &c2)]);

    // Close, purge everything: head goes too.
    let close = w.proof(Action::MergeRequestClose).await;
    let (closed, _) = w
        .review
        .close(&request("req-close"), &close)
        .await
        .expect("close");
    assert_eq!(closed, forge_review::StateOutcome::Changed);
    let purge = w.proof(Action::MergeRequestVersionRefsPurge).await;
    w.review
        .purge_version_refs(&request("req-p2"), &purge, ready(w.repo), None)
        .await
        .expect("purge all");
    assert!(hidden(&w.path).is_empty());
    assert_eq!(w.refresh(mr.id, "req-r5").await, "not_open");
    assert!(hidden(&w.path).is_empty(), "never recreated");

    // Reopen at the purged head: refused; after moving the branch: allowed.
    let reopen = w.proof(Action::MergeRequestReopen).await;
    let err = w
        .review
        .reopen(&request("req-reopen"), &reopen, ready(w.repo))
        .await
        .expect_err("purged head");
    assert_eq!(err.code(), ErrorCode::PurgePreconditionFailed);
    assert_reason(
        &err,
        ErrorCode::PurgePreconditionFailed,
        "reopen_at_purged_head",
    );
    let c3 = commit(&w.path, "after the purge\n");
    set_branch(&w.path, "feature", &c3);
    // A push drops the browse cache (forge-web does it); here by hand.
    w.browser.invalidate(w.repo);
    let reopen = w.proof(Action::MergeRequestReopen).await;
    let (_, reopened) = w
        .review
        .reopen(&request("req-reopen-2"), &reopen, ready(w.repo))
        .await
        .expect("reopen after the source moved");
    assert_eq!(reopened.state, forge_core::MergeRequestState::Open);
    assert_eq!(w.refresh(mr.id, "req-r6").await, "added");
    assert_eq!(hidden(&w.path), [pair("head", &c3), pair("v3", &c3)]);
    let tip = ObjectId::parse(&c3).expect("oid");
    let stored = list_versions(&w.t.db, mr.id).await.expect("versions");
    assert_eq!(stored.last().map(|v| &v.head_sha), Some(&tip));
    w.t.drop_now().await;
}
