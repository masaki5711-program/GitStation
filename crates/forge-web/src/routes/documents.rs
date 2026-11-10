//! The document API (plan 13 stage 3, §7.3 "文書の JSON", ADR-0026;
//! `RepoBrowse`, `read_api`): a project's documents in the plan 13 format
//! at a revision, read from its repository with the reader's permissions.
//!
//! | Route | Answer |
//! |---|---|
//! | `GET …/documents[?ref=&doctype=&limit=&after=]` | the documents (id, title, type, path, classification, translations), by id, paged |
//! | `GET …/documents/{doc_id}[?ref=&lang=]` | one document's view ([`forge_doc::render`]): front matter, sections of text and blocks, every `[[…]]` reference resolved |
//! | `GET …/documents/{doc_id}/referenced-by[?ref=&block=&limit=&after=]` | the documents (and translations) that refer to it or to one block, by key, paged ([`referenced_by`]) |
//! | `GET …/documents/{doc_id}/blocks/{block_id}[?ref=&lang=]` | one block alone with its references resolved ([`block`]) |
//!
//! (`…` is `/api/v1/projects/{id}`.) The registry authenticates, the
//! loader reads the project by id and `prove(RepoBrowse)` decides (the
//! blob API's rule: whoever may read the files may read the documents).
//! A revision is the browse API's (`ref`: a full commit id, a full or short
//! branch / tag name; default: the default branch).
//!
//! **The index** is the document lint's ([`forge_checks::doc_lint`]): one
//! listing of the whole tree and one read of its `.md` files and
//! `.forge/doctypes/*.yml` within the lint's bounds (a bound hit is
//! `incomplete {error_code: FG-DOC-0002, reason}`, never a silent cut).
//! A document is a `.md` whose front matter names a `doctype` and whose
//! file is named after its `id` (`<id>.md`; the first by path keeps an id,
//! as in the lint); a translation is `<id>.<lang>.md` with `lang` and
//! `translation_of: <id>`. The type's default classification is read from
//! `.forge/doctypes/<type>.yml`; a type that is missing or cannot be read
//! is `TypeDefault::Unresolved` (a document without its own mark is then
//! `confidential`: fail closed).
//!
//! **References** resolve against the same index ([`DocIndex`] is the
//! view's `Resolver`): a target more confidential than the document shown
//! is `hidden` — its id only, never its title or text (plan 13 Q14) — one
//! not in the index is `broken`, another repository's is `external`.
//!
//! **Plan 06 D-5**: for an agent audience
//! ([`crate::routes::context::audience`]) a `confidential` document is
//! answered as one that is not there (`FG-REPO-0005`, `reason=
//! doc_confidential` and the event `forge.context.withheld` in the log:
//! review stage 3, no existence oracle — decided before a translation is
//! looked up) and left out of the list, which does not say how many it
//! left out (`withheld` stays `null`: a count or a flag with `after` /
//! `doctype` would tell whether a confidential id lies in a range). A
//! document counts as confidential when it is at the revision read **or at
//! the default branch's tip** (as the context API: an agent cannot pick an
//! unmarked revision), by id and by the path of its file (the repository
//! read routes' rule, [`forge_checks::confidential`]); with no default
//! branch born every document does (fail closed). A reference's target is
//! measured the same way for an agent, so one confidential at the tip is
//! `hidden` at any revision.
//!
//! A document that is not there (no such id, a translation that is not
//! there) answers what an invisible project answers: `FG-REPO-0005`
//! (`reason=document_not_found` / `translation_not_found` in the log, with
//! `incomplete=` when the index hit a bound). Each request reads at most
//! two revisions under the spec API's budget, limit and in-flight cap
//! ([`crate::routes::specs::respond`]).

