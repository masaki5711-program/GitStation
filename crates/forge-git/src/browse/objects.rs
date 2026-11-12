//! The fast object read path of the browser (see [`super`] and
//! [`crate::batch`]): trees, blobs and commit headers read through the
//! repository's long-lived `cat-file --batch-command` process, and an
//! in-memory cache of what is immutable by object id.
//!
//! - **Same answers**: a tree object is parsed here (`<mode> <name>\0<id>`)
//!   with git's own rules — modes canonicalised like `tree-walk.c`
//!   (`canon_mode`), the type from the mode, entries in the tree's order,
//!   blob sizes from `info` (`None` for a tree, a submodule, or a blob git
//!   does not have, as `ls-tree --long` prints `-` / `BAD`) — so a listing
//!   is exactly what `ls-tree -z --long` gives.
//! - **Fallback**: every function here returns `None` when it cannot
//!   answer (no process, a failed exchange — logged once by
//!   [`Browser::batch_failed`] — a missing or oversized object, a tree it
//!   cannot parse, the deadline); the caller then runs the one-shot
//!   command, which gives the canonical answer or error.
//! - **Cache**: per `(repository, object id)` — trees parsed, listings
//!   with sizes, a commit's root tree, and history pages of a commit id
//!   (a page of `log <commit> -- <path>` never changes: replace refs are
//!   off, `%an` is not mailmapped). Bounded by
//!   [`super::BrowseLimits::object_cache_bytes`], least recently used
//!   first. Refs are never cached here.

use std::collections::{BTreeMap, HashMap};
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use forge_core::RepoId;

use super::path::RepoPath;
use super::tree::{log_skipped, EntryKind, ParsedTree, TreeEntry};
use super::{Browser, Deadline};
use crate::batch::{BatchError, Contents, Info};

/// A tree object larger than this is listed by `ls-tree` (it would be
/// read whole here).
const MAX_RAW_TREE_BYTES: u64 = 4 * 1024 * 1024;

/// A commit object larger than this is not read here (its header is all
/// that is needed; a real commit is far smaller).
const MAX_COMMIT_BYTES: u64 = 1024 * 1024;

/// A history page larger than this is not cached.
const MAX_CACHED_LOG_BYTES: usize = 1024 * 1024;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// One entry of a tree object, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawEntry {
    /// Canonical mode (see [`canon_mode`]).
    pub(super) mode: u32,
    pub(super) name: Vec<u8>,
    pub(super) oid: String,
}

impl RawEntry {
    fn kind(&self) -> EntryKind {
        match self.mode {
            0o040_000 => EntryKind::Tree,
            0o160_000 => EntryKind::Submodule,
            0o120_000 => EntryKind::Symlink,
            _ => EntryKind::Blob,
        }
    }
}

/// git's `canon_mode` (`cache.h`): what `ls-tree` prints for a stored mode.
pub(super) fn canon_mode(mode: u32) -> u32 {
    match mode & 0o170_000 {
        0o100_000 => {
            if mode & 0o100 != 0 {
                0o100_755
            } else {
                0o100_644
            }
        }
        0o120_000 => 0o120_000,
        0o040_000 => 0o040_000,
        _ => 0o160_000,
    }
}

/// A tree object's bytes → its entries (`None`: not a well-formed tree;
/// `ls-tree` decides then). `id_bytes`: 20 (SHA-1) or 32 (SHA-256).
pub(super) fn parse_raw_tree(data: &[u8], id_bytes: usize) -> Option<Vec<RawEntry>> {
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|b| *b == b' ')?;
        let mode_text = rest.get(..space)?;
        if mode_text.is_empty() || mode_text.len() > 7 {
            return None;
        }
        let mut mode: u32 = 0;
        for d in mode_text {
            if !(b'0'..=b'7').contains(d) {
                return None;
            }
            mode = mode * 8 + u32::from(d - b'0');
        }
        rest = rest.get(space + 1..)?;
        let nul = rest.iter().position(|b| *b == 0)?;
        let name = rest.get(..nul)?;
        if name.is_empty() {
            return None;
        }
        let id = rest.get(nul + 1..nul + 1 + id_bytes)?;
        let mut oid = String::with_capacity(id_bytes * 2);
        for b in id {
            oid.push(char::from(HEX[usize::from(b >> 4)]));
            oid.push(char::from(HEX[usize::from(b & 0xf)]));
        }
        entries.push(RawEntry {
            mode: canon_mode(mode),
            name: name.to_vec(),
            oid,
        });
        rest = rest.get(nul + 1 + id_bytes..)?;
    }
    entries.shrink_to_fit();
    Some(entries)
}

