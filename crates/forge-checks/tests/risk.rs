#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `forge/risk` end to end on real bare repositories and a migrated
//! database (M3 X7, ADR-0023 Amendment X7): the head's diff against the
//! merge base with each target is classified by the deterministic rules and
//! the run is recorded by forge's worker (no reporter).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_checks::risk::{record_error, Limits, Ran, Risk};
use forge_core::checks::{CheckAnnotationLevel, CheckConclusion};
use forge_core::{ObjectId, ProjectId, RefName, RequestId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::check_runs;
use forge_db::checks::risk::{RiskRequest, RiskTargets};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};

fn plumb(repo: &Path, index: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_INDEX_FILE", index)
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn request_id(raw: &str) -> RequestId {
    RequestId::parse(raw).expect("request id")
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    scratch: tempfile::TempDir,
    project: ProjectId,
    owner: forge_core::UserId,
    repo: PathBuf,
    repo_id: forge_core::RepoId,
    risk: Risk,
}

impl World {
    /// A commit of `files` (path, content); every file is a regular one.
    fn commit(&self, files: &[(&str, &str)], parent: Option<&str>) -> String {
        let index = self
            .scratch
            .path()
            .join(format!("index-{}", uuid::Uuid::new_v4()));
        for (path, content) in files {
            let object = plumb(
                &self.repo,
                &index,
                &["hash-object", "-w", "--stdin"],
                Some(content.as_bytes()),
            );
            plumb(
                &self.repo,
                &index,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{object},{path}"),
                ],
                None,
            );
        }
        let tree = plumb(&self.repo, &index, &["write-tree"], None);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
        if let Some(parent) = parent {
            args.push("-p");
            args.push(parent);
        }
        plumb(&self.repo, &index, &args, None)
    }

    /// A commit of `base_files` plus `extra` entries given as raw bytes
    /// (path, blob content, mode): a non-UTF-8 name, a symlink (`120000`,
    /// its content the target), a gitlink (`160000`, its content the
    /// commit id, not stored). `update-index --index-info` reads the paths
    /// from stdin, so no argument has to be valid Unicode.
    fn commit_raw(&self, extra: &[(&[u8], &[u8], &str)], parent: Option<&str>) -> String {
        let index = self
            .scratch
            .path()
            .join(format!("index-{}", uuid::Uuid::new_v4()));
        let mut info: Vec<u8> = Vec::new();
        let base: Vec<(&[u8], &[u8], &str)> = base_files()
            .into_iter()
            .map(|(p, c)| (p.as_bytes(), c.as_bytes(), "100644"))
            .collect();
        for (path, content, mode) in base.iter().chain(extra.iter()) {
            let object = if *mode == "160000" {
                String::from_utf8(content.to_vec()).expect("commit id")
            } else {
                plumb(
                    &self.repo,
                    &index,
                    &["hash-object", "-w", "--stdin"],
                    Some(content),
                )
            };
            info.extend_from_slice(format!("{mode} {object}\t").as_bytes());
            info.extend_from_slice(path);
            info.push(b'\n');
        }
        plumb(
            &self.repo,
            &index,
            &["update-index", "--add", "--index-info"],
            Some(&info),
        );
        let tree = plumb(&self.repo, &index, &["write-tree"], None);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
        if let Some(parent) = parent {
            args.push("-p");
            args.push(parent);
        }
        plumb(&self.repo, &index, &args, None)
    }

    fn branch(&self, name: &str, commit: &str) {
        let index = self.scratch.path().join("index-ref");
        plumb(
            &self.repo,
            &index,
            &["update-ref", &format!("refs/heads/{name}"), commit],
            None,
        );
    }

    fn request(&self, head: &str) -> RiskRequest {
        RiskRequest {
            project: self.project,
            head: ObjectId::parse(head).expect("sha"),
        }
    }

    /// Run against `main` and read the one run back.
    async fn assess(&self, head: &str) -> forge_db::checks::StoredCheckRun {
        self.assess_against(head, &["main"]).await
    }

    async fn assess_against(
        &self,
        head: &str,
        targets: &[&str],
    ) -> forge_db::checks::StoredCheckRun {
        let targets = RiskTargets {
            targets: targets
                .iter()
                .map(|t| RefName::parse(&format!("refs/heads/{t}")).expect("branch"))
                .collect(),
            more: false,
        };
        let ran = self
            .risk
            .run_against(&self.request(head), &targets, &request_id("req-risk"))
            .await
            .expect("run");
        assert!(matches!(ran, Ran::Recorded(_)), "{ran:?}");
        let run = self.run_named(head, forge_core::checks::RISK_CHECK).await;
        assert_eq!(run.reported_by, None, "forge's own run has no reporter");
        run
    }

    /// The run `name` on `head` (plan 16 A7: the risk job writes
    /// `forge/risk` and `forge/agent-lock`, nothing else).
    async fn run_named(&self, head: &str, name: &str) -> forge_db::checks::StoredCheckRun {
        let runs = check_runs(
            &self.t.db,
            self.project,
            &ObjectId::parse(head).expect("sha"),
        )
        .await
        .expect("runs");
        assert_eq!(runs.len(), 2, "{runs:?}");
        runs.into_iter()
            .find(|r| r.name.as_str() == name)
            .expect("the run")
    }
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
    let _ready = forge_db::repositories::mark_ready(&t.db, &system("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let repo = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let db = Arc::new(t.db.clone());
    let risk = Risk::new(t.db.clone(), git, storage, browser, db);
    World {
        t,
        _home: home,
        _root: root,
        scratch: tempfile::tempdir().expect("scratch"),
        project: web.project_id,
        owner: alice.user_id,
        repo,
        repo_id: web.repository_id,
        risk,
    }
}

/// `main` with a migration, a manifest and a source file.
fn base_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("README.md", "synthetic\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("Cargo.toml", "[package]\nname = \"x\"\n"),
        ("db/migrations/0001_init.sql", "CREATE TABLE t (id int);\n"),
    ]
}