use axum::http::request::Parts;
use axum::response::Response;
use forge_auth::ProjectCtx;
use forge_checks::confidential::{confidential_in, type_defaults, type_of, Confidential};
use forge_checks::doc_lint::{read_tree, DocLimits, DocTree};
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_doc::document::{
    parse_document, reference_of, Classification, Document, TypeDefault, Value,
};
use forge_doc::render::{
    display_markdown, referrers, view_with_body, DocumentView, Piece, Referrer, Resolver,
    Target as RefTarget, TargetDoc, VIEW_VERSION,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::ResolvedRev;
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::browse::{
    load_project_lookup, query, rev_body, take, RepositoryOf, RevBody, Target,
};
use crate::routes::context::{audience, withheld, Audience, ContextWithheldBody};
use crate::routes::projects::PREFIX as PROJECT_PREFIX;
use crate::routes::specs::{caller_of, charge, respond, revision};

/// `GET` the documents of a revision.
pub const LIST: &str = "/api/v1/projects/{id}/documents";
/// `GET` one document's view.
pub const ONE: &str = "/api/v1/projects/{id}/documents/{doc_id}";

/// The loader of [`LIST`].
pub const LIST_OF: RepositoryOf = RepositoryOf("/documents");

/// Documents one list answers by default, and at most.
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
/// A document's parse problems one answer carries at most (the count is
/// whole).
pub const MAX_PROBLEMS: usize = 50;

const INFIX: &str = "/documents/";

const REF_PARAM: QueryParam = QueryParam {
    name: "ref",
    description: "A full commit id, refs/heads/… or refs/tags/…, or a short name (branch first, then tag); default: the default branch",
    integer_range: None,
};

/// The query parameters of [`LIST`].
pub const LIST_QUERY: &[QueryParam] = &[
    REF_PARAM,
    QueryParam {
        name: "doctype",
        description: "Only documents of this type (e.g. safety.risk-assessment)",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "At most this many documents (default 100)",
        integer_range: Some((1, MAX_LIMIT as i64)),
    },
    QueryParam {
        name: "after",
        description: "The previous page's next_after: only documents whose id sorts after it",
        integer_range: None,
    },
];

/// The query parameters of [`ONE`].
pub const ONE_QUERY: &[QueryParam] = &[
    REF_PARAM,
    QueryParam {
        name: "lang",
        description: "A translation (<id>.<lang>.md, e.g. en); default: the original",
        integer_range: None,
    },
];

/// `GET …/documents/{doc_id}/referenced-by`.
pub const REFERENCED_BY: &str = "/api/v1/projects/{id}/documents/{doc_id}/referenced-by";
/// `GET …/documents/{doc_id}/blocks/{block_id}`.
pub const BLOCK: &str = "/api/v1/projects/{id}/documents/{doc_id}/blocks/{block_id}";

const REFERENCED_BY_SUFFIX: &str = "/referenced-by";
const BLOCKS_INFIX: &str = "/blocks/";

/// The query parameters of [`REFERENCED_BY`].
pub const REFERENCED_BY_QUERY: &[QueryParam] = &[
    REF_PARAM,
    QueryParam {
        name: "block",
        description: "Only the references to this block of the document (e.g. H-01)",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "At most this many referrers (default 100)",
        integer_range: Some((1, MAX_LIMIT as i64)),
    },
    QueryParam {
        name: "after",
        description: "The previous page's next_after: only referrers whose key (<id> or <id>.<lang>) sorts after it",
        integer_range: None,
    },
];

/// The query parameters of [`BLOCK`].
pub const BLOCK_QUERY: &[QueryParam] = ONE_QUERY;

/// The loader of [`ONE`]: the project id before `/documents/` (the
/// `{doc_id}` is checked after the decision).
pub struct DocumentOf;

impl Loader<AppState> for DocumentOf {
    type Ctx = ProjectCtx;

    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let id = parts
                .uri
                .path()
                .strip_prefix(PROJECT_PREFIX)
                .and_then(|rest| rest.split_once(INFIX))
                .and_then(|(id, _)| (!id.contains('/')).then_some(id))
                .and_then(parse_positive_id)
                .map(ProjectId::new);
            let Some(id) = id else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            let found: Lookup<ProjectCtx> = load_project_lookup(state, id, viewer).await?;
            Ok(found)
        })
    }
}

// ── the answers ────────────────────────────────────────────────────────

/// A bound the read of the documents hit.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocIncompleteBody {
    /// `FG-DOC-0002`.
    pub error_code: String,
    /// `doc_tree`, `doc_files`, `doc_large_files`, `doc_bytes` or
    /// `doctype_files`.
    pub reason: String,
}

/// One document of the list.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentSummaryBody {
    pub id: String,
    /// `null` when the front matter has none.
    #[schema(required = true)]
    pub title: Option<String>,
    pub doctype: String,
    pub path: String,
    /// `public`, `internal` or `confidential` (plan 06 D-5: how an AI may
    /// be given it; not access control).
    pub classification: String,
    /// The languages of its translations, sorted.
    pub translations: Vec<String>,
}

/// A revision's documents.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentListBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// By id.
    pub documents: Vec<DocumentSummaryBody>,
    /// More documents follow: ask again with `after=<next_after>`.
    #[schema(required = true)]
    pub next_after: Option<String>,
    #[schema(required = true)]
    pub incomplete: Option<DocIncompleteBody>,
    /// Always `null` (plan 06 D-5, review stage 3: an agent's list does not
    /// say what it left out; the log does). Kept for the clients.
    #[schema(required = true)]
    pub withheld: Option<ContextWithheldBody>,
}

/// A front matter field the base keys do not cover.
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentFieldBody {
    pub key: String,
    /// As written: a string, a list, a map or `null`.
    #[schema(value_type = Value)]
    pub value: serde_json::Value,
}

/// A block's attribute, as written.
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentAttrBody {
    pub key: String,
    pub value: String,
}

/// A block (`::: <kind> <id> <attrs>`).
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentBlockBody {
    pub kind: String,
    /// `null` for a plain container (`::: note`).
    #[schema(required = true)]
    pub id: Option<String>,
    /// In the written order.
    pub attrs: Vec<DocumentAttrBody>,
    /// `status=deprecated`.
    pub deprecated: bool,
    /// The body as written (one sentence per line).
    pub body: String,
    /// The body for display: Markdown with the prose lines joined
    /// (`forge_doc::render::display_markdown`, plan 13 §4.1).
    pub display: String,
}

/// A piece of a section, in order: Markdown text, or a block.
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentPieceBody {
    /// `text` or `block`.
    #[serde(rename = "type")]
    pub kind: String,
    /// 1-based.
    pub line: usize,
    /// The text as written (`type: text`; `null` for a block).
    #[schema(required = true)]
    pub markdown: Option<String>,
    /// The text for display (`type: text`): prose lines joined.
    #[schema(required = true)]
    pub display: Option<String>,
    /// `type: block`.
    #[schema(required = true)]
    pub block: Option<DocumentBlockBody>,
}

/// A level-2 section.
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentSectionBody {
    /// `null`: the text before the first heading.
    #[schema(required = true)]
    pub heading: Option<String>,
    pub line: usize,
    pub pieces: Vec<DocumentPieceBody>,
}

/// A `[[…]]` reference and how it resolved.
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentRefBody {
    /// Another repository's (`[[repo:ID]]`); `null` for this one.
    #[schema(required = true)]
    pub repo: Option<String>,
    pub doc: String,
    #[schema(required = true)]
    pub block: Option<String>,
    pub line: usize,
    /// `found` (a document of this repository the reader may be shown),
    /// `hidden` (more confidential than this document: its id only — plan
    /// 13 Q14), `broken` (no such document or block) or `external` (another
    /// repository's; not resolved here).
    pub target: String,
    /// `found` only.
    #[schema(required = true)]
    pub title: Option<String>,
    /// `found` with a block only: the block's kind.
    #[schema(required = true)]
    pub block_kind: Option<String>,
}

