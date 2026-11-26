//! The lint: documents checked against their type and against each other
//! (plan 13 §5, ADR-0026). Deterministic checks only — required fields and
//! headings, id shapes, duplicates, broken references and the type's rules;
//! whether the content is good enough is the AI's advice, never the lint's.
//!
//! [`lint`] is pure: the repository's type definitions and documents (paths
//! and bytes) in, one [`FileReport`] per file with problems out, sorted by
//! path then line. A document is a `.md` whose front matter names a
//! `doctype`; other Markdown files are not documents (the older spec format
//! is still read by [`crate::spec_doc`]).
//!
//! References within the repository (`[[ID]]`, `[[ID#BLOCK]]`, the
//! `related` field, `docref` / `blockref` values and `replaced_by`) must
//! resolve; a reference to another repository (`[[repo:ID]]`) is resolved by
//! the caller against `.forge/repo.yml` and is not checked here.
//!
//! References to the project's linked wiki (`[[wiki:ID]]`, plan 20 W3,
//! ADR-0040 §5) are checked only when the caller says what the wiki holds
//! ([`lint_with_wiki`], [`WikiRefs`]): the caller reads the wiki's
//! default-branch tip (this crate does no I/O). A page or block that is not
//! there is `wiki_ref_broken`, a wiki reference in a project with no linked
//! wiki is `wiki_not_linked` — both **warnings** ([`is_warning`],
//! `FG-DOC-0008`): the wiki is edited on its own and must never block the
//! project's merges. [`lint`] does not check them ([`WikiRefs::Unchecked`]).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::doctype::{
    duration_ok, parse_doctype, Doctype, DoctypeProblem, Pattern, Rule, ValueDef, ValueType,
    BASE_FIELDS, DEFAULT_BLOCK_ID,
};
use crate::document::{
    parse_document, Block, Classification, Document, MetaField, Problem, Value,
    MAX_PROBLEMS_PER_DOCUMENT,
};
use crate::spec_doc::RequirementId;

/// Where the type definitions live (`<dir>/<name>.yml`).
pub const DOCTYPES_DIR: &str = ".forge/doctypes";

/// One file of the repository.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    /// The path from the repository's root, `/`-separated.
    pub path: &'a str,
    pub bytes: &'a [u8],
}

/// The problems of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    pub path: String,
    /// In line order, at most [`MAX_PROBLEMS_PER_DOCUMENT`].
    pub problems: Vec<Problem>,
    pub problems_dropped: usize,
}

const REQUIRED_KEYS: &[&str] = &["doctype", "id", "title", "owner"];

/// A `[[wiki:…]]` page or block that is not at the wiki's tip.
pub const WIKI_REF_BROKEN: &str = "wiki_ref_broken";
/// A `[[wiki:…]]` reference in a project with no linked wiki.
pub const WIKI_NOT_LINKED: &str = "wiki_not_linked";

/// The rules that are always warnings, never errors (plan 20 W3).
pub const WARNING_RULES: &[&str] = &[WIKI_REF_BROKEN, WIKI_NOT_LINKED];

/// Whether `rule` is a warning (it never fails a lint on its own).
pub fn is_warning(rule: &str) -> bool {
    WARNING_RULES.contains(&rule)
}

/// The pages of a wiki that references may name: id → its blocks' ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WikiIndex {
    pages: HashMap<String, HashSet<String>>,
}