/// A tree listing as the API returns it (at most `max_tree_entries`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listing {
    pub(super) entries: Vec<TreeEntry>,
    pub(super) truncated: bool,
    pub(super) not_utf8: usize,
}

// ── the cache ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    RawTree,
    Listing,
    RootTree,
    Log,
}

type Key = (RepoId, Kind, String);

#[derive(Debug, Clone)]
enum Value {
    RawTree(Arc<Vec<RawEntry>>),
    Listing(Arc<Listing>),
    RootTree(Arc<str>),
    Log(Arc<(Vec<u8>, bool)>),
}

/// An `Arc`'s two counters.
const ARC_HEADER: usize = 2 * size_of::<usize>();

impl Value {
    /// The memory the value holds: its allocations at their capacity.
    fn heap_bytes(&self) -> usize {
        match self {
            Self::RawTree(t) => {
                ARC_HEADER
                    + size_of::<Vec<RawEntry>>()
                    + t.capacity() * size_of::<RawEntry>()
                    + t.iter()
                        .map(|e| e.name.capacity() + e.oid.capacity())
                        .sum::<usize>()
            }
            Self::Listing(l) => {
                ARC_HEADER
                    + size_of::<Listing>()
                    + l.entries.capacity() * size_of::<TreeEntry>()
                    + l.entries
                        .iter()
                        .map(|e| e.name.capacity() + e.mode.capacity() + e.oid.capacity())
                        .sum::<usize>()
            }
            Self::RootTree(t) => ARC_HEADER + t.len(),
            Self::Log(l) => ARC_HEADER + size_of::<(Vec<u8>, bool)>() + l.0.capacity(),
        }
    }
}

/// What one entry costs besides its value: the key in both indexes (the
/// order index holds a copy), the slot, and the hash table's and the
/// B-tree's slack per entry.
fn entry_overhead(key: &Key) -> usize {
    2 * (size_of::<Key>() + key.2.capacity()) + size_of::<Slot>() + size_of::<u64>() + 48
}

#[derive(Debug)]
struct Slot {
    value: Value,
    bytes: usize,
    tick: u64,
}

#[derive(Debug, Default)]
struct CacheInner {
    map: HashMap<Key, Slot>,
    order: BTreeMap<u64, Key>,
    bytes: usize,
    tick: u64,
}

/// Every cache of this process, so a repository whose files are removed
/// or rewritten is forgotten wherever it is cached ([`forget_everywhere`]).
static CACHES: Mutex<Vec<Weak<ObjectCache>>> = Mutex::new(Vec::new());

/// What is immutable by object id, least recently used out first.
#[derive(Debug)]
pub(super) struct ObjectCache {
    cap: usize,
    inner: Mutex<CacheInner>,
}

impl ObjectCache {
    pub(super) fn new(cap: usize) -> Arc<Self> {
        let cache = Arc::new(Self {
            cap,
            inner: Mutex::new(CacheInner::default()),
        });
        let mut caches = CACHES.lock().unwrap_or_else(PoisonError::into_inner);
        caches.retain(|c| c.strong_count() > 0);
        caches.push(Arc::downgrade(&cache));
        cache
    }