/// A rule the document's text breaks (the parser's; `gstn doc lint` lists
/// them all with the type's).
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentProblemBody {
    pub line: usize,
    pub rule: String,
    #[schema(required = true)]
    pub subject: Option<String>,
}

/// One document's view (plan 13 §7.3).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentBody {
    /// The view's schema version (`forge_doc::render::VIEW_VERSION`): a
    /// change that removes or renames a field raises it.
    pub version: u32,
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// The file shown (the translation's when `lang` is set).
    pub path: String,
    /// The translation shown; `null`: the original.
    #[schema(required = true)]
    pub lang: Option<String>,
    /// The languages of the document's translations, sorted.
    pub translations: Vec<String>,
    pub id: String,
    pub doctype: String,
    #[schema(required = true)]
    pub title: Option<String>,
    #[schema(required = true)]
    pub owner: Option<String>,
    /// `public`, `internal` or `confidential` (the type's default when
    /// the document has none; `confidential` when the type cannot be read).
    pub classification: String,
    pub fields: Vec<DocumentFieldBody>,
    pub sections: Vec<DocumentSectionBody>,
    pub refs: Vec<DocumentRefBody>,
    /// The first parse problems ([`MAX_PROBLEMS`]).
    pub problems: Vec<DocumentProblemBody>,
    /// Problems in all (with those not listed).
    pub problem_count: usize,
    #[schema(required = true)]
    pub incomplete: Option<DocIncompleteBody>,
}

/// One reference of a referrer to the target.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferrerRefBody {
    pub line: usize,
    /// The block named (`[[ID#BLOCK]]`); `null`: the document.
    #[schema(required = true)]
    pub block: Option<String>,
}

/// A document (or translation) that refers to the target.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferrerBody {
    pub id: String,
    /// A translation's language; `null`: the original.
    #[schema(required = true)]
    pub lang: Option<String>,
    /// More confidential than the target (plan 13 Q14): its id only —
    /// `title`, `doctype`, `path` and `classification` are `null`, `refs`
    /// is empty.
    pub hidden: bool,
    #[schema(required = true)]
    pub title: Option<String>,
    #[schema(required = true)]
    pub doctype: Option<String>,
    #[schema(required = true)]
    pub path: Option<String>,
    #[schema(required = true)]
    pub classification: Option<String>,
    /// Its references to the target, in the written order.
    pub refs: Vec<ReferrerRefBody>,
}

/// The documents that refer to one document (or one of its blocks).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferrersBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    pub id: String,
    #[schema(required = true)]
    pub block: Option<String>,
    /// By key (`<id>`, or `<id>.<lang>` for a translation).
    pub referrers: Vec<ReferrerBody>,
    /// Referrers in all (a person's answer); `null` for an agent audience
    /// (plan 06 D-5: a count would tell what was left out).
    #[schema(required = true)]
    pub total: Option<usize>,
    /// More referrers follow: ask again with `after=<next_after>`.
    #[schema(required = true)]
    pub next_after: Option<String>,
    /// A bound of the read (`FG-DOC-0002`), or `reason=doc_refs`: a
    /// document's references hit the parser's cap, so a referrer may be
    /// missing.
    #[schema(required = true)]
    pub incomplete: Option<DocIncompleteBody>,
}

/// One block of a document, alone.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentBlockAloneBody {
    pub version: u32,
    #[serde(rename = "ref")]
    pub rev: RevBody,
    pub path: String,
    #[schema(required = true)]
    pub lang: Option<String>,
    pub id: String,
    pub doctype: String,
    #[schema(required = true)]
    pub title: Option<String>,
    pub classification: String,
    /// The level-2 heading the block is under.
    #[schema(required = true)]
    pub section: Option<String>,
    pub block: DocumentBlockAtBody,
    /// The `[[…]]` references inside the block, resolved as in the
    /// document's view.
    pub refs: Vec<DocumentRefBody>,
    #[schema(required = true)]
    pub incomplete: Option<DocIncompleteBody>,
}

/// A block with where it is (the block route).
#[derive(Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentBlockAtBody {
    pub kind: String,
    pub id: String,
    /// In the written order.
    pub attrs: Vec<DocumentAttrBody>,
    pub deprecated: bool,
    /// The body as written.
    pub body: String,
    /// The body for display (prose lines joined).
    pub display: String,
    /// The opening line (1-based).
    pub line: usize,
    /// The closing `:::`.
    pub end_line: usize,
}

// ── the index ──────────────────────────────────────────────────────────

/// A document (or a translation) of the index.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) path: String,
    pub(crate) id: String,
    pub(crate) doctype: String,
    /// A translation's language (`None`: the original).
    pub(crate) lang: Option<String>,
    pub(crate) doc: Document,
    pub(crate) ty: TypeDefault,
    pub(crate) classification: Classification,
    /// Its bytes: an index into [`DocTree::markdown`].
    pub(crate) file: usize,
}

/// One revision's documents (module docs): the originals by id, the
/// translations, and each type's default.
#[derive(Debug)]
pub(crate) struct DocIndex {
    pub(crate) tree: DocTree,
    /// By id.
    pub(crate) originals: Vec<Entry>,
    /// By id, then language.
    pub(crate) translations: Vec<Entry>,
    /// The files an agent is not given at this revision (the repository
    /// read routes' rule, [`forge_checks::confidential::confidential_in`]):
    /// compared by path, so a file that is no document here (a front matter
    /// that cannot be read) still counts.
    pub(crate) files: Confidential,
}

