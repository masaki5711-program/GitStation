#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! Branch existence through the branch snapshot (plan 18 S8), against a
//! real bare repository with synthetic content: what a merge request
//! create asks before it writes anything.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::RefName;
use forge_errors::ErrorCode;

use super::tests::{blob, commit, fixture, mktree, plumb, reason, update_ref};
use super::{BrowseLimits, Deadline};

fn branch(short: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{short}")).expect("branch")
}

/// One synthetic commit.
fn seed(repo: &Path) -> String {
    let readme = blob(repo, b"# synthetic\n");
    let tree = mktree(repo, &[("100644", "blob", &readme, "README.md")]);
    commit(repo, &tree, None)
}

/// A snapshot that never expires by itself during a test: only what the
/// test does can change an answer.
fn long_lived() -> BrowseLimits {
    BrowseLimits {
        refs_ttl: Duration::from_secs(3600),
        ..BrowseLimits::default()
    }
}

/// Plan 18 S3 (`forge_refs`): a ref write of THIS process
/// (`Browser::invalidate`) is told to whoever announces it — once per
/// call, through every clone, and only to the first one registered. What
/// another process announced (`Browser::forget_refs`) drops the same
/// caches and tells no one: no echo between the processes.
#[tokio::test]
async fn only_this_process_ref_writes_are_announced() {
    let f = fixture().await;
    let c1 = seed(&f.path);
    for name in ["main", "old"] {
        update_ref(&f.path, &format!("refs/heads/{name}"), &c1);
    }
    let b = f.browser(long_lived());
    let told = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&told);
    assert!(b.on_refs_changed(move |repo| sink.lock().expect("told").push(repo)));
    assert!(
        !b.on_refs_changed(|_| panic!("the first one stays")),
        "set once"
    );

    b.require_branch(f.id, &branch("old"))
        .await
        .expect("old exists (the snapshot is read)");
    plumb(&f.path, &["update-ref", "-d", "refs/heads/old"], None);
    // Heard from another process: the snapshot is dropped, no one is told.
    b.forget_refs(f.id);
    let err = b
        .require_branch(f.id, &branch("old"))
        .await
        .expect_err("deleted");
    assert_eq!(reason(&err), "ref_not_found");
    assert!(told.lock().expect("told").is_empty(), "no echo");

    // This process wrote: told once per write, through a clone too.
    b.invalidate(f.id);
    b.clone().invalidate(f.id); // clone: shares the registration
    assert_eq!(*told.lock().expect("told"), vec![f.id, f.id]);
}

/// A branch that is not in the snapshot is asked of git before the answer
/// is "missing": one pushed after the snapshot was read is found, with no
/// invalidation at all. Afterwards it is a hit (no git).
#[tokio::test]
async fn a_branch_pushed_after_the_snapshot_is_found() {
    let f = fixture().await;
    let c1 = seed(&f.path);
    update_ref(&f.path, "refs/heads/main", &c1);
    let b = f.browser(long_lived());
    b.require_branch(f.id, &branch("main"))
        .await
        .expect("main exists (the snapshot is read)");
    let err = b
        .require_branch(f.id, &branch("feature"))
        .await
        .expect_err("not pushed yet");
    assert_eq!(err.code(), ErrorCode::RepositoryPathNotResolved);
    assert_eq!(reason(&err), "ref_not_found");

    // The push: nothing tells the cache (another process wrote it).
    update_ref(&f.path, "refs/heads/feature", &c1);
    b.require_branch(f.id, &branch("feature"))
        .await
        .expect("a branch missing from the snapshot is confirmed with git");

    let before = crate::git_spawns();
    b.require_branch(f.id, &branch("feature"))
        .await
        .expect("now in the snapshot");
    b.require_branch(f.id, &branch("main"))
        .await
        .expect("still in the snapshot");
    assert_eq!(crate::git_spawns(), before, "a hit starts no git");
}

