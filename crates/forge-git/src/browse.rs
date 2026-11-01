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
//! - **Ref cache**: the `for-each-ref` result is kept per repository for
//!   [`BrowseLimits::refs_ttl`] (5 s) and dropped by
//!   [`Browser::invalidate`] (every push). A list loaded while an
//!   invalidation happened is not stored.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use forge_core::{RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::time::Instant;

use crate::command::Git;
use crate::revision::Revision;
use crate::storage::StorageRoot;
use crate::stream::GitStream;

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

// ── paths ──────────────────────────────────────────────────────────────

/// A path inside a repository's tree: `/`-separated, no empty, `.` or `..`
/// segment, no control character, at most [`RepoPath::MAX_BYTES`]. The
/// root is the empty path. Passed to git only after `--`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoPath(String);

impl RepoPath {
    pub const MAX_BYTES: usize = 4096;

    pub fn root() -> Self {
        Self(String::new())
    }

    /// Parse a path (one leading and one trailing `/` are ignored).
    /// `Err` names the rule: `too_long`, `empty_segment`, `dot_segment`,
    /// `control`.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let trimmed = raw.strip_prefix('/').unwrap_or(raw);
        let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
        if trimmed.is_empty() {
            return Ok(Self::root());
        }
        let segments: Vec<&str> = trimmed.split('/').collect();
        Self::from_segments(&segments)
    }

    /// A path from segments (each checked as in [`RepoPath::parse`]).
    pub fn from_segments<S: AsRef<str>>(segments: &[S]) -> Result<Self, &'static str> {
        let mut out = String::new();
        for segment in segments {
            let segment = segment.as_ref();
            if segment.is_empty() {
                return Err("empty_segment");
            }
            if segment == "." || segment == ".." {
                return Err("dot_segment");
            }
            if segment.chars().any(char::is_control) {
                return Err("control");
            }
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(segment);
            if out.len() > Self::MAX_BYTES {
                return Err("too_long");
            }
        }
        Ok(Self(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The last segment (`None` for the root).
    pub fn file_name(&self) -> Option<&str> {
        self.0.rsplit('/').next().filter(|s| !s.is_empty())
    }

    /// `self/name` (`name` for the root).
    pub fn child(&self, name: &str) -> String {
        if self.is_root() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.0)
        }
    }
}

// ── refs ───────────────────────────────────────────────────────────────

/// A browsable ref's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
}

impl RefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Tag => "tag",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Branch => "refs/heads/",
            Self::Tag => "refs/tags/",
        }
    }
}

/// One branch or tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub name: RefName,
    pub kind: RefKind,
    /// The object the ref names (an annotated tag: the tag object).
    pub target: String,
    /// The commit it peels to (`None`: a tag of a tree or a blob).
    pub commit: Option<String>,
}

impl RefEntry {
    /// The name without `refs/heads/` / `refs/tags/`.
    pub fn short(&self) -> &str {
        self.name
            .as_str()
            .strip_prefix(self.kind.prefix())
            .unwrap_or(self.name.as_str())
    }
}

/// A repository's branches and tags, in ref name order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefList {
    pub refs: Vec<RefEntry>,
    /// The branch `HEAD` names (`None`: an unborn or detached `HEAD`).
    pub default_branch: Option<String>,
    /// More than [`BrowseLimits::max_refs`] refs: the list stops there.
    pub truncated: bool,
}

impl RefList {
    fn find(&self, full: &str) -> Option<&RefEntry> {
        self.refs
            .binary_search_by(|r| r.name.as_str().cmp(full))
            .ok()
            .and_then(|i| self.refs.get(i))
    }
}

/// What a resolved revision is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Branch(RefName),
    Tag(RefName),
    /// A full commit id was given.
    Commit,
}

/// A revision resolved to a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRev {
    pub target: Target,
    /// The commit id (40 / 64 lower-case hex).
    pub commit: String,
}

