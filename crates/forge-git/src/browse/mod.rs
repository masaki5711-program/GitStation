//! Repository browsing (M2 MR7, ADR-0015 "ref とパスの境目" and Amendment
//! M2, plan 10 §5 "git の実行"): the refs, how a ref (and a path) is
//! resolved, a tree, a blob, a raw file and an archive — for the browse API
//! (`RepoBrowse`) and the content routes `raw` / `archive` (`RepoRead`).
//! Every caller has already passed `authorize()`; nothing here decides who
//! may read.
//!
//! - **Arguments**: every subcommand is an argument vector built by
//!   [`crate::GitCommand`]. Revisions reach git only resolved — a full
//!   commit id or a validated full ref name, after `--end-of-options`;
//!   paths only after `--` (and `GIT_LITERAL_PATHSPECS=1`); blob ids only
//!   as resolved ids. A user's ref or path never reaches an option
//!   position: `-n`, `--output=x` are names that do not exist.
//! - **Resolution** (ADR-0015 order): (1) a 40 / 64-hex commit id, (2) the
//!   longest branch, (3) the longest tag — by kind before length. git
//!   keeps `a` and `a/b` from coexisting in one kind (D/F conflict), so at
//!   most one ref of a kind matches. Only `refs/heads/` and `refs/tags/`.
//!   A name that matches nothing is `FG-REPO-0023` (`reason`).
//! - **Timeouts**: one deadline per operation, [`BrowseLimits::timeout`]
//!   (10 s), shared by its git calls (`FG-GIT-0002 operation=browse`).
//!   Archives and large raw files stream (idle 60 s, total 10 min) through
//!   [`crate::GitStream`]: a client that goes away kills git's process
//!   tree.
//! - **Reachability** (M3 P6, `reach.rs`): a commit named by id resolves
//!   only when a branch or a tag reaches it; anything else an id lookup
//!   finds is answered like an id that does not exist (`FG-REPO-0023
//!   reason=ref_not_found`). Every id lookup counts against its caller
//!   (token bucket and concurrent slots, `FG-GIT-0004`).
//! - **Caps**: tree entries and blob bytes are truncated at their limit
//!   (git is killed at the cap, `truncated: true`), never buffered whole;
//!   at most [`BrowseLimits::max_per_repo`] captured browse calls run per
//!   repository at once. More wait in a bounded FIFO queue
//!   ([`BrowseLimits::max_repo_waiters`]) for at most
//!   [`BrowseLimits::repo_wait`] (never past the operation's deadline);
//!   a full queue or a wait that ran out is `FG-GIT-0004
//!   reason=repo_browse` (`queue=full|timeout`; load test F1,
//!   2026-11-17). The global subprocess and stream pools still apply.
//! - **History** (M2 MR8, `history.rs` / `diff.rs` / `blame.rs`): commits
//!   (`log -z`, a cursor that is checked against the commit it names), one
//!   commit and a comparison (`diff-tree` with per-file patches under
//!   file / byte caps) and blame (`blame --porcelain`, 20 s). Blame and
//!   compare have their own time limits; every captured call is one slot
//!   of the repository's cap, like the rest.
//! - **Ref cache**: the `for-each-ref` result is kept per repository for
//!   [`BrowseLimits::refs_ttl`] (5 s) and dropped by
//!   [`Browser::invalidate`] (every push). A list loaded while an
//!   invalidation happened is not stored.
//! - **Branch snapshot** (plan 18 S8, `branches.rs`,
//!   [`crate::refs_cache`]): "does this branch exist" — what a merge
//!   request create asks — is answered per repository from one shared
//!   `for-each-ref refs/heads/` (one browse slot for any number of
//!   concurrent askers), kept for `refs_ttl` and dropped by
//!   [`Browser::invalidate`]. "Exists" comes from the snapshot; "missing"
//!   only from a read that began after the question. Names only: no
//!   commit id is ever answered from it.
//! - **Object reads** (`objects.rs`): trees, blobs and commit headers are
//!   read through the repository's long-lived `cat-file --batch-command`
//!   process ([`crate::batch`], no git start per read) and what is
//!   immutable by id is cached; any read it cannot answer runs the
//!   one-shot command above (a failed exchange logged once,
//!   FG-GIT-0032). Refs never go through it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use tokio::time::Instant;

