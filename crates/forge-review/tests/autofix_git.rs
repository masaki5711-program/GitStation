#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The deterministic autofix (M4 C14, ADR-0017 Amendment C14) against a
//! real bare repository and the development database: `Autofixer::tick`
//! starts a fixer's pipeline from the target branch's `.forge-ci.yml`, a
//! simulated runner (real git, outside forge's repository) builds the
//! bundle of its transport commit, the next tick checks it and lands the
//! bot's commit on the source branch — or refuses, proposes, supersedes.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::*;
use forge_core::autofix::AttemptState;
use forge_core::{ProjectId, RefPattern, Role, UserId};
use forge_db::autofix::{self as afdb, StoredAttempt};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::approvals::ReviewGit;
use forge_review::{Autofixer, NewFields, Review, StoredMergeRequest};

fn git_cmd(repo: &Path, args: &[&str], input: Option<&[u8]>, index: Option<&Path>) -> String {
    use std::io::Write as _;
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let mut child = cmd.spawn().expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn plumb(repo: &Path, args: &[&str]) -> String {
    git_cmd(repo, args, None, None)
}

/// A commit of `files` (nested paths allowed): on `base` (its tree with
/// these files changed; `None` content removes one), with `parents`.
fn commit_on(
    repo: &Path,
    scratch: &Path,
    base: Option<&str>,
    files: &[(&str, Option<&str>)],
    parents: &[&str],
) -> String {
    let index = scratch.join(format!("index-{}", uuid::Uuid::new_v4()));
    match base {
        Some(b) => git_cmd(repo, &["read-tree", b], None, Some(&index)),
        None => git_cmd(repo, &["read-tree", "--empty"], None, Some(&index)),
    };
    for (path, content) in files {
        match content {
            Some(c) => {
                let blob = git_cmd(
                    repo,
                    &["hash-object", "-w", "--stdin"],
                    Some(c.as_bytes()),
                    None,
                );
                let info = format!("100644,{blob},{path}");
                git_cmd(
                    repo,
                    &["update-index", "--add", "--cacheinfo", &info],
                    None,
                    Some(&index),
                );
            }
            None => {
                git_cmd(
                    repo,
                    &["update-index", "--force-remove", path],
                    None,
                    Some(&index),
                );
            }
        }
    }
    let tree = git_cmd(repo, &["write-tree"], None, Some(&index));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    for p in parents {
        args.extend(["-p", p]);
    }
    plumb(repo, &args)
}

fn image(c: char) -> String {
    format!("registry.internal/rust@sha256:{}", c.to_string().repeat(64))
}

/// The target branch's definition: a job, a `format` fixer for Rust and
/// SQL files, a `regenerate` fixer for the lock file.
fn definition() -> String {
    format!(
        "version: 1
stages: [build]
jobs:
  build:
    stage: build
    image: {job}
    timeout: 10m
    script: [sh ./ci/check.sh]
autofix:
  fixers:
    fmt:
      kind: format
      image: {fix}
      script: [cargo fmt --all]
      timeout: 10m
      paths: [\"**/*.rs\", \"**/*.sql\"]
    lockfile:
      kind: regenerate
      image: {fix}
      script: [cargo generate-lockfile --offline]
      timeout: 10m
      paths: [Cargo.lock]
",
        job = image('a'),
        fix = image('b'),
    )
}

const LOCK_BASE: &str = "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"z\"\nversion = \"1.0.0\"\n";
const LOCK_TARGET: &str = "[[package]]\nname = \"a\"\nversion = \"1.1.0\"\n\n[[package]]\nname = \"z\"\nversion = \"1.0.0\"\n";
const LOCK_HEAD: &str = "[[package]]\nname = \"a\"\nversion = \"1.2.0\"\n\n[[package]]\nname = \"z\"\nversion = \"1.0.0\"\n";

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    scratch: tempfile::TempDir,
    path: PathBuf,
    autofixer: Autofixer,
    owner: UserId,
    project: ProjectId,
    mr: StoredMergeRequest,
    base: String,
    head: String,
    target: String,
}

#[derive(Clone, Copy)]
enum Shape {
    /// `feature` leaves `src/lib.rs` unformatted (and changes its own
    /// `.forge-ci.yml`, which the bot must keep).
    Unformatted,
    /// `feature` and `main` change the same hunk of `Cargo.lock`.
    LockConflict,
}

/// `acme/web`: olga owns it, dave (Developer) opens `feature` → `main`.
async fn world(shape: Shape) -> World {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dev = user(&t, "dave").await;
    let web = project(&t, owner, &[(dev, Role::Developer)]).await;
    let _marked = repositories::mark_ready(&t.db, &system("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let scratch = tempfile::tempdir().expect("scratch");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let s = scratch.path();
    let def = definition();
    let base = commit_on(
        &path,
        s,
        None,
        &[
            (".forge-ci.yml", Some(def.as_str())),
            ("README.md", Some("base\n")),
            ("src/lib.rs", Some("fn main() {}\n")),
            (
                "db/migrations/0001_init.sql",
                Some("CREATE TABLE t (id int);\n"),
            ),
            ("Cargo.lock", Some(LOCK_BASE)),
        ],
        &[],
    );
    let (head, target) = match shape {
        Shape::Unformatted => {
            let head_def = format!("{def}# the feature's own comment\n");
            let head = commit_on(
                &path,
                s,
                Some(&base),
                &[
                    ("src/lib.rs", Some("fn  main(){}\n")),
                    (".forge-ci.yml", Some(head_def.as_str())),
                ],
                &[&base],
            );
            (head, base.clone())
        }
        Shape::LockConflict => {
            let target = commit_on(
                &path,
                s,
                Some(&base),
                &[("Cargo.lock", Some(LOCK_TARGET))],
                &[&base],
            );
            let head = commit_on(
                &path,
                s,
                Some(&base),
                &[("Cargo.lock", Some(LOCK_HEAD))],
                &[&base],
            );
            (head, target)
        }
    };
    plumb(&path, &["update-ref", "refs/heads/main", &target]);
    plumb(&path, &["update-ref", "refs/heads/feature", &head]);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let review = Review::new(t.db.clone(), Arc::new(browser.clone()))
        .with_review_git(ReviewGit::new(git.clone(), storage.clone()));
    let proof = open_proof(
        &t,
        &session(dev),
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
                title: "Format the library".to_owned(),
                description: "Synthetic description.".to_owned(),
            },
        )
        .await
        .expect("open");
    t.exec_superuser(&format!(
        "UPDATE forge.merge_requests SET head_sha = '{head}' WHERE id = {}",
        mr.id.get()
    ))
    .await;
    let artifacts = scratch.path().join("artifacts");
    let bundle_file: forge_review::BundleFile = Arc::new(move |project: i64, id: i64| {
        artifacts.join(project.to_string()).join(id.to_string())
    });
    let autofixer = Autofixer::new(t.db.clone(), git, storage, bundle_file).with_browser(browser);
    World {
        t,
        _home: home,
        _root: root,
        scratch,
        path,
        autofixer,
        owner,
        project: web.project_id,
        mr,
        base,
        head,
        target,
    }
}

impl World {
    async fn enable(&self) {
        let (_, s, ()) = afdb::set_enabled_decided(
            &self.t.db,
            &system("req-on"),
            self.project,
            self.owner,
            true,
            |_| Ok(()),
        )
        .await
        .expect("on");
        assert!(s.enabled);
    }

    /// A queued attempt as the trigger subscriber records it.
    async fn queue(&self, trigger: &str) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.autofix_attempts (project_id, merge_request_id, trigger, \
                   head_sha, target_sha, state, request_id) \
                 VALUES ({}, {}, '{trigger}', '{}', '{}', 'queued', 'req-trigger')",
                self.project.get(),
                self.mr.id.get(),
                self.head,
                self.target,
            ))
            .await;
    }

    async fn latest(&self) -> StoredAttempt {
        afdb::latest(&self.t.db, self.mr.id)
            .await
            .expect("read")
            .expect("an attempt")
    }

    async fn tick(&self) {
        self.autofixer.tick().await.expect("tick");
    }

    fn tip(&self, name: &str) -> String {
        plumb(&self.path, &["rev-parse", &format!("refs/heads/{name}")])
    }

    /// The runner's part (real git, outside forge's repository): fetch the
    /// input commit, record `files` on it in one transport commit on
    /// `parent` (the input unless a broken runner chose another), bundle
    /// `refs/forge/autofix ^<parent>`, store it as the job's artifact and
    /// end the job `succeeded`.
    async fn run_fixer(&self, files: &[(&str, Option<&str>)], parent: Option<&str>) {
        let a = self.latest().await;
        assert_eq!(a.state, AttemptState::Running, "{a:?}");
        let input = a.input_sha.clone().expect("input").as_str().to_owned();
        let s = self.scratch.path();
        let runner = s.join(format!("runner-{}.git", a.id));
        plumb(&runner, &["init", "--bare", "--quiet"]);
        plumb(
            &self.path,
            &["update-ref", "refs/heads/autofix-input", &input],
        );
        let parent = parent.map_or(input.clone(), str::to_owned);
        plumb(
            &self.path,
            &["update-ref", "refs/heads/autofix-parent", &parent],
        );
        plumb(
            &runner,
            &[
                "fetch",
                "--quiet",
                self.path.to_str().expect("utf8"),
                "refs/heads/autofix-input:refs/forge/job",
                "refs/heads/autofix-parent:refs/forge/parent",
            ],
        );
        plumb(
            &self.path,
            &["update-ref", "-d", "refs/heads/autofix-input"],
        );
        plumb(
            &self.path,
            &["update-ref", "-d", "refs/heads/autofix-parent"],
        );
        let transport = commit_on(&runner, s, Some(&input), files, &[&parent]);
        plumb(&runner, &["update-ref", "refs/forge/autofix", &transport]);
        let bundle = s.join(format!("bundle-{}", a.id));
        plumb(
            &runner,
            &[
                "bundle",
                "create",
                bundle.to_str().expect("utf8"),
                "refs/forge/autofix",
                &format!("^{parent}"),
            ],
        );
        let bytes = std::fs::read(&bundle).expect("bundle");
        let digest: String = {
            use sha2::Digest as _;
            sha2::Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let job = a.job_id.expect("job");
        let pipeline = a.pipeline_id.expect("pipeline");
        let rows = self
            .t
            .json_rows(&format!(
                "INSERT INTO forge.ci_artifacts (project_id, pipeline_id, job_id, attempt, path, \
                   size_bytes, sha256, expires_at) VALUES ({}, {pipeline}, {job}, 1, \
                   'autofix.bundle', {}, decode('{digest}', 'hex'), now() + interval '1 day') \
                 RETURNING id",
                self.project.get(),
                bytes.len()
            ))
            .await;
        let id = rows[0]["id"].as_i64().expect("id");
        let file = s
            .join("artifacts")
            .join(self.project.get().to_string())
            .join(id.to_string());
        std::fs::create_dir_all(file.parent().expect("dir")).expect("dir");
        std::fs::write(&file, &bytes).expect("artifact");
        self.end_job("succeeded").await;
    }

    async fn end_job(&self, status: &str) {
        let a = self.latest().await;
        self.t
            .exec_superuser(&format!(
                "UPDATE forge.ci_jobs SET status = '{status}' WHERE id = {}",
                a.job_id.expect("job")
            ))
            .await;
    }
}

