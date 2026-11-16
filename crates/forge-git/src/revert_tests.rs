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
    _home: tempfile::TempDir,
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
        _home: home,
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

fn commit(repo: &Path, files: &[(&str, &str)], parents: &[&ObjectId]) -> ObjectId {
    let mut listing = String::new();
    let mut sorted: Vec<&(&str, &str)> = files.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    for (name, content) in sorted {
        let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(content));
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(&listing));
    let mut args = vec![
        "commit-tree".to_owned(),
        tree,
        "-m".into(),
        "synthetic".into(),
    ];
    for p in parents {
        args.push("-p".into());
        args.push(p.as_str().to_owned());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    ObjectId::parse(&plumb(repo, &args, None)).expect("oid")
}

fn tree(repo: &Path, commit: &ObjectId) -> ObjectId {
    ObjectId::parse(&plumb(
        repo,
        &["rev-parse", &format!("{}^{{tree}}", commit.as_str())],
        None,
    ))
    .expect("tree")
}

/// The files of `tree` with their content.
fn files(repo: &Path, tree: &ObjectId) -> Vec<(String, String)> {
    let names = plumb(repo, &["ls-tree", "--name-only", tree.as_str()], None);
    names
        .lines()
        .map(|n| {
            let content = plumb(
                repo,
                &["cat-file", "-p", &format!("{}:{n}", tree.as_str())],
                None,
            );
            (n.to_owned(), content)
        })
        .collect()
}

fn limits() -> MergeTreeLimits {
    MergeTreeLimits {
        timeout: T,
        ..MergeTreeLimits::default()
    }
}

fn clean(r: RevertTree) -> ObjectId {
    match r {
        RevertTree::Clean { tree } => tree,
        other @ (RevertTree::Conflicted { .. } | RevertTree::Unchanged) => {
            panic!("not clean: {other:?}")
        }
    }
}

/// `tree_of` reads a commit's tree; a missing commit is FG-GIT-0003
/// `not_a_commit`, never a tree.
#[tokio::test]
async fn the_tree_of_a_commit_is_read_and_a_missing_one_is_refused() {
    let f = fixture().await;
    let c = commit(&f.path, &[("a.txt", "1\n")], &[]);
    let got = f.git.tree_of(&f.root, f.id, &c, T).await.expect("tree");
    assert_eq!(got, tree(&f.path, &c));
    let missing = ObjectId::parse(&"e".repeat(40)).expect("oid");
    let err = f
        .git
        .tree_of(&f.root, f.id, &missing, T)
        .await
        .expect_err("missing");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "not_a_commit"));
}

/// A merge commit (`git revert -m 1`): main moved on with an unrelated
/// change after the merge; the revert removes the merged file and keeps the
/// later change, exactly what `git revert -m 1` leaves.
#[tokio::test]
async fn a_merge_commit_is_reverted_against_its_first_parent() {
    let f = fixture().await;
    let base = commit(&f.path, &[("README.md", "base\n")], &[]);
    let head = commit(
        &f.path,
        &[("README.md", "base\n"), ("feature.txt", "f\n")],
        &[&base],
    );
    let merge = commit(
        &f.path,
        &[("README.md", "base\n"), ("feature.txt", "f\n")],
        &[&base, &head],
    );
    let later = commit(
        &f.path,
        &[
            ("README.md", "base\n"),
            ("feature.txt", "f\n"),
            ("later.txt", "l\n"),
        ],
        &[&merge],
    );
    let got = clean(
        f.git
            .revert_tree(
                &f.root,
                f.id,
                RevertInputs {
                    merged: &merge,
                    before: &base,
                    onto: &later,
                },
                limits(),
            )
            .await
            .expect("revert"),
    );
    assert_eq!(
        files(&f.path, &got),
        [
            ("README.md".to_owned(), "base".to_owned()),
            ("later.txt".to_owned(), "l".to_owned())
        ]
    );
}

/// A squash commit (one parent): the revert undoes the squashed change on
/// a tip that is still the squash itself.
#[tokio::test]
async fn a_squash_commit_is_reverted_to_its_parent() {
    let f = fixture().await;
    let base = commit(&f.path, &[("a.txt", "1\n2\n3\n")], &[]);
    let squash = commit(
        &f.path,
        &[("a.txt", "1\nTWO\n3\n"), ("b.txt", "b\n")],
        &[&base],
    );
    let got = clean(
        f.git
            .revert_tree(
                &f.root,
                f.id,
                RevertInputs {
                    merged: &squash,
                    before: &base,
                    onto: &squash,
                },
                limits(),
            )
            .await
            .expect("revert"),
    );
    assert_eq!(got, tree(&f.path, &base));
}

/// A later change to the lines the merge changed makes the revert
/// conflict: the paths are listed, nothing else is decided.
#[tokio::test]
async fn a_revert_over_a_later_change_of_the_same_lines_conflicts() {
    let f = fixture().await;
    let base = commit(&f.path, &[("a.txt", "one\n")], &[]);
    let squash = commit(&f.path, &[("a.txt", "two\n")], &[&base]);
    let later = commit(&f.path, &[("a.txt", "three\n")], &[&squash]);
    let got = f
        .git
        .revert_tree(
            &f.root,
            f.id,
            RevertInputs {
                merged: &squash,
                before: &base,
                onto: &later,
            },
            limits(),
        )
        .await
        .expect("revert");
    assert_eq!(
        got,
        RevertTree::Conflicted {
            files: vec!["a.txt".to_owned()],
            files_truncated: false,
        }
    );
}

/// A tip that no longer holds the change (it was reverted by hand) has
/// nothing to revert.
#[tokio::test]
async fn a_change_already_gone_from_the_tip_is_unchanged() {
    let f = fixture().await;
    let base = commit(&f.path, &[("a.txt", "1\n")], &[]);
    let squash = commit(&f.path, &[("a.txt", "1\n"), ("b.txt", "b\n")], &[&base]);
    let undone = commit(&f.path, &[("a.txt", "1\n")], &[&squash]);
    let got = f
        .git
        .revert_tree(
            &f.root,
            f.id,
            RevertInputs {
                merged: &squash,
                before: &base,
                onto: &undone,
            },
            limits(),
        )
        .await
        .expect("revert");
    assert_eq!(got, RevertTree::Unchanged);
}
