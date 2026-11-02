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

async fn snapshot(f: &Fixture) -> RefSnapshot {
    f.git
        .ref_snapshot(&f.root, f.id, "user:0", T)
        .await
        .expect("snapshot")
}

async fn listed(
    f: &Fixture,
    before: &RefSnapshot,
    updates: &[CommitQuery],
    cap: usize,
) -> Introduced {
    f.git
        .introduced_commits(&f.root, f.id, before, updates, "user:1", cap, T)
        .await
        .expect("listed")
}

fn ids(got: &Introduced) -> Vec<&str> {
    got.commits.iter().map(ObjectId::as_str).collect()
}

/// R5 round 4: the commits a push made reachable that no ref reached
/// before; a tag or a branch on an existing commit and a delete bring none;
/// past the cap the listing is incomplete (`reason=cap`).
#[tokio::test]
async fn a_push_introduces_only_commits_no_ref_reached_before() {
    let f = fixture().await;
    let base = commit(&f.path, "0", None);
    plumb(&f.path, &["update-ref", "refs/heads/main", &base], None);
    let b1 = commit(&f.path, "1", Some(&base));
    let b2 = commit(&f.path, "2", Some(&b1));
    // The snapshot before receive-pack, then the push of `bob` at b2.
    let before = snapshot(&f).await;
    plumb(&f.path, &["update-ref", "refs/heads/bob", &b2], None);
    let got = listed(&f, &before, &[query("refs/heads/bob", ZERO, &b2)], 100).await;
    assert!(got.complete && got.reason.is_none());
    assert_eq!(ids(&got), [b2.as_str(), b1.as_str()]);
    // Alice creates `feature` at b1 and a tag at b2: nothing new.
    let before = snapshot(&f).await;
    plumb(&f.path, &["update-ref", "refs/heads/feature", &b1], None);
    plumb(&f.path, &["update-ref", "refs/tags/v1", &b2], None);
    let alice = [
        query("refs/heads/feature", ZERO, &b1),
        query("refs/tags/v1", ZERO, &b2),
    ];
    let got = listed(&f, &before, &alice, 100).await;
    assert!(got.complete && got.commits.is_empty(), "{got:?}");
    // A fast-forward of main onto b2: nothing new either.
    let before = snapshot(&f).await;
    plumb(&f.path, &["update-ref", "refs/heads/main", &b2], None);
    let got = listed(&f, &before, &[query("refs/heads/main", &base, &b2)], 100).await;
    assert!(got.complete && got.commits.is_empty(), "{got:?}");
    // A delete: nothing.
    let before = snapshot(&f).await;
    let got = listed(&f, &before, &[query("refs/heads/feature", &b1, ZERO)], 100).await;
    assert!(got.complete && got.commits.is_empty());
    // The cap: incomplete.
    let b3 = commit(&f.path, "3", Some(&b2));
    let b4 = commit(&f.path, "4", Some(&b3));
    let before = snapshot(&f).await;
    plumb(&f.path, &["update-ref", "refs/heads/bob", &b4], None);
    let got = listed(&f, &before, &[query("refs/heads/bob", &b2, &b4)], 1).await;
    assert!(!got.complete);
    assert_eq!(got.reason, Some("cap"));
    assert_eq!(got.commits.len(), 1);
}

/// R5 review round 5 (TOCTOU): a second ref to the pushed commit created
/// between receive-pack and the listing (another pusher's concurrent push
/// of the same commit) hides nothing — both pushers get rows.
#[tokio::test]
async fn a_concurrent_second_ref_to_the_pushed_commit_hides_nothing() {
    let f = fixture().await;
    let base = commit(&f.path, "0", None);
    plumb(&f.path, &["update-ref", "refs/heads/main", &base], None);
    let b1 = commit(&f.path, "1", Some(&base));
    // Both pushes start before either applies: one snapshot each.
    let bob_before = snapshot(&f).await;
    let alice_before = snapshot(&f).await;
    plumb(&f.path, &["update-ref", "refs/heads/bob", &b1], None);
    plumb(&f.path, &["update-ref", "refs/heads/alice", &b1], None);
    let bob = listed(&f, &bob_before, &[query("refs/heads/bob", ZERO, &b1)], 100).await;
    let alice = listed(
        &f,
        &alice_before,
        &[query("refs/heads/alice", ZERO, &b1)],
        100,
    )
    .await;
    assert_eq!(ids(&bob), [b1.as_str()]);
    assert_eq!(ids(&alice), [b1.as_str()]);
}

