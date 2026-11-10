#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `Pipelines::plan_and_record` against a real bare repository and the
//! development database (ADR-0011): a push with a definition is exactly one
//! pipeline (a second run is the same row); no file is no pipeline; an
//! invalid definition is an `invalid` row with its reason; a merge request
//! with a secret job is `invalid` (`secret_on_unprotected_ref`); a git that
//! never answers is an error the job retries, and the last attempt records
//! the `error` row (FG-CI-0002). Synthetic data only.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_core::{MergeRequestId, ObjectId, ProjectId, RefName, RepoId, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_pipelines::{
    CopyMark, MarkFuture, MarkMeter, Outcome, PipelineRequest, PipelineStatus, Pipelines, Source,
};

struct Marks(CopyMark);

impl MarkMeter for Marks {
    fn measure<'a>(&'a self, _: RepoId, _: &'a ObjectId) -> MarkFuture<'a> {
        let mark = self.0;
        Box::pin(async move { mark })
    }
}

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

/// A commit with `files` at the root.
fn commit(repo: &Path, files: &[(&str, &str)]) -> ObjectId {
    let mut listing = String::new();
    for (name, body) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(body.as_bytes()),
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
    ObjectId::parse(&plumb(
        repo,
        &["commit-tree", &tree, "-m", "synthetic"],
        None,
    ))
    .expect("oid")
}

fn image() -> String {
    format!("registry.internal/rust@sha256:{}", "a".repeat(64))
}

fn valid() -> String {
    format!(
        "version: 1\nstages: [build, test]\njobs:\n  build:\n    stage: build\n    image: {i}\n    timeout: 10m\n    script: [cargo build]\n  test:\n    stage: test\n    image: {i}\n    timeout: 20m\n    script: [cargo test]\n    tags: [linux]\n",
        i = image()
    )
}

fn with_secret_for_merge_requests() -> String {
    format!(
        "version: 1\nstages: [deploy]\njobs:\n  deploy:\n    stage: deploy\n    image: {i}\n    timeout: 10m\n    script: [./deploy.sh]\n    secrets: [DEPLOY_KEY]\n",
        i = image()
    )
}

/// A deploy job with a secret that runs for pushes to `feature` only.
fn with_secret_on_feature() -> String {
    format!(
        "version: 1
stages: [deploy]
jobs:
  deploy:
    stage: deploy
    image: {i}
    timeout: 10m
    script: [./deploy.sh]
    secrets: [DEPLOY_KEY]
    rules:
      branches: [feature]
      merge_requests: false
",
        i = image()
    )
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    project: ProjectId,
    path: std::path::PathBuf,
    git: Git,
    storage: StorageRoot,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let alice = users::create(
            &t.db,
            &ctx("req-alice"),
            &NewUser::human("alice", "Alice", "alice@example.test"),
        )
        .await
        .expect("alice");
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
        )
        .await
        .expect("web");
        let _ = forge_db::repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
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
        Self {
            t,
            project: web.project_id,
            path,
            git,
            storage,
            _home: home,
            _root: root,
        }
    }

    fn pipelines(&self, git: Git, limits: BrowseLimits) -> Pipelines {
        Pipelines::new(
            self.t.db.clone(),
            Browser::new(git, self.storage.clone(), limits),
            Arc::new(Marks(CopyMark::Open)),
        )
    }

    fn normal(&self) -> Pipelines {
        self.pipelines(self.git.clone(), BrowseLimits::default())
    }

    fn request(&self, sha: &ObjectId, source: Source) -> PipelineRequest {
        PipelineRequest {
            project: self.project,
            git_ref: RefName::parse("refs/heads/feature").expect("ref"),
            sha: sha.clone(),
            source,
            merge_request: None,
            triggered_by: None,
        }
    }

    async fn rows(&self) -> Vec<(String, Option<String>, serde_json::Value)> {
        sqlx::query_as("SELECT status, error_code, problems FROM forge.ci_pipelines ORDER BY id")
            .fetch_all(&mut self.t.superuser().await)
            .await
            .expect("rows")
    }
}

fn rid(raw: &str) -> RequestId {
    RequestId::parse(raw).expect("id")
}

