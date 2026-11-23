//! The train lint's document index (plan 18 S7, ADR-0037 (b) 3): one
//! [`LintCache`] per repository in `forge serve`'s memory, bounded in all
//! by `merge_queue.doc_index_bytes`.
//!
//! A tree is listed once (`ls-tree -r`, the listing names every blob's id)
//! and only the documents whose blob the cache does not hold are read (one
//! `cat-file --batch`, and a capped read per file over the parser's
//! limit); the type definitions are always read (at most 64 small files).
//! The answer is [`LintCache::lint`]'s — equal to the whole lint of the
//! same files (`forge_doc`'s property test) — within the same bounds
//! ([`super::TRAIN_DOC_LIMITS`]): a bound hit is still "could not be
//! linted".
//!
//! A repository's cache is taken out of the map while its tree is linted
//! and put back after, so no lock is held during the work; two trains of
//! one repository linted at once (two targets) each work on their own
//! cache and the last one back is kept. Past the bound in all, the least
//! recently used repositories' caches are dropped whole.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use forge_core::{ObjectId, RepoId};
use forge_doc::lint::incremental::{KeyedSource, LintCache, LintStats};
use forge_doc::lint::{FileReport, Source};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, ListedBlob};

use crate::doc_lint::{is_doctype_file, is_document_file, read_cap, select, DocLimits, TreeShape};

/// The repositories' document caches (module docs).
pub struct DocIndex {
    max_bytes: usize,
    caches: Mutex<HashMap<RepoId, Kept>>,
}

struct Kept {
    cache: LintCache,
    used: u64,
}

impl std::fmt::Debug for DocIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocIndex")
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

/// One tree linted through the index.
pub(super) struct Indexed {
    pub(super) shape: TreeShape,
    pub(super) reports: Vec<FileReport>,
    pub(super) stats: LintStats,
    /// Documents whose bytes were read now.
    pub(super) read: usize,
}

impl DocIndex {
    /// An index that keeps about `max_bytes` in all.
    pub fn new(max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            max_bytes,
            caches: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RepoId, Kept>> {
        match self.caches.lock() {
            Ok(map) => map,
            // A panic while the map was held: the caches start over (they
            // only save work; a lost one is read again).
            Err(poisoned) => {
                let mut map = poisoned.into_inner();
                map.clear();
                tracing::warn!(
                    target: "forge_checks",
                    event = "forge.train_lint.index_reset",
                    error_code = ErrorCode::Internal.id(),
                    "the document index's lock was poisoned; every cache is dropped"
                );
                map
            }
        }
    }

    fn take(&self, repo: RepoId) -> LintCache {
        self.lock()
            .remove(&repo)
            .map_or_else(|| LintCache::new(self.max_bytes), |k| k.cache)
    }

    fn put(&self, repo: RepoId, cache: LintCache) {
        let mut map = self.lock();
        let used = map
            .values()
            .map(|k| k.used)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        map.insert(repo, Kept { cache, used });
        let mut total: usize = map.values().map(|k| k.cache.bytes()).sum();
        while total > self.max_bytes {
            let Some(oldest) = map
                .iter()
                .filter(|(r, _)| **r != repo)
                .min_by_key(|(_, k)| k.used)
                .map(|(r, _)| *r)
            else {
                break;
            };
            if let Some(dropped) = map.remove(&oldest) {
                total = total.saturating_sub(dropped.cache.bytes());
            }
        }
    }

    /// `commit`'s documents linted through `repo`'s cache, within
    /// `limits` (a bound hit is the caller's to refuse: [`TreeShape`]).
    /// Errors: forge-git's, each with the commit; an inconsistency of the
    /// cache (`FG-SYS-0001 reason=doc_index_missing_bytes`).
    pub(super) async fn lint(
        &self,
        browse: &Browser,
        repo: RepoId,
        commit: &ObjectId,
        limits: &DocLimits,
    ) -> Result<Indexed, ForgeError> {
        let with_commit = |e: ForgeError| e.with_context("commit", commit.as_str().to_owned());
        let listing = browse
            .blobs_in_tree(repo, commit.as_str(), limits.max_tree_entries)
            .await
            .map_err(with_commit)?;
        let (chosen, tree) = select(&listing, limits);
        let shape = TreeShape {
            markdown: chosen
                .whole
                .iter()
                .chain(&chosen.prefix)
                .filter(|b| is_document_file(&b.path))
                .count(),
            doctypes: chosen
                .whole
                .iter()
                .chain(&chosen.prefix)
                .filter(|b| is_doctype_file(&b.path))
                .count(),
            incomplete: tree.incomplete,
        };
        if shape.incomplete.is_some() {
            return Ok(Indexed {
                shape,
                reports: Vec::new(),
                stats: LintStats::default(),
                read: 0,
            });
        }
        let mut cache = self.take(repo);
        // Only what the cache does not hold (and every type definition).
        let wanted = |b: &&ListedBlob| !is_document_file(&b.path) || !cache.holds(&b.oid);
        let whole: Vec<ListedBlob> = chosen.whole.iter().filter(wanted).cloned().collect(); // clone: the read owns its list
        let prefix: Vec<&ListedBlob> = chosen.prefix.iter().filter(wanted).collect();
        let read = async {
            let mut bytes: HashMap<String, Vec<u8>> = HashMap::new();
            let contents = browse.read_blobs(repo, &whole).await.map_err(with_commit)?;
            for (b, mut content) in whole.iter().zip(contents) {
                content.truncate(read_cap(&b.path));
                bytes.insert(b.oid.clone(), content); // clone: keyed by the blob
            }
            for b in &prefix {
                let content = browse
                    .read_blob_prefix(repo, b, read_cap(&b.path))
                    .await
                    .map_err(with_commit)?;
                bytes.insert(b.oid.clone(), content); // clone: as above
            }
            Ok::<_, ForgeError>(bytes)
        }
        .await;
        let bytes = match read {
            Ok(bytes) => bytes,
            Err(e) => {
                self.put(repo, cache);
                return Err(e);
            }
        };
        let read_now = whole
            .iter()
            .map(|b| &b.path)
            .chain(prefix.iter().map(|b| &b.path))
            .filter(|p| is_document_file(p))
            .count();
        let mut files: Vec<ListedBlob> = chosen.whole;
        files.extend(chosen.prefix);
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let linted = tokio::task::spawn_blocking(move || {
            let doctypes: Vec<Source<'_>> = files
                .iter()
                .filter(|b| is_doctype_file(&b.path))
                .map(|b| Source {
                    path: &b.path,
                    bytes: bytes.get(&b.oid).map_or(&[][..], Vec::as_slice),
                })
                .collect();
            let documents: Vec<KeyedSource<'_>> = files
                .iter()
                .filter(|b| is_document_file(&b.path))
                .map(|b| KeyedSource {
                    path: &b.path,
                    key: &b.oid,
                    bytes: bytes.get(&b.oid).map(Vec::as_slice),
                })
                .collect();
            let answer = cache.lint(&doctypes, &documents);
            drop(documents);
            drop(doctypes);
            (cache, answer)
        })
        .await;
        let (cache, answer) = linted.map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "train_lint_task_failed")
                .with_context("panicked", e.is_panic())
        })?;
        self.put(repo, cache);
        let (reports, stats) = answer.map_err(|missing| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "doc_index_missing_bytes")
                .with_context("path", missing.path)
                .with_context("commit", commit.as_str().to_owned())
        })?;
        Ok(Indexed {
            shape,
            reports,
            stats,
            read: read_now,
        })
    }
}
