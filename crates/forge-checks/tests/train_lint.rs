#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge queue's train lint (plan 18 S1, ADR-0037 (b)) on a real bare
//! repository and the development database: it answers for a commit what
//! `gstn doc lint` answers for a working tree of the same bytes, the
//! queue's verdict is that report against the base's, and the report is
//! the `forge/doc-lint` check run of the commit. The queue's side (who
//! leaves, what lands) is forge-review's `queue_train_lint`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use forge_checks::train_lint::{lint_commit, TrainLint};
use forge_core::checks::{CheckConclusion, DOC_LINT_CHECK};
use forge_core::train_lint::{TrainDocLint, TrainLintInput, TrainLintProblem, TrainLintVerdict};
use forge_core::{ObjectId, ProjectId, RepoId, RequestId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::check_runs;
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_doc::lint::{FileReport, Source};
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

/// A tree of nested `files` (`dir/sub/name` paths), built bottom up.
fn tree(repo: &Path, files: &[(&str, &str)]) -> String {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Dir {
        files: BTreeMap<String, String>,
        dirs: BTreeMap<String, Dir>,
    }
    fn write(repo: &Path, d: &Dir) -> String {
        let mut listing = String::new();
        for (name, sub) in &d.dirs {
            listing.push_str(&format!("040000 tree {}\t{name}\n", write(repo, sub)));
        }
        for (name, blob) in &d.files {
            listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
        }
        plumb(repo, &["mktree"], Some(listing.as_bytes()))
    }
    let mut root = Dir::default();
    for (path, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        let mut segs: Vec<&str> = path.split('/').collect();
        let name = segs.pop().expect("name");
        let mut d = &mut root;
        for s in segs {
            d = d.dirs.entry(s.to_owned()).or_default();
        }
        d.files.insert(name.to_owned(), blob);
    }
    write(repo, &root)
}

fn commit(repo: &Path, files: &[(&str, &str)], parent: Option<&str>) -> ObjectId {
    let t = tree(repo, files);
    let mut args = vec!["commit-tree", t.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    ObjectId::parse(&plumb(repo, &args, None)).expect("sha")
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
    browser: Browser,
    project: ProjectId,
    repo: RepoId,
    path: PathBuf,
}

/// alice's project `web` with an empty bare repository.
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
    let _ready = forge_db::repositories::mark_ready(&t.db, &system("req-ready"), web.repository_id)
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
    World {
        browser: Browser::new(git.clone(), storage.clone(), BrowseLimits::default()),
        t,
        _home: home,
        _root: root,
        git,
        storage,
        project: web.project_id,
        repo: web.repository_id,
        path,
    }
}

const MANUAL_T: &str = "name: qms.manual\nid_pattern: '^QM-\\d{3}$'\n";

fn doc(id: &str, body: &str) -> String {
    format!(
        "---\ndoctype: qms.manual\nid: {id}\ntitle: 品質マニュアル\nowner: qa-team\n---\n\n{body}\n"
    )
}

/// What `gstn doc lint` computes for a working tree of `files`
/// (forge-client `doc::lint`): `forge_doc::lint::lint` over the type
/// definitions and every `.md`, by path.
fn gstn_doc_lint(files: &[(&str, &str)]) -> Vec<FileReport> {
    let mut sorted: Vec<(&str, &str)> = files.to_vec();
    sorted.sort();
    let doctypes: Vec<Source<'_>> = sorted
        .iter()
        .filter(|(path, _)| path.starts_with(".forge/doctypes/") && path.ends_with(".yml"))
        .map(|(path, text)| Source {
            path,
            bytes: text.as_bytes(),
        })
        .collect();
    let documents: Vec<Source<'_>> = sorted
        .iter()
        .filter(|(path, _)| path.ends_with(".md"))
        .map(|(path, text)| Source {
            path,
            bytes: text.as_bytes(),
        })
        .collect();
    forge_doc::lint::lint(&doctypes, &documents)
}

/// The problems of `reports` as a verdict lists them.
fn keys(reports: &[FileReport]) -> Vec<TrainLintProblem> {
    let set: BTreeSet<TrainLintProblem> = reports
        .iter()
        .flat_map(|r| {
            r.problems.iter().map(|p| TrainLintProblem {
                path: r.path.clone(), // clone: the key owns its path
                rule: p.rule.to_owned(),
                reference: p.subject.clone(), // clone: as above
            })
        })
        .collect();
    set.into_iter().collect()
}

#[tokio::test]
async fn the_train_lint_answers_like_gstn_doc_lint() {
    let w = world().await;
    let one = doc("QM-001", "本文。");
    let two = doc("QM-002", "本文。");
    let clean: Vec<(&str, &str)> = vec![
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("README.md", "# web\n"),
        ("qms/QM-001.md", &one),
        ("qms/deep/QM-002.md", &two),
    ];
    // The tip: QM-002 is gone and QM-001 cites it, a second document has
    // an id its type does not accept and cites an id nobody has, a third
    // has no owner.
    let citing = doc("QM-001", "本文。[[QM-002]]");
    let bad_id = doc("QM-3", "参照 [[QM-404]]。");
    let no_owner = "---\ndoctype: qms.manual\nid: QM-005\ntitle: 無責任\n---\n\n本文。\n";
    let dirty: Vec<(&str, &str)> = vec![
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        (
            "README.md",
            "# web\n\nSee [[QM-777]] (not a document: no doctype).\n",
        ),
        ("qms/QM-001.md", &citing),
        ("qms/QM-003.md", &bad_id),
        ("qms/deep/QM-005.md", no_owner),
        ("src/notes.txt", "[[QM-999]]\n"),
    ];
    let base = commit(&w.path, &clean, None);
    let tip = commit(&w.path, &dirty, Some(base.as_str()));

    // The same bytes, the same report.
    let expected = gstn_doc_lint(&dirty);
    let rules: BTreeSet<&str> = expected
        .iter()
        .flat_map(|r| r.problems.iter().map(|p| p.rule))
        .collect();
    assert!(
        rules.contains("ref_broken") && rules.len() >= 3,
        "the fixture has several kinds of problems: {expected:?}"
    );
    assert!(gstn_doc_lint(&clean).iter().all(|r| r.problems.is_empty()));
    let reports = lint_commit(&w.browser, w.repo, &tip).await.expect("lint");
    assert_eq!(reports, expected, "the commit's report is gstn's");
    let at_base = lint_commit(&w.browser, w.repo, &base).await.expect("lint");
    assert_eq!(at_base, gstn_doc_lint(&clean));

    // The queue's verdict: that report against the base's (clean: every
    // problem of the tip is new), at the only step.
    let lint = TrainLint::new(
        w.t.db.clone(),
        w.git.clone(),
        w.storage.clone(),
        w.browser.clone(),
    );
    let request_id = RequestId::parse("req-train-lint").expect("id");
    let steps = [tip.clone()]; // clone: the input borrows the list
    let input = TrainLintInput {
        project: w.project,
        repo: w.repo,
        base: &base,
        steps: &steps,
        budget: Duration::from_secs(60),
        request_id: &request_id,
    };
    let verdict = lint.lint_train(input).await.expect("verdict");
    assert_eq!(
        verdict,
        TrainLintVerdict::NewProblems {
            first_bad_step: 0,
            problems: keys(&expected),
        }
    );

    // The report is the commit's `forge/doc-lint` run: every problem of
    // the tree, one annotation each, and the line that says whose verdict
    // it is.
    let runs = check_runs(&w.t.db, w.project, &tip).await.expect("runs");
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(run.name.as_str(), DOC_LINT_CHECK);
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    let problems: usize = expected.iter().map(|r| r.problems.len()).sum();
    assert_eq!(run.annotations.len(), problems, "{run:?}");
    assert!(
        run.summary.contains(&format!(
            "Merge queue: the tree of a train built on {}",
            base.as_str()
        )),
        "{}",
        run.summary
    );
    assert!(
        run.summary
            .contains("qms/QM-001.md:8 ref_broken — QM-002 [FG-DOC-0001]"),
        "{}",
        run.summary
    );

    // A train of the clean tree on the dirty one repairs: clean, and the
    // repaired tip's run succeeds.
    let repaired = commit(&w.path, &clean, Some(tip.as_str()));
    let steps = [repaired.clone()]; // clone: the input borrows the list
    let verdict = lint
        .lint_train(TrainLintInput {
            base: &tip,
            steps: &steps,
            ..input
        })
        .await
        .expect("verdict");
    assert_eq!(verdict, TrainLintVerdict::Clean);
    let runs = check_runs(&w.t.db, w.project, &repaired)
        .await
        .expect("runs");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].conclusion, Some(CheckConclusion::Success));

    // A train that touches no document is not linted: nothing is recorded.
    let mut with_code = clean.clone(); // clone: the base's files and one more
    with_code.push(("src/main.rs", "fn main() {}\n"));
    let code = commit(&w.path, &with_code, Some(repaired.as_str()));
    let steps = [code.clone()]; // clone: the input borrows the list
    let verdict = lint
        .lint_train(TrainLintInput {
            base: &repaired,
            steps: &steps,
            ..input
        })
        .await
        .expect("verdict");
    assert_eq!(verdict, TrainLintVerdict::NotApplicable);
    assert!(check_runs(&w.t.db, w.project, &code)
        .await
        .expect("runs")
        .is_empty());

    // Plan 18 S7: through the document index — cold, then warm (nothing
    // read again), and on a bound so small it drops everything after each
    // tree — the verdicts are the whole reading's.
    for max_bytes in [64 * 1024 * 1024, 1] {
        let indexed = TrainLint::new(
            w.t.db.clone(),
            w.git.clone(),
            w.storage.clone(),
            w.browser.clone(),
        )
        .with_doc_index(max_bytes);
        let two = [tip.clone(), repaired.clone()]; // clone: the input borrows the list
        for _ in 0..2 {
            let red = indexed.lint_train(input).await.expect("verdict");
            assert_eq!(
                red,
                TrainLintVerdict::NewProblems {
                    first_bad_step: 0,
                    problems: keys(&expected),
                }
            );
            let fixed = indexed
                .lint_train(TrainLintInput {
                    base: &tip,
                    steps: &steps_of(&two[1..]),
                    ..input
                })
                .await
                .expect("verdict");
            assert_eq!(fixed, TrainLintVerdict::Clean);
            // The first step breaks the base, the second repairs it back to
            // the base's very tree: nothing changed, nothing to lint.
            let both = indexed
                .lint_train(TrainLintInput {
                    steps: &two,
                    ..input
                })
                .await
                .expect("verdict");
            assert_eq!(both, TrainLintVerdict::NotApplicable);
        }
    }
    w.t.drop_now().await;
}

fn steps_of(steps: &[ObjectId]) -> Vec<ObjectId> {
    steps.to_vec()
}