/// The file name without `.md` (`None`: not a `.md`).
fn stem_of(path: &str) -> Option<&str> {
    path.rsplit('/').next()?.strip_suffix(".md")
}

impl DocIndex {
    /// Index `tree` (sorted by path: the first file by path keeps an id).
    pub(crate) fn build(tree: DocTree) -> Self {
        let types = type_defaults(&tree);
        let mut originals: Vec<Entry> = Vec::new();
        let mut translations: Vec<Entry> = Vec::new();
        for (file, (path, bytes)) in tree.markdown.iter().enumerate() {
            let doc = parse_document(bytes);
            let (Some(id), Some(doctype), Some(stem)) = (doc.id(), doc.doctype(), stem_of(path))
            else {
                continue;
            };
            let ty = type_of(&types, doctype);
            let (id, doctype) = (id.to_owned(), doctype.to_owned());
            let lang = doc.meta_text("lang").map(str::to_owned);
            let of = doc.meta_text("translation_of");
            let original = stem == id && of.is_none();
            let translation = of == Some(id.as_str())
                && lang.as_deref().is_some_and(|l| stem == format!("{id}.{l}"));
            let list = if original {
                &mut originals
            } else if translation {
                &mut translations
            } else {
                continue; // named after another id: the lint's id_not_filename
            };
            let lang = if original { None } else { lang };
            if list.iter().any(|e| e.id == id && e.lang == lang) {
                continue; // the lint's id_duplicate: the first by path keeps it
            }
            let classification = doc.classification(ty);
            list.push(Entry {
                path: path.clone(), // clone: the index owns its strings
                id,
                doctype,
                lang,
                doc,
                ty,
                classification,
                file,
            });
        }
        originals.sort_by(|a, b| a.id.cmp(&b.id));
        translations.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.lang.cmp(&b.lang)));
        let files = confidential_in(&tree);
        Self {
            tree,
            originals,
            translations,
            files,
        }
    }

    pub(crate) fn original(&self, id: &str) -> Option<&Entry> {
        self.originals.iter().find(|e| e.id == id)
    }

    pub(crate) fn translation(&self, id: &str, lang: &str) -> Option<&Entry> {
        self.translations
            .iter()
            .find(|e| e.id == id && e.lang.as_deref() == Some(lang))
    }

    pub(crate) fn languages(&self, id: &str) -> Vec<String> {
        self.translations
            .iter()
            .filter(|e| e.id == id)
            .filter_map(|e| e.lang.clone()) // clone: the answer owns its strings
            .collect()
    }

    /// The text of `entry`, when the parser could read it whole.
    pub(crate) fn text(&self, entry: &Entry) -> Option<&str> {
        let unreadable = entry
            .doc
            .problems
            .iter()
            .any(|p| matches!(p.rule, "not_utf8" | "too_large"));
        if unreadable {
            return None;
        }
        let (_, bytes) = self.tree.markdown.get(entry.file)?;
        std::str::from_utf8(bytes).ok()
    }

    /// The incomplete marker of the answer.
    pub(crate) fn incomplete(&self) -> Option<DocIncompleteBody> {
        self.tree.incomplete.map(|reason| DocIncompleteBody {
            error_code: ErrorCode::DocIndexBounded.id().to_owned(),
            reason: reason.to_owned(),
        })
    }
}

impl Resolver for DocIndex {
    /// Every document of the repository is one the reader may see (the
    /// repository's permission decided the request); absent ones are
    /// `None` (broken). The view hides the more confidential ones.
    fn document(&self, id: &str) -> Option<TargetDoc> {
        let e = self.original(id)?;
        Some(TargetDoc {
            title: e.doc.meta_text("title").unwrap_or_default().to_owned(),
            classification: e.classification,
            blocks: e
                .doc
                .blocks
                .iter()
                .filter_map(|b| Some((b.id.clone()?, b.kind.clone())))
                .collect(),
        })
    }
}

// ── the mapping (forge-doc has no serde) ───────────────────────────────

pub(crate) fn value_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Text(t) => serde_json::Value::String(t.clone()),
        Value::List(items) => items.iter().map(|(_, v)| value_json(v)).collect(),
        Value::Map(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(k, _, v)| (k.clone(), value_json(v)))
                .collect(),
        ),
    }
}

pub(crate) fn target_word(t: &RefTarget) -> &'static str {
    match t {
        RefTarget::Found { .. } => "found",
        RefTarget::Hidden => "hidden",
        RefTarget::Broken => "broken",
        RefTarget::External => "external",
    }
}

