#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

//! M4 C14: the bundle import, the tree changes, the server's conflicted
//! merge and the root entry edit, with the real git. All data synthetic.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::autofix::ChangeKind;
use forge_core::{ObjectId, RepoId};
use forge_errors::ErrorCode;

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
    let scratch = tempfile::tempdir().expect("scratch");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    let id = RepoId::new(14);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        scratch,
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
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input.as_bytes()).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A commit of `files` (flat, mode 100644) on `parents`.
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

/// What a runner does: a fresh repository with the input commit fetched
/// (as the job's fetch does), a transport commit of `files` on `parent`
/// (the input, or anything a broken runner chose), and a bundle of
/// `refs/forge/autofix ^<input>`.
fn runner_bundle(
    f: &Fixture,
    input: &ObjectId,
    parent: &ObjectId,
    files: &[(&str, &str)],
    name: &str,
) -> PathBuf {
    let runner = f.scratch.path().join(format!("runner-{name}.git"));
    plumb(&runner, &["init", "--bare", "--quiet"], None);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/autofix-input", input.as_str()],
        None,
    );
    plumb(
        &runner,
        &[
            "fetch",
            "--quiet",
            f.path.to_str().expect("utf8"),
            "refs/heads/autofix-input:refs/forge/job",
        ],
        None,
    );
    plumb(
        &f.path,
        &["update-ref", "-d", "refs/heads/autofix-input"],
        None,
    );
    let transport = commit(&runner, files, &[parent]);
    plumb(
        &runner,
        &["update-ref", "refs/forge/autofix", transport.as_str()],
        None,
    );
    let out = f.scratch.path().join(format!("{name}.bundle"));
    plumb(
        &runner,
        &[
            "bundle",
            "create",
            out.to_str().expect("utf8"),
            "refs/forge/autofix",
            &format!("^{}", input.as_str()),
        ],
        None,
    );
    out
}

fn limits() -> ImportLimits {
    ImportLimits {
        max_bytes: 1024 * 1024,
        timeout: T,
    }
}

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[test]
fn bundle_headers_are_read_strictly() {
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let v2 = format!("# v2 git bundle\n-{a} parent\n{b} refs/forge/autofix\n\nPACK");
    let h = bundle_header(v2.as_bytes()).expect("v2");
    assert_eq!(h.prerequisites, [ObjectId::parse(&a).expect("oid")]);
    assert_eq!(h.refs[0].1, "refs/forge/autofix");
    let v3 = format!("# v3 git bundle\n@object-format=sha1\n{b} refs/forge/autofix\n\n");
    assert!(bundle_header(v3.as_bytes()).is_ok());
    for (bad, why) in [
        (format!("# v9 git bundle\n{b} r\n\n"), "bundle_signature"),
        (
            format!("# v3 git bundle\n@filter=blob:none\n{b} r\n\n"),
            "bundle_capability",
        ),
        (
            format!("# v2 git bundle\n{b} r\n"),
            "bundle_header_unterminated",
        ),
        ("# v2 git bundle\nzz r\n\n".to_owned(), "bundle_ref_line"),
    ] {
        assert_eq!(bundle_header(bad.as_bytes()), Err(why), "{bad}");
    }
}

/// The runner's bundle is imported with fsck; only its tree is used.
#[tokio::test]
async fn a_runner_bundle_on_the_input_commit_yields_its_tree() {
    let f = fixture().await;
    let input = commit(&f.path, &[("src.rs", "fn  main(){}\n")], &[]);
    let bundle = runner_bundle(&f, &input, &input, &[("src.rs", "fn main() {}\n")], "ok");
    let got = f
        .git
        .import_autofix_bundle(&f.root, f.id, &bundle, &input, limits())
        .await
        .expect("import");
    // The transport commit's tree arrived in forge's repository.
    assert_eq!(got.tree, tree(&f.path, &got.transport));
    // No ref was written for it.
    let refs = plumb(&f.path, &["for-each-ref", "--format=%(refname)"], None);
    assert!(refs.is_empty(), "{refs}");
}

