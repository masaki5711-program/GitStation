#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `forge/spec-drift` end to end on real bare repositories and a migrated
//! database (M3 X2, ADR-0023 Amendment X2): the head's `.forge/repo.yml` is
//! read, every reference is followed for every signed-in person, version
//! pins are compared with the referenced repository's tags, and the run is
//! recorded by forge's worker (no reporter).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_checks::spec_drift::{Ran, SpecDrift};
use forge_core::checks::{CheckAnnotationLevel, CheckConclusion};
use forge_core::{ObjectId, ProjectId, RepoId, RequestId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::check_runs;
use forge_db::checks::spec_drift::SpecDriftRequest;
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
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

/// A commit whose tree is `README.md` and, when given, `.forge/repo.yml`.
fn commit(repo: &Path, yml: Option<&str>) -> String {
    let readme = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(b"synthetic\n"),
    );
    let mut root = format!("100644 blob {readme}\tREADME.md\n");
    if let Some(yml) = yml {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(yml.as_bytes()),
        );
        let forge = plumb(
            repo,
            &["mktree"],
            Some(format!("100644 blob {blob}\trepo.yml\n").as_bytes()),
        );
        root = format!("040000 tree {forge}\t.forge\n{root}");
    }
    let tree = plumb(repo, &["mktree"], Some(root.as_bytes()));
    plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None)
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    git: Git,
    storage: StorageRoot,
    web: CreatedProject,
    web_path: std::path::PathBuf,
    drift: SpecDrift,
}

impl World {
    /// A ready bare repository for `project` with `tags` and a `main`
    /// branch, all on one commit.
    async fn repo(&self, project: &CreatedProject, tags: &[&str]) -> std::path::PathBuf {
        let path = self.init(project.repository_id).await;
        let tip = commit(&path, None);
        plumb(&path, &["update-ref", "refs/heads/main", &tip], None);
        for tag in tags {
            plumb(
                &path,
                &["update-ref", &format!("refs/tags/{tag}"), &tip],
                None,
            );
        }
        path
    }

    async fn init(&self, repo: RepoId) -> std::path::PathBuf {
        let _ready = forge_db::repositories::mark_ready(&self.t.db, &system("req-ready"), repo)
            .await
            .expect("ready");
        self.storage
            .init_bare(&self.git, repo, uuid::Uuid::new_v4(), None)
            .await
            .expect("init")
    }

    /// A head of `web` carrying `yml`.
    async fn head(&self, yml: Option<&str>) -> ObjectId {
        let path = &self.web_path;
        let head = commit(path, yml);
        plumb(path, &["update-ref", "refs/heads/feature", &head], None);
        ObjectId::parse(&head).expect("sha")
    }

    fn request(&self, head: &ObjectId) -> SpecDriftRequest {
        SpecDriftRequest {
            project: self.web.project_id,
            head: head.clone(),
        }
    }
}

async fn project(
    t: &FreshDb,
    owner: &users::Created,
    segment: &str,
    v: Visibility,
) -> CreatedProject {
    let mut new = NewProject::new(segment, owner.namespace_id, segment, owner.user_id);
    new.visibility = v;
    projects::create(&t.db, &system(&format!("req-project-{segment}")), &new)
        .await
        .expect("project")
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
    let web = project(&t, &alice, "web", Visibility::Private).await;
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let db = Arc::new(t.db.clone());
    let drift = SpecDrift::new(t.db.clone(), browser, db.clone(), db);
    let mut w = World {
        t,
        _home: home,
        _root: root,
        git,
        storage,
        web,
        web_path: std::path::PathBuf::new(),
        drift,
    };
    w.web_path = w.init(w.web.repository_id).await;
    // The shared specifications: internal (every signed-in person reads
    // them) and private (Alice owns it: she may read it, not everyone).
    let specs = project(&w.t, &alice, "specs", Visibility::Internal).await;
    w.repo(&specs, &["v1.0", "v1.1", "v2.0-rc1", "spec-v9"])
        .await;
    let api = project(&w.t, &alice, "api", Visibility::Internal).await;
    w.repo(&api, &["v3"]).await;
    let docs = project(&w.t, &alice, "docs", Visibility::Internal).await;
    w.repo(&docs, &["v1"]).await;
    let secret = project(&w.t, &alice, "secret", Visibility::Private).await;
    w.repo(&secret, &["v1.0", "v1.7"]).await;
    w
}

const DRIFTED: &str = "specs:
  - repo: alice/specs
    version: v1.0
  - repo: alice/api
    track: main
  - repo: alice/secret
    version: v1.0
  - repo: alice/docs
    version: v9.9
  - repo: alice/nothing-here
    version: v1
  - path: spec/
";