use crate::batch::{BatchConfig, CatFilePool};
use crate::command::Git;
use crate::gate::{GatePermit, KeyedGate};
use crate::refs_cache::RefSnapshotCache;
use crate::storage::StorageRoot;

mod blame;
mod branches;
mod content;
mod diff;
mod history;
mod objects;
mod path;
mod reach;
mod refs;
mod scan;
mod tree;

pub use blame::{Blame, BlameCommit, BlameHunk};
pub use content::{ArchiveFormat, Blob, RawBlob, RawBody, SmallFile};
pub use diff::{Diff, DiffFile, DiffStatus, PatchOmitted};
pub use history::{CommitDetail, CommitInfo, CommitPage, Comparison, LogCursor};
pub use path::RepoPath;
pub use reach::ReachRoots;
pub use refs::{RefEntry, RefKind, RefList, RefLookup, ResolvedRev, ResolvedSpec, Target};
pub use scan::{BlobListing, GrepHit, GrepHits, ListedBlob};
pub use tree::{EntryKind, Tree, TreeEntry};

/// The browse limits (plan 10 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowseLimits {
    /// One browse operation (all its git calls together).
    pub timeout: Duration,
    /// Captured browse calls at once per repository.
    pub max_per_repo: usize,
    /// How long a call over `max_per_repo` waits for a slot (never past
    /// its operation's deadline); zero: refused at once.
    pub repo_wait: Duration,
    /// Calls that may wait for a repository's slots at once; one more is
    /// refused at once (`queue=full`).
    pub max_repo_waiters: usize,
    /// Refs kept by one listing; more marks the list `truncated` (and
    /// resolution then asks git directly).
    pub max_refs: usize,
    /// How long a repository's ref list is reused.
    pub refs_ttl: Duration,
    /// Repositories whose ref list is cached at once.
    pub max_cached_repos: usize,
    /// Entries of one tree listing.
    pub max_tree_entries: usize,
    /// Bytes of one blob the API returns.
    pub max_blob_bytes: usize,
    /// A raw file up to this size is read whole (its type is sniffed);
    /// a larger one streams as `application/octet-stream`.
    pub raw_inline_max_bytes: u64,
    /// Streams (archive, large raw): no byte for this long kills git.
    pub stream_idle: Duration,
    /// Streams: upper bound.
    pub stream_total: Duration,
    /// One blame (all its git calls together; plan 10 §5).
    pub blame_timeout: Duration,
    /// One compare (its commit list and diff together).
    pub compare_timeout: Duration,
    /// Commits of one history page (`limit` is clamped to this).
    pub max_log_page: usize,
    /// How many commits a history cursor may have skipped.
    pub max_log_skip: usize,
    /// Commits a compare lists; more marks it `truncated`.
    pub max_compare_commits: usize,
    /// Files of one diff; more marks it `truncated`.
    pub max_diff_files: usize,
    /// Patch bytes git may produce for one diff (the sum over its files);
    /// git is killed past it and the files after are `total_cap`.
    pub max_diff_bytes: usize,
    /// Patch bytes of one file; a larger patch is left out (`too_large`).
    pub max_file_patch_bytes: usize,
    /// Lines of one blame; more marks it `truncated`.
    pub max_blame_lines: usize,
    /// One reachability check of a commit named by id (`reach.rs`; never
    /// longer than what is left of the operation's deadline).
    pub reach_timeout: Duration,
    /// Reachability answers kept at once (0: none kept).
    pub max_cached_reach: usize,
    /// Commit-id lookups one caller runs at once (`reach.rs`; ADR-0024
    /// #37). More wait up to `reach_caller_wait`, then `FG-GIT-0004
    /// reason=caller_reach`.
    pub reach_per_caller: usize,
    /// How long a caller's lookup waits for one of its slots.
    pub reach_caller_wait: Duration,
    /// Commit-id lookups one caller may make in a burst (its token
    /// bucket); an empty bucket is `FG-GIT-0004 reason=caller_reach_rate`.
    pub reach_caller_burst: u32,
    /// One token comes back per this interval.
    pub reach_caller_refill: Duration,
    /// Long-lived `cat-file --batch-command` processes per repository
    /// (`objects.rs`, [`crate::batch`]); 0: none, every object read is a
    /// one-shot git.
    pub batch_per_repo: usize,
    /// Repositories with such processes at once.
    pub batch_max_repos: usize,
    /// A process unused this long is closed.
    pub batch_idle: Duration,
    /// One exchange with a process (never past the operation's deadline);
    /// past it the process is killed and the one-shot command answers
    /// (FG-GIT-0032).
    pub batch_timeout: Duration,
    /// How long a read waits for a busy repository's process before it
    /// runs the one-shot command instead.
    pub batch_wait: Duration,
    /// Bytes of the cache of what is immutable by object id (trees,
    /// listings, a commit's root tree, history pages of a commit id).
    pub object_cache_bytes: usize,
}

