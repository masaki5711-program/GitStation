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
    let id = RepoId::new(9);
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

/// Plumbing on the bare repository (test setup only; synthetic content).
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

/// A commit whose tree holds `files` (flat names) on top of `parents`.
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

/// A base, a target that changed `README` and a head that changed it
/// differently (conflict), and a head that only added a file (clean).
struct Histories {
    base: ObjectId,
    target: ObjectId,
    conflicting: ObjectId,
    clean: ObjectId,
}

fn histories(repo: &Path) -> Histories {
    let base = commit(repo, &[("README", "base\n"), ("other", "o\n")], &[]);
    let target = commit(repo, &[("README", "target\n"), ("other", "o\n")], &[&base]);
    let conflicting = commit(repo, &[("README", "head\n"), ("other", "o\n")], &[&base]);
    let clean = commit(
        repo,
        &[("README", "base\n"), ("other", "o\n"), ("added", "a\n")],
        &[&base],
    );
    Histories {
        base,
        target,
        conflicting,
        clean,
    }
}

impl Fixture {
    async fn trial(
        &self,
        target: &ObjectId,
        head: &ObjectId,
        limits: MergeTreeLimits,
    ) -> Result<MergeTree, ForgeError> {
        self.git
            .merge_tree(&self.root, self.id, target, head, limits)
            .await
    }
}

/// ADR-0024 §3: the argument vector and the exit codes of every case the
/// mergeability job relies on. Run against the local git (2.53 on the
/// maintainer's PC); git 2.38 itself — the minimum — is not available
/// here, so the "2.38" in the name is the contract, verified on newer git
/// only (the options used all exist since 2.38: `--write-tree`,
/// `--name-only`, `-z`; `merge-base --is-ancestor` is older).
#[tokio::test]
async fn merge_tree_exit_codes_on_git_2_38() {
    let f = fixture().await;
    let h = histories(&f.path);
    let argv: Vec<String> = f
        .git
        .merge_tree_command(&f.path, &h.target, &h.conflicting)
        .expect("command")
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        argv,
        [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "-z",
            "--end-of-options",
            h.target.as_str(),
            h.conflicting.as_str(),
        ]
    );
    // Exit 0: one tree id.
    let clean = f
        .trial(&h.target, &h.clean, MergeTreeLimits::default())
        .await
        .expect("clean");
    assert!(matches!(clean, MergeTree::Clean { .. }), "{clean:?}");
    // Exit 1: the conflicted path, no messages in the list.
    let conflict = f
        .trial(&h.target, &h.conflicting, MergeTreeLimits::default())
        .await
        .expect("conflict");
    assert_eq!(
        conflict,
        MergeTree::Conflicted {
            files: vec!["README".to_owned()],
            files_truncated: false,
        }
    );
    // merge-base: exit 0 with the id; --is-ancestor: exit 0 / 1.
    let base = f
        .git
        .merge_base(&f.root, f.id, &h.target, &h.conflicting, T)
        .await
        .expect("merge-base");
    assert_eq!(base, Some(h.base.clone()));
    assert!(f
        .git
        .is_ancestor(&f.root, f.id, &h.base, &h.target, T)
        .await
        .expect("ancestor"));
    assert!(!f
        .git
        .is_ancestor(&f.root, f.id, &h.target, &h.conflicting, T)
        .await
        .expect("not an ancestor"));
}

/// A clean merge prints one id: it fits even the smallest cap, so the
/// overflow rule never turns a clean merge into a conflict.
#[tokio::test]
async fn a_clean_merge_fits_the_smallest_cap() {
    let f = fixture().await;
    let h = histories(&f.path);
    let limits = MergeTreeLimits {
        max_output_bytes: 80,
        ..MergeTreeLimits::default()
    };
    let clean = f.trial(&h.target, &h.clean, limits).await.expect("clean");
    assert!(matches!(clean, MergeTree::Clean { .. }), "{clean:?}");
}

