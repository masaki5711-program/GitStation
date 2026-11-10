//! Which files of a commit an AI agent is not given (plan 06 D-5,
//! ADR-0026 §6, ADR-0023 Amendment X5): the one answer every agent-reachable
//! read of file content asks (blob, raw, blame, compare, a commit, a merge
//! request's diffs and context).
//!
//! A path is **confidential** at a commit when
//!
//! 1. it is a specification document of the commit's index
//!    ([`crate::spec_index::index_commit`]) that
//!    [`forge_core::spec_doc::SpecDocument::confidential`] withholds (the
//!    older `confidential: true`, `classification: confidential`, both keys,
//!    a front matter that cannot be read, the `spec.system` type's default);
//!    or
//! 2. it is a `.md` file whose classification
//!    ([`forge_doc::document::Document::classification`]) is
//!    `confidential`: for a file naming a `doctype`, with that type's default
//!    from `.forge/doctypes/<type>.yml` as a floor (a type that is missing or
//!    cannot be read: confidential); for any other `.md`, its own mark (a
//!    front matter that cannot be read, a file that is no text: confidential).
//!    A translation (`translation_of: <id>`) is never less confidential than
//!    the document it translates; or
//! 3. the read of the documents hit a bound ([`DocTree::incomplete`]) and the
//!    path is a `.md` that was not read: what was not read is not known to
//!    be open (fail closed).
//!
//! **Bounds**: those of [`DocLimits`] and [`IndexLimits`] (each git call has
//! the repository's own timeout), and the whole answer is given at most
//! [`ConfidentialLimits::timeout`]: past it `FG-GIT-0002`
//! (`operation=confidential_paths`) — the caller refuses, it never serves.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use forge_core::RepoId;
use forge_doc::doctype::parse_doctype;
use forge_doc::document::{parse_document, Classification, TypeDefault};
use forge_doc::lint::DOCTYPES_DIR;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;

use crate::doc_lint::{is_document_file, read_tree, DocLimits, DocTree};
use crate::spec_index::{index_commit, IndexLimits, Tags};

/// The bounds of one answer (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfidentialLimits {
    pub doc: DocLimits,
    pub spec: IndexLimits,
    /// The whole answer (both reads) at most.
    pub timeout: Duration,
}

impl Default for ConfidentialLimits {
    fn default() -> Self {
        Self {
            doc: DocLimits::default(),
            spec: IndexLimits::default(),
            timeout: Duration::from_secs(20),
        }
    }
}

/// The confidential paths of one or more commits (module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Confidential {
    paths: BTreeSet<String>,
    /// For each read that hit a bound: the `.md` files it did read (any
    /// other `.md` is confidential, rule 3).
    partial: Vec<BTreeSet<String>>,
}

impl Confidential {
    /// Whether an AI agent is not given `path`.
    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
            || (is_document_file(path) && self.partial.iter().any(|read| !read.contains(path)))
    }

    /// Every `.md` file (nothing to measure against: the default branch is
    /// not born — fail closed).
    pub fn every_document() -> Self {
        Self {
            paths: BTreeSet::new(),
            partial: vec![BTreeSet::new()],
        }
    }

    /// Mark `path` (the specification index's answer, rule 1).
    pub fn insert(&mut self, path: &str) {
        self.paths.insert(path.to_owned());
    }

    /// Both answers: confidential in either.
    pub fn extend(&mut self, other: Confidential) {
        self.paths.extend(other.paths);
        self.partial.extend(other.partial);
    }

    /// Whether a bound left some `.md` unread (rule 3 is in force).
    pub fn is_partial(&self) -> bool {
        !self.partial.is_empty()
    }

    /// Whether no path is confidential (no mark, and no bound left a
    /// `.md` unread). A partial answer is never empty (fail closed).
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.partial.is_empty()
    }

    /// Some confidential paths (at most `n`, in order) — for a log line.
    pub fn first(&self, n: usize) -> Vec<&str> {
        self.paths.iter().take(n).map(String::as_str).collect()
    }
}