/// The task: the fmt fixer end to end with real git — the bot's commit
/// lands on the source branch, built by the server (bot identity, parent
/// the head, fixed message and trailers), with the feature's own
/// `.forge-ci.yml` kept (the input carried the target's).
#[tokio::test]
async fn the_fmt_fixer_lands_the_bots_commit_on_the_source_branch() {
    let w = world(Shape::Unformatted).await;
    w.enable().await;
    w.queue("pipeline_failed").await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Running, "{a:?}");
    assert_eq!(a.fixer.as_deref(), Some("fmt"));
    // The fixer's own pipeline: source autofix, one job, the target's
    // definition, at the input commit (head's tree, target's definition).
    let rows =
        w.t.json_rows(&format!(
            "SELECT p.source, p.sha, p.merge_request_id, j.name, j.image, j.secrets \
             FROM forge.ci_pipelines p JOIN forge.ci_jobs j ON j.pipeline_id = p.id \
             WHERE p.id = {}",
            a.pipeline_id.expect("pipeline")
        ))
        .await;
    assert_eq!(rows[0]["source"], "autofix");
    assert_eq!(rows[0]["name"], "fmt");
    assert_eq!(rows[0]["image"], image('b'));
    assert_eq!(rows[0]["secrets"], serde_json::json!([]));
    let input = a.input_sha.clone().expect("input");
    let input_def = plumb(
        &w.path,
        &[
            "cat-file",
            "-p",
            &format!("{}:.forge-ci.yml", input.as_str()),
        ],
    );
    assert_eq!(format!("{input_def}\n"), definition());
    w.run_fixer(&[("src/lib.rs", Some("fn main() {}\n"))], None)
        .await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Applied, "{a:?}");
    let bot = a.commit_sha.clone().expect("commit");
    assert_eq!(w.tip("feature"), bot.as_str());
    assert_eq!(
        plumb(&w.path, &["rev-parse", &format!("{}^@", bot.as_str())]),
        w.head
    );
    let ident = plumb(
        &w.path,
        &["log", "-1", "--format=%an <%ae>|%cn <%ce>", bot.as_str()],
    );
    assert_eq!(
        ident,
        "forge-autofix <autofix@forge.invalid>|forge-autofix <autofix@forge.invalid>"
    );
    let message = plumb(&w.path, &["log", "-1", "--format=%B", bot.as_str()]);
    assert!(
        message.starts_with("autofix(pipeline_failed): apply the fmt fixer"),
        "{message}"
    );
    assert!(
        message.contains(&format!("Forge-Autofix-Attempt: {}", a.id)),
        "{message}"
    );
    assert!(!message.contains("Signed-off-by"));
    let lib = plumb(
        &w.path,
        &["cat-file", "-p", &format!("{}:src/lib.rs", bot.as_str())],
    );
    assert_eq!(lib, "fn main() {}");
    let def = plumb(
        &w.path,
        &["cat-file", "-p", &format!("{}:.forge-ci.yml", bot.as_str())],
    );
    assert!(def.ends_with("# the feature's own comment"), "{def}");
    // Recorded as the bot's commit and as a push; the event of the end.
    assert!(afdb::is_bot_commit(&w.t.db, w.project, &bot)
        .await
        .expect("bot"));
    let events =
        w.t.json_rows(
            "SELECT payload FROM forge.events WHERE event_type = 'merge_request.autofix_finished'",
        )
        .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["payload"]["state"], "applied");
    let pushes = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'repository.pushed'",
    )
    .await;
    assert_eq!(pushes, 1);
    w.t.drop_now().await;
}

