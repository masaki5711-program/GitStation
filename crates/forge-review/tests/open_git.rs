#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Merge request creation against a real bare repository (synthetic
//! content) through the repository's branch snapshot (plan 18 S8,
//! ADR-0037 (g)): the development database, the real browse API as the
//! branch probe, the version job as `forge worker` runs it.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use forge_auth::{Action, MergeRequestEdit};
use forge_core::{RepoId, RequestId, UserId};
use forge_db::repositories;
use forge_db::review::{list_versions, RefreshRequest};
use forge_db::test_support::FreshDb;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{refresh, BlockReason, NewFields, Review, StoredMergeRequest, VersionRefs};

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

/// One synthetic commit.
fn commit(repo: &Path) -> String {
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(b"# synthetic\n"),
    );
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tREADME.md\n").as_bytes()),
    );
    plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None)
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

/// A project of `olga` whose repository is ready and empty; the browse API
/// with `limits` is the branch probe, as `forge serve` wires it.
async fn world(limits: BrowseLimits) -> World {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
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
    let browser = Browser::new(git.clone(), storage.clone(), limits);
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

/// A snapshot that never expires by itself during a test.
fn long_lived() -> BrowseLimits {
    BrowseLimits {
        refs_ttl: Duration::from_secs(3600),
        ..BrowseLimits::default()
    }
}

/// Load test 2026-11-17 F1 / plan 18 S8: fifty merge requests opened at
/// once on one repository. Every create needs to know its two branches
/// exist; all of them are answered by one `for-each-ref`, so the
/// repository's browse cap is never the reason a create fails
/// (`FG-GIT-0004 reason=repo_browse`, HTTP 503). The cap is one slot with
/// no queue here: a create that ran its own git would be refused at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_concurrent_creates_on_one_repo_never_answer_503() {
    const CREATES: usize = 50;
    let w = world(BrowseLimits {
        max_per_repo: 1,
        max_repo_waiters: 0,
        ..long_lived()
    })
    .await;
    let c1 = commit(&w.path);
    let mut updates = format!("update refs/heads/main {c1}\n");
    for n in 0..CREATES {
        updates.push_str(&format!("update refs/heads/topic-{n} {c1}\n"));
    }
    plumb(
        &w.path,
        &["update-ref", "--stdin"],
        Some(updates.as_bytes()),
    );
    let mut proofs = Vec::with_capacity(CREATES);
    for n in 0..CREATES {
        let source = format!("topic-{n}");
        proofs.push(
            open_proof(&w.t, &session(w.owner), w.project, (&source, "main"), false)
                .await
                .expect("open proof"),
        );
    }

    let before = forge_git::git_spawns();
    let creates: Vec<_> = proofs
        .into_iter()
        .enumerate()
        .map(|(n, proof)| {
            let review = w.review.clone(); // clone: one handle per task
            let repo = w.repo;
            tokio::spawn(async move {
                review
                    .open(
                        &request(&format!("req-open-{n}")),
                        &proof,
                        ready(repo),
                        &NewFields {
                            title: format!("Topic {n}"),
                            description: String::new(),
                        },
                    )
                    .await
            })
        })
        .collect();
    let mut failed = Vec::new();
    for create in creates {
        if let Err(e) = create.await.expect("join") {
            failed.push(e.to_string());
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {CREATES} creates failed; the first: {:?}",
        failed.len(),
        failed.first()
    );
    assert_eq!(
        forge_git::git_spawns() - before,
        1,
        "one for-each-ref answers every create"
    );
    assert_eq!(
        count(&w.t, "SELECT count(*) AS n FROM forge.merge_requests").await,
        CREATES as i64
    );
    w.t.drop_now().await;
}

async fn open(w: &World, source: &str) -> StoredMergeRequest {
    let proof = open_proof(&w.t, &session(w.owner), w.project, (source, "main"), false)
        .await
        .expect("open proof");
    w.review
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
        .expect("open")
}

/// A stale positive (ADR-0037 (g)): the snapshot still has a source branch
/// that another process deleted and nothing told this process. The create
/// is answered from the snapshot and records no head; the version job
/// (`prepare`) reads the live branch, finds the source missing and writes
/// no version, and the merge request shows as not mergeable — the same
/// visible state as a branch deleted just after a create. No commit id
/// ever came from the snapshot.
#[tokio::test]
async fn a_deleted_branch_in_the_snapshot_yields_source_branch_missing_after_prepare() {
    let w = world(long_lived()).await;
    let c1 = commit(&w.path);
    for name in ["main", "feature"] {
        plumb(
            &w.path,
            &["update-ref", &format!("refs/heads/{name}"), &c1],
            None,
        );
    }
    w.browser
        .require_branch(w.repo, &branch("feature"))
        .await
        .expect("feature exists (the snapshot is read)");
    plumb(&w.path, &["update-ref", "-d", "refs/heads/feature"], None);

    let mr = open(&w, "feature").await;
    assert_eq!(mr.head_sha, None, "a create records no head");

    let report = refresh(
        &w.t.db,
        &w.refs,
        RefreshRequest {
            merge_request: mr.id,
            pushed_by: None,
        },
        &RequestId::parse("req-prepare").expect("id"),
    )
    .await
    .expect("refresh");
    assert_eq!(report.outcome, "source_missing");
    assert_eq!(report.version, None);
    assert!(list_versions(&w.t.db, mr.id)
        .await
        .expect("versions")
        .is_empty());

    let proof = stored_proof(
        &w.t,
        &w.review,
        &session(w.owner),
        Action::MergeRequestRead,
        w.project,
        mr.iid,
        MergeRequestEdit::default(),
    )
    .await
    .expect("proof");
    let status = w
        .review
        .merge_status(
            &RequestId::parse("req-status").expect("id"),
            &proof,
            ready(w.repo),
            None,
        )
        .await
        .expect("status");
    assert!(!status.mergeable);
    assert_eq!(status.head_sha, None, "no head from the snapshot");
    assert_eq!(status.reasons, [BlockReason::MergeabilityUnknown]);
    w.t.drop_now().await;
}
