//! `forge/doc-lint` (plan 13 stage 2, ADR-0026): the document lint of a
//! merge request version's head. Written by the `check.spec_index` job
//! beside `forge/spec-lint` and `forge/spec-trace`, from the same read of
//! the head ([`crate::spec_index::index_head`]): no other job kind, and the
//! tree is listed and its blobs read once.
//!
//! **Documents** are the `.md` files whose front matter names a `doctype`,
//! in any folder; their types are `.forge/doctypes/<type>.yml`. The lint is
//! `forge_doc::lint::lint` — the same function, on the same bytes, as
//! `gstn doc lint` on a working tree, so the answers agree. Every `.md` of
//! the tree is read (whether it names a type is in its front matter).
//!
//! **Bounds** ([`DocLimits`]): the tree listing (`doc_tree`), the number of
//! `.md` files (`doc_files`), the files over the parser's limit — read in
//! part, only as far as the parser needs ([`read_cap`], like `gstn doc`) —
//! (`doc_large_files`), the bytes read in all (`doc_bytes`) and the number
//! of type definitions (`doctype_files`). The first bound hit is named
//! ([`DocTree::incomplete`], `FG-DOC-0002`) and the run never claims more
//! than it read.
//!
//! **Conclusions**: `success` (no problem), `failure` (a problem:
//! `FG-DOC-0001`; a type definition that cannot be used: `FG-DOC-0003`),
//! `neutral` (no problem, but a bound was hit; or only broken references
//! while incomplete — the target may be in what was not read). Broken
//! references are errors (plan 13 Q13) unless the repository's migration
//! setting is in force ([`assess_doc_lint_with`]): then they are warnings
//! (annotations of level `warning`), a run whose only problems they are
//! concludes `success`, and the summary's first line is
//! `migrating: until YYYY-MM-DD`. The date is the database's to compare
//! (`forge_db::checks::doc_lint_migration`); past it, they are errors again
//! with nothing written.

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, DOC_LINT_CHECK};
use forge_core::doc_lint_migration::LOWERED_RULE;
use forge_core::RepoId;
use forge_doc::doctype::MAX_DOCTYPE_BYTES;
use forge_doc::document::MAX_DOCUMENT_BYTES;
use forge_doc::lint::{lint, FileReport, Source, DOCTYPES_DIR};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BlobListing, Browser, ListedBlob};

use crate::spec_checks::{annotation, completed};

/// Problems the summary lists at most (the summary is cut at its own
/// bound too; the annotations carry the first ones with their lines).
const MAX_SUMMARY_PROBLEMS: usize = 300;

/// The bounds of one read of a head's documents (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocLimits {
    /// Files of the tree listed at most (`doc_tree` past it).
    pub max_tree_entries: usize,
    /// `.md` files read at most (`doc_files` past it).
    pub max_files: usize,
    /// Files over the parser's limit read in part (one `cat-file` each)
    /// at most (`doc_large_files` past it).
    pub max_large_files: usize,
    /// Bytes read in all (`doc_bytes` past it).
    pub max_total_bytes: u64,
    /// Type definitions read at most (`doctype_files` past it).
    pub max_doctypes: usize,
}

impl Default for DocLimits {
    fn default() -> Self {
        Self {
            max_tree_entries: 50_000,
            max_files: 2_000,
            max_large_files: 100,
            max_total_bytes: 32 * 1024 * 1024,
            max_doctypes: 64,
        }
    }
}

/// The bytes of `path` the lint needs at most: one past the parser's limit
/// (a document's, a type definition's). More says nothing more: the lint
/// answers `too_large` either way — and `gstn doc` reads no more.
pub fn read_cap(path: &str) -> usize {
    if is_doctype_file(path) {
        MAX_DOCTYPE_BYTES.saturating_add(1)
    } else {
        MAX_DOCUMENT_BYTES.saturating_add(1)
    }
}

/// One revision's files for the lint (module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocTree {
    /// `.forge/doctypes/<type>.yml`, by path.
    pub doctypes: Vec<(String, Vec<u8>)>,
    /// Every `.md` read, by path.
    pub markdown: Vec<(String, Vec<u8>)>,
    /// The first bound hit (module docs).
    pub incomplete: Option<&'static str>,
}

impl DocTree {
    /// Name the first bound hit.
    pub fn left_out(&mut self, reason: &'static str) {
        self.incomplete.get_or_insert(reason);
    }

    /// Keep a file that was read, cut at [`read_cap`]: the lint answers
    /// `too_large` for it exactly as for the whole file (and as
    /// `gstn doc lint`, which reads no more).
    pub fn add(&mut self, path: &str, mut bytes: Vec<u8>) {
        bytes.truncate(read_cap(path));
        if is_doctype_file(path) {
            self.doctypes.push((path.to_owned(), bytes));
        } else if is_document_file(path) {
            self.markdown.push((path.to_owned(), bytes));
        }
    }

