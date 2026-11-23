#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The version job's document revision step on a real bare repository
//! (plan 13 stage 4 part A, review HIGH 2): it never fails the job — a
//! document git cannot read is skipped (logged), a repository without type
//! definitions reads no tree — and it records what a version revises.

use std::path::Path;
use std::process::{Command, Stdio};

use forge_checks::doc_lint::DocLimits;
use forge_checks::doc_revision::{record_version, revised_documents, VersionRecord};
use forge_core::doc_revision::DocKey;
use forge_core::{MergeRequestId, ObjectId, ProjectId, RequestId};
use forge_db::test_support::FreshDb;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};

fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>, index: Option<&Path>) -> String {
    use std::io::Write as _;
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
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
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn commit(repo: &Path, files: &[(&str, String)]) -> ObjectId {
    let mut listing = String::new();
    for (name, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
            None,
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let dir = tempfile::tempdir().expect("index dir");
    let index = dir.path().join("index");
    plumb(
        repo,
        &["update-index", "--add", "--index-info"],
        Some(listing.as_bytes()),
        Some(&index),
    );
    let tree = plumb(repo, &["write-tree"], None, Some(&index));
    ObjectId::parse(&plumb(
        repo,
        &["commit-tree", &tree, "-m", "synthetic"],
        None,
        None,
    ))
    .expect("sha")
}

fn doc(id: &str, body: &str) -> String {
    format!("---\ndoctype: qms.procedure\nid: {id}\ntitle: T\nowner: qa\n---\n\n# P\n\n{body}\n")
}

const DOCTYPE: (&str, &str) = (".forge/doctypes/qms.procedure.yml", "name: qms.procedure\n");

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
    let id = forge_core::RepoId::new(31);
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

fn with_doctype<'a>(files: &[(&'a str, String)]) -> Vec<(&'a str, String)> {
    let mut out = vec![(DOCTYPE.0, DOCTYPE.1.to_owned())];
    out.extend(files.iter().cloned());
    out
}

#[tokio::test]
async fn changed_and_removed_documents_are_revised_and_others_are_not() {
    let r = repo().await;
    let base = commit(
        &r.path,
        &with_doctype(&[
            ("a/QP-1.md", doc("QP-1", "v1")),
            ("a/QP-2.md", doc("QP-2", "v1")),
            ("a/QP-3.md", doc("QP-3", "v1")),
        ]),
    );
    let head = commit(
        &r.path,
        &with_doctype(&[
            ("a/QP-1.md", doc("QP-1", "v1")),
            ("a/QP-2.md", doc("QP-2", "v2")),
            ("README.md", "# not a document\n".to_owned()),
        ]),
    );
    let got = revised_documents(&r.browser, r.id, Some(&base), &head, &DocLimits::default())
        .await
        .expect("revised");
    assert_eq!(
        got.keys,
        vec![DocKey::original("QP-2"), DocKey::original("QP-3")]
    );
    assert_eq!(got.incomplete, None);
}

#[tokio::test]
async fn a_repository_without_type_definitions_revises_nothing() {
    let r = repo().await;
    let base = commit(&r.path, &[("a/QP-1.md", doc("QP-1", "v1"))]);
    let head = commit(&r.path, &[("a/QP-1.md", doc("QP-1", "v2"))]);
    let got = revised_documents(&r.browser, r.id, Some(&base), &head, &DocLimits::default())
        .await
        .expect("revised");
    assert!(got.keys.is_empty(), "{got:?}");
}

/// Review HIGH 2: a document git cannot read never fails the version job —
/// the step answers `Skipped` (logged) and nothing is recorded.
#[tokio::test]
async fn an_unreadable_document_is_skipped_never_a_job_failure() {
    let t = FreshDb::migrated().await;
    let r = repo().await;
    let base = commit(&r.path, &with_doctype(&[("a/QP-1.md", doc("QP-1", "v1"))]));
    let head = commit(
        &r.path,
        &with_doctype(&[("a/QP-1.md", doc("QP-1", "gone"))]),
    );
    let blob = plumb(
        &r.path,
        &["hash-object", "--stdin"],
        Some(doc("QP-1", "gone").as_bytes()),
        None,
    );
    std::fs::remove_file(r.path.join("objects").join(&blob[..2]).join(&blob[2..]))
        .expect("remove the blob");
    let got = record_version(
        &t.db,
        &r.browser,
        &DocLimits::default(),
        (ProjectId::new(1), 1, MergeRequestId::new(1)),
        r.id,
        (Some(&base), &head),
        &RequestId::parse("req-doc-revision").expect("id"),
    )
    .await;
    assert_eq!(got, VersionRecord::Skipped);
    let rows = t
        .json_rows("SELECT doc_id FROM forge.merge_request_documents")
        .await;
    assert!(rows.is_empty(), "{rows:?}");
}

/// A version of a repository without documents records nothing (no row,
/// no merge request needed).
#[tokio::test]
async fn a_version_without_documents_records_none() {
    let t = FreshDb::migrated().await;
    let r = repo().await;
    let base = commit(&r.path, &[("README.md", "# a\n".to_owned())]);
    let head = commit(&r.path, &[("README.md", "# b\n".to_owned())]);
    let got = record_version(
        &t.db,
        &r.browser,
        &DocLimits::default(),
        (ProjectId::new(1), 1, MergeRequestId::new(1)),
        r.id,
        (Some(&base), &head),
        &RequestId::parse("req-doc-revision").expect("id"),
    )
    .await;
    assert_eq!(got, VersionRecord::Recorded(0));
}
