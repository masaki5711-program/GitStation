#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::{ObjectId, RefName, RepoId};
use forge_errors::ErrorCode;

use super::*;
use crate::mergeability::{MergeTree, MergeTreeLimits};
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
    let id = RepoId::new(11);
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

fn ident(name: &str) -> CommitIdentity {
    CommitIdentity::new(
        name,
        &format!("{}@example.test", name.to_lowercase()),
        "user",
    )
    .expect("identity")
}

fn os(v: &[&str]) -> Vec<std::ffi::OsString> {
    v.iter().map(std::ffi::OsString::from).collect()
}

/// ADR-0024 §4.1: the argument vectors of the merge plumbing, pinned —
/// parents as `-p <id>`, the message from stdin (`-F -`), the tree after
/// `--end-of-options` (commit-tree parses options with parse-options since
/// before 2.38, so the separator is accepted), hooks pinned empty; and
/// `merge-tree` / `merge-base` with their revisions after the separator.
/// The commands run on the local git (2.53; every option exists in 2.38).
#[tokio::test]
async fn merge_plumbing_argv_on_git_2_38() {
    let f = fixture().await;
    let base = commit(&f.path, &[("a.txt", "1\n")], &[]);
    let head = commit(&f.path, &[("a.txt", "1\n"), ("b.txt", "2\n")], &[&base]);
    let tree = ObjectId::parse(&plumb(
        &f.path,
        &["rev-parse", &format!("{}^{{tree}}", head.as_str())],
        None,
    ))
    .expect("tree");
    let author = ident("Alice");
    let committer = ident("Bob");
    let parents = [base.clone(), head.clone()];
    let new = NewCommit {
        tree: &tree,
        parents: &parents,
        message: "Merge feature\n",
        author: &author,
        committer: &committer,
    };
    let cmd = f.git.commit_tree_command(&f.path, &new).expect("command");
    let argv = cmd.argv();
    let hooks = f.git.config().empty_hooks_dir();
    let mut expected = os(&["-c"]);
    expected.push(format!("core.hooksPath={}", hooks.display()).into());
    expected.extend(os(&[
        "commit-tree",
        "-p",
        base.as_str(),
        "-p",
        head.as_str(),
        "-F",
        "-",
        "--end-of-options",
        tree.as_str(),
    ]));
    assert_eq!(argv, expected);
    assert_eq!(
        f.git
            .merge_tree_command(&f.path, &base, &head)
            .expect("merge-tree")
            .argv(),
        os(&[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "-z",
            "--end-of-options",
            base.as_str(),
            head.as_str()
        ])
    );
    // It runs: the commit has both parents in order, the identity from the
    // environment and the message from stdin.
    let made = f
        .git
        .commit_tree(&f.root, f.id, &new, T)
        .await
        .expect("commit-tree");
    let raw = plumb(&f.path, &["cat-file", "commit", made.as_str()], None);
    assert!(
        raw.contains(&format!(
            "parent {}\nparent {}\n",
            base.as_str(),
            head.as_str()
        )),
        "{raw}"
    );
    assert!(raw.contains("author Alice <alice@example.test>"), "{raw}");
    assert!(raw.contains("committer Bob <bob@example.test>"), "{raw}");
    assert!(raw.ends_with("Merge feature"), "{raw}");
    assert!(f
        .git
        .is_ancestor(&f.root, f.id, &base, &made, T)
        .await
        .expect("ancestor"));
}