/// The view's fields, sections and references as the answer carries them.
pub(crate) fn view_parts(
    view: &DocumentView,
) -> (
    Vec<DocumentFieldBody>,
    Vec<DocumentSectionBody>,
    Vec<DocumentRefBody>,
) {
    let fields = view
        .fields
        .iter()
        .map(|f| DocumentFieldBody {
            key: f.key.clone(), // clone: the answer owns its strings
            value: value_json(&f.value),
        })
        .collect();
    let sections = view
        .sections
        .iter()
        .map(|s| DocumentSectionBody {
            heading: s.heading.clone(), // clone: as above
            line: s.line,
            pieces: s
                .pieces
                .iter()
                .map(|p| match p {
                    Piece::Text { markdown, line } => DocumentPieceBody {
                        kind: "text".to_owned(),
                        line: *line,
                        markdown: Some(markdown.clone()), // clone: as above
                        display: Some(display_markdown(markdown)),
                        block: None,
                    },
                    Piece::Block(b) => DocumentPieceBody {
                        kind: "block".to_owned(),
                        line: b.line,
                        markdown: None,
                        display: None,
                        block: Some(DocumentBlockBody {
                            kind: b.kind.clone(), // clone: as above
                            id: b.id.clone(),     // clone: as above
                            attrs: b
                                .attrs
                                .iter()
                                .map(|(key, value)| DocumentAttrBody {
                                    key: key.clone(),     // clone: as above
                                    value: value.clone(), // clone: as above
                                })
                                .collect(),
                            deprecated: b.deprecated,
                            body: b.body.clone(), // clone: as above
                            display: display_markdown(&b.body),
                        }),
                    },
                })
                .collect(),
        })
        .collect();
    let refs = view
        .refs
        .iter()
        .map(|r| {
            let (title, block_kind) = match &r.target {
                RefTarget::Found { title, block_kind } => (Some(title.clone()), block_kind.clone()), // clone: as above
                RefTarget::Hidden | RefTarget::Broken | RefTarget::External => (None, None),
            };
            DocumentRefBody {
                repo: r.repo.clone(), // clone: as above
                doc: r.doc.clone(),   // clone: as above
                block: r.block.clone(),
                line: r.line,
                target: target_word(&r.target).to_owned(),
                title,
                block_kind,
            }
        })
        .collect();
    (fields, sections, refs)
}

// ── the handlers ───────────────────────────────────────────────────────

/// A document or block id: a reference's id (ASCII letters, digits, `.`,
/// `_`, `-`; at most 128 bytes) — `FG-WEB-0006 param=<name>`.
fn ref_id(name: &'static str, raw: &str) -> Result<String, ForgeError> {
    match reference_of(raw, 0) {
        Some(r) if r.repo.is_none() && r.block.is_none() && r.doc == raw => Ok(r.doc),
        _ => Err(param_invalid(name, "invalid")),
    }
}

/// The path after `/documents/`.
fn after_infix(raw_path: &str) -> Result<&str, ForgeError> {
    raw_path
        .split_once(INFIX)
        .map(|(_, rest)| rest)
        .ok_or_else(|| param_invalid("doc_id", "missing"))
}

/// The document id of [`ONE`]'s path.
fn doc_id_param(raw_path: &str) -> Result<String, ForgeError> {
    ref_id("doc_id", after_infix(raw_path)?)
}

/// The document id of [`REFERENCED_BY`]'s path.
fn referenced_by_param(raw_path: &str) -> Result<String, ForgeError> {
    let id = after_infix(raw_path)?
        .strip_suffix(REFERENCED_BY_SUFFIX)
        .ok_or_else(|| param_invalid("doc_id", "invalid"))?;
    ref_id("doc_id", id)
}

/// The document and block ids of [`BLOCK`]'s path.
fn block_params(raw_path: &str) -> Result<(String, String), ForgeError> {
    let (doc, block) = after_infix(raw_path)?
        .split_once(BLOCKS_INFIX)
        .ok_or_else(|| param_invalid("block_id", "missing"))?;
    Ok((ref_id("doc_id", doc)?, ref_id("block_id", block)?))
}

/// `en`, `ja`, `pt-BR` (the lint's `lang`).
fn lang_param(raw: Option<&str>) -> Result<Option<String>, ForgeError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let (lang, region) = raw.split_once('-').unwrap_or((raw, ""));
    let ok = lang.len() == 2
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && (region.is_empty()
            || region.len() == 2 && region.bytes().all(|b| b.is_ascii_uppercase()));
    if ok {
        Ok(Some(raw.to_owned()))
    } else {
        Err(param_invalid("lang", "invalid"))
    }
}

fn limit_param(params: &[(&'static str, String)]) -> Result<usize, ForgeError> {
    match take(params, "limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| param_invalid("limit", "out_of_range")),
    }
}

/// The documents of `rev` (one read of its tree).
async fn index_at(t: &Target, rev: &ResolvedRev) -> Result<DocIndex, ForgeError> {
    let tree = read_tree(
        &t.smart.browse,
        t.repository.id,
        &rev.commit,
        &DocLimits::default(),
    )
    .await?;
    Ok(DocIndex::build(tree))
}

/// What an answer is measured against at the default branch's tip
/// (review X5 MEDIUM a: a revision of the agent's choosing does not unmark
/// a document).
pub(crate) enum TipMarks {
    /// A person's answer: nothing is withheld.
    Person,
    /// The revision read is the tip: its index already says.
    IsTip,
    /// The tip's index (one more read, charged).
    Read(Box<DocIndex>),
    /// The default branch is not born: nothing to measure against, so an
    /// agent is given no document (review stage 3 LOW: fail closed).
    NoTip,
}

impl TipMarks {
    fn index(&self) -> Option<&DocIndex> {
        match self {
            Self::Read(i) => Some(i),
            Self::Person | Self::IsTip | Self::NoTip => None,
        }
    }
}

/// [`TipMarks`] of `rev` for `who`.
async fn tip_marks(
    state: &AppState,
    caller: UserId,
    t: &Target,
    who: Audience,
    rev: &ResolvedRev,
) -> Result<TipMarks, ForgeError> {
    if !who.is_agent() {
        return Ok(TipMarks::Person);
    }
    let tip = match revision(t, None).await {
        Ok(tip) => tip,
        Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => return Ok(TipMarks::NoTip),
        Err(e) => return Err(e),
    };
    if tip.commit == rev.commit {
        return Ok(TipMarks::IsTip);
    }
    charge(state, caller, 1)?;
    Ok(TipMarks::Read(Box::new(index_at(t, &tip).await?)))
}

/// Whether an agent may not be given `id` (in `lang`, when that
/// translation is there): confidential at the revision or at the tip —
/// by id, and by the path of its file(s) here (review stage 3 LOW: a file
/// whose front matter cannot be read at the tip has no id to compare) —
/// or there is no tip ([`TipMarks::NoTip`]). Always `false` for a person.
fn secret(index: &DocIndex, tip: &TipMarks, id: &str, lang: Option<&str>) -> bool {
    match tip {
        TipMarks::Person => return false,
        TipMarks::NoTip => return true,
        TipMarks::IsTip | TipMarks::Read(_) => {}
    }
    let paths: Vec<&str> = [
        index.original(id),
        lang.and_then(|l| index.translation(id, l)),
    ]
    .into_iter()
    .flatten()
    .map(|e| e.path.as_str())
    .collect();
    let marked = |i: &DocIndex| {
        let by_id = [i.original(id), lang.and_then(|l| i.translation(id, l))]
            .into_iter()
            .flatten()
            .any(|e| e.classification == Classification::Confidential);
        by_id || paths.iter().any(|p| i.files.contains(p))
    };
    marked(index) || tip.index().is_some_and(marked)
}

/// The view's resolver for an agent (review stage 3 MEDIUM): a reference's
/// target counts as `confidential` when [`secret`] says so — at the tip as
/// well as at the revision — so the view hides it (its id only).
struct AgentResolver<'a> {
    index: &'a DocIndex,
    tip: &'a TipMarks,
}

impl Resolver for AgentResolver<'_> {
    fn document(&self, id: &str) -> Option<TargetDoc> {
        let mut target = self.index.document(id)?;
        if secret(self.index, self.tip, id, None) {
            target.classification = Classification::Confidential;
        }
        Some(target)
    }
}

