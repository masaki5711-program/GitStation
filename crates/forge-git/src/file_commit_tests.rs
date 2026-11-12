#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::{ObjectId, RepoId};
use forge_errors::ErrorCode;

use super::*;
use crate::GitConfig;

const T: Duration = Duration::from_secs(20);

struct Fixture {
    home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    git: Git,
    root: StorageRoot,
    id: RepoId,
    path: PathBuf,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root_dir = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    let id = RepoId::new(12);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        home,
        _root_dir: root_dir,
        git,
        root,
        id,
        path,
    }
}

fn plumb(repo: &Path, args: &[&str], input: Option<&str>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input.as_bytes()).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn blob(repo: &Path, content: &str) -> String {
    plumb(repo, &["hash-object", "-w", "--stdin"], Some(content))
}

/// A tree from `(mode, type, oid, name)` entries.
fn tree(repo: &Path, entries: &[(&str, &str, &str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str, &str, &str)> = entries.iter().collect();
    sorted.sort_by_key(|e| e.3);
    let listing: String = sorted
        .iter()
        .map(|(mode, kind, oid, name)| format!("{mode} {kind} {oid}\t{name}\n"))
        .collect();
    plumb(repo, &["mktree"], Some(&listing))
}

fn commit_of(repo: &Path, tree: &str) -> ObjectId {
    ObjectId::parse(&plumb(
        repo,
        &["commit-tree", tree, "-m", "synthetic"],
        None,
    ))
    .expect("oid")
}

/// `README` (regular), `run.sh` (executable), `link` (a symlink),
/// `sub` (a submodule), `docs/guide/intro.md`.
fn sample(repo: &Path) -> ObjectId {
    let readme = blob(repo, "readme\n");
    let run = blob(repo, "#!/bin/sh\n");
    let target = blob(repo, "README");
    let intro = blob(repo, "# Intro\n");
    let guide = tree(repo, &[("100644", "blob", &intro, "intro.md")]);
    let docs = tree(repo, &[("040000", "tree", &guide, "guide")]);
    let gitlink = "1".repeat(40);
    let root = tree(
        repo,
        &[
            ("100644", "blob", &readme, "README"),
            ("100755", "blob", &run, "run.sh"),
            ("120000", "blob", &target, "link"),
            ("160000", "commit", &gitlink, "sub"),
            ("040000", "tree", &docs, "docs"),
        ],
    );
    commit_of(repo, &root)
}

fn ident(name: &str) -> CommitIdentity {
    CommitIdentity::new(name, &format!("{}@example.test", name.to_lowercase()), "u")
        .expect("identity")
}

fn rule_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "rule")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[test]
fn every_path_rule_has_its_reason() {
    for good in [
        "README.md",
        "docs/spec.md",
        "a/b/c/d.txt",
        ".github/x.yml",
        "日本語.md",
    ] {
        assert_eq!(FilePath::parse(good).expect(good).as_str(), good);
    }
    let long = "a".repeat(MAX_PATH_BYTES + 1);
    for (bad, rule) in [
        ("", "empty"),
        ("/etc/passwd", "absolute"),
        ("a\\b.md", "backslash"),
        ("a//b.md", "empty_segment"),
        ("docs/", "empty_segment"),
        ("./a.md", "dot_segment"),
        ("a/../../b.md", "dot_segment"),
        ("..", "dot_segment"),
        (".git/config", "dot_git"),
        ("a/.GIT/hooks/pre-receive", "dot_git"),
        ("a\0b", "control"),
        ("a\nb", "control"),
        (long.as_str(), "too_long"),
    ] {
        let e = FilePath::parse(bad).expect_err(bad);
        assert_eq!(e.code(), ErrorCode::FileCommitPathInvalid, "{bad:?}");
        assert_eq!(rule_of(&e), rule, "{bad:?}");
    }
}

#[test]
fn content_over_the_limit_is_too_large() {
    check_content("hello\n", 1024).expect("small");
    let e = check_content(&"x".repeat(11), 10).expect_err("over the caller's limit");
    assert_eq!(e.code(), ErrorCode::FileCommitTooLarge);
    let e = check_content(&"x".repeat(MAX_FILE_BYTES + 1), usize::MAX).expect_err("over 1 MiB");
    assert_eq!(e.code(), ErrorCode::FileCommitTooLarge);
    check_content(&"x".repeat(MAX_FILE_BYTES), usize::MAX).expect("exactly 1 MiB");
}

