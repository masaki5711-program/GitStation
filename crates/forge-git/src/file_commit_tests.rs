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
        // Not git's directory: a longer name, or `~` not followed by digits.
        ".gitignore",
        ".git-blame-ignore-revs",
        "git~a/x.md",
        "notgit~1/x.md",
        "a.b/c..d.md",
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
        // Review E1: what NTFS / HFS+ read as `.git`.
        (".git./config", "dot_git"),
        (".git ./config", "dot_git"),
        (".git::$INDEX_ALLOCATION/config", "dot_git"),
        (".git:stream", "dot_git"),
        (".g\u{200B}it/config", "dot_git"),
        ("\u{FEFF}.git/config", "dot_git"),
        (".gi\u{200C}t\u{200D}/hooks/x", "dot_git"),
        ("a/.Git. . ./x", "dot_git"),
        ("git~1/config", "dot_git"),
        ("GIT~12/config", "dot_git"),
        ("a/Git~1./hooks/x", "dot_git"),
        ("...", "dot_segment"),
        ("a/. ./b.md", "dot_segment"),
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

/// One run of [`Git::commit_one_file`] on the fixture's `README`.
async fn run_one(
    f: &Fixture,
    (start, target, create): (ObjectId, RefName, bool),
    content: &str,
    total: Duration,
    bump: &(dyn Fn() + Send + Sync),
) -> Result<OneFileCommitted, ForgeError> {
    let main = RefName::parse("refs/heads/main").expect("ref");
    let path = FilePath::parse("README").expect("path");
    let author = ident("Dave");
    f.git
        .commit_one_file(
            &f.root,
            f.id,
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
            (T, total),
            bump,
        )
        .await
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
    let written = AtomicUsize::new(0);
    let bump = || {
        written.fetch_add(1, Ordering::SeqCst);
    };
    let run = |start: &ObjectId, target: &RefName, create: bool, content: &'static str| {
        run_one(
            &f,
            (start.clone(), target.clone(), create),
            content,
            COMMIT_DEADLINE,
            &bump,
        )
    };
    let first = run(&base, &main, false, "v2\n").await.expect("first");
    assert_eq!(first.parent, base);
    assert_eq!(
        plumb(&f.path, &["rev-parse", "refs/heads/main"], None),
        first.commit.as_str()
    );
    assert_eq!(written.load(Ordering::SeqCst), 1);
    // Started at `base`, the head moved by README itself: a conflict.
    let e = run(&base, &main, false, "v3\n").await.expect_err("changed");
    assert_eq!(
        (e.code(), reason_of(&e).as_str()),
        (ErrorCode::FileCommitConflict, "file_changed")
    );
    // The same content: nothing to commit.
    let e = run(&first.commit, &main, false, "v2\n")
        .await
        .expect_err("same");
    assert_eq!(e.code(), ErrorCode::FileCommitUnchanged);
    // An unknown start.
    let unknown = ObjectId::parse(&"e".repeat(40)).expect("oid");
    let e = run(&unknown, &main, false, "v4\n")
        .await
        .expect_err("unknown");
    assert_eq!(reason_of(&e), "start_unknown");
    // A new branch; the same name again is never moved.
    let edit = RefName::parse("refs/heads/edit/readme").expect("ref");
    let made = run(&first.commit, &edit, true, "v5\n")
        .await
        .expect("branch");
    assert_eq!(made.parent, first.commit);
    let e = run(&first.commit, &edit, true, "v6\n")
        .await
        .expect_err("exists");
    assert_eq!(reason_of(&e), "branch_exists");
    assert_eq!(
        plumb(&f.path, &["rev-parse", "refs/heads/edit/readme"], None),
        made.commit.as_str()
    );
}

/// Review E1 (LOW): a start commit that is not an ancestor of the head
/// (another line of history, e.g. after a force push) is a conflict, even
/// when the file is the same there.
#[tokio::test]
async fn a_start_off_the_branch_is_not_an_ancestor() {
    let f = fixture().await;
    let base = sample(&f.path);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", base.as_str()],
        None,
    );
    // The same tree, committed apart: not an ancestor of main.
    let tree = plumb(
        &f.path,
        &["rev-parse", &format!("{}^{{tree}}", base.as_str())],
        None,
    );
    let side = ObjectId::parse(&plumb(
        &f.path,
        &["commit-tree", &tree, "-m", "another line of history"],
        None,
    ))
    .expect("oid");
    assert_ne!(side, base);
    let main = RefName::parse("refs/heads/main").expect("ref");
    let e = run_one(&f, (side, main, false), "v2\n", COMMIT_DEADLINE, &|| {})
        .await
        .expect_err("not an ancestor");
    assert_eq!(
        (e.code(), reason_of(&e).as_str()),
        (ErrorCode::FileCommitConflict, "start_not_ancestor")
    );
}