/// ADR-0017 §1 ④: the preconditions are exactly {input}; a bundle built
/// on another commit, or whose transport commit has another parent, is
/// refused (FG-GIT-0037 precondition_mismatch).
#[tokio::test]
async fn a_bundle_on_another_commit_is_a_precondition_mismatch() {
    let f = fixture().await;
    let input = commit(&f.path, &[("a.txt", "1\n")], &[]);
    let other = commit(&f.path, &[("a.txt", "2\n")], &[]);
    // Built on `other` (the runner fetched something else).
    let elsewhere = runner_bundle(&f, &other, &other, &[("a.txt", "3\n")], "elsewhere");
    let err = f
        .git
        .import_autofix_bundle(&f.root, f.id, &elsewhere, &input, limits())
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::AutofixCandidateInvalid);
    assert_eq!(reason(&err), "precondition_mismatch");
    // The prerequisite is the input, but two commits ride on it.
    let middle_files = [("a.txt", "x\n")];
    let runner = f.scratch.path().join("runner-two.git");
    plumb(&runner, &["init", "--bare", "--quiet"], None);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/i", input.as_str()],
        None,
    );
    plumb(
        &runner,
        &[
            "fetch",
            "--quiet",
            f.path.to_str().expect("utf8"),
            "refs/heads/i:refs/forge/job",
        ],
        None,
    );
    plumb(&f.path, &["update-ref", "-d", "refs/heads/i"], None);
    let middle = commit(&runner, &middle_files, &[&input]);
    let top = commit(&runner, &[("a.txt", "y\n")], &[&middle]);
    plumb(
        &runner,
        &["update-ref", "refs/forge/autofix", top.as_str()],
        None,
    );
    let two = f.scratch.path().join("two.bundle");
    plumb(
        &runner,
        &[
            "bundle",
            "create",
            two.to_str().expect("utf8"),
            "refs/forge/autofix",
            &format!("^{}", input.as_str()),
        ],
        None,
    );
    let err = f
        .git
        .import_autofix_bundle(&f.root, f.id, &two, &input, limits())
        .await
        .expect_err("refused");
    assert_eq!(reason(&err), "precondition_mismatch");
}

#[tokio::test]
async fn an_oversized_or_foreign_bundle_is_refused_before_git_reads_it() {
    let f = fixture().await;
    let input = commit(&f.path, &[("a.txt", "1\n")], &[]);
    let bundle = runner_bundle(&f, &input, &input, &[("a.txt", "2\n")], "big");
    let tiny = ImportLimits {
        max_bytes: 10,
        timeout: T,
    };
    let err = f
        .git
        .import_autofix_bundle(&f.root, f.id, &bundle, &input, tiny)
        .await
        .expect_err("too large");
    assert_eq!(reason(&err), "bundle_too_large");
    let junk = f.scratch.path().join("junk.bundle");
    std::fs::write(&junk, b"not a bundle\n\n").expect("write");
    let err = f
        .git
        .import_autofix_bundle(&f.root, f.id, &junk, &input, limits())
        .await
        .expect_err("junk");
    assert_eq!(reason(&err), "bundle_signature");
}

#[tokio::test]
async fn tree_changes_carry_kinds_modes_and_ids() {
    let f = fixture().await;
    let a = commit(
        &f.path,
        &[("keep.rs", "k\n"), ("gone.rs", "g\n"), ("m.rs", "1\n")],
        &[],
    );
    let b = commit(
        &f.path,
        &[("keep.rs", "k\n"), ("new.rs", "n\n"), ("m.rs", "2\n")],
        &[],
    );
    let TreeChanges::Complete(changes) = f
        .git
        .tree_changes(
            &f.root,
            f.id,
            (&tree(&f.path, &a), &tree(&f.path, &b)),
            10,
            T,
        )
        .await
        .expect("diff")
    else {
        panic!("complete");
    };
    let kinds: Vec<(&str, ChangeKind)> =
        changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("gone.rs", ChangeKind::Deleted),
            ("m.rs", ChangeKind::Modified),
            ("new.rs", ChangeKind::Added)
        ]
    );
    let gone = &changes[0];
    assert_eq!((gone.old_mode, gone.new_mode), (0o100_644, 0));
    assert!(gone.old_id.is_some() && gone.new_id.is_none());
    let blob = f
        .git
        .blob_at_most(
            &f.root,
            f.id,
            changes[1].new_id.as_ref().expect("id"),
            100,
            T,
        )
        .await
        .expect("blob");
    assert_eq!(blob.as_deref(), Some(b"2\n".as_slice()));
    let capped = f
        .git
        .blob_at_most(&f.root, f.id, changes[1].new_id.as_ref().expect("id"), 1, T)
        .await
        .expect("blob");
    assert_eq!(capped, None);
    let too_many = f
        .git
        .tree_changes(
            &f.root,
            f.id,
            (&tree(&f.path, &a), &tree(&f.path, &b)),
            2,
            T,
        )
        .await
        .expect("diff");
    assert_eq!(too_many, TreeChanges::TooMany);
}