impl Default for BrowseLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_per_repo: 8,
            repo_wait: Duration::from_secs(5),
            max_repo_waiters: 64,
            max_refs: 5_000,
            refs_ttl: Duration::from_secs(5),
            max_cached_repos: 256,
            max_tree_entries: 5_000,
            max_blob_bytes: 1024 * 1024,
            raw_inline_max_bytes: 4 * 1024 * 1024,
            stream_idle: Duration::from_secs(60),
            stream_total: Duration::from_secs(600),
            blame_timeout: Duration::from_secs(20),
            compare_timeout: Duration::from_secs(20),
            max_log_page: 100,
            max_log_skip: 100_000,
            max_compare_commits: 200,
            max_diff_files: 500,
            max_diff_bytes: 2 * 1024 * 1024,
            max_file_patch_bytes: 256 * 1024,
            max_blame_lines: 10_000,
            reach_timeout: Duration::from_secs(5),
            max_cached_reach: 4_096,
            reach_per_caller: 2,
            reach_caller_wait: Duration::from_secs(2),
            reach_caller_burst: 60,
            reach_caller_refill: Duration::from_millis(250),
            batch_per_repo: 2,
            batch_max_repos: 32,
            batch_idle: Duration::from_secs(60),
            batch_timeout: Duration::from_secs(2),
            batch_wait: Duration::from_millis(50),
            object_cache_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Bytes of `ls-tree` output read at most (the entry cap decides first
/// on any real tree; this bounds pathological names).
const TREE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Bytes of `for-each-ref` output read at most.
const REFS_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// `FG-REPO-0023` with its `reason`.
fn not_resolved(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RepositoryPathNotResolved).with_context("reason", reason)
}

// ── the browser ────────────────────────────────────────────────────────

/// The browse handle: git, the storage root, the limits, the per-repository
/// slots and the ref cache. Cheap to clone (shared state).
#[derive(Debug, Clone)]
pub struct Browser {
    git: Git,
    storage: StorageRoot,
    limits: BrowseLimits,
    slots: KeyedGate<RepoId>,
    cache: Arc<RefCache>,
    /// Which branches exist (`branches.rs`).
    branches: RefSnapshotCache,
    reach: Arc<reach::ReachCache>,
    /// `None` when `batch_per_repo` is 0.
    batch: Option<CatFilePool>,
    objects: Arc<objects::ObjectCache>,
    /// Who hears of this process's ref writes (`forge_refs`).
    announce: Arc<Announce>,
}

/// What [`Browser::on_refs_changed`] registered.
type Told = Box<dyn Fn(RepoId) + Send + Sync>;

/// Who is told about every [`Browser::invalidate`] — the ref writes of
/// THIS process — so that the process can announce them to the others
/// (`forge_refs`, plan 18 S3; this crate knows no database). Set once and
/// shared by every clone of the browser.
#[derive(Default)]
struct Announce(OnceLock<Told>);

impl std::fmt::Debug for Announce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Announce")
            .field("set", &self.0.get().is_some())
            .finish()
    }
}

