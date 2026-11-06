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
    repo: PathBuf,
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
        let mut runs = check_runs(
            &self.t.db,
            self.project,
            &ObjectId::parse(head).expect("sha"),
        )
        .await
        .expect("runs");
        assert_eq!(runs.len(), 1, "{runs:?}");
        let run = runs.remove(0);
        assert_eq!(run.name.as_str(), forge_core::checks::RISK_CHECK);
        assert_eq!(run.reported_by, None, "forge's own run has no reporter");
        run
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
        repo,
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