/// ADR-0017 §1.2: the server computes the conflicted merge itself; the
/// conflicted file holds its blocks, the clean files are merged.
#[tokio::test]
async fn the_conflicted_merge_keeps_the_blocks_and_lists_the_paths() {
    let f = fixture().await;
    let base = commit(&f.path, &[("Cargo.lock", "a = 1\n"), ("x.rs", "x\n")], &[]);
    let target = commit(
        &f.path,
        &[("Cargo.lock", "a = 2\n"), ("x.rs", "x\n")],
        &[&base],
    );
    let head = commit(
        &f.path,
        &[("Cargo.lock", "a = 3\n"), ("x.rs", "y\n")],
        &[&base],
    );
    let merged = f
        .git
        .conflict_merge(&f.root, f.id, &target, &head, MergeTreeLimits::default())
        .await
        .expect("merge");
    let ConflictMerge::Conflicted {
        tree,
        files,
        files_truncated,
    } = merged
    else {
        panic!("conflicted: {merged:?}");
    };
    assert_eq!(files, ["Cargo.lock"]);
    assert!(!files_truncated);
    let lock = plumb(
        &f.path,
        &["cat-file", "-p", &format!("{}:Cargo.lock", tree.as_str())],
        None,
    );
    assert!(
        lock.contains("<<<<<<<") && lock.contains(">>>>>>>"),
        "{lock}"
    );
    let x = plumb(
        &f.path,
        &["cat-file", "-p", &format!("{}:x.rs", tree.as_str())],
        None,
    );
    assert_eq!(x, "y");
    let clean = f
        .git
        .conflict_merge(&f.root, f.id, &base, &head, MergeTreeLimits::default())
        .await
        .expect("merge");
    assert!(matches!(clean, ConflictMerge::Clean { .. }));
}

/// The input commit carries the target's `.forge-ci.yml`; the bot's tree
/// gets the base's back (or none).
#[tokio::test]
async fn a_root_entry_is_replaced_and_removed() {
    let f = fixture().await;
    let head = commit(
        &f.path,
        &[(".forge-ci.yml", "head\n"), ("a.rs", "a\n")],
        &[],
    );
    let target = commit(&f.path, &[(".forge-ci.yml", "target\n")], &[]);
    let bare = commit(&f.path, &[("a.rs", "a\n")], &[]);
    let wanted = f
        .git
        .root_entry(&f.root, f.id, &target, ".forge-ci.yml", T)
        .await
        .expect("ls")
        .expect("present");
    assert_eq!(
        (wanted.mode.as_str(), wanted.kind.as_str()),
        ("100644", "blob")
    );
    let swapped = f
        .git
        .tree_with_root_entry(
            &f.root,
            f.id,
            &tree(&f.path, &head),
            ".forge-ci.yml",
            Some(&wanted),
            T,
        )
        .await
        .expect("mktree");
    let content = plumb(
        &f.path,
        &[
            "cat-file",
            "-p",
            &format!("{}:.forge-ci.yml", swapped.as_str()),
        ],
        None,
    );
    assert_eq!(content, "target");
    let a = plumb(
        &f.path,
        &["cat-file", "-p", &format!("{}:a.rs", swapped.as_str())],
        None,
    );
    assert_eq!(a, "a");
    let removed = f
        .git
        .tree_with_root_entry(
            &f.root,
            f.id,
            &tree(&f.path, &head),
            ".forge-ci.yml",
            None,
            T,
        )
        .await
        .expect("mktree");
    assert_eq!(removed, tree(&f.path, &bare));
    assert_eq!(
        f.git
            .root_entry(&f.root, f.id, &bare, ".forge-ci.yml", T)
            .await
            .expect("ls"),
        None
    );
}