/// The task: a path outside the fixer's `paths` is refused (FG-GIT-0037),
/// and nothing moves.
#[tokio::test]
async fn a_change_outside_the_fixer_paths_is_refused() {
    let w = world(Shape::Unformatted).await;
    w.enable().await;
    w.queue("pipeline_failed").await;
    w.tick().await;
    w.run_fixer(
        &[
            ("src/lib.rs", Some("fn main() {}\n")),
            ("README.md", Some("rewritten\n")),
        ],
        None,
    )
    .await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Refused, "{a:?}");
    assert_eq!(a.reason.as_deref(), Some("outside_fixer_paths"));
    assert_eq!(
        a.error_code.as_deref(),
        Some(ErrorCode::AutofixCandidateInvalid.id())
    );
    assert_eq!(w.tip("feature"), w.head);
    w.t.drop_now().await;
}

/// The task: a migration is a proposal only (FG-REVIEW-0031); nothing is
/// pushed, the line names the path.
#[tokio::test]
async fn a_migration_change_is_a_proposal_only() {
    let w = world(Shape::Unformatted).await;
    w.enable().await;
    w.queue("pipeline_failed").await;
    w.tick().await;
    w.run_fixer(
        &[
            ("src/lib.rs", Some("fn main() {}\n")),
            (
                "db/migrations/0001_init.sql",
                Some("CREATE TABLE t (id bigint);\n"),
            ),
        ],
        None,
    )
    .await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Proposed, "{a:?}");
    assert_eq!(a.reason.as_deref(), Some("migration"));
    assert_eq!(
        a.error_code.as_deref(),
        Some(ErrorCode::AutofixProposalOnly.id())
    );
    assert_eq!(a.detail[0].path, "db/migrations/0001_init.sql");
    assert_eq!(w.tip("feature"), w.head);
    w.t.drop_now().await;
}

