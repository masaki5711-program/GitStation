#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::risk::{ChangeStatus, ChangedFile};
use forge_core::{ObjectId, RepoId};

use super::*;
use crate::GitConfig;

const T: Duration = Duration::from_secs(20);

struct Fixture {
    _home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    scratch: tempfile::TempDir,
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
    let id = RepoId::new(9);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        scratch: tempfile::tempdir().expect("scratch"),
        git,
        root,
        id,
        path,
    }
}

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
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
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

impl Fixture {
    /// A commit of `files` (path, content, mode); a `160000` entry's
    /// content is the commit id it points at.
    fn commit(&self, files: &[(&str, &[u8], &str)], parent: Option<&ObjectId>) -> ObjectId {
        let index = self
            .scratch
            .path()
            .join(format!("index-{}", uuid::Uuid::new_v4()));
        for (path, content, mode) in files {
            let object = if *mode == "160000" {
                String::from_utf8(content.to_vec()).expect("id")
            } else {
                plumb(
                    &self.path,
                    &index,
                    &["hash-object", "-w", "--stdin"],
                    Some(content),
                )
            };
            plumb(
                &self.path,
                &index,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{object},{path}"),
                ],
                None,
            );
        }
        let tree = plumb(&self.path, &index, &["write-tree"], None);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "c"];
        let p;
        if let Some(parent) = parent {
            p = parent.as_str().to_owned();
            args.push("-p");
            args.push(&p);
        }
        ObjectId::parse(&plumb(&self.path, &index, &args, None)).expect("oid")
    }
}

fn find<'a>(files: &'a [ChangedFile], path: &str) -> &'a ChangedFile {
    files.iter().find(|f| f.path == path).expect(path)
}

async fn diff(f: &Fixture, base: &ObjectId, head: &ObjectId, max: usize) -> RiskDiff {
    f.git
        .risk_diff(&f.root, f.id, base, head, max, T)
        .await
        .expect("risk diff")
}

/// Every status git reports, with modes and line counts.
#[tokio::test]
async fn risk_diff_reports_status_modes_and_counts() {
    let f = fixture().await;
    let base = f.commit(
        &[
            ("a.txt", b"one\ntwo\nthree\n", "100644"),
            ("gone.txt", b"x\ny\n", "100644"),
            ("flip", b"target\n", "100644"),
        ],
        None,
    );
    let head = f.commit(
        &[
            ("a.txt", b"one\n", "100644"),
            ("new.txt", b"n\n", "100644"),
            ("blob.bin", b"\x00\x01\x02", "100644"),
            ("flip", b"a.txt", "120000"),
        ],
        Some(&base),
    );
    // `commit` builds each tree from nothing: gone.txt is not in head.
    let RiskDiff::Complete(files) = diff(&f, &base, &head, 100).await else {
        panic!("complete");
    };
    let a = find(&files, "a.txt");
    assert_eq!(
        (a.status, a.added, a.deleted),
        (ChangeStatus::Modified, Some(0), Some(2))
    );
    let gone = find(&files, "gone.txt");
    assert_eq!(
        (gone.status, gone.old_mode, gone.new_mode, gone.deleted),
        (ChangeStatus::Deleted, 0o100_644, 0, Some(2))
    );
    let new = find(&files, "new.txt");
    assert_eq!(
        (new.status, new.old_mode, new.added),
        (ChangeStatus::Added, 0, Some(1))
    );
    let bin = find(&files, "blob.bin");
    assert_eq!(
        (bin.status, bin.added, bin.deleted),
        (ChangeStatus::Added, None, None)
    );
    let flip = find(&files, "flip");
    assert_eq!(
        (flip.status, flip.old_mode, flip.new_mode),
        (ChangeStatus::TypeChanged, 0o100_644, 0o120_000)
    );
}

