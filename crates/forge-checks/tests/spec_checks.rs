#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The requirement index on a real bare repository (M3 X4, ADR-0023
//! Amendment X4): the own specification directories, the bounds, the
//! `@req` tags; and `forge/spec-lint` / `forge/spec-trace` writing nothing
//! for a merge request that is gone and answering `error` for both when a
//! job dies. The full run of a merge request version is `forge worker`'s
//! test (forge-cli), where merge requests are opened through forge-review.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_checks::doc_lint::{assess_doc_lint, DocLimits};
use forge_checks::spec_checks::{record_errors, Ran, SpecChecks};
use forge_checks::spec_index::{index_commit, index_head, IndexLimits, Tags};
use forge_checks::spec_refs::RepoConfigRead;
use forge_core::checks::CheckConclusion;
use forge_core::{ObjectId, RequestId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::check_runs;
use forge_db::checks::spec_index::SpecIndexRequest;
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

/// A tree of nested `files` (`dir/sub/name` paths), built bottom up.
fn tree(repo: &Path, files: &[(&str, &[u8])]) -> String {
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
        let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(content));
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

fn commit(repo: &Path, files: &[(&str, &[u8])]) -> String {
    let t = tree(repo, files);
    plumb(repo, &["commit-tree", &t, "-m", "synthetic"], None)
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

const SPEC_A: &[u8] =
    b"---\nspec: a\ntitle: A\nstatus: approved\n---\n::: req ZEVA-CFG-1\nreload\n:::\n";

struct Repo {
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    browser: Browser,
    id: forge_core::RepoId,
    path: std::path::PathBuf,
}

async fn repo() -> Repo {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let id = forge_core::RepoId::new(21);
    let path = storage
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Repo {
        browser: Browser::new(git, storage, BrowseLimits::default()),
        _home: home,
        _root: root,
        id,
        path,
    }
}

#[tokio::test]
async fn the_index_reads_the_own_spec_directory_and_every_tag() {
    let r = repo().await;
    let c = commit(
        &r.path,
        &[
            ("spec/a.md", SPEC_A),
            ("spec/README.md", b"# Specs\n"),
            ("spec/notes.txt", b"::: req ZEVA-CFG-2\nx\n:::\n"),
            (
                "docs/other.md",
                b"---\nspec: o\ntitle: O\nstatus: draft\n---\n",
            ),
            ("tests/t.rs", b"// @req ZEVA-CFG-1\n// @req PBI-3/AC-1\n"),
        ],
    );
    let i = index_commit(&r.browser, r.id, &c, &IndexLimits::default(), Tags::Read)
        .await
        .expect("index");
    assert_eq!(i.config, RepoConfigRead::Absent);
    assert_eq!(i.dirs, vec!["spec/".to_owned()]);
    let docs: Vec<&str> = i.index.documents.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(
        docs,
        vec!["spec/a.md"],
        "README is no spec; .txt and docs/ are not read"
    );
    let tags: Vec<(String, usize, String)> = i
        .index
        .tags
        .iter()
        .map(|t| (t.path.clone(), t.line, t.target.to_string()))
        .collect();
    assert_eq!(
        tags,
        vec![
            ("tests/t.rs".to_owned(), 1, "ZEVA-CFG-1".to_owned()),
            ("tests/t.rs".to_owned(), 2, "PBI-3/AC-1".to_owned()),
        ]
    );
    assert_eq!(i.index.incomplete, None);
    // A referenced specification: its documents only.
    let docs_only = index_commit(&r.browser, r.id, &c, &IndexLimits::default(), Tags::Skip)
        .await
        .expect("index");
    assert!(docs_only.index.tags.is_empty());
}

#[tokio::test]
async fn path_entries_name_the_own_spec_directories() {
    let r = repo().await;
    let c = commit(
        &r.path,
        &[
            (".forge/repo.yml", b"specs:\n  - path: docs/requirements/\n  - repo: x/specs\n    version: v1\n    scope: [EXT-*]\n"),
            ("spec/a.md", SPEC_A),
            ("docs/requirements/b.md", b"---\nspec: b\ntitle: B\nstatus: draft\n---\n::: req B-1\nx\n:::\n"),
        ],
    );
    let i = index_commit(&r.browser, r.id, &c, &IndexLimits::default(), Tags::Read)
        .await
        .expect("index");
    assert_eq!(i.dirs, vec!["docs/requirements/".to_owned()]);
    let docs: Vec<&str> = i.index.documents.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(docs, vec!["docs/requirements/b.md"]);
    assert!(i.external.covers("EXT-1"));
    assert!(!i.external.covers("B-2"));
}

#[tokio::test]
async fn the_index_stops_at_its_bounds_and_says_which() {
    let r = repo().await;
    let big = [b'x'; 600];
    let c = commit(
        &r.path,
        &[
            ("spec/a.md", SPEC_A),
            ("spec/b.md", SPEC_A),
            ("spec/big.md", &big),
            ("spec/c.md", SPEC_A),
            (
                "t.rs",
                b"// @req ZEVA-CFG-1\n// @req ZEVA-CFG-1\n// @req ZEVA-CFG-1\n",
            ),
        ],
    );
    let few = IndexLimits {
        max_files: 2,
        ..IndexLimits::default()
    };
    let i = index_commit(&r.browser, r.id, &c, &few, Tags::Read)
        .await
        .expect("index");
    assert_eq!(i.index.documents.len(), 2);
    assert_eq!(i.index.incomplete, Some("spec_files"));
    let small = IndexLimits {
        max_file_bytes: 200,
        ..IndexLimits::default()
    };
    let i = index_commit(&r.browser, r.id, &c, &small, Tags::Read)
        .await
        .expect("index");
    let big_doc = i
        .index
        .documents
        .iter()
        .find(|d| d.path == "spec/big.md")
        .expect("named");
    assert_eq!(big_doc.doc.problems[0].rule, "file_too_large");
    assert_eq!(i.index.incomplete, None);
    let total = IndexLimits {
        max_total_bytes: 100,
        ..IndexLimits::default()
    };
    let i = index_commit(&r.browser, r.id, &c, &total, Tags::Read)
        .await
        .expect("index");
    assert_eq!(i.index.incomplete, Some("spec_bytes"));
    let tags = IndexLimits {
        max_tag_output_bytes: 80,
        ..IndexLimits::default()
    };
    let i = index_commit(&r.browser, r.id, &c, &tags, Tags::Read)
        .await
        .expect("index");
    assert_eq!(i.index.incomplete, Some("tag_lines"));
    assert!(i.index.tags.len() < 3);
}

/// A project with a ready repository and no merge request `iid` 1.
async fn project_world() -> (FreshDb, Repo, forge_core::ProjectId) {
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
    let r = repo().await;
    (t, r, web.project_id)
}

#[tokio::test]
async fn a_merge_request_that_is_gone_records_nothing() {
    let (t, r, project) = project_world().await;
    let head = commit(&r.path, &[("spec/a.md", SPEC_A)]);
    let checks = SpecChecks::new(t.db.clone(), r.browser.clone(), Arc::new(t.db.clone()));
    let request = SpecIndexRequest {
        project,
        iid: 1,
        head: ObjectId::parse(&head).expect("sha"),
    };
    let ran = checks
        .run(&request, &RequestId::parse("req-gone").expect("id"))
        .await
        .expect("run");
    assert_eq!(ran, Ran::Gone);
    assert!(check_runs(&t.db, project, &request.head)
        .await
        .expect("runs")
        .is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn a_failed_job_answers_error_for_every_check() {
    let (t, _r, project) = project_world().await;
    let request = SpecIndexRequest {
        project,
        iid: 4,
        head: ObjectId::parse(&"c".repeat(40)).expect("sha"),
    };
    assert!(
        record_errors(
            &t.db,
            &request,
            &RequestId::parse("req-dies").expect("id"),
            "FG-GIT-0003"
        )
        .await
    );
    let runs = check_runs(&t.db, project, &request.head)
        .await
        .expect("runs");
    let names: Vec<(&str, Option<CheckConclusion>)> = runs
        .iter()
        .map(|r| (r.name.as_str(), r.conclusion))
        .collect();
    assert_eq!(
        names,
        vec![
            ("forge/ci-lint", Some(CheckConclusion::Error)),
            ("forge/doc-lint", Some(CheckConclusion::Error)),
            ("forge/spec-lint", Some(CheckConclusion::Error)),
            ("forge/spec-trace", Some(CheckConclusion::Error)),
        ]
    );
    for run in &runs {
        assert!(run.summary.contains("FG-GIT-0003"), "{}", run.summary);
        assert!(run.summary.contains("req-dies"), "{}", run.summary);
        assert_eq!(run.reported_by, None);
    }
    t.drop_now().await;
}

/// Review X4 (HIGH-2): `@req` written in a specification document — prose
/// or an example in a code fence — is no test: tags in Markdown under the
/// own specification directories are not indexed (a test elsewhere is).
#[tokio::test]
async fn tags_in_the_specification_documents_are_no_tests() {
    let r = repo().await;
    let c = commit(
        &r.path,
        &[
            (
                "spec/a.md",
                b"---
spec: a
title: A
status: approved
---
::: req ZEVA-CFG-1
Test it with `@req ZEVA-CFG-1`.
:::
```rust
// @req ZEVA-CFG-9
```
",
            ),
            (
                "spec/notes/guide.md",
                b"Write @req ZEVA-CFG-1 in a test.
",
            ),
            (
                "docs/howto.md",
                b"// @req ZEVA-CFG-1
",
            ),
            (
                "tests/t.rs",
                b"// @req ZEVA-CFG-1
",
            ),
        ],
    );
    let i = index_commit(&r.browser, r.id, &c, &IndexLimits::default(), Tags::Read)
        .await
        .expect("index");
    let paths: Vec<&str> = i.index.tags.iter().map(|t| t.path.as_str()).collect();
    assert_eq!(paths, vec!["docs/howto.md", "tests/t.rs"]);
    assert!(
        forge_core::spec_index::lint(&i.index, &i.external).is_empty(),
        "the fenced example names no broken reference"
    );
}

const MANUAL_T: &[u8] = b"name: qms.manual\nid_pattern: '^QM-\\d{3}$'\n";

fn manual(id: &str, body: &str) -> Vec<u8> {
    format!("---\ndoctype: qms.manual\nid: {id}\ntitle: t\nowner: qa\n---\n\n{body}\n").into_bytes()
}

/// Plan 13 stage 2: the job's head is listed and read once; the index is
/// the one [`index_commit`] builds, and the documents (`.md` naming a
/// `doctype`, in any folder) and their types come from the same read.
#[tokio::test]
async fn the_head_is_read_once_for_the_index_and_the_documents() {
    let r = repo().await;
    let qm1 = manual("QM-001", "本文。[[QM-002]]");
    let qm2 = manual("QM-002", "本文。[[QM-404]]");
    let c = commit(
        &r.path,
        &[
            (".forge/doctypes/qms.manual.yml", MANUAL_T),
            ("spec/a.md", SPEC_A),
            ("spec/QM-001.md", &qm1),
            ("qms/deep/QM-002.md", &qm2),
            ("specs-not/x.md", b"# x\n"),
            ("tests/t.rs", b"// @req ZEVA-CFG-1\n"),
        ],
    );
    let limits = IndexLimits::default();
    let (head, docs) = index_head(&r.browser, r.id, &c, &limits, &DocLimits::default())
        .await
        .expect("head");
    let alone = index_commit(&r.browser, r.id, &c, &limits, Tags::Read)
        .await
        .expect("index");
    assert_eq!(head, alone, "the same index as index_commit");
    let paths: Vec<&str> = docs.markdown.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "qms/deep/QM-002.md",
            "spec/QM-001.md",
            "spec/a.md",
            "specs-not/x.md"
        ]
    );
    assert_eq!(docs.doctypes.len(), 1);
    assert_eq!(docs.incomplete, None);
    let run = assess_doc_lint(&docs).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(
        run.summary
            .contains("qms/deep/QM-002.md:8 ref_broken — QM-404"),
        "{}",
        run.summary
    );
    // A listing cut at its bound leaves the documents incomplete; the
    // specification directories are listed on their own (review MR3):
    // forge/spec-lint's answer does not depend on the tree's size.
    let cut = DocLimits {
        max_tree_entries: 2,
        ..DocLimits::default()
    };
    let (head, docs) = index_head(&r.browser, r.id, &c, &limits, &cut)
        .await
        .expect("head");
    assert_eq!(head, alone, "the same index as index_commit");
    assert_eq!(head.index.incomplete, None);
    assert_eq!(docs.incomplete, Some("doc_tree"));
    let run = assess_doc_lint(&docs).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("[FG-DOC-0002]"), "{}", run.summary);
}

/// A Markdown file over the parser's limit is read only as far as the
/// parser needs: one without a `doctype` is no document and no bound
/// (review MR3); one naming a type is `too_large`, as gstn says.
#[tokio::test]
async fn a_large_markdown_file_is_read_in_part_and_is_no_bound() {
    let r = repo().await;
    let big = "x".repeat(forge_doc::document::MAX_DOCUMENT_BYTES + 10);
    let readme = format!("# notes\n{big}");
    let large_doc = String::from_utf8(manual("QM-001", &big)).expect("utf8");
    let c = commit(
        &r.path,
        &[
            (".forge/doctypes/qms.manual.yml", MANUAL_T),
            ("README.md", readme.as_bytes()),
        ],
    );
    let limits = IndexLimits::default();
    let (_, docs) = index_head(&r.browser, r.id, &c, &limits, &DocLimits::default())
        .await
        .expect("head");
    assert_eq!(docs.incomplete, None);
    assert_eq!(
        docs.markdown[0].1.len(),
        forge_doc::document::MAX_DOCUMENT_BYTES + 1
    );
    let run = assess_doc_lint(&docs).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    let c = commit(
        &r.path,
        &[
            (".forge/doctypes/qms.manual.yml", MANUAL_T),
            ("QM-001.md", large_doc.as_bytes()),
        ],
    );
    let (_, docs) = index_head(&r.browser, r.id, &c, &limits, &DocLimits::default())
        .await
        .expect("head");
    let run = assess_doc_lint(&docs).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("QM-001.md:0 too_large"),
        "{}",
        run.summary
    );
    // More large files than the bound: named, never a silent pass.
    let none = DocLimits {
        max_large_files: 0,
        ..DocLimits::default()
    };
    let (_, docs) = index_head(&r.browser, r.id, &c, &limits, &none)
        .await
        .expect("head");
    assert_eq!(docs.incomplete, Some("doc_large_files"));
}