/// R5 review round 5: a merge request's version pin (a hidden ref) written
/// right after the push still leaves the pushed commits listed.
#[tokio::test]
async fn a_version_pin_after_the_push_still_lists_its_commits() {
    let f = fixture().await;
    let base = commit(&f.path, "0", None);
    plumb(&f.path, &["update-ref", "refs/heads/main", &base], None);
    let b1 = commit(&f.path, "1", Some(&base));
    let b2 = commit(&f.path, "2", Some(&b1));
    let before = snapshot(&f).await;
    plumb(&f.path, &["update-ref", "refs/heads/bob", &b2], None);
    plumb(
        &f.path,
        &["update-ref", "refs/merge-requests/1/versions/1", &b2],
        None,
    );
    plumb(
        &f.path,
        &["update-ref", "refs/merge-requests/1/head", &b2],
        None,
    );
    let got = listed(&f, &before, &[query("refs/heads/bob", ZERO, &b2)], 100).await;
    assert!(got.complete);
    assert_eq!(ids(&got), [b2.as_str(), b1.as_str()]);
}

/// R5 review round 5: hidden refs are part of the snapshot (a commit a
/// merge request pin held before is not new), symbolic refs are not (a
/// symbolic ref resolving to the pushed branch hides nothing).
#[tokio::test]
async fn the_snapshot_reads_hidden_refs_and_skips_symbolic_refs() {
    let f = fixture().await;
    let base = commit(&f.path, "0", None);
    plumb(&f.path, &["update-ref", "refs/heads/main", &base], None);
    let pinned = commit(&f.path, "p", Some(&base));
    plumb(
        &f.path,
        &["update-ref", "refs/merge-requests/7/head", &pinned],
        None,
    );
    // An annotated tag (peeled to its commit) and a symbolic ref.
    plumb(&f.path, &["tag", "-a", "-m", "t", "v0", &base], None);
    plumb(
        &f.path,
        &["symbolic-ref", "refs/heads/alias", "refs/heads/bob"],
        None,
    );
    let before = snapshot(&f).await;
    let tips: Vec<String> = before.tip_hexes().collect();
    let mut want = vec![base.clone(), pinned.clone()];
    want.sort_unstable();
    assert_eq!(
        tips, want,
        "each commit once, sorted, the symbolic ref left out"
    );
    assert_eq!(before.len(), 2);
    // The symbolic ref dangles (bob is not there yet): git lists it not at all.
    assert_eq!(before.refs, 3, "main, the pin, the tag");
    let b1 = commit(&f.path, "1", Some(&pinned));
    plumb(&f.path, &["update-ref", "refs/heads/bob", &b1], None);
    // `alias` now resolves to b1; the snapshot never read it.
    let got = listed(&f, &before, &[query("refs/heads/bob", ZERO, &b1)], 100).await;
    assert_eq!(ids(&got), [b1.as_str()], "the pinned commit is not new");
}

#[test]
fn snapshot_lines_contribute_commits_only() {
    let c = "a".repeat(40);
    let t = "b".repeat(40);
    assert_eq!(snapshot_tip(&format!("{c} commit   ")), Some(c.as_str()));
    assert_eq!(
        snapshot_tip(&format!("{t} tag {c} commit ")),
        Some(c.as_str())
    );
    assert_eq!(snapshot_tip(&format!("{t} tag {c} tree ")), None);
    assert_eq!(snapshot_tip(&format!("{t} tree   ")), None);
    assert_eq!(
        snapshot_tip(&format!("{c} commit   refs/heads/main")),
        None,
        "symbolic"
    );
    assert_eq!(snapshot_tip("garbage"), None);
}

#[test]
fn snapshot_tips_round_trip_as_sorted_unique_bytes() {
    let mut s = RefSnapshot::default();
    let b = "b".repeat(40);
    let a = "0123456789abcdef0123456789abcdef01234567".to_owned();
    for tip in [&b, &a, &b] {
        s.add(tip);
    }
    s.seal();
    assert_eq!(s.tip_hexes().collect::<Vec<_>>(), [a, b]);
    let mut wide = RefSnapshot::default();
    let long = "fe".repeat(32);
    wide.add(&long);
    wide.seal();
    assert_eq!(wide.tip_hexes().collect::<Vec<_>>(), [long]);
}