    /// In path order (the answers are by path).
    pub fn sort(&mut self) {
        self.doctypes.sort_by(|a, b| a.0.cmp(&b.0));
        self.markdown.sort_by(|a, b| a.0.cmp(&b.0));
    }
}

/// A file that may be a document: `.md` (the name is the document's id).
pub fn is_document_file(path: &str) -> bool {
    path.ends_with(".md")
}

/// `.forge/doctypes/<type>.yml` (directly in the directory).
pub fn is_doctype_file(path: &str) -> bool {
    path.strip_prefix(DOCTYPES_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| name.ends_with(".yml") && !name.contains('/'))
}

/// What to read of a head's documents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Read whole (with the specification files, one `cat-file --batch`).
    pub whole: Vec<ListedBlob>,
    /// Over [`read_cap`]: only their first [`read_cap`] bytes are read
    /// (whether a large `.md` names a `doctype` is in its front matter).
    pub prefix: Vec<ListedBlob>,
}

/// What to read of the whole tree's `listing`, within `limits`, and the
/// tree to fill (its bound already named when one was hit while choosing).
pub fn select(listing: &BlobListing, limits: &DocLimits) -> (Selection, DocTree) {
    let mut tree = DocTree::default();
    if listing.truncated {
        tree.left_out("doc_tree");
    }
    let mut chosen = Selection::default();
    let mut total = 0u64;
    let doctypes: Vec<&ListedBlob> = listing
        .blobs
        .iter()
        .filter(|b| is_doctype_file(&b.path))
        .collect();
    if doctypes.len() > limits.max_doctypes {
        tree.left_out("doctype_files");
    }
    let markdown: Vec<&ListedBlob> = listing
        .blobs
        .iter()
        .filter(|b| is_document_file(&b.path))
        .collect();
    if markdown.len() > limits.max_files {
        tree.left_out("doc_files");
    }
    let wanted = doctypes
        .into_iter()
        .take(limits.max_doctypes)
        .chain(markdown.into_iter().take(limits.max_files));
    for b in wanted {
        let cap = u64::try_from(read_cap(&b.path)).unwrap_or(u64::MAX);
        let large = b.size > cap;
        if large && chosen.prefix.len() >= limits.max_large_files {
            tree.left_out("doc_large_files");
            continue;
        }
        let size = b.size.min(cap);
        if total.saturating_add(size) > limits.max_total_bytes {
            tree.left_out("doc_bytes");
            break;
        }
        total = total.saturating_add(size);
        if large {
            chosen.prefix.push(b.clone()); // clone: the read owns its list
        } else {
            chosen.whole.push(b.clone()); // clone: as above
        }
    }
    (chosen, tree)
}

/// One revision's documents and type definitions, read within `limits`
/// (the document API, plan 13 stage 3): the listing, selection and reads of
/// [`crate::spec_index::index_head`] without the specification index — one
/// listing of the whole tree, one `cat-file --batch` for the files read
/// whole, and a capped read of each file over the parser's limit. A bound
/// hit is named in [`DocTree::incomplete`]. Errors: forge-git's
/// (`FG-GIT-…`, the repository's timeout and output caps), each with the
/// commit.
pub async fn read_tree(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
    limits: &DocLimits,
) -> Result<DocTree, ForgeError> {
    let with_commit = |e: ForgeError| e.with_context("commit", commit.to_owned());
    let listing = browse
        .blobs_in_tree(repo, commit, limits.max_tree_entries)
        .await
        .map_err(with_commit)?;
    let (chosen, mut tree) = select(&listing, limits);
    let contents = browse
        .read_blobs(repo, &chosen.whole)
        .await
        .map_err(with_commit)?;
    for (b, bytes) in chosen.whole.iter().zip(contents) {
        tree.add(&b.path, bytes);
    }
    for b in &chosen.prefix {
        let bytes = browse
            .read_blob_prefix(repo, b, read_cap(&b.path))
            .await
            .map_err(with_commit)?;
        tree.add(&b.path, bytes);
    }
    tree.sort();
    Ok(tree)
}

/// The code a problem of `path` with `rule` is reported under.
fn code_of(path: &str, rule: &str) -> ErrorCode {
    if is_doctype_file(path) || rule == "doctype_invalid" {
        ErrorCode::DoctypeDefinitionInvalid
    } else {
        ErrorCode::DocLintProblem
    }
}

fn problem_line(path: &str, p: &forge_doc::document::Problem, warning: bool) -> String {
    let code = code_of(path, p.rule).id();
    let level = if warning { " (warning: migrating)" } else { "" };
    match &p.subject {
        Some(s) => format!("{path}:{} {}{level} — {s} [{code}]", p.line, p.rule),
        None => format!("{path}:{} {}{level} [{code}]", p.line, p.rule),
    }
}