/// The three methods' results on real git (ADR-0024 §4.1): a merge commit
/// and a squash carry the trial merge's tree (parents `[target, head]` /
/// `[target]`); a fast-forward moves to the head itself and is possible only
/// when the target is its ancestor.
#[tokio::test]
async fn the_three_methods_build_the_expected_trees_and_parents() {
    let f = fixture().await;
    let base = commit(&f.path, &[("a.txt", "1\n")], &[]);
    let target = commit(&f.path, &[("a.txt", "1\n"), ("t.txt", "t\n")], &[&base]);
    let head = commit(&f.path, &[("a.txt", "1\n"), ("h.txt", "h\n")], &[&base]);
    let MergeTree::Clean { tree } = f
        .git
        .merge_tree(&f.root, f.id, &target, &head, MergeTreeLimits::default())
        .await
        .expect("trial merge")
    else {
        panic!("a clean merge");
    };
    let listing = plumb(&f.path, &["ls-tree", "--name-only", tree.as_str()], None);
    assert_eq!(listing, "a.txt\nh.txt\nt.txt");
    let (author, committer) = (ident("Alice"), ident("Bob"));
    for (parents, n) in [
        (vec![target.clone(), head.clone()], 2),
        (vec![target.clone()], 1),
    ] {
        let made = f
            .git
            .commit_tree(
                &f.root,
                f.id,
                &NewCommit {
                    tree: &tree,
                    parents: &parents,
                    message: "m",
                    author: &author,
                    committer: &committer,
                },
                T,
            )
            .await
            .expect("commit");
        let raw = plumb(&f.path, &["cat-file", "commit", made.as_str()], None);
        assert_eq!(
            raw.matches("\nparent ").count() + usize::from(raw.starts_with("parent")),
            n
        );
        assert!(raw.contains(&format!("tree {}", tree.as_str())));
        assert!(f
            .git
            .is_ancestor(&f.root, f.id, &target, &made, T)
            .await
            .expect("descendant"));
    }
    // Fast-forward: only when the target is an ancestor of the head.
    assert!(!f
        .git
        .is_ancestor(&f.root, f.id, &target, &head, T)
        .await
        .expect("diverged"));
    let ahead = commit(&f.path, &[("a.txt", "2\n")], &[&target]);
    assert!(f
        .git
        .is_ancestor(&f.root, f.id, &target, &ahead, T)
        .await
        .expect("ff"));
}

/// ADR-0024 §4.1: the identity comes from the database and is checked —
/// a newline, a NUL, `<` / `>` are refused (git would misread them), an
/// empty display name becomes the username (flagged), an empty e-mail is
/// refused. Nothing is quoted into the error.
#[test]
fn commit_identities_refuse_what_git_would_misread() {
    for (name, email) in [
        ("Al\nice", "a@example.test"),
        ("Alice", "a@exa\0mple.test"),
        ("Alice <root>", "a@example.test"),
        ("Alice", "<a@example.test>"),
        ("Alice", "  "),
    ] {
        let err = CommitIdentity::new(name, email, "alice").expect_err(name);
        assert_eq!(err.code(), ErrorCode::Internal);
        assert!(!format!("{err:?}").contains("root"), "{err:?}");
    }
    let fallback = CommitIdentity::new("   ", "a@example.test", "alice").expect("fallback");
    assert!(fallback.fell_back());
    assert_eq!(fallback.name(), "alice");
    let plain = CommitIdentity::new(" Alice A ", "a@example.test", "alice").expect("plain");
    assert!(!plain.fell_back());
    assert_eq!(plain.name(), "Alice A");
}

/// Revisions that look like options never reach argv: a "parent" or tree
/// starting with `-` is refused before git runs (object ids are hex only);
/// a ref name with a `-` component is refused by `RefName` itself, and ref
/// names reach `update-ref` on stdin only.
#[tokio::test]
async fn refs_and_ids_starting_with_a_dash_are_refused() {
    let f = fixture().await;
    assert_eq!(
        Revision::object_id("-c").expect_err("dash").code(),
        ErrorCode::InvalidRefName
    );
    assert!(RefName::parse("refs/heads/-x").is_err());
    assert!(RefName::parse("refs/heads/a/-x").is_err());
    assert!(!f
        .git
        .ref_locked(
            &f.root,
            f.id,
            &RefName::parse("refs/heads/main").expect("ref")
        )
        .expect("check"));
    std::fs::create_dir_all(f.path.join("refs/heads")).expect("dir");
    std::fs::write(f.path.join("refs/heads/main.lock"), b"").expect("lock");
    assert!(f
        .git
        .ref_locked(
            &f.root,
            f.id,
            &RefName::parse("refs/heads/main").expect("ref")
        )
        .expect("check"));
}

/// A commit-tree that runs past its timeout is FG-GIT-0002 and leaves at
/// most an unreferenced object (no ref moves here).
#[tokio::test]
async fn a_commit_tree_past_its_timeout_is_git_timeout() {
    let f = fixture().await;
    let base = commit(&f.path, &[("a.txt", "1\n")], &[]);
    let tree = ObjectId::parse(&plumb(
        &f.path,
        &["rev-parse", &format!("{}^{{tree}}", base.as_str())],
        None,
    ))
    .expect("tree");
    let who = ident("Alice");
    let err = f
        .git
        .commit_tree(
            &f.root,
            f.id,
            &NewCommit {
                tree: &tree,
                parents: std::slice::from_ref(&base),
                message: "m",
                author: &who,
                committer: &who,
            },
            Duration::from_nanos(1),
        )
        .await
        .expect_err("timeout");
    assert_eq!(err.code(), ErrorCode::GitTimeout);
}