impl WikiIndex {
    /// The index of the wiki's files at its tip (the `.md` files; the
    /// same rule as this repository's index: originals named after their
    /// id, the first by path keeps an id).
    pub fn build(documents: &[Source<'_>]) -> Self {
        let parsed: Vec<(&str, Document)> = documents
            .iter()
            .map(|src| (src.path, parse_document(src.bytes)))
            .collect();
        let mut docs: Vec<(&str, &Document)> = documents
            .iter()
            .zip(&parsed)
            .filter(|(src, (_, doc))| names_type(doc, src.bytes))
            .map(|(_, (path, doc))| (*path, doc))
            .collect();
        docs.sort_by(|a, b| a.0.cmp(b.0));
        let (index, _) = build_index(&docs);
        Self {
            pages: index
                .entries
                .into_iter()
                .map(|(id, e)| (id, e.blocks.into_keys().collect()))
                .collect(),
        }
    }

    /// Whether `doc` (and its block `block`) is a page of the wiki.
    pub fn resolves(&self, doc: &str, block: Option<&str>) -> bool {
        self.pages
            .get(doc)
            .is_some_and(|blocks| block.is_none_or(|b| blocks.contains(b)))
    }

    /// Pages in the index.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// What the caller knows of the project's linked wiki (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikiRefs {
    /// Not known (`gstn doc lint` on a working tree, the incremental and
    /// train lints): wiki references are not checked.
    Unchecked,
    /// The project has no linked wiki: each wiki reference is
    /// `wiki_not_linked`.
    NotLinked,
    /// The wiki's pages at its default-branch tip.
    Linked(WikiIndex),
}

/// What other documents may refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    doctype: String,
    /// Block id → kind.
    blocks: HashMap<String, String>,
}

/// The index of the documents other documents may refer to (originals
/// only). Every lookup goes through [`Index::get`] / [`Index::contains_key`],
/// so a check can be told exactly which ids it read (plan 18 S7: the
/// incremental lint's reverse references, [`incremental`]).
struct Index {
    entries: HashMap<String, Entry>,
    /// The ids looked up while recording (`None`: not recording).
    looked_up: RefCell<Option<Vec<String>>>,
    /// A lookup could not be recorded (never expected: one thread, no
    /// re-entry) — the recording is then not to be trusted.
    lost: Cell<bool>,
}

impl Index {
    fn new(entries: HashMap<String, Entry>) -> Self {
        Self {
            entries,
            looked_up: RefCell::new(None),
            lost: Cell::new(false),
        }
    }

    fn note(&self, id: &str) {
        match self.looked_up.try_borrow_mut() {
            Ok(mut recording) => {
                if let Some(ids) = recording.as_mut() {
                    ids.push(id.to_owned());
                }
            }
            Err(_) => self.lost.set(true),
        }
    }

    fn get(&self, id: &str) -> Option<&Entry> {
        self.note(id);
        self.entries.get(id)
    }

    fn contains_key(&self, id: &str) -> bool {
        self.note(id);
        self.entries.contains_key(id)
    }

    /// Start recording the ids looked up.
    fn record(&self) {
        self.lost.set(false);
        match self.looked_up.try_borrow_mut() {
            Ok(mut recording) => *recording = Some(Vec::new()),
            Err(_) => self.lost.set(true),
        }
    }

    /// The ids looked up since [`Index::record`]; `None` when one could
    /// not be recorded.
    fn recorded(&self) -> Option<Vec<String>> {
        let ids = match self.looked_up.try_borrow_mut() {
            Ok(mut recording) => recording.take(),
            Err(_) => None,
        };
        if self.lost.get() {
            return None;
        }
        ids
    }
}

/// Check the repository's documents (module docs). `doctypes` are the
/// files under [`DOCTYPES_DIR`]; `documents` the `.md` files.
pub fn lint(doctypes: &[Source<'_>], documents: &[Source<'_>]) -> Vec<FileReport> {
    lint_with_wiki(doctypes, documents, &WikiRefs::Unchecked)
}