/// The original `id` the audience may be given (with `lang`'s translation
/// measured too), or the absent document's 404: `document_not_found`, or
/// `doc_confidential` with the withheld event (review stage 3 LOW: decided
/// before a translation or a block is looked up).
fn visible_original<'i>(
    request: &Dispatched<ProjectCtx>,
    route: &'static str,
    who: Audience,
    (index, tip): (&'i DocIndex, &TipMarks),
    id: &str,
    lang: Option<&str>,
) -> Result<&'i Entry, ForgeError> {
    let Some(original) = index.original(id) else {
        return Err(not_found("document_not_found", index).with_context("doc_id", id));
    };
    if secret(index, tip, id, lang) {
        let _logged = withheld(request, route, who, 1, 0, 0);
        return Err(not_found("doc_confidential", index)
            .with_context("doc_id", id)
            .with_context("path", &original.path));
    }
    Ok(original)
}

/// A referrer's sort and cursor key: `<id>`, or `<id>.<lang>` (its file
/// name without `.md`).
fn key_of(e: &Entry) -> String {
    match &e.lang {
        Some(l) => format!("{}.{l}", e.id),
        None => e.id.clone(), // clone: the key is owned
    }
}

fn not_found(reason: &'static str, index: &DocIndex) -> ForgeError {
    let err = ForgeError::new(ErrorCode::NotFound).with_context("reason", reason);
    match index.tree.incomplete {
        Some(bound) => err.with_context("incomplete", bound),
        None => err,
    }
}

/// `GET /api/v1/projects/{id}/documents`.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "documents_list",
        |state, request, t| async move {
            let params = query(request.raw_query(), &["ref", "doctype", "limit", "after"])?;
            let who = audience(&request)?;
            let limit = limit_param(&params)?;
            let doctype = take(&params, "doctype").filter(|d| !d.is_empty());
            let after = take(&params, "after").filter(|a| !a.is_empty());
            let rev = revision(&t, take(&params, "ref")).await?;
            let index = index_at(&t, &rev).await?;
            let caller = caller_of(&request)?;
            let tip = tip_marks(&state, caller, &t, who, &rev).await?;
            let mut withheld_documents = 0usize;
            let mut documents: Vec<DocumentSummaryBody> = Vec::new();
            let mut next_after = None;
            for e in &index.originals {
                if doctype.is_some_and(|d| d != e.doctype) {
                    continue;
                }
                if after.is_some_and(|a| e.id.as_str() <= a) {
                    continue;
                }
                if secret(&index, &tip, &e.id, None) {
                    withheld_documents += 1;
                    continue;
                }
                if documents.len() == limit {
                    // One more is shown on the next page: this page ends here.
                    next_after = documents.last().map(|d| d.id.clone()); // clone: the cursor
                    break;
                }
                documents.push(DocumentSummaryBody {
                    id: e.id.clone(), // clone: the answer owns its strings
                    title: e.doc.meta_text("title").map(str::to_owned),
                    doctype: e.doctype.clone(), // clone: as above
                    path: e.path.clone(),       // clone: as above
                    classification: e.classification.as_str().to_owned(),
                    translations: index.languages(&e.id),
                });
            }
            // Logged once; never answered (review stage 3 LOW: a count, or
            // even a flag, with `after` / `doctype` tells whether a
            // confidential id lies in a range).
            let _logged = withheld(&request, "documents_list", who, withheld_documents, 0, 0);
            Ok(DocumentListBody {
                rev: rev_body(&rev),
                documents,
                next_after,
                incomplete: index.incomplete(),
                withheld: None,
            })
        },
    )
    .await
}

