#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::{ObjectId, RefName, RepoId};

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

fn commit(repo: &Path, content: &str, parent: Option<&str>) -> String {
    let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(content));
    let tree = plumb(repo, &["mktree"], Some(&format!("100644 blob {blob}\tf\n")));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "c"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn query(name: &str, old: &str, new: &str) -> CommitQuery {
    CommitQuery {
        name: RefName::parse(name).expect("ref"),
        old: old.to_owned(),
        new: new.to_owned(),
    }
}

const ZERO: &str = "0000000000000000000000000000000000000000";

/// R5 round 4: the commits a push made reachable that no ref reached
/// before; a tag or a branch on an existing commit and a delete bring none;
/// past the cap the listing is incomplete.
#[tokio::test]
async fn a_push_introduces_only_commits_no_ref_reached_before() {
    let f = fixture().await;
    let base = commit(&f.path, "0", None);
    plumb(&f.path, &["update-ref", "refs/heads/main", &base], None);
    let b1 = commit(&f.path, "1", Some(&base));
    let b2 = commit(&f.path, "2", Some(&b1));
    // The push of `bob` at b2 has been applied (refs move before the record).
    plumb(&f.path, &["update-ref", "refs/heads/bob", &b2], None);
    let bob = [query("refs/heads/bob", ZERO, &b2)];
    let got = f
        .git
        .introduced_commits(&f.root, f.id, &bob, "user:1", 100, T)
        .await
        .expect("listed");
    assert!(got.complete);
    let ids: Vec<&str> = got.commits.iter().map(ObjectId::as_str).collect();
    assert_eq!(ids, [b2.as_str(), b1.as_str()]);
    // Alice creates `feature` at b1 and a tag at b2: nothing new.
    plumb(&f.path, &["update-ref", "refs/heads/feature", &b1], None);
    plumb(&f.path, &["update-ref", "refs/tags/v1", &b2], None);
    let alice = [
        query("refs/heads/feature", ZERO, &b1),
        query("refs/tags/v1", ZERO, &b2),
    ];
    let got = f
        .git
        .introduced_commits(&f.root, f.id, &alice, "user:2", 100, T)
        .await
        .expect("listed");
    assert!(got.complete && got.commits.is_empty(), "{got:?}");
    // A fast-forward of main onto b2: nothing new either.
    plumb(&f.path, &["update-ref", "refs/heads/main", &b2], None);
    let ff = [query("refs/heads/main", &base, &b2)];
    let got = f
        .git
        .introduced_commits(&f.root, f.id, &ff, "user:2", 100, T)
        .await
        .expect("listed");
    assert!(got.complete && got.commits.is_empty(), "{got:?}");
    // A delete: nothing.
    let del = [query("refs/heads/feature", &b1, ZERO)];
    let got = f
        .git
        .introduced_commits(&f.root, f.id, &del, "user:2", 100, T)
        .await
        .expect("listed");
    assert!(got.complete && got.commits.is_empty());
    // The cap: incomplete.
    let b3 = commit(&f.path, "3", Some(&b2));
    let b4 = commit(&f.path, "4", Some(&b3));
    plumb(&f.path, &["update-ref", "refs/heads/bob", &b4], None);
    let more = [query("refs/heads/bob", &b2, &b4)];
    let got = f
        .git
        .introduced_commits(&f.root, f.id, &more, "user:1", 1, T)
        .await
        .expect("listed");
    assert!(!got.complete);
    assert_eq!(got.commits.len(), 1);
}