    fn get(&self, key: &Key) -> Option<Value> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.tick += 1;
        let tick = inner.tick;
        let CacheInner { map, order, .. } = &mut *inner;
        let slot = map.get_mut(key)?;
        order.remove(&slot.tick);
        slot.tick = tick;
        order.insert(tick, key.clone()); // clone: the order index owns its key
        Some(slot.value.clone())
    }

    fn put(&self, key: Key, value: Value) {
        let bytes = value.heap_bytes() + entry_overhead(&key);
        if bytes > self.cap / 4 {
            return; // one entry never takes a large share
        }
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(old) = inner.map.remove(&key) {
            inner.order.remove(&old.tick);
            inner.bytes -= old.bytes;
        }
        while inner.bytes + bytes > self.cap {
            let Some((_, oldest)) = inner.order.pop_first() else {
                break;
            };
            if let Some(old) = inner.map.remove(&oldest) {
                inner.bytes -= old.bytes;
            }
        }
        inner.order.insert(tick, key.clone()); // clone: both indexes own the key
        inner.map.insert(key, Slot { value, bytes, tick });
        inner.bytes += bytes;
    }

    /// Drop everything cached for `repo`.
    pub(super) fn forget(&self, repo: RepoId) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let CacheInner {
            map, order, bytes, ..
        } = &mut *inner;
        map.retain(|key, slot| {
            if key.0 != repo {
                return true;
            }
            order.remove(&slot.tick);
            *bytes -= slot.bytes;
            false
        });
    }

    /// Bytes held (tests).
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes
    }

    /// Entries held for `repo` (tests).
    #[cfg(test)]
    pub(super) fn entries_of(&self, repo: RepoId) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .keys()
            .filter(|k| k.0 == repo)
            .count()
    }
}

/// Drop `repo` from every object cache of this process.
pub(super) fn forget_everywhere(repo: RepoId) {
    let caches: Vec<Arc<ObjectCache>> = CACHES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    for cache in caches {
        cache.forget(repo);
    }
}

// ── the fast reads ─────────────────────────────────────────────────────

impl Browser {
    /// A failed batch exchange, said once (warn, FG-GIT-0032); a busy
    /// pool is only a debug event (nothing failed).
    fn batch_failed(&self, repo: RepoId, op: &'static str, err: BatchError) {
        match err {
            BatchError::Busy => tracing::debug!(
                target: "forge_git::browse",
                repo_id = repo.get(),
                op,
                "forge.git.batch.busy"
            ),
            BatchError::Failed(e) => {
                let reason = e
                    .context()
                    .iter()
                    .find(|(k, _)| *k == "reason")
                    .map(|(_, v)| v.clone()) // clone: logged after the borrow
                    .unwrap_or_default();
                tracing::warn!(
                    target: "forge_git::browse",
                    error_code = e.code().id(),
                    error = %e,
                    reason,
                    op,
                    repo_id = repo.get(),
                    "forge.git.batch.fallback"
                );
            }
        }
    }

    pub(super) async fn batch_info(
        &self,
        repo: RepoId,
        dir: &Path,
        oids: &[&str],
        deadline: Deadline,
    ) -> Option<Vec<Info>> {
        let pool = self.batch.as_ref()?;
        let budget = deadline.left().ok()?.min(self.limits.batch_timeout);
        match pool.info(dir, oids, budget).await {
            Ok(v) if v.len() == oids.len() => Some(v),
            Ok(_) => None,
            Err(e) => {
                self.batch_failed(repo, "info", e);
                None
            }
        }
    }

    pub(super) async fn batch_contents(
        &self,
        repo: RepoId,
        dir: &Path,
        oid: &str,
        max: u64,
        deadline: Deadline,
    ) -> Option<Contents> {
        let pool = self.batch.as_ref()?;
        let budget = deadline.left().ok()?.min(self.limits.batch_timeout);
        match pool.contents(dir, oid, max, budget).await {
            Ok(c) => Some(c),
            Err(e) => {
                self.batch_failed(repo, "contents", e);
                None
            }
        }
    }