/// Each type's default classification when its definition can be read
/// (`TypeDefault::Unresolved` when it cannot), by type name.
pub fn type_defaults(tree: &DocTree) -> HashMap<String, TypeDefault> {
    let prefix = format!("{DOCTYPES_DIR}/");
    tree.doctypes
        .iter()
        .filter_map(|(path, bytes)| {
            let stem = path.strip_prefix(&prefix)?.strip_suffix(".yml")?;
            let ty = match parse_doctype(stem, bytes) {
                Ok(t) => TypeDefault::Known(t.default_classification),
                Err(_) => TypeDefault::Unresolved,
            };
            Some((stem.to_owned(), ty))
        })
        .collect()
}

/// What is known of a document's type `doctype` in `types`: a type without
/// a definition read is [`TypeDefault::Unresolved`] (fail closed).
pub fn type_of(types: &HashMap<String, TypeDefault>, doctype: &str) -> TypeDefault {
    types
        .get(doctype)
        .copied()
        .unwrap_or(TypeDefault::Unresolved)
}

/// Rules 2 and 3 for one read of a commit's documents (pure).
pub fn confidential_in(tree: &DocTree) -> Confidential {
    let types = type_defaults(tree);
    let mut by_id: HashMap<String, Classification> = HashMap::new();
    let mut files: Vec<(&str, Classification, Option<String>)> = Vec::new();
    for (path, bytes) in &tree.markdown {
        let doc = parse_document(bytes);
        let class = match doc.doctype() {
            Some(doctype) => doc.classification(type_of(&types, doctype)),
            // Not a document of a type: its own mark only (no default).
            None => doc.classification(TypeDefault::Known(None)),
        };
        if doc.doctype().is_some() && doc.meta_text("translation_of").is_none() {
            if let Some(id) = doc.id() {
                let e = by_id.entry(id.to_owned()).or_insert(class);
                *e = (*e).max(class);
            }
        }
        let of = doc.meta_text("translation_of").map(str::to_owned);
        files.push((path.as_str(), class, of));
    }
    let mut out = Confidential::default();
    for (path, class, of) in files {
        let original = of.and_then(|id| by_id.get(&id).copied());
        if original.map_or(class, |o| class.max(o)) == Classification::Confidential {
            out.insert(path);
        }
    }
    if tree.incomplete.is_some() {
        out.partial
            .push(tree.markdown.iter().map(|(p, _)| p.clone()).collect());
    }
    out
}

fn timed_out(commit: &str, timeout: Duration) -> ForgeError {
    ForgeError::new(ErrorCode::GitTimeout)
        .with_context("operation", "confidential_paths")
        .with_context("timeout_ms", timeout.as_millis())
        .with_context("commit", commit.to_owned())
}

/// Rules 2 and 3 at `commit` (one read of its documents), within
/// `limits.timeout` — for a caller that has the specification index of the
/// commit already (rule 1 is then its own `confidential_paths`).
pub async fn documents_confidential_at(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
    limits: &ConfidentialLimits,
) -> Result<Confidential, ForgeError> {
    let read = read_tree(browse, repo, commit, &limits.doc);
    match tokio::time::timeout(limits.timeout, read).await {
        Ok(tree) => Ok(confidential_in(&tree?)),
        Err(_) => Err(timed_out(commit, limits.timeout)),
    }
}