/// A ref write by another process (the worker's merge deletes a source
/// branch, a maintenance job, a restore) reaches the cache three ways:
/// the signal (`Browser::invalidate`: `forge serve`'s own ref writes;
/// `Browser::forget_refs`: the `forge_refs` listener of plan 18 S3, for
/// the ref writes other processes announce) drops the snapshot at once;
/// with no signal a deleted branch is still answered "present" for
/// at most `refs_ttl` (the stated bound); a created branch is found at
/// once whatever the snapshot says.
#[tokio::test]
async fn another_process_ref_write_invalidates_the_cache() {
    let f = fixture().await;
    let c1 = seed(&f.path);
    for name in ["main", "old", "gone"] {
        update_ref(&f.path, &format!("refs/heads/{name}"), &c1);
    }
    let b = f.browser(long_lived());
    b.require_branch(f.id, &branch("old"))
        .await
        .expect("old exists (the snapshot is read)");

    // Another process deletes `old`. Not told: the snapshot still has it
    // (bounded by `refs_ttl`, one hour here).
    plumb(&f.path, &["update-ref", "-d", "refs/heads/old"], None);
    b.require_branch(f.id, &branch("old"))
        .await
        .expect("a stale positive within refs_ttl");
    // Told: the next answer is read from git.
    b.invalidate(f.id);
    let err = b
        .require_branch(f.id, &branch("old"))
        .await
        .expect_err("deleted");
    assert_eq!(reason(&err), "ref_not_found");

    // Another process creates `new`. Not told: found all the same.
    update_ref(&f.path, "refs/heads/new", &c1);
    b.require_branch(f.id, &branch("new"))
        .await
        .expect("a branch another process created is found with no signal");

    // Not told and never asked again: the snapshot ages out by itself.
    let short = f.browser(BrowseLimits {
        refs_ttl: Duration::from_millis(1),
        ..BrowseLimits::default()
    });
    short
        .require_branch(f.id, &branch("gone"))
        .await
        .expect("gone exists (the snapshot is read)");
    plumb(&f.path, &["update-ref", "-d", "refs/heads/gone"], None);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let err = short
        .require_branch(f.id, &branch("gone"))
        .await
        .expect_err("past refs_ttl the snapshot is read again");
    assert_eq!(reason(&err), "ref_not_found");
}

/// A create that waits for another create's read is not refused with it.
/// The create that ran the read found the repository's only slot taken
/// for `repo_wait` and is refused alone; the one that waited reads in its
/// place, on its own deadline, and is answered once the slot is free.
#[tokio::test]
async fn a_create_waiting_on_a_refused_read_reads_itself_and_is_answered() {
    let f = fixture().await;
    let c1 = seed(&f.path);
    update_ref(&f.path, "refs/heads/main", &c1);
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        timeout: Duration::from_secs(60),
        repo_wait: Duration::from_secs(5),
        ..BrowseLimits::default()
    });
    // No git while the slot is held: the waits run on tokio's paused
    // clock, independent of the machine's speed.
    tokio::time::pause();
    let held = b
        .claim(f.id, Deadline::new(Duration::from_secs(60)))
        .await
        .expect("the only slot");
    let create = || {
        let b = b.clone(); // clone: one handle per task
        let id = f.id;
        tokio::spawn(async move { b.require_branch(id, &branch("main")).await })
    };
    let first = create();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let second = create();
    tokio::task::yield_now().await;

    // 5 s (`repo_wait`): the first is refused. Only the first.
    let err = first
        .await
        .expect("task")
        .expect_err("no slot within repo_wait");
    assert_eq!(err.code(), ErrorCode::GitBusy);
    assert_eq!(reason(&err), "repo_browse");

    // The second waits for a slot itself, from now. Real time again: it
    // runs git.
    tokio::time::resume();
    drop(held);
    second
        .await
        .expect("task")
        .expect("the create that waited is answered");
    assert_eq!(b.users(f.id), 0);
}