impl ResolvedRev {
    /// `branch`, `tag` or `commit`.
    pub fn kind(&self) -> &'static str {
        match self.target {
            Target::Branch(_) => "branch",
            Target::Tag(_) => "tag",
            Target::Commit => "commit",
        }
    }

    /// The full ref name (`None` for a commit id).
    pub fn ref_name(&self) -> Option<&RefName> {
        match &self.target {
            Target::Branch(r) | Target::Tag(r) => Some(r),
            Target::Commit => None,
        }
    }

    /// The short ref name, or the commit id.
    pub fn display_name(&self) -> &str {
        match &self.target {
            Target::Branch(r) => r.branch().unwrap_or(r.as_str()),
            Target::Tag(r) => r.tag().unwrap_or(r.as_str()),
            Target::Commit => &self.commit,
        }
    }
}

/// A spec split into a revision and a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSpec {
    pub rev: ResolvedRev,
    pub path: RepoPath,
}

// ── trees and blobs ────────────────────────────────────────────────────

/// What a tree entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Tree,
    Blob,
    /// A blob with mode `120000`.
    Symlink,
    /// A gitlink (mode `160000`, a commit of another repository).
    Submodule,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Symlink => "symlink",
            Self::Submodule => "submodule",
        }
    }

    fn is_blob(self) -> bool {
        matches!(self, Self::Blob | Self::Symlink)
    }
}

/// One entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The entry's name (lossy UTF-8 for a name that is not).
    pub name: String,
    pub kind: EntryKind,
    /// The octal mode as git prints it (`100644`, `040000`, …).
    pub mode: String,
    pub oid: String,
    /// Blobs only.
    pub size: Option<u64>,
}

/// A tree listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    /// The tree's own id.
    pub oid: String,
    /// In git's order (byte order of the names).
    pub entries: Vec<TreeEntry>,
    /// More than [`BrowseLimits::max_tree_entries`] entries.
    pub truncated: bool,
}

/// A blob's head for the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub entry: TreeEntry,
    pub size: u64,
    /// At most [`BrowseLimits::max_blob_bytes`].
    pub content: Vec<u8>,
    pub truncated: bool,
}

/// A raw file: read whole (small), or streamed.
#[derive(Debug)]
pub enum RawBody {
    Bytes(Vec<u8>),
    Stream(GitStream),
}

/// A raw file of a commit.
#[derive(Debug)]
pub struct RawBlob {
    pub size: u64,
    pub oid: String,
    pub body: RawBody,
}

/// The archive formats (`git archive --format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
}

impl ArchiveFormat {
    /// Split `name.zip` / `name.tar.gz` (the extension is removed once).
    pub fn split(name: &str) -> Option<(&str, Self)> {
        if let Some(stem) = name.strip_suffix(".tar.gz") {
            return Some((stem, Self::TarGz));
        }
        name.strip_suffix(".zip").map(|stem| (stem, Self::Zip))
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Zip => "application/zip",
            Self::TarGz => "application/gzip",
        }
    }

    fn git_format(self) -> &'static str {
        match self {
            Self::Zip => "--format=zip",
            Self::TarGz => "--format=tar.gz",
        }
    }
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

