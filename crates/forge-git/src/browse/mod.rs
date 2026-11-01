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
//! - **Caps**: tree entries and blob bytes are truncated at their limit
//!   (git is killed at the cap, `truncated: true`), never buffered whole;
//!   at most [`BrowseLimits::max_per_repo`] captured browse calls run per
//!   repository at once (more are `FG-GIT-0004 reason=repo_browse`, at
//!   once); the global subprocess and stream pools still apply.
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use tokio::time::Instant;

use crate::command::Git;
use crate::storage::StorageRoot;

mod blame;
mod content;
mod diff;
mod history;
mod path;
mod refs;
mod tree;

pub use blame::{Blame, BlameCommit, BlameHunk};
pub use content::{ArchiveFormat, Blob, RawBlob, RawBody};
pub use diff::{Diff, DiffFile, DiffStatus, PatchOmitted};
pub use history::{CommitDetail, CommitInfo, CommitPage, Comparison, LogCursor};
pub use path::RepoPath;
pub use refs::{RefEntry, RefKind, RefList, ResolvedRev, ResolvedSpec, Target};
pub use tree::{EntryKind, Tree, TreeEntry};

/// The browse limits (plan 10 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowseLimits {
    /// One browse operation (all its git calls together).
    pub timeout: Duration,
    /// Captured browse calls at once per repository.
    pub max_per_repo: usize,
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
}

impl Default for BrowseLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_per_repo: 8,
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
    slots: Arc<Mutex<HashMap<RepoId, usize>>>,
    cache: Arc<RefCache>,
}

#[derive(Debug, Default)]
struct RefCache {
    entries: Mutex<HashMap<RepoId, Cached>>,
    /// Bumped by every invalidation: a load that saw another value at its
    /// start does not store its list (it may predate a push).
    epoch: AtomicU64,
}

#[derive(Debug, Clone)]
struct Cached {
    at: Instant,
    list: Arc<RefList>,
}

/// One captured browse call's place in its repository's cap; released on
/// drop.
#[derive(Debug)]
struct RepoSlot {
    map: Arc<Mutex<HashMap<RepoId, usize>>>,
    repo: RepoId,
}

impl Drop for RepoSlot {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = map.get_mut(&self.repo) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.repo);
            }
        }
    }
}

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
            return Err(ForgeError::new(ErrorCode::GitTimeout)
                .with_context("operation", "browse")
                .with_context("timeout_ms", self.total.as_millis()));
        }
        Ok(rest)
    }
}

/// A full commit id in any case, lower-cased (`None`: not 40 / 64 hex).
fn object_id(raw: &str) -> Option<String> {
    (matches!(raw.len(), 40 | 64) && raw.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| raw.to_ascii_lowercase())
}

impl Browser {
    pub fn new(git: Git, storage: StorageRoot, limits: BrowseLimits) -> Self {
        Self {
            git,
            storage,
            limits,
            slots: Arc::new(Mutex::new(HashMap::new())),
            cache: Arc::new(RefCache::default()),
        }
    }

    pub fn limits(&self) -> &BrowseLimits {
        &self.limits
    }

    /// Drop `repo`'s cached ref list (a push changed its refs).
    pub fn invalidate(&self, repo: RepoId) {
        self.cache.epoch.fetch_add(1, Ordering::SeqCst);
        let mut entries = self
            .cache
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        entries.remove(&repo);
    }

    fn claim(&self, repo: RepoId) -> Result<RepoSlot, ForgeError> {
        let mut map = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let n = map.entry(repo).or_insert(0);
        if *n >= self.limits.max_per_repo.max(1) {
            return Err(ForgeError::new(ErrorCode::GitBusy)
                .with_context("reason", "repo_browse")
                .with_context("repo_id", repo.get())
                .with_context("max_per_repo", self.limits.max_per_repo));
        }
        *n += 1;
        Ok(RepoSlot {
            map: Arc::clone(&self.slots),
            repo,
        })
    }
}

#[cfg(test)]
mod history_tests;

#[cfg(test)]
use forge_core::RefName;
#[cfg(test)]
use refs::parse_refs;
#[cfg(test)]
use tree::parse_tree;

#[cfg(test)]
mod tests;