/// The task: a bundle whose transport commit is not on the input commit
/// is a precondition mismatch (FG-GIT-0037), nothing moves.
#[tokio::test]
async fn a_bundle_on_another_commit_is_refused_as_a_precondition_mismatch() {
    let w = world(Shape::Unformatted).await;
    w.enable().await;
    w.queue("pipeline_failed").await;
    w.tick().await;
    // Built on the head, not on the input commit.
    w.run_fixer(
        &[("src/lib.rs", Some("fn main() {}\n"))],
        Some(&w.head.clone()),
    )
    .await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Refused, "{a:?}");
    assert_eq!(a.reason.as_deref(), Some("precondition_mismatch"));
    assert_eq!(w.tip("feature"), w.head);
    w.t.drop_now().await;
}

/// The task (plan 04 C-5): a lock file conflict is resolved by the
/// `regenerate` fixer inside the conflicted hunk; the bot's commit merges
/// head and target (parents head, target) and fast-forwards the source.
#[tokio::test]
async fn a_lock_file_conflict_is_resolved_by_regeneration() {
    let w = world(Shape::LockConflict).await;
    w.enable().await;
    w.queue("conflict").await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Running, "{a:?}");
    assert_eq!(a.fixer.as_deref(), Some("lockfile"));
    let input = a.input_sha.clone().expect("input");
    let parents = plumb(&w.path, &["rev-parse", &format!("{}^@", input.as_str())]);
    assert_eq!(parents, format!("{}\n{}", w.head, w.target));
    w.run_fixer(&[("Cargo.lock", Some(LOCK_HEAD))], None).await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Applied, "{a:?}");
    let bot = a.commit_sha.clone().expect("commit");
    assert_eq!(w.tip("feature"), bot.as_str());
    let parents = plumb(&w.path, &["rev-parse", &format!("{}^@", bot.as_str())]);
    assert_eq!(parents, format!("{}\n{}", w.head, w.target));
    let lock = plumb(
        &w.path,
        &["cat-file", "-p", &format!("{}:Cargo.lock", bot.as_str())],
    );
    assert_eq!(format!("{lock}\n"), LOCK_HEAD);
    w.t.drop_now().await;
}