/// A world whose `main` holds `base_files`, and the base commit.
async fn world_with_main() -> (World, String) {
    let w = world().await;
    let base = w.commit(&base_files(), None);
    w.branch("main", &base);
    (w, base)
}

fn with<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut files = base_files();
    files.extend_from_slice(extra);
    files
}

#[tokio::test]
async fn a_change_that_drops_a_table_is_a_failure_with_risk_stop() {
    let (w, base) = world_with_main().await;
    let head = w.commit(
        &with(&[(
            "db/migrations/0002_drop.sql",
            "SELECT 1;\nDROP TABLE customers;\n",
        )]),
        Some(&base),
    );
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("FG-REVIEW-0023"), "{}", run.summary);
    assert!(run.summary.contains("sql_drop"), "{}", run.summary);
    assert!(
        run.summary.contains("db/migrations/0002_drop.sql"),
        "{}",
        run.summary
    );
    // The matched text is never copied into a run every reader sees.
    assert!(!run.summary.contains("customers"), "{}", run.summary);
    let note = run
        .annotations
        .iter()
        .find(|a| a.message.contains("sql_drop"))
        .expect("annotation");
    assert_eq!(
        (note.path.as_str(), note.start_line, note.level),
        (
            "db/migrations/0002_drop.sql",
            2,
            CheckAnnotationLevel::Failure
        )
    );
    assert!(run
        .annotations
        .iter()
        .all(|a| !a.message.contains("customers")));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_new_migration_and_a_dependency_change_are_neutral_with_risk_warn() {
    let (w, base) = world_with_main().await;
    let mut files = with(&[(
        "db/migrations/0002_add.sql",
        "ALTER TABLE t ADD COLUMN c int;\n",
    )]);
    files.retain(|(p, _)| *p != "Cargo.toml");
    files.push((
        "Cargo.toml",
        "[package]\nname = \"x\"\n[dependencies]\nserde = \"1\"\n",
    ));
    let head = w.commit(&files, Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("FG-REVIEW-0024"), "{}", run.summary);
    assert!(!run.summary.contains("FG-REVIEW-0023"), "{}", run.summary);
    assert!(run.summary.contains("new_migration"), "{}", run.summary);
    assert!(run.summary.contains("dependency_change"), "{}", run.summary);
    assert!(run
        .annotations
        .iter()
        .all(|a| a.level == CheckAnnotationLevel::Warning));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_plain_change_is_a_success() {
    let (w, base) = world_with_main().await;
    let mut files = base_files();
    files.retain(|(p, _)| *p != "src/main.rs");
    files.push(("src/main.rs", "fn main() { println!(\"hi\"); }\n"));
    let head = w.commit(&files, Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    assert!(run.annotations.is_empty());
    assert!(run.summary.contains("No risk"), "{}", run.summary);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_head_with_no_open_merge_request_is_neutral_and_not_assessed() {
    let (w, base) = world_with_main().await;
    let head = w.commit(&with(&[("src/lib.rs", "pub fn f() {}\n")]), Some(&base));
    let no_targets = RiskTargets {
        targets: Vec::new(),
        more: false,
    };
    w.risk
        .run_against(&w.request(&head), &no_targets, &request_id("req-none"))
        .await
        .expect("run");
    let runs = check_runs(&w.t.db, w.project, &ObjectId::parse(&head).expect("sha"))
        .await
        .expect("runs");
    // Plan 16 A7: the job writes forge/agent-lock beside it.
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].conclusion, Some(CheckConclusion::Neutral));
    assert!(
        runs[0].summary.contains("FG-REVIEW-0025"),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("no_open_merge_request"),
        "{}",
        runs[0].summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_missing_target_or_unrelated_history_is_not_assessed_never_a_success() {
    let (w, base) = world_with_main().await;
    let head = w.commit(&with(&[("src/lib.rs", "pub fn f() {}\n")]), Some(&base));
    let run = w.assess_against(&head, &["gone"]).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("target_missing"), "{}", run.summary);
    // A root commit shares no history with main.
    let stranger = w.commit(&[("other.txt", "x\n")], None);
    let run = w.assess_against(&stranger, &["main"]).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("no_common_history"), "{}", run.summary);
    w.t.drop_now().await;
}

#[tokio::test]
async fn deleting_half_the_files_is_a_stop_but_moving_them_is_not() {
    let w = world().await;
    let names: Vec<String> = (0..40).map(|i| format!("src/m{i}.rs")).collect();
    let bodies: Vec<String> = (0..40)
        .map(|i| {
            (0..10)
                .map(|l| format!("fn f{i}_{l}() {{}}\n"))
                .collect::<String>()
        })
        .collect();
    let all: Vec<(&str, &str)> = names
        .iter()
        .zip(&bodies)
        .map(|(n, b)| (n.as_str(), b.as_str()))
        .collect();
    let base = w.commit(&all, None);
    w.branch("main", &base);
    // 20 of 40 deleted: (b) fires. The lines removed are 200 of 400: under
    // 500, so (a) does not.
    let head = w.commit(&all[20..], Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("mass_deletion_files"),
        "{}",
        run.summary
    );
    assert!(
        !run.summary.contains("mass_deletion_lines"),
        "{}",
        run.summary
    );
    // The same files moved to another directory: renames, no deletion.
    let moved_names: Vec<String> = (0..40).map(|i| format!("lib/m{i}.rs")).collect();
    let moved: Vec<(&str, &str)> = moved_names
        .iter()
        .zip(&bodies)
        .map(|(n, b)| (n.as_str(), b.as_str()))
        .collect();
    let head = w.commit(&moved, Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn removing_most_of_a_big_file_is_a_stop_by_lines() {
    let w = world().await;
    let big: String = (0..1000).map(|i| format!("line {i}\n")).collect();
    let base = w.commit(&[("big.txt", &big), ("keep.txt", "k\n")], None);
    w.branch("main", &base);
    let small: String = (0..300).map(|i| format!("line {i}\n")).collect();
    let head = w.commit(&[("big.txt", &small), ("keep.txt", "k\n")], Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("mass_deletion_lines"),
        "{}",
        run.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_secret_in_a_file_or_a_credential_file_is_a_stop_without_the_secret() {
    let (w, base) = world_with_main().await;
    let key = "AKIAQWERTYUIOPASDFGH";
    let head = w.commit(
        &with(&[
            ("config/prod.txt", &format!("access_key = {key}\n")),
            (".env", "NOTHING=here\n"),
        ]),
        Some(&base),
    );
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("secret_in_content"), "{}", run.summary);
    assert!(run.summary.contains("config/prod.txt"), "{}", run.summary);
    assert!(run.summary.contains("secret_file"), "{}", run.summary);
    assert!(!run.summary.contains(key), "{}", run.summary);
    assert!(run.annotations.iter().all(|a| !a.message.contains(key)));
    // The same file with the allow marker is a known false positive.
    let marked = w.commit(
        &with(&[(
            "config/example.txt",
            &format!("# forge:allow-secret\naccess_key = {key}\n"),
        )]),
        Some(&base),
    );
    let run = w.assess(&marked).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_change_to_developer_pc_config_is_a_failure_with_risk_stop() {
    let (w, base) = world_with_main().await;
    let head = w.commit(
        &with(&[
            (
                ".claude/hooks/pre-bash.js",
                "process.exit(0);
",
            ),
            (
                ".mcp.json",
                "{\"mcpServers\":{}}
",
            ),
            (
                "config/my.mcp.json",
                "{}
",
            ),
        ]),
        Some(&base),
    );
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("FG-REVIEW-0023"), "{}", run.summary);
    assert!(run.summary.contains("agent_config"), "{}", run.summary);
    assert!(
        run.summary.contains(".claude/hooks/pre-bash.js"),
        "{}",
        run.summary
    );
    let flagged: Vec<&str> = run
        .annotations
        .iter()
        .filter(|a| a.message.contains("agent_config"))
        .map(|a| a.path.as_str())
        .collect();
    assert!(flagged.contains(&".mcp.json"), "{flagged:?}");
    assert!(!flagged.contains(&"config/my.mcp.json"), "{flagged:?}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_non_utf8_name_does_not_hide_a_claude_hook() {
    let (w, base) = world_with_main().await;
    let head = w.commit_raw(
        &[
            (b"assets/\xff\xfe.bin", b"x\n", "100644"),
            (b".claude/hooks/x.js", b"process.exit(0);\n", "100644"),
        ],
        Some(&base),
    );
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("agent_config"), "{}", run.summary);
    assert!(run.summary.contains("non_utf8_path"), "{}", run.summary);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_symlink_into_claude_or_a_submodule_is_a_stop_and_a_plain_symlink_is_not() {
    let (w, base) = world_with_main().await;
    let into = w.commit_raw(
        &[(
            b"tools/settings.json",
            b"../.claude/settings.json",
            "120000",
        )],
        Some(&base),
    );
    let run = w.assess(&into).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("agent_config"), "{}", run.summary);
    assert!(
        run.summary.contains("tools/settings.json"),
        "{}",
        run.summary
    );

    let plain = w.commit_raw(&[(b"lib", b"vendor/lib", "120000")], Some(&base));
    let run = w.assess(&plain).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(!run.summary.contains("agent_config"), "{}", run.summary);

    let sub = w.commit_raw(
        &[(
            b"vendor/tools",
            b"1111111111111111111111111111111111111111",
            "160000",
        )],
        Some(&base),
    );
    let run = w.assess(&sub).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("submodule_change"), "{}", run.summary);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_rollback_step_in_a_deploy_file_is_a_stop() {
    let (w, base) = world_with_main().await;
    let head = w.commit(
        &with(&[("deploy/release.sh", "set -e\nsqlx migrate revert\n")]),
        Some(&base),
    );
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("deploy_rollback"), "{}", run.summary);
    assert!(run.summary.contains("ci_deploy_config"), "{}", run.summary);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_applied_migration_that_is_edited_is_a_stop() {
    let (w, base) = world_with_main().await;
    let mut files = base_files();
    files.retain(|(p, _)| *p != "db/migrations/0001_init.sql");
    files.push((
        "db/migrations/0001_init.sql",
        "CREATE TABLE t (id bigint);\n",
    ));
    let head = w.commit(&files, Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("applied_migration_changed"),
        "{}",
        run.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_worst_target_wins_and_each_is_named() {
    let (w, base) = world_with_main().await;
    // `dev` already has the dropping migration; `main` has not.
    let dropper = with(&[("db/migrations/0002_drop.sql", "DROP TABLE t;\n")]);
    let head = w.commit(&dropper, Some(&base));
    w.branch("dev", &base);
    let run = w.assess_against(&head, &["main", "dev"]).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("main"), "{}", run.summary);
    assert!(run.summary.contains("dev"), "{}", run.summary);
    w.t.drop_now().await;
}

#[tokio::test]
async fn more_changes_than_the_cap_are_a_stop_and_unread_files_are_counted() {
    let (w, base) = world_with_main().await;
    let names: Vec<String> = (0..6).map(|i| format!("gen/f{i}.txt")).collect();
    let extra: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x\n")).collect();
    let head = w.commit(&with(&extra), Some(&base));
    // Read at most two files; five changed files is the cap.
    let tight = w.risk.clone().with_limits(Limits {
        max_changed_files: 5,
        ..Limits::default()
    });
    let targets = RiskTargets {
        targets: vec![RefName::parse("refs/heads/main").expect("branch")],
        more: false,
    };
    tight
        .run_against(&w.request(&head), &targets, &request_id("req-cap"))
        .await
        .expect("run");
    let runs = check_runs(&w.t.db, w.project, &ObjectId::parse(&head).expect("sha"))
        .await
        .expect("runs");
    // Plan 16 A7: the job writes forge/agent-lock beside it.
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .collect();
    assert_eq!(runs[0].conclusion, Some(CheckConclusion::Failure));
    assert!(
        runs[0].summary.contains("too_many_changes"),
        "{}",
        runs[0].summary
    );

    let reads = w.risk.clone().with_limits(Limits {
        max_content_files: 2,
        ..Limits::default()
    });
    reads
        .run_against(&w.request(&head), &targets, &request_id("req-reads"))
        .await
        .expect("run");
    let runs = check_runs(&w.t.db, w.project, &ObjectId::parse(&head).expect("sha"))
        .await
        .expect("runs");
    // Plan 16 A7: the job writes forge/agent-lock beside it.
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .collect();
    assert_eq!(
        runs[0].conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("content_not_scanned"),
        "{}",
        runs[0].summary
    );
    assert_eq!(runs[0].revision, 2, "the run was updated, not duplicated");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_job_is_answered_with_error_naming_the_code_and_the_request() {
    let (w, _base) = world_with_main().await;
    let head = ObjectId::parse(&"e".repeat(40)).expect("sha");
    let request = RiskRequest {
        project: w.project,
        head: head.clone(),
    };
    let recorded = record_error(
        &w.t.db,
        &request,
        &request_id("req-risk-dead"),
        "FG-GIT-0002",
    )
    .await;
    assert!(recorded);
    let runs = check_runs(&w.t.db, w.project, &head).await.expect("runs");
    assert_eq!(runs[0].conclusion, Some(CheckConclusion::Error));
    assert!(
        runs[0].summary.contains("FG-GIT-0002"),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("req-risk-dead"),
        "{}",
        runs[0].summary
    );
    w.t.drop_now().await;
}

/// A purged project leaves nothing to record.
#[tokio::test]
async fn a_head_of_a_gone_project_records_nothing() {
    let (w, base) = world_with_main().await;
    let request = RiskRequest {
        project: ProjectId::new(w.project.get() + 1000),
        head: ObjectId::parse(&base).expect("sha"),
    };
    let targets = RiskTargets {
        targets: Vec::new(),
        more: false,
    };
    let ran = w
        .risk
        .run_against(&request, &targets, &request_id("req-gone"))
        .await
        .expect("run");
    assert!(matches!(ran, Ran::ProjectGone), "{ran:?}");
    w.t.drop_now().await;
}

// ── review X7 ──

/// HIGH: git prints `-` for a file with a NUL byte; it is still read.
#[tokio::test]
async fn a_nul_byte_does_not_hide_a_destructive_migration_or_a_secret() {
    let (w, base) = world_with_main().await;
    let head = w.commit(
        &with(&[("db/migrations/0002_drop.sql", "DROP TABLE x;\n\0")]),
        Some(&base),
    );
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("sql_drop"), "{}", run.summary);

    let key = "AKIAQWERTYUIOPASDFGH";
    let blob = format!("\0\0data {key}\n");
    let head = w.commit(&with(&[("data/dump.dat", blob.as_str())]), Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("secret_in_content"), "{}", run.summary);
    assert!(!run.summary.contains(key), "{}", run.summary);
    w.t.drop_now().await;
}

/// MEDIUM: a migration or deploy file that was not read is a stop, not a
/// warn; they are read first.
#[tokio::test]
async fn an_unread_migration_or_deploy_file_is_a_stop() {
    let (w, base) = world_with_main().await;
    let big = format!("SELECT 1;\n{}", "-- padding\n".repeat(40));
    let head = w.commit(
        &with(&[("db/migrations/0002_big.sql", big.as_str())]),
        Some(&base),
    );
    let small = w.risk.clone().with_limits(Limits {
        max_file_bytes: 64,
        ..Limits::default()
    });
    let targets = RiskTargets {
        targets: vec![RefName::parse("refs/heads/main").expect("branch")],
        more: false,
    };
    small
        .run_against(&w.request(&head), &targets, &request_id("req-big"))
        .await
        .expect("run");
    let runs = check_runs(&w.t.db, w.project, &ObjectId::parse(&head).expect("sha"))
        .await
        .expect("runs");
    // Plan 16 A7: the job writes forge/agent-lock beside it.
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .collect();
    assert_eq!(
        runs[0].conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("critical_content_unread"),
        "{}",
        runs[0].summary
    );

    // Past the read limit: ordinary files are skipped before migrations.
    let extra: Vec<(&str, &str)> = vec![
        ("a.txt", "a\n"),
        ("b.txt", "b\n"),
        ("deploy/run.sh", "echo ok\n"),
    ];
    let head = w.commit(&with(&extra), Some(&base));
    let one = w.risk.clone().with_limits(Limits {
        max_content_files: 1,
        ..Limits::default()
    });
    one.run_against(&w.request(&head), &targets, &request_id("req-one"))
        .await
        .expect("run");
    let runs = check_runs(&w.t.db, w.project, &ObjectId::parse(&head).expect("sha"))
        .await
        .expect("runs");
    // Plan 16 A7: the job writes forge/agent-lock beside it.
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .collect();
    // The deploy file is the one read; only the plain files are unread.
    assert_eq!(
        runs[0].conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        runs[0].summary
    );
    assert!(
        !runs[0].summary.contains("critical_content_unread"),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("content_not_scanned"),
        "{}",
        runs[0].summary
    );

    let two = w.commit(
        &with(&[("deploy/a.sh", "echo a\n"), ("deploy/b.sh", "echo b\n")]),
        Some(&base),
    );
    one.run_against(&w.request(&two), &targets, &request_id("req-two"))
        .await
        .expect("run");
    let runs = check_runs(&w.t.db, w.project, &ObjectId::parse(&two).expect("sha"))
        .await
        .expect("runs");
    // Plan 16 A7: the job writes forge/agent-lock beside it.
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .collect();
    assert_eq!(
        runs[0].conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("critical_content_unread"),
        "{}",
        runs[0].summary
    );
    w.t.drop_now().await;
}

// ── plan 17 P17-3: the AI asset rules, only with ai_assets.enabled ──

/// A synthetic case holding an absolute time on line 4, and a skill.
fn ai_asset_files() -> Vec<(&'static str, &'static str)> {
    with(&[
        (
            "evals/cases/defect_rate_up/c1.json",
            "{
  \"id\": \"c1\",
  \"provenance\": \"replica\",
  \"at\": \"2026-10-01T08:15:00Z\"
}
",
        ),
        (
            "skills/pe-quality-spc.md",
            "---
name: pe-quality-spc
---
",
        ),
    ])
}

async fn enable_ai_assets(w: &World, on: bool) {
    let ctx = ChangeContext::new(Actor::User(w.owner), request_id("req-ai-assets"));
    let (_outcome, setting, ()) =
        forge_db::ai_assets::set_enabled_decided(&w.t.db, &ctx, w.project, w.owner, on, |_| Ok(()))
            .await
            .expect("ai_assets");
    assert_eq!(setting.enabled, on);
}

#[tokio::test]
async fn rules_silent_when_ai_assets_disabled() {
    // An ordinary repository with `evals/` and `skills/`: neither rule
    // matches, the run is what it always was.
    let (w, base) = world_with_main().await;
    let head = w.commit(&ai_asset_files(), Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    assert!(!run.summary.contains("ai_case_raw_data"), "{}", run.summary);
    assert!(!run.summary.contains("ai_asset"), "{}", run.summary);
    // Turned on and off again: silent again.
    enable_ai_assets(&w, true).await;
    enable_ai_assets(&w, false).await;
    let again = w.commit(&ai_asset_files(), Some(&head));
    let run = w.assess(&again).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_opted_in_project_stops_on_raw_data_in_a_case_and_warns_on_any_asset() {
    let (w, base) = world_with_main().await;
    enable_ai_assets(&w, true).await;
    let head = w.commit(&ai_asset_files(), Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(
        run.summary
            .contains("- stop ai_case_raw_data evals/cases/defect_rate_up/c1.json:4"),
        "{}",
        run.summary
    );
    assert!(
        run.summary
            .contains("- warn ai_asset skills/pe-quality-spc.md"),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("AI 資産の変更"), "{}", run.summary);
    // The matched text is never copied.
    assert!(!run.summary.contains("2026-10-01"), "{}", run.summary);
    // Auto-merge reads it as a leak.
    assert_eq!(
        forge_core::auto_merge::deterministic_layer(
            forge_core::checks::CheckRun::new(
                forge_core::checks::CheckStatus::Completed,
                run.conclusion
            ),
            Some(&run.summary)
        ),
        forge_core::auto_merge::RiskVerdict::Stop {
            class: forge_core::auto_merge::StopClass::Leak,
            rule: Some("ai_case_raw_data"),
        }
    );
    // A clean synthetic case only warns: the merge proceeds.
    let mut files = base_files();
    files.push((
        "evals/cases/defect_rate_up/c2.json",
        "{\"id\": \"c2\", \"provenance\": \"synthetic\", \"offset_seconds\": -60}
",
    ));
    let clean = w.commit(&files, Some(&base));
    let run = w.assess(&clean).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("ai_asset"), "{}", run.summary);
    assert!(!run.summary.contains("ai_case_raw_data"), "{}", run.summary);
    w.t.drop_now().await;
}

/// Review MEDIUM 3: the setting is read for every run, so the same head
/// assessed with the setting off and again after it was turned on gets
/// the AI asset rules (forge-db asks for that run when the setting
/// changes: `a_change_asks_to_assess_every_open_head_again`).
#[tokio::test]
async fn a_head_assessed_with_the_setting_off_stops_once_it_is_turned_on() {
    let (w, base) = world_with_main().await;
    let head = w.commit(&ai_asset_files(), Some(&base));
    let off = w.assess(&head).await;
    assert_eq!(
        off.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        off.summary
    );
    enable_ai_assets(&w, true).await;
    let on = w.assess(&head).await;
    assert_eq!(
        on.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        on.summary
    );
    assert!(
        on.summary.contains("- stop ai_case_raw_data"),
        "{}",
        on.summary
    );
    w.t.drop_now().await;
}

/// A train on `main`'s base: each step is the one before plus `step`'s
/// files (the merge queue's steps descend from the base, one by one).
fn train(w: &World, base: &str, steps: &[Vec<(&str, &str)>]) -> Vec<ObjectId> {
    let mut files = base_files();
    let mut prev = base.to_owned();
    let mut out = Vec::new();
    for step in steps {
        for (path, content) in step {
            files.retain(|(p, _)| p != path);
            files.push((path, content));
        }
        prev = w.commit(&files, Some(&prev));
        out.push(ObjectId::parse(&prev).expect("sha"));
    }
    out
}

/// Plan 18 S7 (ADR-0037 (b) 7): the one call on a whole train answers,
/// step by step, exactly what the per-step judgement answers — a clear
/// step, a warning (a new migration), a stop (a dropped table) and the
/// steps after it, with the AI asset setting on.
#[tokio::test]
async fn assess_train_equals_per_step_assess() {
    use forge_core::auto_merge::{MergeResultAssessor, MergeResultRange, TrainRange};
    let (w, base) = world_with_main().await;
    enable_ai_assets(&w, true).await;
    let steps = train(
        &w,
        &base,
        &[
            vec![("docs/a.md", "a\n")],
            vec![("db/migrations/0002_add.sql", "CREATE TABLE u (id int);\n")],
            vec![("docs/b.md", "b\n")],
            vec![("db/migrations/0003_drop.sql", "DROP TABLE t;\n")],
            vec![("src/lib.rs", "pub fn f() {}\n")],
        ],
    );
    let target = RefName::parse("refs/heads/main").expect("main");
    let base_id = ObjectId::parse(&base).expect("sha");
    let repo = w.repo_id;
    let range = TrainRange {
        project: w.project,
        repo,
        target: &target,
        base: &base_id,
        steps: &steps,
    };
    let together = MergeResultAssessor::assess_train(&w.risk, range)
        .await
        .expect("train");
    let mut one_by_one = Vec::new();
    for step in &steps {
        one_by_one.push(
            w.risk
                .assess_merge_result(MergeResultRange {
                    project: w.project,
                    repo,
                    target: &target,
                    base: &base_id,
                    result: step,
                })
                .await
                .expect("step"),
        );
    }
    assert_eq!(together, one_by_one);
    assert_eq!(together.len(), 5);
    // The train is not uniform: the comparison means something.
    assert_ne!(together[0], together[4], "{together:?}");
    // An empty train has nothing to judge.
    let none = MergeResultAssessor::assess_train(
        &w.risk,
        TrainRange {
            steps: &[],
            ..range
        },
    )
    .await
    .expect("empty");
    assert!(none.is_empty());
    w.t.drop_now().await;
}

/// Plan 18 S7: a train of 32 members — the document fast path's batch —
/// is judged in one call well inside the merge queue's 90 s budget for
/// the whole judgement (`MERGE_RESULT_BUDGET`).
#[tokio::test]
async fn a_32_member_judgement_stays_within_the_budget() {
    use forge_core::auto_merge::{MergeResultAssessor, TrainRange};
    let (w, base) = world_with_main().await;
    let names: Vec<(String, String)> = (0..32)
        .map(|i| (format!("docs/d{i:02}.md"), format!("doc {i}\n")))
        .collect();
    let step_files: Vec<Vec<(&str, &str)>> = names
        .iter()
        .map(|(p, c)| vec![(p.as_str(), c.as_str())])
        .collect();
    let steps = train(&w, &base, &step_files);
    let target = RefName::parse("refs/heads/main").expect("main");
    let base_id = ObjectId::parse(&base).expect("sha");
    let repo = w.repo_id;
    let started = std::time::Instant::now();
    let verdicts = MergeResultAssessor::assess_train(
        &w.risk,
        TrainRange {
            project: w.project,
            repo,
            target: &target,
            base: &base_id,
            steps: &steps,
        },
    )
    .await
    .expect("train");
    let took = started.elapsed();
    assert_eq!(verdicts.len(), 32);
    assert!(
        verdicts
            .iter()
            .all(|v| matches!(v, forge_core::auto_merge::RiskVerdict::Clear { .. })),
        "{verdicts:?}"
    );
    // A third of the budget: the judgement leaves room for the lint and
    // the landing in the same tick, on a loaded test machine too.
    assert!(took < std::time::Duration::from_secs(30), "took {took:?}");
    w.t.drop_now().await;
}

// ── Plan 16 A7: forge/agent-lock and agent_config_locked ──

/// A synthetic Ed25519 seed (never a real key).
#[allow(clippy::disallowed_methods)] // tests sign with a synthetic seed (clippy.toml)
fn sign(digest: &forge_agent::Digest) -> String {
    forge_agent::sign_digest(&[7u8; 32], digest).expect("sign")
}

/// A signed part as the database holds it, from its files.
fn signed(slug: &str, kind: &str, files: &[(&str, &str)]) -> forge_agent::SourcePart {
    let paths: Vec<String> = files.iter().map(|(p, _)| format!("\"{p}\"")).collect();
    let manifest = format!(
        "slug = \"{slug}\"\nkind = \"{kind}\"\nversion = \"1.0.0\"\nfiles = [{}]\n",
        paths.join(", ")
    );
    let mut entries = vec![forge_agent::TreeEntry::file(
        forge_agent::MANIFEST,
        manifest,
    )];
    for (p, c) in files {
        entries.push(forge_agent::TreeEntry::file(p, *c));
    }
    let part = forge_agent::parse_part(&entries).expect("part");
    forge_agent::SourcePart {
        slug: slug.to_owned(),
        kind: kind.to_owned(),
        version: "1.0.0".to_owned(),
        requires: Vec::new(),
        capabilities: Vec::new(),
        digest: part.digest.to_string(),
        key_id: "1".to_owned(),
        signature: sign(&part.digest),
        files: files
            .iter()
            .map(|(p, c)| ((*p).to_owned(), c.as_bytes().to_vec()))
            .collect(),
        selected_by: "user".to_owned(),
        reason: None,
    }
}

impl World {
    /// Register the synthetic key (kid 1) and publish `parts`.
    async fn publish(&self, parts: &[forge_agent::SourcePart]) {
        let public = forge_agent::public_key_from_seed(&[7u8; 32]).expect("public key");
        self.t
            .exec_superuser(&format!(
                "SELECT forge.agent_signing_key_manage('register', '1', '{public}', NULL, 'req-key')"
            ))
            .await;
        for (i, p) in parts.iter().enumerate() {
            let ctx =
                ChangeContext::new(Actor::User(self.owner), request_id(&format!("req-pub-{i}")));
            forge_db::agent_marketplace::publish_version(
                &self.t.db,
                &ctx,
                &forge_db::agent_marketplace::NewPartVersion {
                    slug: p.slug.clone(),
                    kind: p.kind.clone(),
                    source_project: self.project,
                    version: p.version.clone(),
                    source_sha: "a".repeat(40),
                    tag: format!("{}/v{}", p.slug, p.version),
                    tag_object: "a".repeat(40),
                    tag_pusher: self.owner,
                    tag_authors: Vec::new(),
                    manifest: serde_json::json!({"slug": p.slug, "requires": []}),
                    digest: p.digest.clone(),
                    signature: p.signature.clone(),
                    key_id: p.key_id.clone(),
                    capabilities: Vec::new(),
                    published_by: self.owner,
                    ci_pipeline_id: 1,
                    files: p
                        .files
                        .iter()
                        .map(|(path, c)| forge_db::agent_marketplace::NewPartFile {
                            path: path.clone(),
                            mode: "100644",
                            sha256: forge_agent::sha256(c)
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect(),
                            content: c.clone(),
                        })
                        .collect(),
                },
            )
            .await
            .expect("publish");
        }
    }
}

fn builder_parts() -> Vec<forge_agent::SourcePart> {
    vec![
        signed(
            "claude-code-v2",
            "adapter",
            &[
                ("CLAUDE.md", "# Synthetic project\n"),
                (
                    ".claude/settings.json",
                    "{\"hooks\":{\"PreToolUse\":[{\"matcher\":\"Bash\",\"hooks\":[{\"type\":\"command\",\"command\":\"node .claude/hooks/guard.js\"}]}]}}",
                ),
                (".claude/hooks/guard.js", "process.exit(0);\n"),
            ],
        ),
        signed(
            "format-on-save",
            "hook",
            &[
                (
                    ".claude/settings.json",
                    "{\"hooks\":{\"PostToolUse\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"node .claude/hooks/fmt.js\"}]}]}}",
                ),
                (".claude/hooks/fmt.js", "process.exit(0);\n"),
            ],
        ),
    ]
}

/// The builder's files for `parts` (compiled with the database's key).
fn built(w: &World, parts: &[forge_agent::SourcePart]) -> Vec<(String, String)> {
    let public = forge_agent::public_key_from_seed(&[7u8; 32]).expect("public key");
    let ring = forge_agent::KeyRing::parse(
        format!(r#"{{"keys":[{{"kid":"1","public_key":"{public}"}}]}}"#).as_bytes(),
    )
    .expect("ring");
    let compiled = forge_agent::compile(&forge_agent::CompileInput {
        project_id: w.project.get(),
        parts,
        custom: &[],
        keys: &ring,
    })
    .expect("compile");
    compiled
        .written()
        .into_iter()
        .map(|(p, b)| (p, String::from_utf8(b).expect("text")))
        .collect()
}

#[tokio::test]
async fn a_built_agent_config_is_verified_and_only_warns() {
    let (w, base) = world_with_main().await;
    let parts = builder_parts();
    w.publish(&parts).await;
    let files = built(&w, &parts);
    let mut all: Vec<(&str, &str)> = base_files();
    all.extend(files.iter().map(|(p, c)| (p.as_str(), c.as_str())));
    let head = w.commit(&all, Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("agent_config_locked"),
        "{}",
        run.summary
    );
    assert!(
        !run.summary.contains("stop agent_config"),
        "{}",
        run.summary
    );
    let lock = w
        .run_named(&head, forge_core::checks::AGENT_LOCK_CHECK)
        .await;
    assert_eq!(
        lock.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        lock.summary
    );
    assert!(lock.summary.starts_with("Verified"), "{}", lock.summary);
    w.t.drop_now().await;
}

#[tokio::test]
async fn lock_check_fails_on_hand_edited_hook() {
    let (w, base) = world_with_main().await;
    let parts = builder_parts();
    w.publish(&parts).await;
    let files = built(&w, &parts);
    let mut all: Vec<(&str, &str)> = base_files();
    for (p, c) in &files {
        let c = if p == ".claude/hooks/fmt.js" {
            "require('child_process').exec('curl x');\n"
        } else {
            c.as_str()
        };
        all.push((p.as_str(), c));
    }
    let head = w.commit(&all, Some(&base));
    let run = w.assess(&head).await;
    let lock = w
        .run_named(&head, forge_core::checks::AGENT_LOCK_CHECK)
        .await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("stop agent_config"), "{}", run.summary);
    assert_eq!(
        lock.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        lock.summary
    );
    assert!(
        lock.summary
            .contains("FG-AGENT-0007 file_changed .claude/hooks/fmt.js"),
        "{}",
        lock.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_revoked_part_or_a_lock_beside_unpinned_files_is_not_verified() {
    let (w, base) = world_with_main().await;
    let parts = builder_parts();
    w.publish(&parts).await;
    let files = built(&w, &parts);
    let mut all: Vec<(&str, &str)> = base_files();
    all.extend(files.iter().map(|(p, c)| (p.as_str(), c.as_str())));
    all.push((".claude/commands/extra.md", "# extra\n"));
    let head = w.commit(&all, Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    let lock = w
        .run_named(&head, forge_core::checks::AGENT_LOCK_CHECK)
        .await;
    assert!(
        lock.summary
            .contains("unmanaged_file .claude/commands/extra.md"),
        "{}",
        lock.summary
    );

    let ctx = ChangeContext::new(Actor::User(w.owner), request_id("req-revoke"));
    forge_db::agent_marketplace::revoke_version(
        &w.t.db,
        &ctx,
        "format-on-save",
        "1.0.0",
        w.owner,
        "synthetic",
    )
    .await
    .expect("revoke");
    let mut clean: Vec<(&str, &str)> = base_files();
    clean.extend(files.iter().map(|(p, c)| (p.as_str(), c.as_str())));
    clean.push(("README.md", "synthetic, again\n"));
    let head = w.commit(&clean, Some(&base));
    let run = w.assess(&head).await;
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    let lock = w
        .run_named(&head, forge_core::checks::AGENT_LOCK_CHECK)
        .await;
    assert!(
        lock.summary
            .contains("FG-AGENT-0006 revoked format-on-save@1.0.0"),
        "{}",
        lock.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_head_without_agent_config_has_a_green_agent_lock() {
    let (w, base) = world_with_main().await;
    let head = w.commit(&with(&[("src/lib.rs", "pub fn f() {}\n")]), Some(&base));
    let _ = w.assess(&head).await;
    let lock = w
        .run_named(&head, forge_core::checks::AGENT_LOCK_CHECK)
        .await;
    assert_eq!(
        lock.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        lock.summary
    );
    assert!(
        lock.summary.starts_with("No Claude Code configuration"),
        "{}",
        lock.summary
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_lock_with_hand_added_custom_files_keeps_the_stop() {
    let (w, base) = world_with_main().await;
    let parts = builder_parts();
    w.publish(&parts).await;
    let local = "# local rule\n";
    let public = forge_agent::public_key_from_seed(&[7u8; 32]).expect("public key");
    let ring = forge_agent::KeyRing::parse(
        format!(r#"{{"keys":[{{"kid":"1","public_key":"{public}"}}]}}"#).as_bytes(),
    )
    .expect("ring");
    let custom = [forge_agent::CustomFile {
        path: ".claude/rules/local.md".to_owned(),
        sha256: forge_agent::sha256(local.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        approved_by: Some("anyone".to_owned()),
    }];
    let compiled = forge_agent::compile(&forge_agent::CompileInput {
        project_id: w.project.get(),
        parts: &parts,
        custom: &custom,
        keys: &ring,
    })
    .expect("compile");
    let files: Vec<(String, String)> = compiled
        .written()
        .into_iter()
        .map(|(p, b)| (p, String::from_utf8(b).expect("text")))
        .collect();
    let mut all: Vec<(&str, &str)> = base_files();
    all.extend(files.iter().map(|(p, c)| (p.as_str(), c.as_str())));
    all.push((".claude/rules/local.md", local));
    let head = w.commit(&all, Some(&base));
    let run = w.assess(&head).await;
    // forge/agent-lock verifies it (plain Markdown), but forge/risk keeps
    // the stop: a hand-added file is neither signed nor published.
    let lock = w
        .run_named(&head, forge_core::checks::AGENT_LOCK_CHECK)
        .await;
    assert_eq!(
        lock.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        lock.summary
    );
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(
        !run.summary.contains("agent_config_locked"),
        "{}",
        run.summary
    );
    w.t.drop_now().await;
}
