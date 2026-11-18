#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The branch snapshot cache (`refs_cache.rs`) against a scripted read:
//! who reads, who waits, what is kept and for how long. Every test runs
//! on tokio's paused clock and one thread: nothing depends on the
//! machine's speed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{BranchAnswer, BranchSnapshot, RefSnapshotCache};

const TTL: Duration = Duration::from_secs(5);

/// The repository as git would show it: its branches, how many reads
/// began, whether a read is held back and whether it fails.
#[derive(Clone)]
struct FakeGit {
    branches: Arc<Mutex<Vec<&'static str>>>,
    reads: Arc<AtomicUsize>,
    open: Arc<watch::Sender<bool>>,
    fail: Arc<AtomicBool>,
}

impl FakeGit {
    fn with(branches: &[&'static str]) -> Self {
        Self {
            branches: Arc::new(Mutex::new(branches.to_vec())),
            reads: Arc::new(AtomicUsize::new(0)),
            open: Arc::new(watch::channel(true).0),
            fail: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Reads from now on do not finish until [`Self::release`].
    fn hold(&self) {
        self.open.send_replace(false);
    }

    fn release(&self) {
        self.open.send_replace(true);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn set(&self, branches: &[&'static str]) {
        *self.branches.lock().unwrap() = branches.to_vec();
    }

    /// One read: the branches as they are when it begins (the oldest
    /// answer a real read could give), delivered when it is let go.
    async fn load(&self) -> Result<BranchSnapshot, ForgeError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let seen = self.branches.lock().unwrap().clone();
        let mut open = self.open.subscribe();
        open.wait_for(|open| *open).await.expect("the fake lives");
        if self.fail.load(Ordering::SeqCst) {
            return Err(ForgeError::new(ErrorCode::GitBusy)
                .with_context("reason", "repo_browse")
                .with_source(std::io::Error::other("synthetic cause")));
        }
        Ok(snapshot(&seen))
    }
}

fn snapshot(branches: &[&str]) -> BranchSnapshot {
    let out: String = branches
        .iter()
        .map(|b| format!("commit\0\0refs/heads/{b}\n"))
        .collect();
    BranchSnapshot::parse(out.as_bytes(), false, usize::MAX)
}

fn repo(n: i64) -> RepoId {
    RepoId::new(n)
}

async fn ask_in(
    cache: &RefSnapshotCache,
    git: &FakeGit,
    repo: RepoId,
    short: &str,
) -> Result<BranchAnswer, ForgeError> {
    cache
        .branch(repo, &format!("refs/heads/{short}"), || git.load())
        .await
}

async fn ask(cache: &RefSnapshotCache, git: &FakeGit, short: &str) -> BranchAnswer {
    ask_in(cache, git, repo(1), short).await.expect("answer")
}

/// The same question from its own task (a request of its own).
fn asking(
    cache: &RefSnapshotCache,
    git: &FakeGit,
    short: &'static str,
) -> JoinHandle<Result<BranchAnswer, ForgeError>> {
    let cache = cache.clone(); // clone: the task's handle
    let git = git.clone(); // clone: the task's handle
    tokio::spawn(async move { ask_in(&cache, &git, repo(1), short).await })
}

/// Let every spawned task run until it waits (one thread: a yield runs
/// them all; a few more for the wake-ups they cause).
async fn parked() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn fifty_lookups_of_one_repository_share_one_read() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main", "topic"]);
    git.hold();
    let asks: Vec<_> = (0..50).map(|_| asking(&cache, &git, "topic")).collect();
    parked().await;
    assert_eq!(git.reads(), 1, "one reads, forty-nine wait");
    git.release();
    for ask in asks {
        assert_eq!(ask.await.unwrap().unwrap(), BranchAnswer::Commit);
    }
    assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 1, "a hit reads nothing");
}

#[tokio::test(start_paused = true)]
async fn a_miss_is_answered_by_a_read_that_began_after_the_question() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 1);

    assert_eq!(ask(&cache, &git, "feature").await, BranchAnswer::Missing);
    assert_eq!(
        git.reads(),
        2,
        "\"missing\" is never said from the snapshot"
    );

    // Pushed by someone the cache never hears from.
    git.set(&["main", "feature"]);
    assert_eq!(ask(&cache, &git, "feature").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 3);
    assert_eq!(ask(&cache, &git, "feature").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 3, "now a hit");
}