/// Every rule at `commit` (module docs), within `limits.timeout`. Errors:
/// git's (the browse limits), `FG-GIT-0002` past the timeout; each with the
/// commit. A caller never serves on an error (fail closed).
pub async fn confidential_at(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
    limits: &ConfidentialLimits,
) -> Result<Confidential, ForgeError> {
    let both = async {
        let spec = index_commit(browse, repo, commit, &limits.spec, Tags::Skip).await?;
        let tree = read_tree(browse, repo, commit, &limits.doc).await?;
        let mut out = confidential_in(&tree);
        for path in spec.index.confidential_paths() {
            out.insert(path);
        }
        Ok::<_, ForgeError>(out)
    };
    match tokio::time::timeout(limits.timeout, both).await {
        Ok(answer) => answer,
        Err(_) => Err(timed_out(commit, limits.timeout)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

    use super::*;

    fn tree(files: &[(&str, &str)], incomplete: Option<&'static str>) -> DocTree {
        let mut t = DocTree::default();
        for (p, b) in files {
            t.add(p, b.as_bytes().to_vec());
        }
        t.sort();
        t.incomplete = incomplete;
        t
    }

    const NOTE_TYPE: (&str, &str) = (".forge/doctypes/note.yml", "name: note\n");
    const SECRET_TYPE: (&str, &str) = (
        ".forge/doctypes/hr.yml",
        "name: hr\ndefaults:\n  classification: confidential\n",
    );

    fn doc(id: &str, ty: &str, extra: &str) -> String {
        format!("---\nid: {id}\ndoctype: {ty}\ntitle: T\n{extra}---\nbody\n")
    }

    #[test]
    fn both_formats_type_defaults_and_unreadable_marks_are_confidential() {
        let class = doc("A", "note", "classification: confidential\n");
        let older = doc("B", "note", "confidential: true\n");
        let both = doc(
            "C",
            "note",
            "classification: internal\nconfidential: false\n",
        );
        let by_type = doc("D", "hr", "");
        let lowered = doc("E", "hr", "classification: internal\n");
        let no_type = doc("F", "missing", "");
        let internal = doc("G", "note", "classification: internal\n");
        let plain = doc("H", "note", "");
        let broken = "---\nid: [unclosed\n---\nbody\n";
        let t = tree(
            &[
                NOTE_TYPE,
                SECRET_TYPE,
                ("a/A.md", &class),
                ("b/B.md", &older),
                ("C.md", &both),
                ("D.md", &by_type),
                ("E.md", &lowered),
                ("F.md", &no_type),
                ("G.md", &internal),
                ("H.md", &plain),
                ("broken.md", broken),
                ("README.md", "# hello\n"),
                ("marked.md", "---\nclassification: confidential\n---\n"),
            ],
            None,
        );
        let c = confidential_in(&t);
        for p in [
            "a/A.md",
            "b/B.md",
            "C.md",
            "D.md",
            "E.md",
            "F.md",
            "broken.md",
            "marked.md",
        ] {
            assert!(c.contains(p), "{p} must be confidential");
        }
        for p in ["G.md", "H.md", "README.md", "src/lib.rs"] {
            assert!(!c.contains(p), "{p} must be open");
        }
        assert!(!c.is_partial());
        assert!(!c.is_empty());
        assert_eq!(c.first(2), vec!["C.md", "D.md"]);
        let open = tree(
            &[NOTE_TYPE, ("G.md", &internal), ("README.md", "# hi\n")],
            None,
        );
        assert!(confidential_in(&open).is_empty(), "nothing marked");
    }

    #[test]
    fn an_unreadable_type_definition_withholds_its_documents() {
        let d = doc("A", "note", "");
        let t = tree(
            &[(".forge/doctypes/note.yml", ": : not yaml ["), ("A.md", &d)],
            None,
        );
        assert!(confidential_in(&t).contains("A.md"));
    }

    #[test]
    fn a_translation_is_as_confidential_as_its_original() {
        let original = doc("A", "note", "classification: confidential\n");
        let translation = doc(
            "A",
            "note",
            "lang: en\ntranslation_of: A\nclassification: internal\n",
        );
        let t = tree(
            &[NOTE_TYPE, ("A.md", &original), ("A.en.md", &translation)],
            None,
        );
        assert!(confidential_in(&t).contains("A.en.md"));
    }

    #[test]
    fn a_bound_hit_withholds_every_unread_markdown_file() {
        let d = doc("A", "note", "");
        let t = tree(&[NOTE_TYPE, ("A.md", &d)], Some("doc_files"));
        let c = confidential_in(&t);
        assert!(c.is_partial());
        assert!(!c.is_empty(), "a partial answer is never empty");
        assert!(!c.contains("A.md"), "read and open");
        assert!(c.contains("unread.md"));
        assert!(!c.contains("src/main.rs"), "only documents are unknown");
        let all = Confidential::every_document();
        assert!(all.contains("README.md") && !all.contains("src/main.rs"));
    }
}