/// Review (0d59521, MEDIUM): the repository's `spec.system` type default
/// is applied where the index is built (both reads): a stricter default, or
/// a definition that cannot be used, withholds every document from an AI.
#[tokio::test]
async fn the_spec_system_type_default_withholds_the_documents() {
    let r = repo().await;
    let cases: [(&[u8], bool); 4] = [
        (b"name: spec.system\n", false),
        (
            b"name: spec.system\ndefaults:\n  classification: internal\n",
            false,
        ),
        (
            b"name: spec.system\ndefaults:\n  classification: confidential\n",
            true,
        ),
        (b"name: [broken\n", true),
    ];
    for (definition, withheld) in cases {
        let c = commit(
            &r.path,
            &[
                ("spec/a.md", SPEC_A),
                (".forge/doctypes/spec.system.yml", definition),
            ],
        );
        let i = index_commit(&r.browser, r.id, &c, &IndexLimits::default(), Tags::Read)
            .await
            .expect("index");
        assert_eq!(
            i.index.confidential_paths().contains("spec/a.md"),
            withheld,
            "{}",
            String::from_utf8_lossy(definition)
        );
        let (head, _) = index_head(
            &r.browser,
            r.id,
            &c,
            &IndexLimits::default(),
            &Default::default(),
        )
        .await
        .expect("head");
        assert_eq!(head.index, i.index, "the same in both reads");
    }
    // No definition: the document's own mark decides.
    let c = commit(&r.path, &[("spec/a.md", SPEC_A)]);
    let i = index_commit(&r.browser, r.id, &c, &IndexLimits::default(), Tags::Read)
        .await
        .expect("index");
    assert!(i.index.confidential_paths().is_empty());
}