/// A rename is one entry with its old path (plan 04 M-6: a rename is not a
/// deletion); an edit inside it keeps the line counts.
#[tokio::test]
async fn risk_diff_pairs_renames() {
    let f = fixture().await;
    let text = |tag: &str| -> Vec<u8> {
        (0..40)
            .flat_map(|i| format!("{tag} line {i}\n").into_bytes())
            .collect()
    };
    let (a, b) = (text("alpha"), text("beta"));
    let mut b_edited = b.clone();
    b_edited.extend_from_slice(b"one more\n");
    let base = f.commit(
        &[("old/a.rs", &a, "100644"), ("old/b.rs", &b, "100644")],
        None,
    );
    let head = f.commit(
        &[
            ("new/a.rs", &a, "100644"),
            ("new/b.rs", &b_edited, "100644"),
        ],
        Some(&base),
    );
    let RiskDiff::Complete(files) = diff(&f, &base, &head, 100).await else {
        panic!("complete");
    };
    assert_eq!(files.len(), 2, "{files:?}");
    let renamed = find(&files, "new/a.rs");
    assert_eq!(
        (
            renamed.status,
            renamed.old_path.as_deref(),
            renamed.added,
            renamed.deleted
        ),
        (ChangeStatus::Renamed, Some("old/a.rs"), Some(0), Some(0))
    );
    let edited = find(&files, "new/b.rs");
    assert_eq!(
        (
            edited.status,
            edited.old_path.as_deref(),
            edited.added,
            edited.deleted
        ),
        (ChangeStatus::Renamed, Some("old/b.rs"), Some(1), Some(0))
    );
}

#[tokio::test]
async fn risk_diff_stops_at_the_entry_cap() {
    let f = fixture().await;
    let base = f.commit(&[("keep", b"k\n", "100644")], None);
    let head = f.commit(
        &[
            ("keep", b"k\n", "100644"),
            ("a", b"1\n", "100644"),
            ("b", b"2\n", "100644"),
            ("c", b"3\n", "100644"),
        ],
        Some(&base),
    );
    assert_eq!(diff(&f, &base, &head, 2).await, RiskDiff::TooMany);
    let RiskDiff::Complete(files) = diff(&f, &base, &head, 3).await else {
        panic!("complete");
    };
    assert_eq!(files.len(), 3);
}

#[test]
fn the_parser_reads_renames_and_refuses_what_it_cannot_place() {
    // :100644 100644 <id> <id> R090 NUL old NUL new NUL, then numstat.
    let raw = b":100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 R090\0old.rs\0new.rs\0\
:100644 100644 1111111111111111111111111111111111111111 3333333333333333333333333333333333333333 M\0same.rs\0\
3\t1\t\0old.rs\0new.rs\0\
5\t0\tsame.rs\0";
    let RiskDiff::Complete(files) = parse_risk_diff(raw, 10).expect("parse") else {
        panic!("complete");
    };
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].old_path.as_deref(), Some("old.rs"));
    assert_eq!((files[0].added, files[0].deleted), (Some(3), Some(1)));
    assert_eq!((files[1].added, files[1].deleted), (Some(5), Some(0)));

    let bad_status = b":100644 100644 1 2 X\0p\0";
    assert!(parse_risk_diff(bad_status, 10).is_err());
    let non_utf8 = b":100644 100644 1 2 M\0\xff\xfe\0";
    assert_eq!(
        parse_risk_diff(non_utf8, 10).expect("parse"),
        RiskDiff::NonUtf8
    );
}

// ── tree totals ──

#[tokio::test]
async fn tree_totals_count_files_and_text_lines_only() {
    let f = fixture().await;
    let head = f.commit(
        &[
            ("a.txt", b"1\n2\n3\n", "100644"),
            ("dir/b.txt", b"x\ny\n", "100644"),
            ("img.bin", b"\x00\x01\x02\x03", "100644"),
        ],
        None,
    );
    let totals = f
        .git
        .tree_totals(&f.root, f.id, &head, 100, T)
        .await
        .expect("totals");
    assert_eq!(
        totals,
        TreeTotals {
            files: 3,
            lines: 5,
            truncated: false
        }
    );
}

#[tokio::test]
async fn tree_totals_say_so_when_they_stop_at_the_cap() {
    let f = fixture().await;
    let head = f.commit(
        &[
            ("a", b"1\n", "100644"),
            ("b", b"2\n", "100644"),
            ("c", b"3\n", "100644"),
        ],
        None,
    );
    let totals = f
        .git
        .tree_totals(&f.root, f.id, &head, 2, T)
        .await
        .expect("totals");
    assert!(totals.truncated);
    assert!(totals.files <= 3);
}

#[test]
fn the_empty_tree_is_the_one_of_the_hash_length() {
    let sha1 = ObjectId::parse(&"a".repeat(40)).expect("sha1");
    let sha256 = ObjectId::parse(&"a".repeat(64)).expect("sha256");
    assert_eq!(
        empty_tree(&sha1).as_str(),
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    );
    assert_eq!(
        empty_tree(&sha256).as_str(),
        "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
    );
}