/// The `for-each-ref` format: fields separated by NUL, one ref per line
/// (a ref name holds neither).
const REF_FORMAT: &str =
    "--format=%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)%00%(HEAD)%00%(refname)";

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

    /// The branches and tags (cached for `refs_ttl`).
    pub async fn refs(&self, repo: RepoId) -> Result<Arc<RefList>, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        self.refs_within(repo, deadline).await
    }

    async fn refs_within(
        &self,
        repo: RepoId,
        deadline: Deadline,
    ) -> Result<Arc<RefList>, ForgeError> {
        if let Some(list) = self.cached(repo) {
            return Ok(list);
        }
        let epoch = self.cache.epoch.load(Ordering::SeqCst);
        let list = Arc::new(self.load_refs(repo, deadline).await?);
        if self.cache.epoch.load(Ordering::SeqCst) == epoch {
            self.store(repo, Arc::clone(&list));
        }
        Ok(list)
    }

    fn cached(&self, repo: RepoId) -> Option<Arc<RefList>> {
        let entries = self
            .cache
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        entries
            .get(&repo)
            .filter(|c| c.at.elapsed() < self.limits.refs_ttl)
            .map(|c| Arc::clone(&c.list))
    }

    fn store(&self, repo: RepoId, list: Arc<RefList>) {
        let mut entries = self
            .cache
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= self.limits.max_cached_repos.max(1) && !entries.contains_key(&repo) {
            let ttl = self.limits.refs_ttl;
            entries.retain(|_, c| c.at.elapsed() < ttl);
            if entries.len() >= self.limits.max_cached_repos.max(1) {
                let oldest = entries.iter().min_by_key(|(_, c)| c.at).map(|(k, _)| *k);
                if let Some(oldest) = oldest {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(
            repo,
            Cached {
                at: Instant::now(),
                list,
            },
        );
    }

    async fn load_refs(&self, repo: RepoId, deadline: Deadline) -> Result<RefList, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, truncated_bytes) = self
            .git
            .command("for-each-ref")
            .arg(REF_FORMAT)
            .arg("refs/heads/")
            .arg("refs/tags/")
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(REFS_OUTPUT_BYTES)
            .await?;
        let (mut list, skipped) = parse_refs(&out.stdout, truncated_bytes, self.limits.max_refs);
        if skipped > 0 {
            // git printed refs forge cannot name (never pushed through
            // forge): left out of the list, said once per load.
            tracing::warn!(
                target: "forge_git::browse",
                repo_id = repo.get(),
                skipped,
                "forge.git.browse.refs_skipped"
            );
        }
        list.truncated |= truncated_bytes;
        Ok(list)
    }

    /// The refs named exactly by `names` (no cache; used when the cached
    /// list is truncated).
    async fn query_refs(
        &self,
        repo: RepoId,
        names: &[RefName],
        deadline: Deadline,
    ) -> Result<RefList, ForgeError> {
        if names.is_empty() {
            return Ok(RefList::default());
        }
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let mut cmd = self.git.command("for-each-ref").arg(REF_FORMAT);
        for name in names {
            cmd = cmd.revision(name);
        }
        let (out, truncated) = cmd
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(REFS_OUTPUT_BYTES)
            .await?;
        // A pattern also matches refs below it: keep the exact names only.
        let (mut list, _) = parse_refs(&out.stdout, truncated, usize::MAX);
        list.refs
            .retain(|r| names.iter().any(|n| n.as_str() == r.name.as_str()));
        Ok(list)
    }

    /// The commit a full id names (`not_a_commit` / `ref_not_found`).
    async fn commit_of(
        &self,
        repo: RepoId,
        id: &str,
        deadline: Deadline,
    ) -> Result<String, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let out = self
            .git
            .command("cat-file")
            .arg("--batch-check=%(objectname) %(objecttype)")
            .repo(&dir)
            .stdin(format!("{id}\n").into_bytes())
            .timeout(deadline.left()?)
            .output()
            .await?;
        let text = out.stdout_str();
        let mut fields = text.split_whitespace();
        match (fields.next(), fields.next()) {
            (Some(found), Some("commit")) if found == id => Ok(id.to_owned()),
            (Some(_), Some("missing")) | (None, _) => Err(not_resolved("ref_not_found")),
            _ => Err(not_resolved("not_a_commit")),
        }
    }

    /// A ref (resolved from the list) to its revision.
    fn rev_of(entry: &RefEntry) -> Result<ResolvedRev, ForgeError> {
        let Some(commit) = entry.commit.clone() else {
            return Err(not_resolved("not_a_commit"));
        };
        let target = match entry.kind {
            RefKind::Branch => Target::Branch(entry.name.clone()),
            RefKind::Tag => Target::Tag(entry.name.clone()),
        };
        Ok(ResolvedRev { target, commit })
    }

    /// The refs of `repo` that may name one of `candidates` (full names):
    /// the cached list when it is complete, else git asked for exactly
    /// those names.
    async fn lookup(
        &self,
        repo: RepoId,
        candidates: &[RefName],
        deadline: Deadline,
    ) -> Result<Arc<RefList>, ForgeError> {
        let list = self.refs_within(repo, deadline).await?;
        if !list.truncated {
            return Ok(list);
        }
        Ok(Arc::new(self.query_refs(repo, candidates, deadline).await?))
    }

    /// A `?ref=` value: a full commit id, a full `refs/heads/…` /
    /// `refs/tags/…` name, or a short name (branch first, then tag).
    pub async fn resolve_ref(&self, repo: RepoId, raw: &str) -> Result<ResolvedRev, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        if let Some(id) = object_id(raw) {
            let commit = self.commit_of(repo, &id, deadline).await?;
            return Ok(ResolvedRev {
                target: Target::Commit,
                commit,
            });
        }
        let candidates: Vec<RefName> =
            if raw.starts_with("refs/heads/") || raw.starts_with("refs/tags/") {
                RefName::parse(raw).ok().into_iter().collect()
            } else {
                [RefKind::Branch, RefKind::Tag]
                    .iter()
                    .filter_map(|k| RefName::parse(&format!("{}{raw}", k.prefix())).ok())
                    .collect()
            };
        if candidates.is_empty() {
            return Err(not_resolved("ref_not_found"));
        }
        let list = self.lookup(repo, &candidates, deadline).await?;
        for candidate in &candidates {
            if let Some(entry) = list.find(candidate.as_str()) {
                return Self::rev_of(entry);
            }
        }
        Err(not_resolved("ref_not_found"))
    }

    /// `ref/path…` segments (the raw URL's tail, the resolve API's `spec`)
    /// split into a revision and a path, in ADR-0015's order.
    pub async fn resolve_spec<S: AsRef<str>>(
        &self,
        repo: RepoId,
        segments: &[S],
    ) -> Result<ResolvedSpec, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let Some(first) = segments.first() else {
            return Err(not_resolved("ref_not_found"));
        };
        if let Some(id) = object_id(first.as_ref()) {
            let path = RepoPath::from_segments(segments.get(1..).unwrap_or_default())
                .map_err(|_| not_resolved("path_not_found"))?;
            let commit = self.commit_of(repo, &id, deadline).await?;
            return Ok(ResolvedSpec {
                rev: ResolvedRev {
                    target: Target::Commit,
                    commit,
                },
                path,
            });
        }
        // Every prefix that is a valid ref name of each kind, longest first.
        let mut candidates: Vec<(RefKind, usize, RefName)> = Vec::new();
        for kind in [RefKind::Branch, RefKind::Tag] {
            let mut name = String::from(kind.prefix());
            let mut prefixes = Vec::new();
            for (i, segment) in segments.iter().enumerate() {
                if i > 0 {
                    name.push('/');
                }
                name.push_str(segment.as_ref());
                if name.len() > 1024 {
                    break;
                }
                if let Ok(r) = RefName::parse(&name) {
                    prefixes.push((kind, i + 1, r));
                }
            }
            prefixes.reverse();
            candidates.extend(prefixes);
        }
        if candidates.is_empty() {
            return Err(not_resolved("ref_not_found"));
        }
        let names: Vec<RefName> = candidates.iter().map(|(_, _, r)| r.clone()).collect();
        let list = self.lookup(repo, &names, deadline).await?;
        for (_, used, name) in &candidates {
            if let Some(entry) = list.find(name.as_str()) {
                let rev = Self::rev_of(entry)?;
                let path = RepoPath::from_segments(segments.get(*used..).unwrap_or_default())
                    .map_err(|_| not_resolved("path_not_found"))?;
                return Ok(ResolvedSpec { rev, path });
            }
        }
        Err(not_resolved("ref_not_found"))
    }

    /// The entry at `path` in `commit` (`None`: no such entry).
    async fn entry_at(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
        deadline: Deadline,
    ) -> Result<Option<TreeEntry>, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, _) = self
            .git
            .command("ls-tree")
            .arg("-z")
            .arg("--long")
            .arg("--full-tree")
            .revision(Revision::object_id(commit)?)
            .path(path.as_str())
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(64 * 1024)
            .await?;
        Ok(parse_tree(&out.stdout, usize::MAX)
            .0
            .into_iter()
            .find(|e| e.name == path.as_str())
            .map(|mut e| {
                e.name = path.file_name().unwrap_or_default().to_owned();
                e
            }))
    }

    /// The tree at `path` of `commit`.
    pub async fn tree(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
    ) -> Result<Tree, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let target = if path.is_root() {
            commit.to_owned()
        } else {
            match self.entry_at(repo, commit, path, deadline).await? {
                None => return Err(not_resolved("path_not_found")),
                Some(e) if e.kind == EntryKind::Tree => e.oid,
                Some(_) => return Err(not_resolved("not_a_tree")),
            }
        };
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, truncated_bytes) = self
            .git
            .command("ls-tree")
            .arg("-z")
            .arg("--long")
            .revision(Revision::object_id(&target)?)
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(TREE_OUTPUT_BYTES)
            .await?;
        let (entries, truncated) = parse_tree(&out.stdout, self.limits.max_tree_entries);
        let oid = if path.is_root() {
            self.tree_of(repo, commit, deadline).await?
        } else {
            target
        };
        Ok(Tree {
            oid,
            entries,
            truncated: truncated || truncated_bytes,
        })
    }

    /// The root tree id of `commit`.
    async fn tree_of(
        &self,
        repo: RepoId,
        commit: &str,
        deadline: Deadline,
    ) -> Result<String, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let out = self
            .git
            .command("cat-file")
            .arg("--batch-check=%(objectname)")
            .repo(&dir)
            .stdin(format!("{commit}^{{tree}}\n").into_bytes())
            .timeout(deadline.left()?)
            .output()
            .await?;
        let text = out.stdout_str();
        let oid = text.trim();
        if object_id(oid).is_none() {
            return Err(ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("reason", "tree_of_commit_unreadable")
                .with_context("repo_id", repo.get()));
        }
        Ok(oid.to_owned())
    }

    /// The blob entry at `path` (`not_a_blob` for a tree or a submodule).
    async fn blob_entry(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
        deadline: Deadline,
    ) -> Result<TreeEntry, ForgeError> {
        if path.is_root() {
            return Err(not_resolved("not_a_blob"));
        }
        match self.entry_at(repo, commit, path, deadline).await? {
            None => Err(not_resolved("path_not_found")),
            Some(e) if e.kind.is_blob() => Ok(e),
            Some(_) => Err(not_resolved("not_a_blob")),
        }
    }

    /// The blob entry at `path` (id, mode, size) without reading it (a
    /// raw `HEAD`).
    pub async fn raw_entry(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
    ) -> Result<TreeEntry, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        self.blob_entry(repo, commit, path, deadline).await
    }

    /// The first [`BrowseLimits::max_blob_bytes`] of the blob at `path`.
    pub async fn blob(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
    ) -> Result<Blob, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let entry = self.blob_entry(repo, commit, path, deadline).await?;
        let size = entry.size.unwrap_or(0);
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, truncated) = self
            .git
            .command("cat-file")
            .arg("blob")
            .revision(Revision::object_id(&entry.oid)?)
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(self.limits.max_blob_bytes)
            .await?;
        Ok(Blob {
            entry,
            size,
            content: out.stdout,
            truncated,
        })
    }

    /// The blob at `path`, whole (≤ `raw_inline_max_bytes`) or as a stream
    /// (`principal`: the stream cap key, e.g. `user:<id>`).
    pub async fn raw(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
        principal: &str,
    ) -> Result<RawBlob, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let entry = self.blob_entry(repo, commit, path, deadline).await?;
        let size = entry.size.unwrap_or(0);
        let dir = self.storage.existing(repo)?;
        let cmd = self
            .git
            .command("cat-file")
            .arg("blob")
            .revision(Revision::object_id(&entry.oid)?)
            .repo(&dir);
        if size <= self.limits.raw_inline_max_bytes {
            let _slot = self.claim(repo)?;
            let cap = usize::try_from(size).unwrap_or(usize::MAX);
            let (out, truncated) = cmd
                .timeout(deadline.left()?)
                .output_truncated(cap.saturating_add(1))
                .await?;
            if truncated || out.stdout.len() != cap {
                return Err(ForgeError::new(ErrorCode::GitProcessFailed)
                    .with_context("reason", "blob_size_mismatch")
                    .with_context("repo_id", repo.get())
                    .with_context("expected_bytes", size)
                    .with_context("read_bytes", out.stdout.len()));
            }
            return Ok(RawBlob {
                size,
                oid: entry.oid,
                body: RawBody::Bytes(out.stdout),
            });
        }
        let stream = cmd
            .idle_timeout(self.limits.stream_idle)
            .timeout(self.limits.stream_total)
            .spawn_stream(principal)
            .await?;
        Ok(RawBlob {
            size,
            oid: entry.oid,
            body: RawBody::Stream(stream),
        })
    }

    /// `git archive` of `commit` as a stream, every file below `prefix/`
    /// (`prefix`: a forge-made name, `[a-z0-9._-]`, not starting with `-`).
    pub async fn archive(
        &self,
        repo: RepoId,
        commit: &str,
        format: ArchiveFormat,
        prefix: &str,
        principal: &str,
    ) -> Result<GitStream, ForgeError> {
        let valid = !prefix.is_empty()
            && prefix.len() <= 200
            && !prefix.starts_with('-')
            && prefix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
        if !valid {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "archive_prefix_invalid"));
        }
        let dir = self.storage.existing(repo)?;
        self.git
            .command("archive")
            // gzip inside git (2.38+), never a configured filter command.
            .config_override("tar.tar.gz.command", "git archive gzip")
            .config_override("tar.tgz.command", "git archive gzip")
            .arg(format.git_format())
            .arg("--prefix")
            .arg_owned(format!("{prefix}/"))
            .revision(Revision::object_id(commit)?)
            .repo(&dir)
            .idle_timeout(self.limits.stream_idle)
            .timeout(self.limits.stream_total)
            .spawn_stream(principal)
            .await
    }
}

