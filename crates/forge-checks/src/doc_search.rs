//! The documents of the knowledge search (plan 20 K4, ADR-0040 Amendment
//! K4): what the `search.index_documents` job reads from the default
//! branch's tip and hands to `forge_db::search::apply_documents`.
//!
//! - **What is a document**: a `.md` whose front matter names a `doctype`
//!   and an `id` (plan 13), originals and translations alike — Wiki pages
//!   and notes too. Its entry: the title (else the id) at weight A; the id,
//!   the headings and the block ids at weight B (so `REQ-0012` finds its
//!   document); the text after the front matter at weight C.
//! - **Classification at the tip** ([`document_entries`]): the document's
//!   own (with its type's default), and `confidential` whenever
//!   [`crate::confidential::confidential_in`] says so — the rule every
//!   agent-reachable read uses (a type that cannot be read, a translation
//!   of a confidential original, a front matter that cannot be read).
//! - **Changed paths only** ([`changed_paths`]): the tip's listing is
//!   compared with the listing of the SHA indexed before (path and blob
//!   id). Everything is read again instead (`full`) when nothing was
//!   indexed, the rebuild asks for it, the old SHA cannot be listed (a
//!   force push and a gc), either listing was cut, a type definition
//!   changed (it may change every document's classification) or the read
//!   hit a bound. A changed original takes its translations along (their
//!   classification follows it).
//! - **Bounds**: [`DocLimits`] (the document API's), each git call under
//!   the browse limits' timeout. A bound hit is `FG-KNOW-0002
//!   reason=read_bound` (warn, once per run) and recorded `failed` in
//!   `search_index_state` by the write; what was not read stays withheld.
//! - **Failure**: a run that fails for good is `FG-KNOW-0002
//!   reason=read_failed` with the cause chained ([`DocumentIndexer::fail`]),
//!   recorded `failed` too; the next push or `forge search reindex` tries
//!   again.

use std::collections::{BTreeSet, HashMap, HashSet};

use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ProjectId, RepoId, RequestId};
use forge_db::search::{
    apply_documents, documents_repository, indexed_documents_sha, record_documents_failed, Applied,
    DocumentEntry, DocumentScope, DocumentUpdate, DocumentsRequest,
};
use forge_db::Db;
use forge_doc::document::{parse_document, Classification};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BlobListing, Browser};

use crate::confidential::{confidential_in, type_defaults, type_of};
use crate::doc_lint::{is_doctype_file, is_document_file, read_listed, DocLimits, DocTree};

/// `call` as a boxed `Send` future: the proof is made here, on concrete
/// lifetimes (rustc cannot always prove it for a nested `async fn` future
/// inside the job runner's `dyn Future + Send`).
fn boxed<'a, T>(
    call: impl std::future::Future<Output = Result<T, ForgeError>> + Send + 'a,
) -> LookupFuture<'a, T> {
    Box::pin(call)
}

/// The caller name of the indexer's git reads (logs).
const CALLER: &str = "search_documents";
/// How often one run reads again when another job indexed in between.
const MAX_ROUNDS: usize = 3;

/// A document found at the tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundDocument {
    pub entry: DocumentEntry,
    pub id: String,
    /// The original's id when this is a translation.
    pub translation_of: Option<String>,
}

/// The text after the front matter (`---` … `---` / `...`); none when
/// the front matter is not closed.
fn body_of(text: &str) -> &str {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return "";
    };
    if first.trim_end() != "---" {
        return text;
    }
    let mut offset = first.len();
    for line in lines {
        offset += line.len();
        if matches!(line.trim_end(), "---" | "...") {
            return text.get(offset..).unwrap_or_default();
        }
    }
    ""
}