    /// The root tree of `commit` (a commit id; a tree id is its own).
    pub(super) async fn fast_root_tree(
        &self,
        repo: RepoId,
        dir: &Path,
        commit: &str,
        deadline: Deadline,
    ) -> Option<String> {
        let key: Key = (repo, Kind::RootTree, commit.to_owned());
        if let Some(Value::RootTree(t)) = self.objects.get(&key) {
            return Some(t.to_string());
        }
        let tree = match self
            .batch_contents(repo, dir, commit, MAX_COMMIT_BYTES, deadline)
            .await?
        {
            Contents::Found { kind, data } if kind == "commit" => {
                let first = data.split(|b| *b == b'\n').next()?;
                let hex = std::str::from_utf8(first.strip_prefix(b"tree ")?).ok()?;
                super::object_id(hex).filter(|t| t.len() == commit.len())?
            }
            Contents::Found { kind, .. } if kind == "tree" => commit.to_ascii_lowercase(),
            Contents::Found { .. } | Contents::Missing | Contents::TooLarge { .. } => return None,
        };
        self.objects
            .put(key, Value::RootTree(Arc::from(tree.as_str())));
        Some(tree)
    }

    /// The entries of the tree `tree` as stored.
    async fn fast_raw_tree(
        &self,
        repo: RepoId,
        dir: &Path,
        tree: &str,
        deadline: Deadline,
    ) -> Option<Arc<Vec<RawEntry>>> {
        let key: Key = (repo, Kind::RawTree, tree.to_owned());
        if let Some(Value::RawTree(t)) = self.objects.get(&key) {
            return Some(t);
        }
        let Contents::Found { kind, data } = self
            .batch_contents(repo, dir, tree, MAX_RAW_TREE_BYTES, deadline)
            .await?
        else {
            return None;
        };
        if kind != "tree" {
            return None;
        }
        let entries = Arc::new(parse_raw_tree(&data, tree.len() / 2)?);
        self.objects.put(key, Value::RawTree(Arc::clone(&entries)));
        Some(entries)
    }

    /// The listing of the tree `tree` (at most `max_tree_entries`, with
    /// blob sizes).
    pub(super) async fn fast_listing(
        &self,
        repo: RepoId,
        dir: &Path,
        tree: &str,
        deadline: Deadline,
    ) -> Option<Arc<Listing>> {
        let key: Key = (repo, Kind::Listing, tree.to_owned());
        let cached = match self.objects.get(&key) {
            Some(Value::Listing(l)) => Some(l),
            Some(Value::RawTree(_) | Value::RootTree(_) | Value::Log(_)) | None => None,
        };
        let listing = match cached {
            Some(l) => l,
            None => {
                let raw = self.fast_raw_tree(repo, dir, tree, deadline).await?;
                let listing = Arc::new(self.listing_of(repo, dir, &raw, deadline).await?);
                self.objects.put(key, Value::Listing(Arc::clone(&listing)));
                listing
            }
        };
        // Said on every listing, like `ls-tree`'s parse.
        log_skipped(
            repo,
            &ParsedTree {
                not_utf8: listing.not_utf8,
                ..ParsedTree::default()
            },
        );
        Some(listing)
    }