#[tokio::test]
async fn a_pin_behind_the_latest_version_is_neutral_with_spec_version_behind() {
    let w = world().await;
    let head = w.head(Some(DRIFTED)).await;
    let ran = w
        .drift
        .run(
            &w.request(&head),
            &RequestId::parse("req-drift").expect("id"),
        )
        .await
        .expect("run");
    let Ran::Recorded(reported) = ran else {
        panic!("recorded: {ran:?}");
    };
    let run = &reported.run;
    assert_eq!(run.name.as_str(), "forge/spec-drift");
    assert_eq!(run.conclusion, Some(CheckConclusion::Neutral));
    assert_eq!(run.reported_by, None, "forge's own worker");
    let s = &run.summary;
    for needle in [
        "line 2: alice/specs version v1.0 — behind: v1.1 is published (1 later version(s)) [FG-REPO-0027]",
        "line 4: alice/api track main — follows the branch",
        "line 6: alice/secret version v1.0 — not found, or not readable by every signed-in person",
        "line 8: alice/docs version v9.9 — the pinned version does not exist [FG-REPO-0025]",
        "line 10: alice/nothing-here version v1 — not found",
        "line 12: spec/ — this repository's own specification",
    ] {
        assert!(s.contains(needle), "{needle}\nin\n{s}");
    }
    // A private repository Alice may read is still not disclosed to every
    // reader of `web`: nothing of its tags appears.
    assert!(!s.contains("v1.7"), "{s}");
    let notes: Vec<(u32, CheckAnnotationLevel)> = run
        .annotations
        .iter()
        .map(|a| (a.start_line, a.level))
        .collect();
    assert_eq!(
        notes,
        [
            (2, CheckAnnotationLevel::Warning),
            (6, CheckAnnotationLevel::Notice),
            (8, CheckAnnotationLevel::Warning),
            (10, CheckAnnotationLevel::Notice),
        ]
    );
    // Read back like any run of the commit (the merge status, `GET …/checks`).
    let listed = check_runs(&w.t.db, w.web.project_id, &head)
        .await
        .expect("list");
    assert_eq!(listed, vec![run.clone()]);
    w.t.drop_now().await;
}

#[tokio::test]
async fn up_to_date_pins_are_success_and_a_new_head_is_its_own_run() {
    let w = world().await;
    let current = w
        .head(Some(
            "specs:\n  - repo: alice/specs\n    version: v1.1\n  - repo: alice/api\n    version: v3\n",
        ))
        .await;
    let none = w.head(None).await;
    for (head, request) in [(&current, "req-current"), (&none, "req-none")] {
        let ran = w
            .drift
            .run(&w.request(head), &RequestId::parse(request).expect("id"))
            .await
            .expect("run");
        let Ran::Recorded(reported) = ran else {
            panic!("recorded: {ran:?}");
        };
        assert_eq!(
            reported.run.conclusion,
            Some(CheckConclusion::Success),
            "{}",
            reported.run.summary
        );
        assert!(reported.run.annotations.is_empty());
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_refused_repo_yml_is_neutral_and_follows_nothing() {
    let w = world().await;
    let head = w
        .head(Some("specs:\n  - repo: alice/specs\n    kind: spec\n"))
        .await;
    let ran = w
        .drift
        .run(&w.request(&head), &RequestId::parse("req-bad").expect("id"))
        .await
        .expect("run");
    let Ran::Recorded(reported) = ran else {
        panic!("recorded: {ran:?}");
    };
    assert_eq!(reported.run.conclusion, Some(CheckConclusion::Neutral));
    assert!(
        reported.run.summary.contains("FG-REPO-0024"),
        "{}",
        reported.run.summary
    );
    assert_eq!(reported.run.annotations.len(), 1);
    w.t.drop_now().await;
}

/// A head git cannot read fails the run (the job retries it; nothing is
/// recorded), and the job's last failure is recorded as `error` with the
/// code, so the head never stays without an answer.
#[tokio::test]
async fn a_failed_evaluation_records_nothing_until_it_is_recorded_as_error() {
    let w = world().await;
    let missing = ObjectId::parse(&"e".repeat(40)).expect("sha");
    let request = w.request(&missing);
    let id = RequestId::parse("req-missing").expect("id");
    let err = w
        .drift
        .run(&request, &id)
        .await
        .expect_err("no such commit");
    let runs = check_runs(&w.t.db, w.web.project_id, &missing)
        .await
        .expect("list");
    assert!(runs.is_empty(), "nothing recorded on a failure");
    w.drift.fail(&request, &id, &err).await;
    let runs = check_runs(&w.t.db, w.web.project_id, &missing)
        .await
        .expect("list");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].conclusion, Some(CheckConclusion::Error));
    assert!(
        runs[0].summary.contains(err.code().id()) && runs[0].summary.contains("req-missing"),
        "{}",
        runs[0].summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_project_that_is_gone_records_nothing() {
    let w = world().await;
    let request = SpecDriftRequest {
        project: ProjectId::new(w.web.project_id.get() + 1000),
        head: ObjectId::parse(&"a".repeat(40)).expect("sha"),
    };
    let ran = w
        .drift
        .run(&request, &RequestId::parse("req-gone").expect("id"))
        .await
        .expect("no error");
    assert_eq!(ran, Ran::ProjectGone);
    // And recording `error` on it fails loudly in the log, never panics.
    let e = ForgeError::new(ErrorCode::GitTimeout);
    w.drift
        .fail(&request, &RequestId::parse("req-gone").expect("id"), &e)
        .await;
    w.t.drop_now().await;
}
