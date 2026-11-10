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
//! `.md` files (`doc_files`), one file's size (`doc_file_too_large`: not
//! read), the bytes read in all (`doc_bytes`) and the number of type
//! definitions (`doctype_files`). The first bound hit is named
//! ([`DocTree::incomplete`], `FG-DOC-0002`) and the run never claims more
//! than it read.
//!
//! **Conclusions**: `success` (no problem), `failure` (a problem:
//! `FG-DOC-0001`; a type definition that cannot be used: `FG-DOC-0003`),
//! `neutral` (no problem, but a bound was hit; or only broken references
//! while incomplete — the target may be in what was not read). Broken
//! references are errors (plan 13 Q13); the repository setting that lowers
//! them to warnings during a migration is not built yet.

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, DOC_LINT_CHECK};
use forge_doc::document::MAX_DOCUMENT_BYTES;
use forge_doc::lint::{lint, FileReport, Source, DOCTYPES_DIR};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BlobListing, ListedBlob};

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
    /// A larger `.md` is not read (`doc_file_too_large`).
    pub max_file_bytes: u64,
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
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            max_doctypes: 64,
        }
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

    /// Keep a file that was read. A document over the parser's limit is
    /// kept as its first `MAX_DOCUMENT_BYTES + 1` bytes: the lint answers
    /// `too_large` for it exactly as for the whole file (and as
    /// `gstn doc lint`, which reads no more).
    pub fn add(&mut self, path: &str, mut bytes: Vec<u8>) {
        if is_doctype_file(path) {
            self.doctypes.push((path.to_owned(), bytes));
        } else if is_document_file(path) {
            bytes.truncate(MAX_DOCUMENT_BYTES.saturating_add(1));
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

/// What to read of the whole tree's `listing`, within `limits`, and the
/// tree to fill (its bound already named when one was hit while choosing).
pub fn select(listing: &BlobListing, limits: &DocLimits) -> (Vec<ListedBlob>, DocTree) {
    let mut tree = DocTree::default();
    if listing.truncated {
        tree.left_out("doc_tree");
    }
    let mut chosen = Vec::new();
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
        if b.size > limits.max_file_bytes {
            tree.left_out("doc_file_too_large");
            continue;
        }
        if total.saturating_add(b.size) > limits.max_total_bytes {
            tree.left_out("doc_bytes");
            break;
        }
        total = total.saturating_add(b.size);
        chosen.push(b.clone()); // clone: the read owns its list
    }
    (chosen, tree)
}

/// The code a problem of `path` with `rule` is reported under.
fn code_of(path: &str, rule: &str) -> ErrorCode {
    if is_doctype_file(path) || rule == "doctype_invalid" {
        ErrorCode::DoctypeDefinitionInvalid
    } else {
        ErrorCode::DocLintProblem
    }
}

fn problem_line(path: &str, p: &forge_doc::document::Problem) -> String {
    let code = code_of(path, p.rule).id();
    match &p.subject {
        Some(s) => format!("{path}:{} {} — {s} [{code}]", p.line, p.rule),
        None => format!("{path}:{} {} [{code}]", p.line, p.rule),
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

/// The `forge/doc-lint` run of the head's documents (module docs).
pub fn assess_doc_lint(tree: &DocTree) -> Result<forge_db::checks::NewCheckRun, ForgeError> {
    let reports = lint_tree(tree);
    let problems: usize = reports
        .iter()
        .map(|r| r.problems.len().saturating_add(r.problems_dropped))
        .sum();
    let mut lines = Vec::new();
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
        lines.push(format!(
            "{problems} problem(s) in {} file(s) [{}]:",
            reports.len(),
            if definitions {
                ErrorCode::DoctypeDefinitionInvalid.id()
            } else {
                ErrorCode::DocLintProblem.id()
            }
        ));
        let listed: Vec<String> = reports
            .iter()
            .flat_map(|r| r.problems.iter().map(|p| problem_line(&r.path, p)))
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
        if tree.incomplete.is_some() && only_references {
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
                annotation(
                    &r.path,
                    p.line,
                    CheckAnnotationLevel::Failure,
                    problem_line(&r.path, p),
                )
            })
        })
        .collect();
    completed(DOC_LINT_CHECK, conclusion, &lines, annotations)
}

#[cfg(test)]
#[path = "doc_lint_tests.rs"]
mod tests;