#[tokio::test]
async fn one_push_is_exactly_one_pipeline_with_its_jobs() {
    let w = World::new().await;
    let sha = commit(&w.path, &[(".forge-ci.yml", &valid())]);
    let p = w.normal();
    let request = w.request(&sha, Source::Push);
    let first = p
        .plan_and_record(&request, &rid("req-1"))
        .await
        .expect("plan");
    let Outcome::Recorded {
        created,
        status,
        jobs,
    } = first
    else {
        panic!("not recorded: {first:?}");
    };
    assert!(!created.existing);
    assert_eq!((status, jobs), (PipelineStatus::Pending, 2));
    // The job retried (or the event redelivered): the same row.
    let again = p
        .plan_and_record(&request, &rid("req-2"))
        .await
        .expect("again");
    assert!(
        matches!(again, Outcome::Recorded { created: c, .. } if c.existing && c.pipeline_id == created.pipeline_id)
    );
    assert_eq!(w.rows().await.len(), 1);
    let jobs: Vec<(String, String, Vec<String>)> = sqlx::query_as(
        "SELECT name, status, needs FROM forge.ci_jobs WHERE pipeline_id = $1 ORDER BY wave, name",
    )
    .bind(created.pipeline_id)
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("jobs");
    assert_eq!(
        jobs,
        [
            ("build".to_owned(), "pending".to_owned(), vec![]),
            (
                "test".to_owned(),
                "created".to_owned(),
                vec!["build".to_owned()]
            ),
        ]
    );
    let mark: String = sqlx::query_scalar("SELECT copy_mark FROM forge.ci_pipelines")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("mark");
    assert_eq!(mark, "open");
    w.t.drop_now().await;
}

#[tokio::test]
async fn no_definition_is_no_pipeline() {
    let w = World::new().await;
    let sha = commit(&w.path, &[("README.md", "hello\n")]);
    let out = w
        .normal()
        .plan_and_record(&w.request(&sha, Source::Push), &rid("req-none"))
        .await
        .expect("plan");
    assert_eq!(out, Outcome::NoDefinition);
    assert!(w.rows().await.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_invalid_definition_is_an_invalid_row_with_its_reason() {
    let w = World::new().await;
    let sha = commit(&w.path, &[(".forge-ci.yml", "version: 1\nstages: []\n")]);
    let out = w
        .normal()
        .plan_and_record(&w.request(&sha, Source::Push), &rid("req-bad"))
        .await
        .expect("plan");
    assert!(matches!(
        out,
        Outcome::Recorded {
            status: PipelineStatus::Invalid,
            ..
        }
    ));
    let rows = w.rows().await;
    assert_eq!(rows[0].0, "invalid");
    assert_eq!(rows[0].1.as_deref(), Some("FG-CI-0001"));
    assert!(!rows[0].2.as_array().expect("array").is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_merge_request_with_a_secret_job_is_invalid() {
    let w = World::new().await;
    let sha = commit(
        &w.path,
        &[(".forge-ci.yml", &with_secret_for_merge_requests())],
    );
    let mut request = w.request(&sha, Source::MergeRequest);
    request.merge_request = Some(open_merge_request(&w).await);
    let out = w
        .normal()
        .plan_and_record(&request, &rid("req-mr"))
        .await
        .expect("plan");
    assert!(matches!(
        out,
        Outcome::Recorded {
            status: PipelineStatus::Invalid,
            ..
        }
    ));
    let rows = w.rows().await;
    let reasons: Vec<&str> = rows[0]
        .2
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|p| p["reason"].as_str())
        .collect();
    assert!(
        reasons.contains(&"secret_on_unprotected_ref"),
        "{reasons:?}"
    );
    w.t.drop_now().await;
}

/// `ci_pipelines.protected` (what `StoredPipeline::secrets_allowed` reads,
/// ADR-0029): `true` only for a push to a branch protected now. The same
/// definition with a secret job, on the same protected branch: the push is
/// planned and protected; the manual run is `invalid` and not protected.
/// A push to another branch selects no job and is not protected.
#[tokio::test]
async fn only_a_push_to_a_protected_branch_is_recorded_as_protected() {
    let w = World::new().await;
    forge_db::protected_refs::create(
        &w.t.db,
        &ctx("req-protect"),
        w.project,
        &forge_core::RefPattern::parse("refs/heads/feature").expect("pattern"),
        forge_core::Role::Maintainer,
    )
    .await
    .expect("protect");
    let sha = commit(&w.path, &[(".forge-ci.yml", &with_secret_on_feature())]);
    let p = w.normal();
    for (source, request_id) in [(Source::Push, "req-push"), (Source::Manual, "req-manual")] {
        let out = p
            .plan_and_record(&w.request(&sha, source), &rid(request_id))
            .await
            .expect("plan");
        assert!(matches!(out, Outcome::Recorded { .. }), "{out:?}");
    }
    let mut other = w.request(&sha, Source::Push);
    other.git_ref = RefName::parse("refs/heads/other").expect("ref");
    let out = p
        .plan_and_record(&other, &rid("req-other"))
        .await
        .expect("plan");
    assert!(matches!(out, Outcome::Recorded { .. }), "{out:?}");
    let rows: Vec<(String, String, bool, String)> =
        sqlx::query_as("SELECT source, ref, protected, status FROM forge.ci_pipelines ORDER BY id")
            .fetch_all(&mut w.t.superuser().await)
            .await
            .expect("rows");
    let rows: Vec<(&str, &str, bool, &str)> = rows
        .iter()
        .map(|(s, r, p, st)| (s.as_str(), r.as_str(), *p, st.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("push", "refs/heads/feature", true, "pending"),
            ("manual", "refs/heads/feature", false, "invalid"),
            ("push", "refs/heads/other", false, "skipped"),
        ]
    );
    w.t.drop_now().await;
}

/// An open merge request `feature` → `main`, written as rows: the review
/// commands are forge-review's to call (its `decided_callers` guard).
async fn open_merge_request(w: &World) -> MergeRequestId {
    let alice: i64 = sqlx::query_scalar("SELECT id FROM forge.users WHERE username = 'alice'")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("alice");
    let p = w.project.get();
    w.t.commit_audited(
        ("project", p),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id,                  source_branch, target_branch) VALUES ({p}, 1, 'Feature', {alice},                  'refs/heads/feature', 'refs/heads/main')"
            ),
        ],
    )
    .await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM forge.merge_requests WHERE project_id = $1")
        .bind(p)
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("merge request");
    MergeRequestId::new(id)
}

