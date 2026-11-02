//! Refs and revision resolution (see [`super`]): the ref list, its cache,
//! `?ref=` / `ref/path…` resolution and `for-each-ref` parsing.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::PoisonError;

use forge_core::{RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::time::Instant;

use super::path::RepoPath;
use super::{not_resolved, object_id, Browser, Cached, Deadline, REFS_OUTPUT_BYTES};

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
    pub(super) fn find(&self, full: &str) -> Option<&RefEntry> {
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

/// The `for-each-ref` format: fields separated by NUL, one ref per line
/// (a ref name holds neither).
const REF_FORMAT: &str =
    "--format=%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)%00%(HEAD)%00%(refname)";

impl Browser {
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
    pub(super) async fn commit_of(
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
            (Some(_), Some("missing")) => Err(not_resolved("ref_not_found")),
            (None, _) => Err(ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("reason", "batch_check_empty")
                .with_context("repo_id", repo.get())),
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
}

/// `for-each-ref` lines ([`REF_FORMAT`]) → the list (at most `max`) and
/// how many lines could not be named (skipped). `cut`: the output was
/// truncated (the last line may be partial and is dropped).
pub(super) fn parse_refs(out: &[u8], cut: bool, max: usize) -> (RefList, usize) {
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