#[derive(Debug, Default)]
struct RefCache {
    entries: Mutex<HashMap<RepoId, Cached>>,
    /// Per repository, bumped by every invalidation of that repository: a
    /// load (ref list, reachability answer) that saw another value at its
    /// start does not store its result (it may predate a push). One `u64`
    /// per repository ever invalidated; a push to one repository leaves
    /// every other repository's caches alone.
    epochs: Mutex<HashMap<RepoId, u64>>,
}

#[derive(Debug, Clone)]
struct Cached {
    at: Instant,
    list: Arc<RefList>,
}

/// One captured browse call's place in its repository's cap; released on
/// drop.
type RepoSlot = GatePermit<RepoId>;

/// The deadline of one operation.
#[derive(Debug, Clone, Copy)]
struct Deadline {
    at: Instant,
    total: Duration,
}

impl Deadline {
    fn new(total: Duration) -> Self {
        Self {
            at: Instant::now() + total,
            total,
        }
    }

    /// The time left, or `FG-GIT-0002 operation=browse`.
    fn left(&self) -> Result<Duration, ForgeError> {
        let rest = self.at.saturating_duration_since(Instant::now());
        if rest.is_zero() {
            return Err(self.expired());
        }
        Ok(rest)
    }

    /// `FG-GIT-0002 operation=browse`: the operation ran out of time.
    fn expired(&self) -> ForgeError {
        ForgeError::new(ErrorCode::GitTimeout)
            .with_context("operation", "browse")
            .with_context("timeout_ms", self.total.as_millis())
    }
}

/// A full commit id in any case, lower-cased (`None`: not 40 / 64 hex).
fn object_id(raw: &str) -> Option<String> {
    (matches!(raw.len(), 40 | 64) && raw.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| raw.to_ascii_lowercase())
}

/// How long [`release_repository`] waits for the processes it closes.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// Before a repository's files are removed or rewritten (a purge, a gc, a
/// restore): close every long-lived `cat-file` process **this process**
/// runs for it and wait — at most [`RELEASE_TIMEOUT`] — until each has
/// exited (on Windows a file another process holds open can be neither
/// deleted nor renamed), and drop its cached objects (a restored
/// repository never answers from an older copy). Blocking (short sleeps):
/// from async code use [`release_repository_async`]. `false`: a process
/// was still running at the deadline (logged once, FG-GIT-0032
/// `reason=release_timeout`); the caller goes on — its own step reports
/// a file still in use. Another process's batch processes (a `forge
/// serve` while the worker purges) close after
/// [`BrowseLimits::batch_idle`] unused.
pub fn release_repository(storage: &StorageRoot, repo: RepoId) -> bool {
    objects::forget_everywhere(repo);
    let closed = crate::batch::close_everywhere(&storage.path_for(repo), RELEASE_TIMEOUT);
    if !closed {
        release_timed_out(repo);
    }
    closed
}

/// [`release_repository`] on the blocking pool.
pub async fn release_repository_async(storage: &StorageRoot, repo: RepoId) -> bool {
    objects::forget_everywhere(repo);
    let closed =
        crate::batch::close_everywhere_async(&storage.path_for(repo), RELEASE_TIMEOUT).await;
    if !closed {
        release_timed_out(repo);
    }
    closed
}

fn release_timed_out(repo: RepoId) {
    tracing::warn!(
        target: "forge_git::batch",
        error_code = ErrorCode::GitBatchFailed.id(),
        reason = "release_timeout",
        repo_id = repo.get(),
        timeout_ms = RELEASE_TIMEOUT.as_millis(),
        "forge.git.batch.release_timeout"
    );
}

impl Browser {
    pub fn new(git: Git, storage: StorageRoot, limits: BrowseLimits) -> Self {
        let batch = (limits.batch_per_repo > 0).then(|| {
            CatFilePool::new(
                git.clone(), // clone: a shared handle
                BatchConfig {
                    per_repo: limits.batch_per_repo,
                    max_repos: limits.batch_max_repos,
                    idle: limits.batch_idle,
                    wait: limits.batch_wait,
                },
            )
        });
        Self {
            batch,
            objects: objects::ObjectCache::new(limits.object_cache_bytes),
            git,
            storage,
            limits,
            slots: KeyedGate::new(limits.max_per_repo, limits.max_repo_waiters),
            cache: Arc::new(RefCache::default()),
            branches: RefSnapshotCache::new(limits.refs_ttl, limits.max_cached_repos),
            reach: Arc::new(reach::ReachCache::default()),
            announce: Arc::default(),
        }
    }