/// A git that never answers: the plan fails with the git timeout (the job
/// retries it); the last attempt records the `error` row (FG-CI-0002).
#[tokio::test]
async fn a_git_timeout_retries_then_records_an_error_pipeline() {
    let w = World::new().await;
    let sha = commit(&w.path, &[(".forge-ci.yml", &valid())]);
    let stall = tempfile::tempdir().expect("stall");
    let home = tempfile::tempdir().expect("home");
    let slow = w.pipelines(
        forge_git::test_git::stalled_git(home.path(), stall.path(), None),
        BrowseLimits {
            timeout: forge_git::test_git::STALL,
            ..BrowseLimits::default()
        },
    );
    let request = w.request(&sha, Source::Push);
    let err = slow
        .plan_and_record(&request, &rid("req-slow"))
        .await
        .expect_err("timed out");
    assert_eq!(err.code(), ErrorCode::GitTimeout, "{err:?}");
    assert!(w.rows().await.is_empty(), "nothing written on a failure");
    slow.fail(&request, &rid("req-slow"), &err)
        .await
        .expect("error row written");
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "error");
    assert_eq!(rows[0].1.as_deref(), Some("FG-CI-0002"));
    // A later success of the same push does not add a second pipeline.
    let after = w
        .normal()
        .plan_and_record(&request, &rid("req-late"))
        .await
        .expect("plan");
    assert!(matches!(after, Outcome::Recorded { created, .. } if created.existing));
    w.t.drop_now().await;
}

/// Review C2 (MEDIUM): when the `error` row cannot be written either (here:
/// a project that does not exist), `fail` returns the write's error with
/// the cause's code — the job dies with it instead of ending as answered.
#[tokio::test]
async fn a_failed_error_row_is_returned_not_swallowed() {
    let w = World::new().await;
    let sha = commit(&w.path, &[(".forge-ci.yml", &valid())]);
    let mut request = w.request(&sha, Source::Push);
    request.project = ProjectId::new(w.project.get() + 1000);
    let cause = forge_errors::ForgeError::new(ErrorCode::GitTimeout);
    let err = w
        .normal()
        .fail(&request, &rid("req-gone"), &cause)
        .await
        .expect_err("the write fails");
    assert_ne!(err.code(), ErrorCode::GitTimeout, "the write's own code");
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "cause_code" && v == "FG-GIT-0002"));
    assert!(w.rows().await.is_empty());
    w.t.drop_now().await;
}