    async fn listing_of(
        &self,
        repo: RepoId,
        dir: &Path,
        raw: &[RawEntry],
        deadline: Deadline,
    ) -> Option<Listing> {
        let max = self.limits.max_tree_entries;
        let mut listing = Listing {
            entries: Vec::new(),
            truncated: false,
            not_utf8: 0,
        };
        for e in raw {
            let Ok(name) = std::str::from_utf8(&e.name) else {
                listing.not_utf8 += 1;
                continue;
            };
            if listing.entries.len() >= max {
                listing.truncated = true;
                break;
            }
            listing.entries.push(TreeEntry {
                name: name.to_owned(),
                kind: e.kind(),
                mode: format!("{:06o}", e.mode),
                oid: e.oid.clone(), // clone: the cached raw tree keeps its own
                size: None,
            });
        }
        let blobs: Vec<usize> = listing
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind.is_blob())
            .map(|(i, _)| i)
            .collect();
        if !blobs.is_empty() {
            let oids: Vec<&str> = blobs
                .iter()
                .filter_map(|i| listing.entries.get(*i).map(|e| e.oid.as_str()))
                .collect();
            let infos = self.batch_info(repo, dir, &oids, deadline).await?;
            for (i, info) in blobs.iter().zip(infos) {
                if let (Some(entry), Info::Found { size, .. }) = (listing.entries.get_mut(*i), info)
                {
                    entry.size = Some(size);
                }
            }
        }
        Some(listing)
    }

    /// The entry at `path` (not the root) of `commit`, like `ls-tree
    /// --full-tree <commit> -- <path>`: `Some(None)` when there is none.
    pub(super) async fn fast_entry_at(
        &self,
        repo: RepoId,
        dir: &Path,
        commit: &str,
        path: &RepoPath,
        deadline: Deadline,
    ) -> Option<Option<TreeEntry>> {
        let mut tree = self.fast_root_tree(repo, dir, commit, deadline).await?;
        let segments: Vec<&str> = path.as_str().split('/').collect();
        let last = segments.len().checked_sub(1)?;
        for (i, segment) in segments.iter().enumerate() {
            let raw = self.fast_raw_tree(repo, dir, &tree, deadline).await?;
            let Some(found) = raw.iter().find(|e| e.name == segment.as_bytes()) else {
                return Some(None);
            };
            let kind = found.kind();
            if i < last {
                if kind != EntryKind::Tree {
                    return Some(None);
                }
                tree = found.oid.clone(); // clone: the next tree to read
                continue;
            }
            let size = if kind.is_blob() {
                match self
                    .batch_info(repo, dir, &[found.oid.as_str()], deadline)
                    .await?
                    .into_iter()
                    .next()?
                {
                    Info::Found { size, .. } => Some(size),
                    Info::Missing => None,
                }
            } else {
                None
            };
            return Some(Some(TreeEntry {
                name: (*segment).to_owned(),
                kind,
                mode: format!("{:06o}", found.mode),
                oid: found.oid.clone(), // clone: the cached raw tree keeps its own
                size,
            }));
        }
        None
    }

    /// The whole blob `oid` of `size` bytes (`None`: fall back).
    pub(super) async fn fast_blob(
        &self,
        repo: RepoId,
        dir: &Path,
        oid: &str,
        size: u64,
        deadline: Deadline,
    ) -> Option<Vec<u8>> {
        match self.batch_contents(repo, dir, oid, size, deadline).await? {
            Contents::Found { kind, data } if kind == "blob" && data.len() as u64 == size => {
                Some(data)
            }
            Contents::Found { .. } | Contents::Missing | Contents::TooLarge { .. } => None,
        }
    }

    /// `info` of one object (`None`: fall back).
    pub(super) async fn fast_info(
        &self,
        repo: RepoId,
        dir: &Path,
        oid: &str,
        deadline: Deadline,
    ) -> Option<Info> {
        self.batch_info(repo, dir, &[oid], deadline)
            .await?
            .into_iter()
            .next()
    }

    /// A cached history page (`log` stdout and whether it was cut).
    pub(super) fn cached_log(&self, repo: RepoId, key: &str) -> Option<Arc<(Vec<u8>, bool)>> {
        match self.objects.get(&(repo, Kind::Log, key.to_owned())) {
            Some(Value::Log(l)) => Some(l),
            Some(Value::RawTree(_) | Value::RootTree(_) | Value::Listing(_)) | None => None,
        }
    }

    pub(super) fn store_log(&self, repo: RepoId, key: String, page: Arc<(Vec<u8>, bool)>) {
        if page.0.len() > MAX_CACHED_LOG_BYTES {
            return;
        }
        self.objects.put((repo, Kind::Log, key), Value::Log(page));
    }

    /// The object cache (tests).
    #[cfg(test)]
    pub(super) fn object_cache_bytes(&self) -> usize {
        self.objects.bytes()
    }
}