/// Every document of `tree` with its entry (module docs). Pure.
pub fn document_entries(tree: &DocTree) -> Vec<FoundDocument> {
    let withheld = confidential_in(tree);
    let types = type_defaults(tree);
    // The originals read: a translation whose original was not read (a
    // bound cut it, or it is missing) is not known to be open (review K4).
    let originals: HashSet<String> = tree
        .markdown
        .iter()
        .map(|(_, bytes)| parse_document(bytes))
        .filter(|d| d.doctype().is_some() && d.meta_text("translation_of").is_none())
        .filter_map(|d| d.id().map(str::to_owned))
        .collect();
    let mut out = Vec::new();
    for (path, bytes) in &tree.markdown {
        let doc = parse_document(bytes);
        let (Some(id), Some(doctype)) = (doc.id(), doc.doctype()) else {
            continue;
        };
        let orphan = doc
            .meta_text("translation_of")
            .is_some_and(|of| !originals.contains(of));
        let classification = if withheld.contains(path) || orphan {
            Classification::Confidential
        } else {
            doc.classification(type_of(&types, doctype))
        };
        let title = doc
            .meta_text("title")
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(id);
        let mut ids: Vec<&str> = vec![id];
        ids.extend(doc.headings.iter().map(|h| h.text.as_str()));
        ids.extend(doc.blocks.iter().filter_map(|b| b.id.as_deref()));
        let text_unreadable = doc.problems.iter().any(|p| p.rule == "not_utf8");
        let body = if text_unreadable {
            String::new()
        } else {
            // A large file is read in part: its cut may split a character.
            body_of(&String::from_utf8_lossy(bytes)).to_owned()
        };
        out.push(FoundDocument {
            entry: DocumentEntry {
                path: path.clone(), // clone: the entry owns its path
                title: title.to_owned(),
                ids: ids.join("\n"),
                body,
                classification: classification.as_str(),
            },
            id: id.to_owned(),
            translation_of: doc.meta_text("translation_of").map(str::to_owned),
        });
    }
    out
}

/// What changed between two listings of the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangedPaths {
    /// `.md` files added, changed or removed.
    pub documents: BTreeSet<String>,
    /// A type definition was added, changed or removed.
    pub doctypes: bool,
}

/// The `.md` files and type definitions whose blob differs between `old`
/// and `new` (by path and blob id). Pure.
pub fn changed_paths(old: &BlobListing, new: &BlobListing) -> ChangedPaths {
    let relevant = |p: &str| is_document_file(p) || is_doctype_file(p);
    let index = |l: &BlobListing| -> HashMap<String, String> {
        l.blobs
            .iter()
            .filter(|b| relevant(&b.path))
            .map(|b| (b.path.clone(), b.oid.clone())) // clone: the map owns them
            .collect()
    };
    let (before, after) = (index(old), index(new));
    let mut out = ChangedPaths::default();
    let all: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for path in all {
        if before.get(path) == after.get(path) {
            continue;
        }
        if is_doctype_file(path) {
            out.doctypes = true;
        } else {
            out.documents.insert(path.clone()); // clone: the set owns it
        }
    }
    out
}

/// The entries and scope of an update from what was found at the tip.
/// `changed`: the changed `.md` paths (`None`: the whole tip). Pure.
pub fn plan_update(
    found: &[FoundDocument],
    changed: Option<&BTreeSet<String>>,
    listing: &BlobListing,
) -> (Vec<DocumentEntry>, DocumentScope) {
    let Some(changed) = changed else {
        let listed = (!listing.truncated).then(|| {
            listing
                .blobs
                .iter()
                .filter(|b| is_document_file(&b.path))
                .map(|b| b.path.clone()) // clone: the scope owns them
                .collect()
        });
        let entries = found.iter().map(|f| f.entry.clone()).collect(); // clone: written
        return (entries, DocumentScope::Full { listed });
    };
    let changed_ids: HashSet<&str> = found
        .iter()
        .filter(|f| changed.contains(&f.entry.path))
        .map(|f| f.id.as_str())
        .collect();
    let entries = found
        .iter()
        .filter(|f| {
            changed.contains(&f.entry.path)
                || f.translation_of
                    .as_deref()
                    .is_some_and(|o| changed_ids.contains(o))
        })
        .map(|f| f.entry.clone()) // clone: written
        .collect();
    let present: HashSet<&str> = found.iter().map(|f| f.entry.path.as_str()).collect();
    let removed = changed
        .iter()
        .filter(|p| !present.contains(p.as_str()))
        .cloned()
        .collect();
    (entries, DocumentScope::Changed { removed })
}