/// [`lint`], with the project's linked wiki's references checked against
/// `wiki` (module docs; their problems are warnings).
pub fn lint_with_wiki(
    doctypes: &[Source<'_>],
    documents: &[Source<'_>],
    wiki: &WikiRefs,
) -> Vec<FileReport> {
    let (mut reports, types) = lint_doctypes(doctypes);

    // Parse every document; keep those that name a type.
    let parsed: Vec<(&str, Document)> = documents
        .iter()
        .map(|src| (src.path, parse_document(src.bytes)))
        .collect();
    let mut docs: Vec<(&str, &Document)> = documents
        .iter()
        .zip(&parsed)
        .filter(|(src, (_, doc))| names_type(doc, src.bytes))
        .map(|(_, (path, doc))| (*path, doc))
        .collect();
    docs.sort_by(|a, b| a.0.cmp(b.0));

    let (index, duplicates) = build_index(&docs);
    for (path, line, first) in duplicates {
        reports
            .entry(path.to_owned())
            .or_default()
            .push_on(line, "id_duplicate", first);
    }

    for (path, doc) in &docs {
        let report = reports.entry((*path).to_owned()).or_default();
        for p in &doc.problems {
            report.add(p.clone());
        }
        report.dropped += doc.problems_dropped;
        check_document(path, doc, &types, &index, report);
        check_wiki_refs(doc, wiki, report);
    }

    finish_reports(reports)
}

/// `[[wiki:…]]` in the text, against what the caller read of the wiki.
fn check_wiki_refs(doc: &Document, wiki: &WikiRefs, report: &mut Report) {
    for r in doc.refs.iter().filter(|r| r.wiki) {
        match wiki {
            WikiRefs::Unchecked => return,
            WikiRefs::NotLinked => report.push_on(r.line, WIKI_NOT_LINKED, &r.target()),
            WikiRefs::Linked(index) => {
                if !index.resolves(&r.doc, r.block.as_deref()) {
                    report.push_on(r.line, WIKI_REF_BROKEN, &r.target());
                }
            }
        }
    }
}

/// The parsed type definitions by name.
type Types = HashMap<String, Result<Doctype, DoctypeProblem>>;

/// The type definitions parsed, and the reports of the ones that do not
/// parse.
fn lint_doctypes(doctypes: &[Source<'_>]) -> (BTreeMap<String, Report>, Types) {
    let mut reports: BTreeMap<String, Report> = BTreeMap::new();
    let mut types: Types = HashMap::new();
    for src in doctypes {
        let Some(stem) = doctype_stem(src.path) else {
            continue;
        };
        let parsed = parse_doctype(stem, src.bytes);
        if let Err(p) = &parsed {
            reports
                .entry(src.path.to_owned())
                .or_default()
                .push(p.line, doctype_rule(p.reason));
        }
        types.insert(stem.to_owned(), parsed);
    }
    (reports, types)
}

/// Whether `doc` (parsed from `bytes`) is a document: a `doctype` key of
/// any value, or — when the front matter could not be read (not UTF-8, too
/// large, broken YAML) — a line that names one: such a file is a document
/// whose problems are reported, never silently skipped (review MR2a).
fn names_type(doc: &Document, bytes: &[u8]) -> bool {
    doc.meta_field("doctype").is_some() || (doc.meta.is_none() && mentions_doctype(bytes))
}

/// A file told that another file (by path) has its id.
type Duplicate<'d> = (&'d str, usize, &'d str);

/// The index of `docs` (sorted by path): originals only (a translation
/// resolves to its original). The first file (by path) keeps an id; the
/// others are returned as `(path, line, first path)` to be told which file
/// has it.
fn build_index<'d>(docs: &[(&'d str, &Document)]) -> (Index, Vec<Duplicate<'d>>) {
    let mut entries: HashMap<String, Entry> = HashMap::new();
    let mut first_path: HashMap<String, &'d str> = HashMap::new();
    let mut duplicates = Vec::new();
    for (path, doc) in docs {
        let (Some(id), Some(doctype)) = (doc.id(), doc.doctype()) else {
            continue;
        };
        if stem_of(path) != Some(id) {
            continue;
        }
        if let Some(first) = first_path.get(id) {
            let line = doc.meta_field("id").map_or(1, |f| f.line);
            duplicates.push((*path, line, *first));
            continue;
        }
        first_path.insert(id.to_owned(), path);
        entries.insert(
            id.to_owned(),
            Entry {
                doctype: doctype.to_owned(),
                blocks: doc
                    .blocks
                    .iter()
                    .filter_map(|b| Some((b.id.clone()?, b.kind.clone())))
                    .collect(),
            },
        );
    }
    (Index::new(entries), duplicates)
}