/// The task (ADR-0017 §1.2): a conflict candidate that changes the lock
/// file outside its conflicted hunk is refused.
#[tokio::test]
async fn a_change_outside_the_conflicted_hunk_is_refused() {
    let w = world(Shape::LockConflict).await;
    w.enable().await;
    w.queue("conflict").await;
    w.tick().await;
    let sneaky = LOCK_HEAD.replace(
        "name = \"z\"\nversion = \"1.0.0\"",
        "name = \"z\"\nversion = \"6.6.6\"",
    );
    w.run_fixer(&[("Cargo.lock", Some(sneaky.as_str()))], None)
        .await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Refused, "{a:?}");
    assert_eq!(a.reason.as_deref(), Some("outside_conflict_hunk"));
    assert_eq!(w.tip("feature"), w.head);
    w.t.drop_now().await;
}

/// ADR-0017 §2.5: the bot never writes a protected branch (FG-AUTH-0024),
/// decided before git is read: no pipeline is created.
#[tokio::test]
async fn the_bot_may_not_push_to_a_protected_source_branch() {
    let w = world(Shape::Unformatted).await;
    w.enable().await;
    forge_db::protected_refs::create(
        &w.t.db,
        &system("req-protect"),
        w.project,
        &RefPattern::parse("refs/heads/feature").expect("pattern"),
        Role::Developer,
    )
    .await
    .expect("protect");
    w.queue("pipeline_failed").await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Skipped, "{a:?}");
    assert_eq!(a.reason.as_deref(), Some("denied"));
    assert_eq!(
        a.error_code.as_deref(),
        Some(ErrorCode::AutofixPushDenied.id())
    );
    assert_eq!(a.pipeline_id, None);
    w.t.drop_now().await;
}