/// What one run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indexed {
    /// The project has no ready repository (or is gone): nothing to do.
    NoRepository,
    /// The default branch is not born.
    Unborn,
    /// The tip is the one indexed (a push to another branch).
    UpToDate,
    /// Other jobs kept indexing in between; theirs is the newer answer.
    Superseded,
    Written {
        entries: u64,
        removed: u64,
        full: bool,
        incomplete: bool,
    },
}

/// The `search.index_documents` job's work (module docs).
pub struct DocumentIndexer {
    db: Db,
    browse: Browser,
    limits: DocLimits,
}

impl DocumentIndexer {
    pub fn new(db: Db, browse: Browser) -> Self {
        Self {
            db,
            browse,
            limits: DocLimits::default(),
        }
    }

    /// Other read bounds (tests).
    #[must_use]
    pub fn with_limits(mut self, limits: DocLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Index `request`'s project at its default branch's tip. Boxed as a
    /// `Send` future (the job runner's), like forge-checks' other probes.
    pub fn run<'a>(
        &'a self,
        request: DocumentsRequest,
        request_id: &'a RequestId,
    ) -> LookupFuture<'a, Indexed> {
        Box::pin(async move {
            for _ in 0..MAX_ROUNDS {
                if let Some(done) = self.round(request, request_id).await? {
                    return Ok(done);
                }
            }
            tracing::info!(
                target: "forge_checks",
                event = "forge.search.documents_superseded",
                request_id = %request_id,
                project_id = request.project.get(),
                rounds = MAX_ROUNDS,
                "other jobs kept indexing this project's documents; theirs stands"
            );
            Ok(Indexed::Superseded)
        })
    }

    /// The commit `head` (a full branch name) names.
    fn tip<'a>(&'a self, repo: RepoId, head: &'a str) -> LookupFuture<'a, String> {
        Box::pin(async move {
            self.browse
                .resolve_ref(repo, head, CALLER)
                .await
                .map(|rev| rev.commit)
        })
    }

    /// One read and write (`None`: another job indexed in between).
    fn round<'a>(
        &'a self,
        request: DocumentsRequest,
        request_id: &'a RequestId,
    ) -> LookupFuture<'a, Option<Indexed>> {
        Box::pin(async move {
            let project = request.project;
            let Some(repo) = boxed(documents_repository(&self.db, project)).await? else {
                return Ok(Some(Indexed::NoRepository));
            };
            // Another process pushed: never answer from this process's cache.
            self.browse.invalidate(repo);
            let refs = boxed(self.browse.refs(repo)).await?;
            let Some(branch) = refs.default_branch.as_deref() else {
                return Ok(Some(Indexed::Unborn));
            };
            let head = format!("refs/heads/{branch}");
            let resolved = self.tip(repo, &head).await;
            let tip = match resolved {
                Ok(commit) => commit,
                Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => {
                    return Ok(Some(Indexed::Unborn))
                }
                Err(e) => return Err(e),
            };
            let base = boxed(indexed_documents_sha(&self.db, project)).await?;
            if !request.full && base.as_deref() == Some(tip.as_str()) {
                return Ok(Some(Indexed::UpToDate));
            }
            let listing = boxed(self.browse.blobs_in_tree(
                repo,
                &tip,
                self.limits.max_tree_entries,
            ))
            .await
            .map_err(|e| e.with_context("commit", tip.clone()))?; // clone: the context owns it
            let since = if request.full || listing.truncated {
                None
            } else {
                base.clone() // clone: compared again under the write's lock
            };
            let mut changed = match since {
                Some(old) => {
                    boxed(async {
                        Ok(self
                            .changed_since(project, request_id, repo, &old, &listing)
                            .await)
                    })
                    .await?
                }
                None => None,
            };
            let tree = boxed(read_listed(
                &self.browse,
                repo,
                &tip,
                &listing,
                &self.limits,
            ))
            .await?;
            if let Some(bound) = tree.incomplete {
                tracing::warn!(
                    target: "forge_checks",
                    event = "forge.search.index_item",
                    error_code = ErrorCode::SearchIndexItemFailed.id(),
                    reason = "read_bound",
                    bound,
                    source_kind = "document",
                    project_id = project.get(),
                    sha = %tip,
                    request_id = %request_id,
                    "the tip's documents were read in part: the rest stay withheld"
                );
                changed = None;
            }
            let found = document_entries(&tree);
            let full = changed.is_none();
            let (entries, scope) = plan_update(&found, changed.as_ref(), &listing);
            let update = DocumentUpdate {
                tip,
                base,
                entries,
                scope,
                incomplete: tree.incomplete,
            };
            let applied = boxed(apply_documents(&self.db, request_id, project, &update)).await?;
            Ok(match applied {
                Applied::Moved => None,
                Applied::Gone => Some(Indexed::NoRepository),
                Applied::Written { entries, removed } => {
                    tracing::info!(
                        target: "forge_checks",
                        event = "forge.search.documents_indexed",
                        request_id = %request_id,
                        project_id = project.get(),
                        sha = %update.tip,
                        full,
                        entries,
                        removed,
                        incomplete = update.incomplete,
                        "documents indexed"
                    );
                    Some(Indexed::Written {
                        entries,
                        removed,
                        full,
                        incomplete: update.incomplete.is_some(),
                    })
                }
            })
        })
    }

    /// The changed `.md` paths since `old` (`None`: read the whole tip —
    /// the old SHA cannot be listed, its listing was cut, or a type
    /// definition changed).
    async fn changed_since(
        &self,
        project: ProjectId,
        request_id: &RequestId,
        repo: RepoId,
        old: &str,
        listing: &BlobListing,
    ) -> Option<BTreeSet<String>> {
        let before = boxed(
            self.browse
                .blobs_in_tree(repo, old, self.limits.max_tree_entries),
        )
        .await;
        match before {
            Ok(before) if !before.truncated => {
                let c = changed_paths(&before, listing);
                (!c.doctypes).then_some(c.documents)
            }
            Ok(_) => None,
            Err(e) => {
                // Not a failure of the run: the whole tip is read instead.
                tracing::info!(
                    target: "forge_checks",
                    event = "forge.search.documents_full_read",
                    request_id = %request_id,
                    project_id = project.get(),
                    reason = "old_sha_unreadable",
                    cause_code = e.code().id(),
                    "the SHA indexed before could not be listed; reading the whole tip"
                );
                None
            }
        }
    }

    /// A run failed for good: `FG-KNOW-0002 reason=read_failed` (warn,
    /// the cause chained) and the project recorded `failed`. A failure to
    /// record it is logged too (the job is dead either way).
    pub async fn fail(&self, project: ProjectId, request_id: &RequestId, cause: &ForgeError) {
        tracing::warn!(
            target: "forge_checks",
            event = "forge.search.index_item",
            error_code = ErrorCode::SearchIndexItemFailed.id(),
            reason = "read_failed",
            source_kind = "document",
            project_id = project.get(),
            cause_code = cause.code().id(),
            cause = %cause,
            request_id = %request_id,
            "a project's documents could not be indexed"
        );
        if let Err(e) = record_documents_failed(&self.db, project).await {
            tracing::error!(
                target: "forge_checks",
                event = "forge.search.index_state_unrecorded",
                error_code = e.code().id(),
                project_id = project.get(),
                request_id = %request_id,
                "the failed documents index could not be recorded"
            );
        }
    }
}

#[cfg(test)]
#[path = "doc_search_tests.rs"]
mod tests;