fn finish_reports(reports: BTreeMap<String, Report>) -> Vec<FileReport> {
    reports
        .into_iter()
        .filter(|(_, r)| !r.problems.is_empty() || r.dropped > 0)
        .map(|(path, r)| r.finish(path))
        .collect()
}

#[derive(Default)]
struct Report {
    problems: Vec<Problem>,
    dropped: usize,
    /// Recording (the incremental lint): every problem added, in order,
    /// before the de-duplication and the bound — replayed later through a
    /// plain report, which then answers exactly as if checked in place.
    raw: Option<Vec<Problem>>,
}

impl Report {
    fn push(&mut self, line: usize, rule: &'static str) {
        self.add(Problem::new(line, rule));
    }

    /// A problem about `subject` (the field, section or id).
    fn push_on(&mut self, line: usize, rule: &'static str, subject: &str) {
        self.add(Problem::on(line, rule, subject));
    }

    fn add(&mut self, p: Problem) {
        if let Some(raw) = self.raw.as_mut() {
            raw.push(p);
            return;
        }
        if self.problems.contains(&p) {
            return;
        }
        if self.problems.len() < MAX_PROBLEMS_PER_DOCUMENT {
            self.problems.push(p);
        } else {
            self.dropped += 1;
        }
    }

    fn finish(mut self, path: String) -> FileReport {
        self.problems.sort();
        FileReport {
            path,
            problems: self.problems,
            problems_dropped: self.dropped,
        }
    }
}

/// `.forge/doctypes/<stem>.yml` → `<stem>`.
fn doctype_stem(path: &str) -> Option<&str> {
    path.strip_prefix(DOCTYPES_DIR)?
        .strip_prefix('/')?
        .strip_suffix(".yml")
        .filter(|s| !s.contains('/'))
}

/// The file name without `.md` (`qms/QP-07-001.md` → `QP-07-001`).
fn stem_of(path: &str) -> Option<&str> {
    path.rsplit('/').next()?.strip_suffix(".md")
}

/// A front matter that could not be read still names a type when one of
/// its top-level keys is `doctype` (quoted or not, spaces before the `:`
/// allowed), read loosely from the bytes.
fn mentions_doctype(bytes: &[u8]) -> bool {
    let cap = bytes.len().min(crate::document::MAX_FRONT_MATTER_BYTES * 2);
    let text = String::from_utf8_lossy(bytes.get(..cap).unwrap_or(bytes));
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return false;
    }
    lines
        .take_while(|l| !matches!(l.trim_end(), "---" | "..."))
        .filter(|l| !l.starts_with([' ', '\t']))
        .filter_map(|l| l.split_once(':'))
        .any(|(key, _)| key.trim().trim_matches(['"', '\'']) == "doctype")
}