#[tokio::test(start_paused = true)]
async fn concurrent_misses_share_the_confirming_read() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
    git.hold();
    let asks: Vec<_> = (0..20).map(|_| asking(&cache, &git, "nowhere")).collect();
    parked().await;
    assert_eq!(git.reads(), 2, "the first miss reads; the others wait");
    git.release();
    for ask in asks {
        assert_eq!(ask.await.unwrap().unwrap(), BranchAnswer::Missing);
    }
    // The nineteen that asked while that read ran need one that began
    // after they asked: one more, shared.
    assert_eq!(git.reads(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_leader_wakes_its_waiters() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    git.hold();
    let leader = asking(&cache, &git, "main");
    parked().await;
    assert_eq!(git.reads(), 1);
    let waiters: Vec<_> = (0..5).map(|_| asking(&cache, &git, "main")).collect();
    parked().await;
    assert_eq!(git.reads(), 1, "they wait for the leader's read");

    leader.abort();
    assert!(leader.await.unwrap_err().is_cancelled());
    parked().await;
    assert_eq!(git.reads(), 2, "one of the waiters reads instead");
    git.release();
    for waiter in waiters {
        assert_eq!(waiter.await.unwrap().unwrap(), BranchAnswer::Commit);
    }
    assert_eq!(git.reads(), 2);
    assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 2, "and the snapshot is kept");
}

#[tokio::test(start_paused = true)]
async fn a_failed_read_fails_everyone_waiting_and_is_not_kept() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    git.hold();
    git.fail.store(true, Ordering::SeqCst);
    let asks: Vec<_> = (0..5).map(|_| asking(&cache, &git, "main")).collect();
    parked().await;
    git.release();
    for ask in asks {
        let err = ask.await.unwrap().expect_err("the read failed");
        assert_eq!(err.code(), ErrorCode::GitBusy);
        assert_eq!(
            err.to_string(),
            "FG-GIT-0004 reason=repo_browse: synthetic cause",
            "the read's own error, with its cause, for the leader and every waiter"
        );
    }
    assert_eq!(git.reads(), 1);
    assert_eq!(cache.repos(), 0, "a failure is not kept");

    git.fail.store(false, Ordering::SeqCst);
    assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 2, "the next question reads again");
}

#[tokio::test(start_paused = true)]
async fn a_read_in_progress_at_an_invalidation_is_not_kept() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    git.hold();
    let first = asking(&cache, &git, "main");
    parked().await;
    cache.invalidate(repo(1));
    git.release();
    assert_eq!(first.await.unwrap().unwrap(), BranchAnswer::Commit);
    assert_eq!(cache.repos(), 0, "it may predate the write");
    assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_caller_after_an_invalidation_does_not_take_an_older_read() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main", "old"]);
    git.hold();
    let before = asking(&cache, &git, "old");
    parked().await;
    assert_eq!(git.reads(), 1);

    // The branch is deleted and the cache is told, while that read runs.
    git.set(&["main"]);
    cache.invalidate(repo(1));
    let after = asking(&cache, &git, "old");
    parked().await;
    assert_eq!(git.reads(), 1, "one read at a time: it waits for the end");
    git.release();
    assert_eq!(
        before.await.unwrap().unwrap(),
        BranchAnswer::Commit,
        "asked before the delete: its own read answers"
    );
    assert_eq!(after.await.unwrap().unwrap(), BranchAnswer::Missing);
    assert_eq!(git.reads(), 2);
}

#[tokio::test(start_paused = true)]
async fn an_invalidation_drops_the_snapshot() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main", "old"]);
    assert_eq!(ask(&cache, &git, "old").await, BranchAnswer::Commit);
    git.set(&["main"]);
    assert_eq!(
        ask(&cache, &git, "old").await,
        BranchAnswer::Commit,
        "not told: still the snapshot"
    );
    cache.invalidate(repo(1));
    assert_eq!(ask(&cache, &git, "old").await, BranchAnswer::Missing);
    assert_eq!(git.reads(), 2);
    // Another repository's snapshot is left alone.
    assert_eq!(
        ask_in(&cache, &git, repo(2), "main").await.unwrap(),
        BranchAnswer::Commit
    );
    cache.invalidate(repo(1));
    assert_eq!(
        ask_in(&cache, &git, repo(2), "main").await.unwrap(),
        BranchAnswer::Commit
    );
    assert_eq!(git.reads(), 3);
}

/// The stated bound: with no invalidation, a deleted branch is answered
/// "exists" for less than the ttl, counted from the start of the read
/// that saw it.
#[tokio::test(start_paused = true)]
async fn a_snapshot_answers_exists_for_at_most_its_ttl() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main", "old"]);
    assert_eq!(ask(&cache, &git, "old").await, BranchAnswer::Commit);
    git.set(&["main"]);

    tokio::time::advance(TTL - Duration::from_millis(1)).await;
    assert_eq!(ask(&cache, &git, "old").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 1, "inside the bound: the snapshot");

    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(ask(&cache, &git, "old").await, BranchAnswer::Missing);
    assert_eq!(git.reads(), 2, "at the bound: git");
}