// ── parsing git's output ───────────────────────────────────────────────

/// `for-each-ref` lines ([`REF_FORMAT`]) → the list (at most `max`) and
/// how many lines could not be named (skipped). `cut`: the output was
/// truncated (the last line may be partial and is dropped).
fn parse_refs(out: &[u8], cut: bool, max: usize) -> (RefList, usize) {
    let mut list = RefList::default();
    let mut skipped = 0;
    let mut lines: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
    if cut {
        lines.pop();
    }
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            skipped += 1;
            continue;
        };
        let fields: Vec<&str> = text.split('\0').collect();
        let [oid, kind, peeled, peeled_kind, head, name] = fields.as_slice() else {
            skipped += 1;
            continue;
        };
        let Ok(ref_name) = RefName::parse(name) else {
            skipped += 1;
            continue;
        };
        let ref_kind = if ref_name.branch().is_some() {
            RefKind::Branch
        } else if ref_name.tag().is_some() {
            RefKind::Tag
        } else {
            skipped += 1;
            continue;
        };
        let commit = match (*kind, *peeled_kind) {
            ("commit", _) => object_id(oid),
            ("tag", "commit") => object_id(peeled),
            _ => None,
        };
        if *head == "*" && ref_kind == RefKind::Branch {
            list.default_branch = ref_name.branch().map(str::to_owned);
        }
        if list.refs.len() >= max {
            list.truncated = true;
            continue;
        }
        list.refs.push(RefEntry {
            name: ref_name,
            kind: ref_kind,
            target: (*oid).to_owned(),
            commit,
        });
    }
    // git sorts by ref name; keep it sorted for the binary search whatever.
    list.refs.sort_by(|a, b| a.name.cmp(&b.name));
    (list, skipped)
}