fn doctype_rule(reason: &'static str) -> &'static str {
    // The file's own problem is reported with the definition's reason,
    // prefixed so a reader knows it is the definition, not a document.
    match reason {
        "too_large" => "doctype_too_large",
        "not_utf8" => "doctype_not_utf8",
        "name_not_filename" => "doctype_name_not_filename",
        "unknown_key" => "doctype_unknown_key",
        "rule_unknown" => "doctype_rule_unknown",
        "pattern_invalid" => "doctype_pattern_invalid",
        "type_unknown" => "doctype_type_unknown",
        _ => "doctype_invalid",
    }
}

fn check_document(path: &str, doc: &Document, types: &Types, index: &Index, report: &mut Report) {
    let Some(meta) = &doc.meta else {
        return;
    };
    let line_of = |key: &str| doc.meta_field(key).map_or(1, |f| f.line);
    let doctype_name = doc.doctype().unwrap_or_default();
    let ty = match types.get(doctype_name) {
        None => {
            report.push(line_of("doctype"), "doctype_unknown");
            None
        }
        Some(Err(_)) => {
            report.push(line_of("doctype"), "doctype_invalid");
            None
        }
        Some(Ok(t)) => Some(t),
    };

    check_meta(path, doc, meta, ty, index, report);
    let Some(ty) = ty else {
        return;
    };
    check_sections(doc, ty, report);
    check_blocks(doc, ty, index, report);
    check_refs(doc, index, report);
    check_rules(doc, ty, report);
}

// ── the front matter ───────────────────────────────────────────────────

fn check_meta(
    path: &str,
    doc: &Document,
    meta: &[MetaField],
    ty: Option<&Doctype>,
    index: &Index,
    report: &mut Report,
) {
    for key in REQUIRED_KEYS {
        match doc.meta_field(key) {
            None => report.push_on(1, "field_missing", key),
            Some(f) if f.value.as_text().is_none_or(|t| t.trim().is_empty()) => {
                report.push(f.line, "field_invalid");
            }
            Some(_) => {}
        }
    }
    for f in meta {
        let declared = ty.and_then(|t| t.field(&f.key));
        if !BASE_FIELDS.contains(&f.key.as_str()) && declared.is_none() {
            // Unknown to a known type; a type that could not be read has
            // already said so once.
            if ty.is_some() {
                report.push_on(f.line, "field_unknown", &f.key);
            }
            continue;
        }
        match f.key.as_str() {
            "classification" => {
                match f.value.as_text().and_then(Classification::parse) {
                    None => report.push(f.line, "classification_invalid"),
                    // Lowering below the type's default is the type's change
                    // (governed), never one document's (review MR2a).
                    Some(c) => {
                        if ty
                            .and_then(|t| t.default_classification)
                            .is_some_and(|d| c < d)
                        {
                            report.push(f.line, "classification_below_default");
                        }
                    }
                }
                if doc.meta_field("confidential").is_some() {
                    report.push(f.line, "classification_conflict");
                }
            }
            "confidential" => {
                if !matches!(f.value.as_text(), Some("true" | "false")) {
                    report.push(f.line, "classification_invalid");
                }
            }
            "review_cycle" => {
                if !f.value.as_text().is_some_and(duration_ok) {
                    report.push(f.line, "field_invalid");
                }
            }
            "lang" => {
                if !f.value.as_text().is_some_and(lang_ok) {
                    report.push(f.line, "field_invalid");
                }
            }
            "related" => match &f.value {
                Value::List(items) => {
                    for (line, item) in items {
                        match item.as_text() {
                            Some(id) => resolve_doc(id, None, *line, index, report),
                            None => report.push(*line, "field_invalid"),
                        }
                    }
                }
                Value::Null | Value::Text(_) | Value::Map(_) => {
                    report.push(f.line, "field_invalid")
                }
            },
            // A translation keeps its original's id, and the original exists.
            "translation_of" => match f.value.as_text() {
                Some(id) if Some(id) == doc.id() && index.contains_key(id) => {}
                _ => report.push(f.line, "translation_invalid"),
            },
            "retired" => check_retired(f, index, report),
            _ => {
                if let Some(def) = declared {
                    check_field(f, def, index, report);
                }
            }
        }
    }
    if let Some(ty) = ty {
        for def in ty.fields.iter().filter(|d| d.required) {
            if doc.meta_field(&def.key).is_none() {
                report.push_on(1, "field_missing", &def.key);
            }
        }
        if let (Some(id), Some(pattern)) = (doc.id(), &ty.id_pattern) {
            if !pattern.matches(id) {
                report.push(doc.meta_field("id").map_or(1, |f| f.line), "id_invalid");
            }
        }
    }
    check_filename(path, doc, report);
}

fn lang_ok(raw: &str) -> bool {
    let (lang, region) = raw.split_once('-').unwrap_or((raw, ""));
    lang.len() == 2
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && (region.is_empty()
            || region.len() == 2 && region.bytes().all(|b| b.is_ascii_uppercase()))
}

/// The file is named after the document: `<id>.md`, or `<id>.<lang>.md`
/// for a translation (`lang` and `translation_of` set).
fn check_filename(path: &str, doc: &Document, report: &mut Report) {
    let (Some(id), Some(stem)) = (doc.id(), stem_of(path)) else {
        return;
    };
    if stem == id {
        if doc.meta_field("translation_of").is_some() {
            report.push(
                doc.meta_field("translation_of").map_or(1, |f| f.line),
                "translation_invalid",
            );
        }
        return;
    }
    let translation = doc
        .meta_text("lang")
        .is_some_and(|lang| stem == format!("{id}.{lang}"))
        && doc.meta_text("translation_of") == Some(id);
    if !translation {
        report.push(
            doc.meta_field("id").map_or(1, |f| f.line),
            "id_not_filename",
        );
    }
}

fn check_retired(f: &MetaField, index: &Index, report: &mut Report) {
    let Value::Map(entries) = &f.value else {
        report.push(f.line, "retired_invalid");
        return;
    };
    for (key, line, value) in entries {
        match (key.as_str(), value.as_text()) {
            ("date", Some(d)) if date_ok(d) => {}
            ("replaced_by", Some(id)) => resolve_doc(id, None, *line, index, report),
            _ => report.push(*line, "retired_invalid"),
        }
    }
}

/// `YYYY-MM-DD` (the shape; the calendar is the reader's).
fn date_ok(raw: &str) -> bool {
    let b = raw.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

fn check_field(f: &MetaField, def: &ValueDef, index: &Index, report: &mut Report) {
    match (&def.ty, &f.value) {
        (ValueType::List, Value::List(items)) => {
            if items.iter().any(|(_, v)| v.as_text().is_none()) {
                report.push(f.line, "field_invalid");
            }
        }
        (ValueType::List, Value::Null | Value::Text(_) | Value::Map(_)) => {
            report.push(f.line, "field_invalid");
        }
        (ValueType::DocRef { doctype }, Value::Text(id)) => {
            resolve_doc(id, doctype.as_deref(), f.line, index, report);
        }
        (_, Value::Text(t)) => {
            if !scalar_ok(&def.ty, t) {
                report.push(f.line, "field_invalid");
            }
        }
        (_, Value::Null | Value::List(_) | Value::Map(_)) => report.push(f.line, "field_invalid"),
    }
}

/// A scalar of a declared type (references are resolved separately).
fn scalar_ok(ty: &ValueType, raw: &str) -> bool {
    match ty {
        ValueType::Text => !raw.trim().is_empty(),
        ValueType::List => false,
        ValueType::Int { min, max } => raw
            .parse::<i64>()
            .is_ok_and(|n| min.is_none_or(|m| n >= m) && max.is_none_or(|m| n <= m)),
        ValueType::Bool => matches!(raw, "true" | "false"),
        ValueType::Enum(choices) => choices.iter().any(|c| c == raw),
        ValueType::Duration => duration_ok(raw),
        ValueType::DocRef { .. } | ValueType::BlockRef { .. } => !raw.is_empty(),
    }
}

// ── the body ───────────────────────────────────────────────────────────

/// The type's headings, each present and in order.
fn check_sections(doc: &Document, ty: &Doctype, report: &mut Report) {
    let mut last: Option<usize> = None;
    for section in &ty.sections {
        let found = doc
            .headings
            .iter()
            .position(|h| h.level == 2 && h.text == section.title);
        match found {
            None => report.push_on(1, "section_missing", &section.title),
            Some(at) => {
                if last.is_some_and(|l| at < l) {
                    let line = doc.headings.get(at).map_or(1, |h| h.line);
                    report.push_on(line, "section_order", &section.title);
                }
                last = Some(at);
            }
        }
    }
}

fn check_blocks(doc: &Document, ty: &Doctype, index: &Index, report: &mut Report) {
    let default_id = Pattern::new(DEFAULT_BLOCK_ID);
    let mut seen: HashSet<&str> = HashSet::new();
    for b in &doc.blocks {
        let Some(def) = ty.block(&b.kind) else {
            if b.id.is_some() {
                report.push(b.line, "block_kind_unknown");
            }
            // A plain container (`::: note`) of any kind.
            continue;
        };
        let Some(id) = b.id.as_deref() else {
            report.push(b.line, "block_id_missing");
            continue;
        };
        let id_ok = match (&def.id_pattern, def.kind.as_str()) {
            (Some(p), _) => p.matches(id),
            (None, "req") => RequirementId::parse(id).is_some(),
            (None, _) => default_id.as_ref().is_some_and(|p| p.matches(id)),
        };
        if !id_ok {
            report.push(b.line, "block_id_invalid");
        }
        if !seen.insert(id) {
            report.push(b.line, "block_id_duplicate");
        }
        if b.body.is_empty() {
            report.push(b.line, "block_body_empty");
        }
        // The kind belongs under the section(s) that list it.
        let homes: Vec<&str> = ty
            .sections
            .iter()
            .filter(|s| s.blocks.contains(&b.kind))
            .map(|s| s.title.as_str())
            .collect();
        if !homes.is_empty() {
            let under = b
                .section
                .and_then(|i| doc.headings.get(i))
                .map(|h| h.text.as_str());
            if !under.is_some_and(|t| homes.contains(&t)) {
                report.push(b.line, "block_outside_section");
            }
        }
        check_attrs(doc, b, &def.attrs, index, report);
    }
}

fn check_attrs(doc: &Document, b: &Block, defs: &[ValueDef], index: &Index, report: &mut Report) {
    for a in &b.attrs {
        match a.key.as_str() {
            "status" => {
                if !matches!(a.value.as_str(), "active" | "deprecated") {
                    report.push(b.line, "attr_invalid");
                }
            }
            "replaced_by" => {
                if !b.deprecated() {
                    report.push(b.line, "replaced_by_not_deprecated");
                }
                resolve_block(doc, &a.value, None, b.line, report);
            }
            key => match defs.iter().find(|d| d.key == key) {
                None => report.push_on(b.line, "attr_unknown", key),
                Some(def) => match &def.ty {
                    ValueType::DocRef { doctype } => {
                        resolve_doc(&a.value, doctype.as_deref(), b.line, index, report);
                    }
                    ValueType::BlockRef { kind } => {
                        resolve_block(doc, &a.value, kind.as_deref(), b.line, report);
                    }
                    ty @ (ValueType::Text
                    | ValueType::List
                    | ValueType::Int { .. }
                    | ValueType::Bool
                    | ValueType::Enum(_)
                    | ValueType::Duration) => {
                        if !scalar_ok(ty, &a.value) {
                            report.push(b.line, "attr_invalid");
                        }
                    }
                },
            },
        }
    }
    for def in defs.iter().filter(|d| d.required) {
        if b.attr(&def.key).is_none() {
            report.push_on(b.line, "attr_missing", &def.key);
        }
    }
}

/// A block of this document, optionally of one kind.
fn resolve_block(doc: &Document, id: &str, kind: Option<&str>, line: usize, report: &mut Report) {
    match doc.block(id) {
        None => report.push_on(line, "ref_broken", id),
        Some(target) => {
            if kind.is_some_and(|k| k != target.kind) {
                report.push_on(line, "ref_kind_mismatch", id);
            }
        }
    }
}

/// A document of this repository, optionally of one type.
fn resolve_doc(id: &str, doctype: Option<&str>, line: usize, index: &Index, report: &mut Report) {
    match index.get(id) {
        None => report.push_on(line, "ref_broken", id),
        Some(entry) => {
            if doctype.is_some_and(|t| t != entry.doctype) {
                report.push_on(line, "ref_doctype_mismatch", id);
            }
        }
    }
}

/// `[[…]]` in the text: in this repository, the document (and block) exist.
fn check_refs(doc: &Document, index: &Index, report: &mut Report) {
    for r in doc.refs.iter().filter(|r| r.is_local()) {
        let target = r.target();
        let resolves = index.get(&r.doc).is_some_and(|entry| {
            r.block
                .as_ref()
                .is_none_or(|b| entry.blocks.contains_key(b))
        });
        if !resolves {
            report.push_on(r.line, "ref_broken", &target);
        }
    }
}

// ── the type's rules ───────────────────────────────────────────────────

fn active<'a>(doc: &'a Document, kind: &'a str) -> impl Iterator<Item = &'a Block> + 'a {
    doc.blocks
        .iter()
        .filter(move |b| b.kind == kind && b.id.is_some() && !b.deprecated())
}

fn int_attr(b: &Block, key: &str) -> Option<i64> {
    b.attr(key)?.parse().ok()
}

/// The hazards some active control names (`for=`).
fn controlled_hazards(doc: &Document) -> HashSet<&str> {
    active(doc, "control")
        .filter_map(|c| c.attr("for"))
        .collect()
}

fn check_rules(doc: &Document, ty: &Doctype, report: &mut Report) {
    for rule in &ty.rules {
        let name = rule.as_str();
        match rule {
            Rule::DecisionsResolved => {
                for b in active(doc, "decision") {
                    if b.attr("required") == Some("true") && b.attr("state") == Some("open") {
                        report.push(b.line, name);
                    }
                }
            }
            Rule::DecisionNaNeedsReason => {
                for b in active(doc, "decision") {
                    if b.attr("state") == Some("na") && b.body.is_empty() {
                        report.push(b.line, name);
                    }
                }
            }
            Rule::EveryMustReqHasCheck => {
                let covered: HashSet<&str> = active(doc, "check")
                    .filter_map(|c| c.attr("covers"))
                    .collect();
                for b in active(doc, "req") {
                    let must = b.attr("priority") == Some("must");
                    if must && b.id.as_deref().is_some_and(|id| !covered.contains(id)) {
                        report.push(b.line, name);
                    }
                }
            }
            Rule::EveryRecordNamesAForm => {
                for b in active(doc, "record") {
                    if b.attr("form").is_none() {
                        report.push(b.line, name);
                    }
                }
            }
            Rule::EveryHazardHasControl => {
                let controlled = controlled_hazards(doc);
                for b in active(doc, "hazard") {
                    if !b.id.as_deref().is_some_and(|id| controlled.contains(id)) {
                        report.push(b.line, name);
                    }
                }
            }
            Rule::ResidualSetWhenControlled => {
                let controlled = controlled_hazards(doc);
                for b in active(doc, "hazard") {
                    let has = b.id.as_deref().is_some_and(|id| controlled.contains(id));
                    let unset =
                        b.attr("res_severity").is_none() || b.attr("res_likelihood").is_none();
                    if has && unset {
                        report.push(b.line, name);
                    }
                }
            }
            Rule::ResidualNotAboveInitial => {
                for b in active(doc, "hazard") {
                    let above = |res: &str, initial: &str| {
                        int_attr(b, res)
                            .zip(int_attr(b, initial))
                            .is_some_and(|(r, i)| r > i)
                    };
                    if above("res_severity", "severity") || above("res_likelihood", "likelihood") {
                        report.push(b.line, name);
                    }
                }
            }
        }
    }
}

// Plan 18 S7 (ADR-0037 (b) 3): the content-addressed, incremental lint.
#[path = "lint_incremental.rs"]
pub mod incremental;

#[cfg(test)]
#[path = "lint_tests.rs"]
mod tests;