/// ADR-0017 §1.1: a person's push meanwhile supersedes the attempt
/// (FG-GIT-0038); the person's commit is never overwritten.
#[tokio::test]
async fn a_head_that_moved_supersedes_the_attempt() {
    let w = world(Shape::Unformatted).await;
    w.enable().await;
    w.queue("pipeline_failed").await;
    w.tick().await;
    w.run_fixer(&[("src/lib.rs", Some("fn main() {}\n"))], None)
        .await;
    let person = commit_on(
        &w.path,
        w.scratch.path(),
        Some(&w.head),
        &[("README.md", Some("a person's change\n"))],
        &[&w.head],
    );
    plumb(&w.path, &["update-ref", "refs/heads/feature", &person]);
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_requests SET head_sha = '{person}' WHERE id = {}",
        w.mr.id.get()
    ))
    .await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Superseded, "{a:?}");
    assert_eq!(
        a.error_code.as_deref(),
        Some(ErrorCode::AutofixLandingRaced.id())
    );
    assert_eq!(w.tip("feature"), person);
    w.t.drop_now().await;
}

/// A fixer job that failed fails the attempt (FG-CI-0031); the merge
/// request is unchanged.
#[tokio::test]
async fn a_failed_fixer_job_fails_the_attempt() {
    let w = world(Shape::Unformatted).await;
    w.enable().await;
    w.queue("pipeline_failed").await;
    w.tick().await;
    w.end_job("failed").await;
    w.tick().await;
    let rows =
        w.t.json_rows(
            "SELECT state, reason, error_code, fixer FROM forge.autofix_attempts ORDER BY id",
        )
        .await;
    assert_eq!(rows[0]["state"], "failed", "{rows:?}");
    assert_eq!(rows[0]["error_code"], ErrorCode::AutofixFixerFailed.id());
    // The next fixer is queued on the same head; there is none for a
    // pipeline failure (the lock file's is for conflicts): it ends.
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Skipped, "{a:?}");
    assert_eq!(a.reason.as_deref(), Some("no_fix"));
    assert_eq!(w.tip("feature"), w.head);
    w.t.drop_now().await;
}

/// The task: autofix is off by default — a queued attempt of a project
/// that never turned it on runs nothing.
#[tokio::test]
async fn nothing_runs_while_autofix_is_off() {
    let w = world(Shape::Unformatted).await;
    w.queue("pipeline_failed").await;
    w.tick().await;
    let a = w.latest().await;
    assert_eq!(a.state, AttemptState::Skipped, "{a:?}");
    assert_eq!(a.reason.as_deref(), Some("disabled"));
    assert_eq!(a.pipeline_id, None);
    assert_eq!(w.base.len(), 40);
    w.t.drop_now().await;
}