/// `GET /api/v1/projects/{id}/documents/{doc_id}`.
pub async fn one(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "documents_show",
        |state, request, t| async move {
            let params = query(request.raw_query(), &["ref", "lang"])?;
            let who = audience(&request)?;
            let id = doc_id_param(request.raw_path())?;
            let lang = lang_param(take(&params, "lang"))?;
            let rev = revision(&t, take(&params, "ref")).await?;
            let index = index_at(&t, &rev).await?;
            // Read before anything is looked up: the cost does not tell
            // whether the document is there.
            let caller = caller_of(&request)?;
            let tip = tip_marks(&state, caller, &t, who, &rev).await?;
            let original = visible_original(
                &request,
                "documents_show",
                who,
                (&index, &tip),
                &id,
                lang.as_deref(),
            )?;
            let shown = match lang.as_deref() {
                None => original,
                Some(l) => index.translation(&id, l).ok_or_else(|| {
                    not_found("translation_not_found", &index)
                        .with_context("doc_id", id.clone()) // clone: the log's copy
                        .with_context("lang", l.to_owned())
                })?,
            };
            let agent_view = AgentResolver {
                index: &index,
                tip: &tip,
            };
            let resolver: &dyn Resolver = if who.is_agent() { &agent_view } else { &index };
            let view = match index.text(shown) {
                Some(text) => view_with_body(text, &shown.doc, shown.ty, resolver),
                None => forge_doc::render::view(&shown.doc, shown.ty, resolver),
            };
            let (fields, sections, refs) = view_parts(&view);
            let classification = view.classification.max(original.classification);
            Ok(DocumentBody {
                version: VIEW_VERSION,
                rev: rev_body(&rev),
                path: shown.path.clone(), // clone: the answer owns its strings
                lang: shown.lang.clone(), // clone: as above
                translations: index.languages(&id),
                id,
                doctype: shown.doctype.clone(), // clone: as above
                title: view.title,
                owner: view.owner,
                classification: classification.as_str().to_owned(),
                fields,
                sections,
                refs,
                problems: shown
                    .doc
                    .problems
                    .iter()
                    .take(MAX_PROBLEMS)
                    .map(|p| DocumentProblemBody {
                        line: p.line,
                        rule: p.rule.to_owned(),
                        subject: p.subject.clone(), // clone: as above
                    })
                    .collect(),
                problem_count: shown.doc.problems.len() + shown.doc.problems_dropped,
                incomplete: index.incomplete(),
            })
        },
    )
    .await
}

/// `GET /api/v1/projects/{id}/documents/{doc_id}/referenced-by`: the
/// documents of the revision that refer to it (or to its `block`), by key,
/// paged — computed on read over the same one read of the tree
/// ([`forge_doc::render::referrers`]). For an agent audience the target is
/// measured first ([`visible_original`]) and a referrer it may not be given
/// is left out uncounted (`total` is `null`); a referrer more confidential
/// than the target is its id only (plan 13 Q14). A document whose
/// references hit the parser's cap makes the answer `incomplete`
/// (`reason=doc_refs`). Another repository's referrers are not looked for.
pub async fn referenced_by(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    const ROUTE: &str = "documents_referenced_by";
    respond(state, request, ROUTE, |state, request, t| async move {
        let params = query(request.raw_query(), &["ref", "block", "limit", "after"])?;
        let who = audience(&request)?;
        let id = referenced_by_param(request.raw_path())?;
        let block = take(&params, "block")
            .map(|b| ref_id("block", b))
            .transpose()?;
        let limit = limit_param(&params)?;
        let after = take(&params, "after")
            .filter(|a| !a.is_empty())
            .map(referrer_key_param)
            .transpose()?;
        let rev = revision(&t, take(&params, "ref")).await?;
        let index = index_at(&t, &rev).await?;
        let caller = caller_of(&request)?;
        let tip = tip_marks(&state, caller, &t, who, &rev).await?;
        let target = visible_original(&request, ROUTE, who, (&index, &tip), &id, None)?;
        if let Some(b) = &block {
            if target.doc.block(b).is_none() {
                return Err(not_found("block_not_found", &index)
                    .with_context("doc_id", &id)
                    .with_context("block_id", b));
            }
        }
        let entries: Vec<&Entry> = index
            .originals
            .iter()
            .chain(index.translations.iter())
            .collect();
        let docs: Vec<&Document> = entries.iter().map(|e| &e.doc).collect();
        let found = referrers(&docs, &id, block.as_deref());
        // An agent is given neither a confidential referrer nor, by id, one
        // more confidential than the target (plan 06 D-5: nothing it may not
        // read enters its context); a person sees the latter by id only.
        let given = |e: &Entry| {
            !secret(&index, &tip, &e.id, e.lang.as_deref())
                && !(who.is_agent() && more_confidential(e, &index, target.classification))
        };
        // Only a document the audience may be given tells it something.
        let capped = found
            .capped
            .iter()
            .filter_map(|i| entries.get(*i))
            .any(|e| given(e));
        let mut withheld_documents = 0usize;
        let mut shown: Vec<(String, &Entry, &Referrer)> = Vec::new();
        for r in &found.found {
            let Some(e) = entries.get(r.doc) else {
                continue; // an index of `docs`: always there
            };
            if given(e) {
                shown.push((key_of(e), e, r));
            } else {
                withheld_documents += 1;
            }
        }
        shown.sort_by(|a, b| a.0.cmp(&b.0));
        let total = shown.len();
        let mut page: Vec<ReferrerBody> = Vec::new();
        let mut last: Option<&str> = None;
        let mut next_after = None;
        for (key, e, r) in &shown {
            if after.as_deref().is_some_and(|a| key.as_str() <= a) {
                continue;
            }
            if page.len() == limit {
                // One more is shown on the next page: this page ends here.
                next_after = last.map(str::to_owned);
                break;
            }
            last = Some(key.as_str());
            page.push(referrer_body(e, r, &index, target.classification));
        }
        // Logged once; never answered (as the list).
        let _logged = withheld(&request, ROUTE, who, withheld_documents, 0, 0);
        let incomplete = index.incomplete().or_else(|| {
            capped.then(|| DocIncompleteBody {
                error_code: ErrorCode::DocIndexBounded.id().to_owned(),
                reason: "doc_refs".to_owned(),
            })
        });
        Ok(ReferrersBody {
            rev: rev_body(&rev),
            id,
            block,
            referrers: page,
            total: (!who.is_agent()).then_some(total),
            next_after,
            incomplete,
        })
    })
    .await
}