/// ADR-0024 §3 (re-review): an output over the cap stops git before its
/// exit code is known; that is a conflict with a cut list, never
/// FG-GIT-0006, and no half-read name is listed.
#[tokio::test]
async fn an_overflowing_conflict_list_is_a_conflict_with_files_truncated() {
    let f = fixture().await;
    let names: Vec<String> = (0..40)
        .map(|n| format!("conflicting-file-with-a-long-name-{n:04}"))
        .collect();
    let with = |content: &'static str| -> Vec<(&str, &str)> {
        names.iter().map(|n| (n.as_str(), content)).collect()
    };
    let base = commit(
        &f.path,
        &with(
            "base
",
        ),
        &[],
    );
    let target = commit(
        &f.path,
        &with(
            "target
",
        ),
        &[&base],
    );
    let head = commit(
        &f.path,
        &with(
            "head
",
        ),
        &[&base],
    );
    let limits = MergeTreeLimits {
        max_output_bytes: 512,
        ..MergeTreeLimits::default()
    };
    let got = f.trial(&target, &head, limits).await.expect("overflow");
    let MergeTree::Conflicted {
        files,
        files_truncated,
    } = got
    else {
        panic!("an overflow must be a conflict: {got:?}");
    };
    assert!(files_truncated);
    assert!(!files.is_empty());
    for file in &files {
        assert!(names.contains(file), "a half-read name was listed: {file}");
    }
}

#[tokio::test]
async fn more_conflicts_than_max_files_are_cut_and_marked() {
    let f = fixture().await;
    let names = ["a", "b", "c", "d", "e"];
    let with = |content: &'static str| -> Vec<(&str, &str)> {
        names.iter().map(|n| (*n, content)).collect()
    };
    let base = commit(&f.path, &with("base\n"), &[]);
    let target = commit(&f.path, &with("target\n"), &[&base]);
    let head = commit(&f.path, &with("head\n"), &[&base]);
    let limits = MergeTreeLimits {
        max_files: 2,
        ..MergeTreeLimits::default()
    };
    let got = f.trial(&target, &head, limits).await.expect("conflict");
    assert_eq!(
        got,
        MergeTree::Conflicted {
            files: vec!["a".to_owned(), "b".to_owned()],
            files_truncated: true,
        }
    );
}

/// Unrelated histories are never "clean": merge-tree refuses them (exit
/// 128, FG-GIT-0003) and merge-base has no answer (exit 1, `None`).
#[tokio::test]
async fn unrelated_histories_are_an_error_not_a_clean_merge() {
    let f = fixture().await;
    let h = histories(&f.path);
    let orphan = commit(&f.path, &[("elsewhere", "x\n")], &[]);
    let err = f
        .trial(&h.target, &orphan, MergeTreeLimits::default())
        .await
        .expect_err("unrelated");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed, "{err}");
    let base = f
        .git
        .merge_base(&f.root, f.id, &h.target, &orphan, T)
        .await
        .expect("merge-base");
    assert_eq!(base, None);
}

#[tokio::test]
async fn a_trial_merge_past_its_time_limit_is_git_timeout() {
    let f = fixture().await;
    let h = histories(&f.path);
    let limits = MergeTreeLimits {
        timeout: Duration::ZERO,
        ..MergeTreeLimits::default()
    };
    let err = f
        .trial(&h.target, &h.conflicting, limits)
        .await
        .expect_err("timeout");
    assert_eq!(err.code(), ErrorCode::GitTimeout, "{err}");
}

#[test]
fn a_cut_list_drops_the_name_git_was_writing() {
    let tree = "a".repeat(40);
    let out = format!("{tree}\0one\0two\0thr");
    let got = conflicted_files(out.as_bytes(), true, 100).expect("cut");
    assert_eq!(
        got,
        MergeTree::Conflicted {
            files: vec!["one".to_owned(), "two".to_owned()],
            files_truncated: true,
        }
    );
}

#[test]
fn an_exit_one_without_the_end_of_list_is_unexpected_output() {
    let tree = "a".repeat(40);
    let out = format!("{tree}\0one\0");
    let err = conflicted_files(out.as_bytes(), false, 100).expect_err("no end");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
}