#[test]
fn content_with_a_nul_is_not_text() {
    let e = check_content("a\0b", 1024).expect_err("nul");
    assert_eq!(e.code(), ErrorCode::FileCommitNotText);
    check_content("日本語のテキスト\r\n\ttab", 1024).expect("utf-8 text with controls");
}

#[tokio::test]
async fn the_entry_at_a_path_is_classified() {
    let f = fixture().await;
    let c = sample(&f.path);
    let at = |p: &'static str| {
        let (git, root, id, c) = (f.git.clone(), f.root.clone(), f.id, c.clone());
        async move {
            git.file_entry(&root, id, &c, &FilePath::parse(p).expect("path"), T)
                .await
        }
    };
    let readme = at("README").await.expect("readme").expect("present");
    assert_eq!(readme.mode, FileMode::Regular);
    assert_eq!(readme.blob.as_str(), blob(&f.path, "readme\n"));
    let run = at("run.sh").await.expect("run").expect("present");
    assert_eq!(run.mode, FileMode::Executable);
    let intro = at("docs/guide/intro.md")
        .await
        .expect("deep")
        .expect("present");
    assert_eq!(intro.blob.as_str(), blob(&f.path, "# Intro\n"));
    for absent in ["new.md", "docs/new.md", "docs/guide/new.md", "new/dir/x.md"] {
        assert_eq!(at(absent).await.expect(absent), None, "{absent}");
    }
    for (bad, rule) in [
        ("docs", "path_is_directory"),
        ("docs/guide", "path_is_directory"),
        ("README/x.md", "parent_is_file"),
        ("docs/guide/intro.md/x", "parent_is_file"),
        ("link", "not_regular_file"),
        ("sub", "not_regular_file"),
        ("link/x.md", "parent_is_file"),
    ] {
        let e = at(bad).await.expect_err(bad);
        assert_eq!(e.code(), ErrorCode::FileCommitPathInvalid, "{bad}");
        assert_eq!(rule_of(&e), rule, "{bad}");
    }
}

#[tokio::test]
async fn a_commit_id_is_told_from_other_objects() {
    let f = fixture().await;
    let c = sample(&f.path);
    assert!(f
        .git
        .commit_exists(&f.root, f.id, &c, T)
        .await
        .expect("commit"));
    let b = ObjectId::parse(&blob(&f.path, "readme\n")).expect("blob");
    assert!(!f
        .git
        .commit_exists(&f.root, f.id, &b, T)
        .await
        .expect("blob"));
    let missing = ObjectId::parse(&"e".repeat(40)).expect("oid");
    assert!(!f
        .git
        .commit_exists(&f.root, f.id, &missing, T)
        .await
        .expect("missing"));
}

#[tokio::test]
async fn one_file_is_committed_on_its_parent() {
    let f = fixture().await;
    let parent = sample(&f.path);
    let author = ident("Dave");
    let path = FilePath::parse("docs/new/spec.md").expect("path");
    let content = "# Spec\n\n本文\n";
    let new_blob = f
        .git
        .write_blob(&f.root, f.id, content.as_bytes(), T)
        .await
        .expect("blob");
    assert_eq!(new_blob.as_str(), blob(&f.path, content));
    let commit = f
        .git
        .write_file_commit(
            &f.root,
            f.id,
            &FileCommit {
                parent: &parent,
                path: &path,
                blob: &new_blob,
                mode: FileMode::Regular,
                message: "Add the spec\n",
                author: &author,
                committer: &author,
            },
            T,
        )
        .await
        .expect("commit");
    let c = commit.as_str();
    assert_eq!(
        plumb(&f.path, &["rev-parse", &format!("{c}^")], None),
        parent.as_str()
    );
    assert_eq!(
        plumb(&f.path, &["show", &format!("{c}:docs/new/spec.md")], None),
        content.trim_end()
    );
    // Everything else is the parent's, modes and links included.
    let changed = plumb(
        &f.path,
        &["diff-tree", "--no-commit-id", "--name-status", "-r", c],
        None,
    );
    assert_eq!(changed, "A\tdocs/new/spec.md");
    let raw = plumb(&f.path, &["cat-file", "commit", c], None);
    assert!(raw.contains("author Dave <dave@example.test>"), "{raw}");
    assert!(raw.ends_with("Add the spec"), "{raw}");
    // The temporary index is gone.
    let left: Vec<_> = std::fs::read_dir(f.home.path().join("file-commits"))
        .expect("dir")
        .collect();
    assert!(left.is_empty(), "{left:?}");

    // Replacing an executable keeps the mode the caller passes.
    let script = FilePath::parse("run.sh").expect("path");
    let b2 = f
        .git
        .write_blob(&f.root, f.id, b"#!/bin/sh\necho hi\n", T)
        .await
        .expect("blob");
    let second = f
        .git
        .write_file_commit(
            &f.root,
            f.id,
            &FileCommit {
                parent: &commit,
                path: &script,
                blob: &b2,
                mode: FileMode::Executable,
                message: "Edit the script",
                author: &author,
                committer: &author,
            },
            T,
        )
        .await
        .expect("second");
    let listed = plumb(&f.path, &["ls-tree", second.as_str(), "--", "run.sh"], None);
    assert!(listed.starts_with("100755 blob "), "{listed}");
}

fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// The editor's whole commit: built on a moved head when only other files
/// changed, refused when the file changed or is the same, an existing new
/// branch is never moved, and the cache callback runs after each swap.
#[tokio::test]
async fn one_file_is_committed_with_a_compare_and_swap() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = fixture().await;
    let base = sample(&f.path);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", base.as_str()],
        None,
    );
    let main = RefName::parse("refs/heads/main").expect("ref");
    let path = FilePath::parse("README").expect("path");
    let author = ident("Dave");
    let written = AtomicUsize::new(0);
    let bump = || {
        written.fetch_add(1, Ordering::SeqCst);
    };
    let one = |start: &'static str, content: &'static str| (start, content);
    let _ = one;
    let run = |start: ObjectId, target: RefName, create: bool, content: &'static str| {
        let (git, root, id) = (f.git.clone(), f.root.clone(), f.id);
        let (main, path, author) = (main.clone(), path.clone(), author.clone());
        let bump = &bump;
        async move {
            git.commit_one_file(
                &root,
                id,
                &OneFile {
                    branch: &main,
                    target: &target,
                    create,
                    start: &start,
                    path: &path,
                    content: content.as_bytes(),
                    message: "Edit",
                    author: &author,
                },
                T,
                bump,
            )
            .await
        }
    };
    let first = run(base.clone(), main.clone(), false, "v2\n")
        .await
        .expect("first");
    assert_eq!(first.parent, base);
    assert_eq!(
        plumb(&f.path, &["rev-parse", "refs/heads/main"], None),
        first.commit.as_str()
    );
    assert_eq!(written.load(Ordering::SeqCst), 1);
    // Started at `base`, the head moved by README itself: a conflict.
    let e = run(base.clone(), main.clone(), false, "v3\n")
        .await
        .expect_err("changed");
    assert_eq!(
        (e.code(), reason_of(&e).as_str()),
        (ErrorCode::FileCommitConflict, "file_changed")
    );
    // The same content: nothing to commit.
    let e = run(first.commit.clone(), main.clone(), false, "v2\n")
        .await
        .expect_err("same");
    assert_eq!(e.code(), ErrorCode::FileCommitUnchanged);
    // An unknown start.
    let unknown = ObjectId::parse(&"e".repeat(40)).expect("oid");
    let e = run(unknown, main.clone(), false, "v4\n")
        .await
        .expect_err("unknown");
    assert_eq!(reason_of(&e), "start_unknown");
    // A new branch; the same name again is never moved.
    let edit = RefName::parse("refs/heads/edit/readme").expect("ref");
    let made = run(first.commit.clone(), edit.clone(), true, "v5\n")
        .await
        .expect("branch");
    assert_eq!(made.parent, first.commit);
    let e = run(first.commit.clone(), edit.clone(), true, "v6\n")
        .await
        .expect_err("exists");
    assert_eq!(reason_of(&e), "branch_exists");
    assert_eq!(
        plumb(&f.path, &["rev-parse", "refs/heads/edit/readme"], None),
        made.commit.as_str()
    );
}