/// `ls-tree -z --long` records → entries (at most `max`) and whether more
/// followed. A record cut by the output cap is dropped.
fn parse_tree(out: &[u8], max: usize) -> (Vec<TreeEntry>, bool) {
    let mut entries = Vec::new();
    let mut truncated = false;
    let complete = out.ends_with(b"\0") || out.is_empty();
    let mut records: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    // The split leaves an empty tail after the final NUL, or a partial
    // record when the output was cut.
    records.pop();
    if !complete {
        truncated = true;
    }
    for record in records {
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let (head, name) = (
            record.get(..tab).unwrap_or_default(),
            record.get(tab + 1..).unwrap_or_default(),
        );
        let head = String::from_utf8_lossy(head);
        let mut fields = head.split_whitespace();
        let (Some(mode), Some(kind), Some(oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let kind = match (kind, mode) {
            ("tree", _) => EntryKind::Tree,
            ("blob", "120000") => EntryKind::Symlink,
            ("blob", _) => EntryKind::Blob,
            ("commit", _) => EntryKind::Submodule,
            _ => continue,
        };
        if entries.len() >= max {
            truncated = true;
            break;
        }
        entries.push(TreeEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            kind,
            mode: mode.to_owned(),
            oid: oid.to_owned(),
            size: size.parse().ok(),
        });
    }
    (entries, truncated)
}

#[cfg(test)]
#[path = "browse_tests.rs"]
mod tests;