#[tokio::test(start_paused = true)]
async fn a_read_slower_than_the_ttl_answers_only_who_started_it() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main", "old"]);
    git.hold();
    let leader = asking(&cache, &git, "old");
    parked().await;
    git.set(&["main"]);
    let waiter = asking(&cache, &git, "old");
    parked().await;
    tokio::time::advance(TTL).await;
    git.release();
    assert_eq!(leader.await.unwrap().unwrap(), BranchAnswer::Commit);
    assert_eq!(
        waiter.await.unwrap().unwrap(),
        BranchAnswer::Missing,
        "a read that began a ttl ago is as stale as an expired snapshot"
    );
    assert_eq!(git.reads(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_name_an_incomplete_snapshot_lacks_is_unknown() {
    let out = b"commit\0\0refs/heads/a\ncommit\0\0refs/heads/b\ncommit\0\0refs/heads/c\n";
    let capped = BranchSnapshot::parse(out, false, 2);
    assert_eq!(capped.len(), 2);
    assert!(!capped.is_complete());
    assert_eq!(capped.answer("refs/heads/a"), BranchAnswer::Commit);
    assert_eq!(capped.answer("refs/heads/c"), BranchAnswer::Unknown);

    // From the cache: said at once (the caller asks git for that name),
    // with no second read of the same incomplete list.
    let cache = RefSnapshotCache::new(TTL, 16);
    let reads = AtomicUsize::new(0);
    let load = || async {
        reads.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ForgeError>(BranchSnapshot::parse(out, false, 2))
    };
    for _ in 0..2 {
        let answer = cache.branch(repo(1), "refs/heads/c", load).await.unwrap();
        assert_eq!(answer, BranchAnswer::Unknown);
    }
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn a_snapshot_reads_git_s_lines() {
    let out = b"commit\0\0refs/heads/main\n\
tag\0commit\0refs/heads/tagged\n\
tree\0\0refs/heads/a-tree\n\
tag\0blob\0refs/heads/tag-of-blob\n\
\xff\xfe\0\0refs/heads/not-utf8\n\
only-two\0fields\n\
commit\0\0refs/heads/cut-off-in-the-mid";
    let whole = BranchSnapshot::parse(out, false, usize::MAX);
    assert!(whole.is_complete());
    assert_eq!(whole.answer("refs/heads/main"), BranchAnswer::Commit);
    assert_eq!(whole.answer("refs/heads/tagged"), BranchAnswer::Commit);
    assert_eq!(whole.answer("refs/heads/a-tree"), BranchAnswer::NotACommit);
    assert_eq!(
        whole.answer("refs/heads/tag-of-blob"),
        BranchAnswer::NotACommit
    );
    assert_eq!(whole.answer("refs/heads/not-utf8"), BranchAnswer::Missing);
    assert_eq!(whole.answer("refs/heads/nowhere"), BranchAnswer::Missing);
    assert_eq!(
        whole.answer("main"),
        BranchAnswer::Missing,
        "full names only"
    );

    // Truncated output: the last line may be half a name, and is dropped.
    let cut = BranchSnapshot::parse(out, true, usize::MAX);
    assert!(!cut.is_complete());
    assert_eq!(cut.len(), 4);
    assert_eq!(cut.answer("refs/heads/main"), BranchAnswer::Commit);
    assert_eq!(
        cut.answer("refs/heads/cut-off-in-the-mid"),
        BranchAnswer::Unknown
    );
    assert_eq!(cut.answer("refs/heads/nowhere"), BranchAnswer::Unknown);

    let empty = BranchSnapshot::parse(b"", false, usize::MAX);
    assert!(empty.is_empty());
    assert!(empty.is_complete());
    assert_eq!(empty.answer("refs/heads/main"), BranchAnswer::Missing);
}

#[tokio::test(start_paused = true)]
async fn at_most_max_repos_snapshots_are_kept() {
    let cache = RefSnapshotCache::new(TTL, 2);
    let git = FakeGit::with(&["main"]);
    for n in 1..=3 {
        ask_in(&cache, &git, repo(n), "main").await.unwrap();
        tokio::time::advance(Duration::from_millis(10)).await;
    }
    assert_eq!(cache.repos(), 2);
    assert_eq!(git.reads(), 3);
    ask_in(&cache, &git, repo(3), "main").await.unwrap();
    ask_in(&cache, &git, repo(2), "main").await.unwrap();
    assert_eq!(git.reads(), 3, "the two newest are kept");
    ask_in(&cache, &git, repo(1), "main").await.unwrap();
    assert_eq!(git.reads(), 4, "the oldest was dropped");
    assert_eq!(cache.repos(), 2);
}