/// The lint of `tree` (module docs).
pub fn lint_tree(tree: &DocTree) -> Vec<FileReport> {
    let doctypes: Vec<Source<'_>> = tree
        .doctypes
        .iter()
        .map(|(path, bytes)| Source { path, bytes })
        .collect();
    let documents: Vec<Source<'_>> = tree
        .markdown
        .iter()
        .map(|(path, bytes)| Source { path, bytes })
        .collect();
    lint(&doctypes, &documents)
}

/// The `forge/doc-lint` run of the head's documents (module docs), with
/// no migration in force.
pub fn assess_doc_lint(tree: &DocTree) -> Result<forge_db::checks::NewCheckRun, ForgeError> {
    assess_doc_lint_with(tree, None)
}

/// Whether `rule` is lowered to a warning while `migration` is in force.
fn lowered(migration: Option<&str>, rule: &str) -> bool {
    migration.is_some() && rule == LOWERED_RULE
}

/// The `forge/doc-lint` run of the head's documents (module docs).
/// `migration`: the repository's migration date in force (plan 13 Q13,
/// `forge_db::checks::doc_lint_migration::in_force`, compared with the
/// database's date) — broken references (`ref_broken`) are then warnings
/// and the summary's first line says `migrating: until <date>`. Every
/// other rule, and the problems past a file's bound (their rule is not
/// known), stay errors.
pub fn assess_doc_lint_with(
    tree: &DocTree,
    migration: Option<&str>,
) -> Result<forge_db::checks::NewCheckRun, ForgeError> {
    let reports = lint_tree(tree);
    let warnings: usize = reports
        .iter()
        .map(|r| {
            r.problems
                .iter()
                .filter(|p| lowered(migration, p.rule))
                .count()
        })
        .sum();
    let problems: usize = reports
        .iter()
        .map(|r| r.problems.len().saturating_add(r.problems_dropped))
        .sum();
    let errors = problems.saturating_sub(warnings);
    let mut lines = Vec::new();
    if let Some(until) = migration {
        lines.push(format!(
            "migrating: until {until} — broken references ({LOWERED_RULE}) are warnings until \
             then (repository setting; errors again from the next day, UTC)."
        ));
    }
    let conclusion = if problems == 0 {
        lines.push(format!(
            "No document problem at this commit ({} Markdown file(s) and {} type definition(s) \
             read).",
            tree.markdown.len(),
            tree.doctypes.len()
        ));
        if tree.incomplete.is_some() {
            CheckConclusion::Neutral
        } else {
            CheckConclusion::Success
        }
    } else {
        let definitions = reports.iter().any(|r| {
            r.problems
                .iter()
                .any(|p| code_of(&r.path, p.rule) == ErrorCode::DoctypeDefinitionInvalid)
        });
        let code = if definitions {
            ErrorCode::DoctypeDefinitionInvalid.id()
        } else {
            ErrorCode::DocLintProblem.id()
        };
        if migration.is_some() {
            lines.push(format!(
                "{errors} error(s) and {warnings} warning(s) in {} file(s) [{code}]:",
                reports.len()
            ));
        } else {
            lines.push(format!(
                "{problems} problem(s) in {} file(s) [{code}]:",
                reports.len()
            ));
        }
        let listed: Vec<String> = reports
            .iter()
            .flat_map(|r| {
                r.problems
                    .iter()
                    .map(|p| problem_line(&r.path, p, lowered(migration, p.rule)))
            })
            .take(MAX_SUMMARY_PROBLEMS)
            .collect();
        let unlisted = problems.saturating_sub(listed.len());
        lines.extend(listed);
        if unlisted > 0 {
            lines.push(format!("{unlisted} more problem(s) not listed."));
        }
        let only_references = reports
            .iter()
            .all(|r| r.problems_dropped == 0 && r.problems.iter().all(|p| p.rule == "ref_broken"));
        if errors == 0 {
            // Only lowered references: the migration lets them pass.
            if tree.incomplete.is_some() {
                CheckConclusion::Neutral
            } else {
                CheckConclusion::Success
            }
        } else if tree.incomplete.is_some() && only_references {
            CheckConclusion::Neutral
        } else {
            CheckConclusion::Failure
        }
    };
    if let Some(reason) = tree.incomplete {
        lines.push(format!(
            "The documents were read only in part ({reason}): a bound was hit, the results are \
             about what was read [{}].",
            ErrorCode::DocIndexBounded.id()
        ));
    }
    let annotations = reports
        .iter()
        .flat_map(|r| {
            r.problems.iter().map(|p| {
                let warning = lowered(migration, p.rule);
                annotation(
                    &r.path,
                    p.line,
                    if warning {
                        CheckAnnotationLevel::Warning
                    } else {
                        CheckAnnotationLevel::Failure
                    },
                    problem_line(&r.path, p, warning),
                )
            })
        })
        .collect();
    completed(DOC_LINT_CHECK, conclusion, &lines, annotations)
}

#[cfg(test)]
#[path = "doc_lint_tests.rs"]
mod tests;