    pub fn limits(&self) -> &BrowseLimits {
        &self.limits
    }

    /// THIS process changed `repo`'s refs: drop what is cached about them
    /// ([`Browser::forget_refs`]) and tell whoever announces this
    /// process's ref writes ([`Browser::on_refs_changed`]). Every ref
    /// writer calls this after its write — and nothing else does: to read
    /// fresh refs, or for a ref write of another process, call
    /// [`Browser::forget_refs`].
    pub fn invalidate(&self, repo: RepoId) {
        self.forget_refs(repo);
        if let Some(told) = self.announce.0.get() {
            told(repo);
        }
    }

    /// Have `told` called with the repository of every
    /// [`Browser::invalidate`] of this browser and its clones (plan 18
    /// S3: `forge serve` sends `forge_refs` from it, so the other
    /// processes drop their caches too). `told` runs on the writer's
    /// path, also inside a `Drop`: it must not block. Set once — `false`
    /// when one was set before (that one stays).
    pub fn on_refs_changed(&self, told: impl Fn(RepoId) + Send + Sync + 'static) -> bool {
        self.announce.0.set(Box::new(told)).is_ok()
    }

    /// Drop `repo`'s cached ref list, branch snapshot and reachability
    /// answers (its refs changed; the epoch makes every older answer
    /// stale) and tell no one. What the `forge_refs` listener calls for a
    /// ref write another process announced
    /// (`forge_db::wake::Channel::Refs`, payload: the repository id) —
    /// announcing it again would echo between the processes for ever —
    /// and what a reader calls before a read that must not be older than
    /// a write it knows of. A ref write nobody announced is seen after at
    /// most `refs_ttl`.
    pub fn forget_refs(&self, repo: RepoId) {
        {
            let mut epochs = self
                .cache
                .epochs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let epoch = epochs.entry(repo).or_insert(0);
            *epoch = epoch.wrapping_add(1);
        }
        {
            let mut entries = self
                .cache
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            entries.remove(&repo);
        }
        self.branches.invalidate(repo);
        self.reach.forget(repo);
    }

    /// `repo`'s cache generation (see [`RefCache`]).
    fn epoch(&self, repo: RepoId) -> u64 {
        let epochs = self
            .cache
            .epochs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        epochs.get(&repo).copied().unwrap_or(0)
    }

    /// One of `repo`'s browse slots: at once when one is free, else after
    /// waiting in the repository's queue (see the module docs).
    async fn claim(&self, repo: RepoId, deadline: Deadline) -> Result<RepoSlot, ForgeError> {
        let wait = deadline.left()?.min(self.limits.repo_wait);
        self.slots.acquire(&repo, wait).await.map_err(|refusal| {
            refusal.into_error(
                ForgeError::new(ErrorCode::GitBusy)
                    .with_context("reason", "repo_browse")
                    .with_context("repo_id", repo.get())
                    .with_context("max_per_repo", self.limits.max_per_repo),
                self.limits.max_repo_waiters,
            )
        })
    }

    /// Slots of `repo` taken now (tests).
    #[cfg(test)]
    fn held(&self, repo: RepoId) -> usize {
        self.slots.held(&repo)
    }

    /// Calls holding or waiting for a slot of `repo` (tests).
    #[cfg(test)]
    fn users(&self, repo: RepoId) -> usize {
        self.slots.users(&repo)
    }
}

#[cfg(test)]
mod history_tests;

#[cfg(test)]
mod reach_tests;

#[cfg(test)]
use forge_core::RefName;
#[cfg(test)]
use refs::parse_refs;
#[cfg(test)]
use tree::parse_tree;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod scan_tests;

#[cfg(test)]
mod slot_tests;

#[cfg(test)]
mod branch_tests;

#[cfg(test)]
mod bench_tests;

#[cfg(test)]
mod objects_tests;