/// A referrer's classification: a translation is at least as confidential
/// as its original.
fn effective_classification(e: &Entry, index: &DocIndex) -> Classification {
    index
        .original(&e.id)
        .map_or(e.classification, |o| o.classification.max(e.classification))
}

/// Whether a referrer is more confidential than the target.
fn more_confidential(e: &Entry, index: &DocIndex, target: Classification) -> bool {
    effective_classification(e, index) > target
}

/// `after` of [`REFERENCED_BY`]: a referrer key, `<id>` or `<id>.<lang>`
/// (`FG-WEB-0006 param=after`).
fn referrer_key_param(raw: &str) -> Result<String, ForgeError> {
    if ref_id("after", raw).is_ok() {
        return Ok(raw.to_owned());
    }
    match raw.rsplit_once('.') {
        Some((id, lang))
            if ref_id("after", id).is_ok() && lang_param(Some(lang)).ok().flatten().is_some() =>
        {
            Ok(raw.to_owned())
        }
        _ => Err(param_invalid("after", "invalid")),
    }
}

/// One referrer as answered: whole, or its id only when it is more
/// confidential than the target (a translation is at least as confidential
/// as its original).
fn referrer_body(
    e: &Entry,
    r: &Referrer,
    index: &DocIndex,
    target: Classification,
) -> ReferrerBody {
    let classification = effective_classification(e, index);
    let id = e.id.clone(); // clone: the answer owns its strings
    let lang = e.lang.clone(); // clone: as above
    if classification > target {
        return ReferrerBody {
            id,
            lang,
            hidden: true,
            title: None,
            doctype: None,
            path: None,
            classification: None,
            refs: Vec::new(),
        };
    }
    ReferrerBody {
        id,
        lang,
        hidden: false,
        title: e.doc.meta_text("title").map(str::to_owned),
        doctype: Some(e.doctype.clone()), // clone: as above
        path: Some(e.path.clone()),       // clone: as above
        classification: Some(classification.as_str().to_owned()),
        refs: r
            .refs
            .iter()
            .map(|(line, block)| ReferrerRefBody {
                line: *line,
                block: block.clone(), // clone: as above
            })
            .collect(),
    }
}

/// `GET /api/v1/projects/{id}/documents/{doc_id}/blocks/{block_id}`: one
/// block alone (of the translation `lang`, when given) with the references
/// inside it resolved as in the document's view. A block that is not there
/// answers the absent document's 404 (`reason=block_not_found`); for an
/// agent audience the document is measured before the block is looked up.
pub async fn block(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    const ROUTE: &str = "documents_block";
    respond(state, request, ROUTE, |state, request, t| async move {
        let params = query(request.raw_query(), &["ref", "lang"])?;
        let who = audience(&request)?;
        let (id, block_id) = block_params(request.raw_path())?;
        let lang = lang_param(take(&params, "lang"))?;
        let rev = revision(&t, take(&params, "ref")).await?;
        let index = index_at(&t, &rev).await?;
        let caller = caller_of(&request)?;
        let tip = tip_marks(&state, caller, &t, who, &rev).await?;
        let original =
            visible_original(&request, ROUTE, who, (&index, &tip), &id, lang.as_deref())?;
        let shown = match lang.as_deref() {
            None => original,
            Some(l) => index.translation(&id, l).ok_or_else(|| {
                not_found("translation_not_found", &index)
                    .with_context("doc_id", &id)
                    .with_context("lang", l)
            })?,
        };
        let Some(b) = shown.doc.block(&block_id) else {
            return Err(not_found("block_not_found", &index)
                .with_context("doc_id", &id)
                .with_context("block_id", &block_id));
        };
        let agent_view = AgentResolver {
            index: &index,
            tip: &tip,
        };
        let resolver: &dyn Resolver = if who.is_agent() { &agent_view } else { &index };
        let view = forge_doc::render::view(&shown.doc, shown.ty, resolver);
        let (_, _, refs) = view_parts(&view);
        let lines = b.line..=b.end_line.max(b.line);
        let refs = refs
            .into_iter()
            .filter(|r| lines.contains(&r.line))
            .collect();
        let classification = view.classification.max(original.classification);
        Ok(DocumentBlockAloneBody {
            version: VIEW_VERSION,
            rev: rev_body(&rev),
            path: shown.path.clone(), // clone: the answer owns its strings
            lang: shown.lang.clone(), // clone: as above
            doctype: shown.doctype.clone(), // clone: as above
            title: view.title,
            classification: classification.as_str().to_owned(),
            section: b
                .section
                .and_then(|i| shown.doc.headings.get(i))
                .map(|h| h.text.clone()), // clone: as above
            block: DocumentBlockAtBody {
                kind: b.kind.clone(), // clone: as above
                id: block_id,
                attrs: b
                    .attrs
                    .iter()
                    .map(|a| DocumentAttrBody {
                        key: a.key.clone(),     // clone: as above
                        value: a.value.clone(), // clone: as above
                    })
                    .collect(),
                deprecated: b.deprecated(),
                body: b.body.clone(), // clone: as above
                display: display_markdown(&b.body),
                line: b.line,
                end_line: b.end_line,
            },
            refs,
            id,
            incomplete: index.incomplete(),
        })
    })
    .await
}

#[cfg(test)]
#[path = "documents_tests.rs"]
mod tests;
