#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `forge/ci-lint` (plan 14 C1) on a real bare repository and a migrated
//! database: `.forge-ci.yml` read at the commit through the browse API
//! (bounded, the git timeout), the project's protected branches read from
//! `forge.protected_refs`, and the run forge-ci's lint gives. The full run of
//! a merge request version is `forge worker`'s test (forge-cli).

use std::path::Path;
use std::process::{Command, Stdio};

use forge_checks::ci_lint::{assess_ci_lint, read_input, CiFile, CI_FILE};
use forge_core::checks::CheckConclusion;
use forge_core::{RefPattern, RequestId, Role, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};

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

/// A commit of flat `files` at the root.
fn commit(repo: &Path, files: &[(&str, &[u8])]) -> String {
    let mut listing = String::new();
    for (name, content) in files {
        let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(content));
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
    plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None)
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

const D: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A valid definition; `deploy` has a secret and runs on `main` only.
fn valid() -> String {
    format!(
        "version: 1
stages: [build, deploy]
jobs:
  build:
    stage: build
    image: registry.internal/rust@sha256:{D}
    timeout: 30m
    script: [cargo build]
  deploy:
    stage: deploy
    image: registry.internal/rust@sha256:{D}
    timeout: 10m
    script: [./deploy.sh]
    secrets: [DEPLOY_KEY]
    rules:
      branches: [main]
      merge_requests: false
"
    )
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    root: tempfile::TempDir,
    browser: Browser,
    repo: forge_core::RepoId,
    path: std::path::PathBuf,
    project: forge_core::ProjectId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let mut new = NewProject::new("web", alice.namespace_id, "web", alice.user_id);
    new.visibility = Visibility::Private;
    let web = projects::create(&t.db, &system("req-project-web"), &new)
        .await
        .expect("project");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    World {
        t,
        _home: home,
        root,
        browser: Browser::new(git, storage, BrowseLimits::default()),
        repo: web.repository_id,
        path,
        project: web.project_id,
    }
}

impl World {
    async fn protect(&self, pattern: &str) {
        forge_db::protected_refs::create(
            &self.t.db,
            &system("req-protect"),
            self.project,
            &RefPattern::parse(pattern).expect("pattern"),
            Role::Maintainer,
        )
        .await
        .expect("protect");
    }

    async fn lint(&self, files: &[(&str, &[u8])]) -> forge_checks::ci_lint::CiLint {
        let head = commit(&self.path, files);
        let input = read_input(&self.t.db, &self.browser, self.project, self.repo, &head)
            .await
            .expect("read");
        assess_ci_lint(&input).expect("assess")
    }
}

#[tokio::test]
async fn a_clean_definition_at_the_commit_succeeds() {
    let w = world().await;
    w.protect("refs/heads/main").await;
    let lint = w.lint(&[(CI_FILE, valid().as_bytes())]).await;
    assert_eq!(
        lint.run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        lint.run.summary
    );
    assert!(lint.invalid.is_none());
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_cycle_at_the_commit_fails_with_reason_and_line() {
    let w = world().await;
    w.protect("refs/heads/main").await;
    let text = valid().replace(
        "  build:\n    stage: build\n",
        "  build:\n    stage: build\n    needs: [build]\n",
    );
    let lint = w.lint(&[(CI_FILE, text.as_bytes())]).await;
    assert_eq!(
        lint.run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        lint.run.summary
    );
    assert!(
        lint.run
            .summary
            .contains(".forge-ci.yml:4 needs_cycle — build [FG-CI-0001]"),
        "{}",
        lint.run.summary
    );
    assert_eq!(lint.invalid.expect("invalid").code().id(), "FG-CI-0001");
    w.t.drop_now().await;
}

/// The protected list is the project's (`forge.protected_refs`): a secret
/// job on `main` fails while `main` is unprotected and passes once it is.
#[tokio::test]
async fn a_secret_job_passes_only_on_a_branch_the_database_protects() {
    let w = world().await;
    let text = valid();
    let files: &[(&str, &[u8])] = &[(CI_FILE, text.as_bytes())];
    let unprotected = w.lint(files).await;
    assert_eq!(
        unprotected.run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        unprotected.run.summary
    );
    assert!(
        unprotected
            .run
            .summary
            .contains("secret_on_unprotected_ref — main"),
        "{}",
        unprotected.run.summary
    );
    assert!(!unprotected.run.summary.contains("DEPLOY_KEY"));
    // A tag rule protects no branch.
    w.protect("refs/tags/main").await;
    let tag_only = w.lint(files).await;
    assert_eq!(tag_only.run.conclusion, Some(CheckConclusion::Failure));
    w.protect("refs/heads/main").await;
    let protected = w.lint(files).await;
    assert_eq!(
        protected.run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        protected.run.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_commit_without_the_file_is_nothing_to_check() {
    let w = world().await;
    let head = commit(&w.path, &[("README", b"no ci here\n")]);
    let input = read_input(&w.t.db, &w.browser, w.project, w.repo, &head)
        .await
        .expect("read");
    assert_eq!(input.file, CiFile::Absent);
    let lint = assess_ci_lint(&input).expect("assess");
    assert_eq!(lint.run.conclusion, Some(CheckConclusion::Success));
    assert!(
        lint.run.summary.contains("nothing to check"),
        "{}",
        lint.run.summary
    );
    w.t.drop_now().await;
}

/// A file over forge-ci's limit is not read at all (`small_file` checks the
/// size first) and fails as `too_large`.
#[tokio::test]
async fn an_oversized_file_is_not_read_and_fails() {
    let w = world().await;
    let mut big = valid();
    big.push_str(&"#".repeat(forge_ci::MAX_BYTES));
    let head = commit(&w.path, &[(CI_FILE, big.as_bytes())]);
    let input = read_input(&w.t.db, &w.browser, w.project, w.repo, &head)
        .await
        .expect("read");
    assert_eq!(input.file, CiFile::TooLarge);
    let lint = assess_ci_lint(&input).expect("assess");
    assert_eq!(lint.run.conclusion, Some(CheckConclusion::Failure));
    assert!(
        lint.run.summary.contains(".forge-ci.yml:0 too_large"),
        "{}",
        lint.run.summary
    );
    w.t.drop_now().await;
}

/// A git that never answers: the read fails with forge-git's timeout and
/// the commit, never a silent pass.
#[tokio::test]
async fn a_git_timeout_is_an_error_with_the_commit() {
    let w = world().await;
    let head = commit(&w.path, &[(CI_FILE, valid().as_bytes())]);
    let home = tempfile::tempdir().expect("home");
    let stall = tempfile::tempdir().expect("stall");
    let storage = StorageRoot::open(w.root.path()).expect("root");
    let slow = Browser::new(
        forge_git::test_git::stalled_git(home.path(), stall.path(), None),
        storage,
        BrowseLimits {
            timeout: forge_git::test_git::STALL,
            ..BrowseLimits::default()
        },
    );
    let err = read_input(&w.t.db, &slow, w.project, w.repo, &head)
        .await
        .expect_err("timeout");
    assert!(err.code().id().starts_with("FG-GIT-"), "{err}");
    assert!(
        err.context()
            .iter()
            .any(|(k, v)| *k == "commit" && v == &head),
        "{err:?}"
    );
    w.t.drop_now().await;
}