/// Review E1: a new branch whose name a ref's directory or file takes
/// (`edit/x` while `edit` exists, or `deep` while `deep/a/b` does) is the
/// user's naming — 409 `branch_name_conflict`, never git's 500 — and
/// nothing moves.
#[tokio::test]
async fn a_branch_name_taken_by_a_ref_path_is_a_conflict() {
    let f = fixture().await;
    let base = sample(&f.path);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", base.as_str()],
        None,
    );
    plumb(
        &f.path,
        &["update-ref", "refs/heads/edit", base.as_str()],
        None,
    );
    plumb(
        &f.path,
        &["update-ref", "refs/heads/deep/a/b", base.as_str()],
        None,
    );
    for name in [
        "refs/heads/edit/x",
        "refs/heads/edit/x/y",
        "refs/heads/deep",
        "refs/heads/deep/a",
    ] {
        let target = RefName::parse(name).expect("ref");
        let e = run_one(
            &f,
            (base.clone(), target, true),
            "v2\n",
            COMMIT_DEADLINE,
            &|| {},
        )
        .await
        .expect_err(name);
        assert_eq!(
            (e.code(), reason_of(&e).as_str()),
            (ErrorCode::FileCommitConflict, "branch_name_conflict"),
            "{name}: {e}"
        );
    }
    // A sibling is fine.
    let sibling = RefName::parse("refs/heads/deep2").expect("ref");
    run_one(&f, (base, sibling, true), "v2\n", COMMIT_DEADLINE, &|| {})
        .await
        .expect("sibling");
}

/// Review E1 (MEDIUM): one overall deadline bounds every attempt; past it
/// the commit is `FG-GIT-0002 operation=single_file_commit`, nothing moves
/// and no temporary index is left.
#[tokio::test]
async fn the_whole_commit_has_one_deadline() {
    let f = fixture().await;
    let base = sample(&f.path);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", base.as_str()],
        None,
    );
    let main = RefName::parse("refs/heads/main").expect("ref");
    let e = run_one(
        &f,
        (base.clone(), main, false),
        "v2\n",
        Duration::ZERO,
        &|| {},
    )
    .await
    .expect_err("deadline");
    assert_eq!(e.code(), ErrorCode::GitTimeout);
    assert!(e
        .context()
        .iter()
        .any(|(k, v)| *k == "operation" && v == "single_file_commit"));
    assert_eq!(
        plumb(&f.path, &["rev-parse", "refs/heads/main"], None),
        base.as_str()
    );
    let dir = f.home.path().join("file-commits");
    let left = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 0);
}

/// Review E1 (MEDIUM): index files a killed server left are swept; fresh
/// ones (a commit in progress) and other files stay.
#[test]
fn stale_temporary_indexes_are_swept() {
    let dir = tempfile::tempdir().expect("dir");
    let old = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
    for name in ["old.index", "old.index.lock", "keep.txt"] {
        let path = dir.path().join(name);
        std::fs::write(&path, b"x").expect("write");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(old)
            .expect("mtime");
    }
    std::fs::write(dir.path().join("fresh.index"), b"x").expect("write");
    assert_eq!(sweep_stale_indexes(dir.path(), STALE_INDEX_AGE), 2);
    let mut left: Vec<String> = std::fs::read_dir(dir.path())
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["fresh.index", "keep.txt"]);
    assert_eq!(
        sweep_stale_indexes(&dir.path().join("none"), STALE_INDEX_AGE),
        0
    );
}

/// Review E1 (MEDIUM): a path git itself refuses with core.protectNTFS /
/// protectHFS (one that slipped past `FilePath::parse`) is FG-GIT-0028
/// `refused_by_git`, a 4xx, never a 500 — and no index is left.
#[tokio::test]
async fn a_path_git_refuses_is_path_invalid() {
    let f = fixture().await;
    let parent = sample(&f.path);
    let author = ident("Dave");
    let b = f
        .git
        .write_blob(&f.root, f.id, b"x\n", T)
        .await
        .expect("blob");
    for raw in [".git/config", "a/.git./x", "GIT~1/config"] {
        // Built directly: `parse` refuses every one of them.
        assert!(FilePath::parse(raw).is_err(), "{raw}");
        let path = FilePath(raw.to_owned());
        let e = f
            .git
            .write_file_commit(
                &f.root,
                f.id,
                &FileCommit {
                    parent: &parent,
                    path: &path,
                    blob: &b,
                    mode: FileMode::Regular,
                    message: "x",
                    author: &author,
                    committer: &author,
                },
                T,
            )
            .await
            .expect_err(raw);
        assert_eq!(e.code(), ErrorCode::FileCommitPathInvalid, "{raw}: {e}");
        assert_eq!(rule_of(&e), "refused_by_git", "{raw}");
    }
    let left: Vec<_> = std::fs::read_dir(f.home.path().join("file-commits"))
        .expect("dir")
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
