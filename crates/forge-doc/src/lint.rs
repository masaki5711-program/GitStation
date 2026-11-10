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

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::doctype::{
    duration_ok, parse_doctype, Doctype, DoctypeProblem, Pattern, Rule, ValueDef, ValueType,
    DEFAULT_BLOCK_ID,
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

/// The front matter keys every document may have (ADR-0026 §1).
const BASE_KEYS: &[&str] = &[
    "doctype",
    "id",
    "title",
    "owner",
    "classification",
    "review_cycle",
    "related",
    "lang",
    "translation_of",
    "retired",
    // The older mark (`true` / `false`), read as a classification.
    "confidential",
];
const REQUIRED_KEYS: &[&str] = &["doctype", "id", "title", "owner"];

/// What other documents may refer to.
struct Entry {
    doctype: String,
    /// Block id → kind.
    blocks: HashMap<String, String>,
}

/// Check the repository's documents (module docs). `doctypes` are the
/// files under [`DOCTYPES_DIR`]; `documents` the `.md` files.
pub fn lint(doctypes: &[Source<'_>], documents: &[Source<'_>]) -> Vec<FileReport> {
    let mut reports: BTreeMap<String, Report> = BTreeMap::new();
    let mut types: HashMap<String, Result<Doctype, DoctypeProblem>> = HashMap::new();
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

    // Parse every document; keep those that name a type.
    let mut docs: Vec<(&str, Document)> = Vec::new();
    for src in documents {
        let doc = parse_document(src.bytes);
        let names_type = doc.doctype().is_some()
            || (doc.meta.is_none() && doc.has_front_matter && mentions_doctype(src.bytes));
        if names_type {
            docs.push((src.path, doc));
        }
    }
    docs.sort_by(|a, b| a.0.cmp(b.0));

    // The index: originals only (a translation resolves to its original).
    let mut index: HashMap<String, Entry> = HashMap::new();
    let mut first_path: HashMap<String, &str> = HashMap::new();
    for (path, doc) in &docs {
        let (Some(id), Some(doctype)) = (doc.id(), doc.doctype()) else {
            continue;
        };
        if stem_of(path) != Some(id) {
            continue;
        }
        if first_path.contains_key(id) {
            // The first file (by path) keeps the id; the others are told.
            let line = doc.meta_field("id").map_or(1, |f| f.line);
            reports
                .entry((*path).to_owned())
                .or_default()
                .push(line, "id_duplicate");
            continue;
        }
        first_path.insert(id.to_owned(), path);
        index.insert(
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

    for (path, doc) in &docs {
        let report = reports.entry((*path).to_owned()).or_default();
        for p in &doc.problems {
            report.push(p.line, p.rule);
        }
        report.dropped += doc.problems_dropped;
        check_document(path, doc, &types, &index, report);
    }

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
}

impl Report {
    fn push(&mut self, line: usize, rule: &'static str) {
        let p = Problem { line, rule };
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
/// its lines starts with `doctype:` — its problems must not be hidden.
fn mentions_doctype(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|t| {
        t.lines()
            .skip(1)
            .take_while(|l| !matches!(l.trim_end(), "---" | "..."))
            .any(|l| l.starts_with("doctype:"))
    })
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

fn check_document(
    path: &str,
    doc: &Document,
    types: &HashMap<String, Result<Doctype, DoctypeProblem>>,
    index: &HashMap<String, Entry>,
    report: &mut Report,
) {
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
    index: &HashMap<String, Entry>,
    report: &mut Report,
) {
    for key in REQUIRED_KEYS {
        match doc.meta_field(key) {
            None => report.push(1, "field_missing"),
            Some(f) if f.value.as_text().is_none_or(|t| t.trim().is_empty()) => {
                report.push(f.line, "field_invalid");
            }
            Some(_) => {}
        }
    }
    for f in meta {
        let declared = ty.and_then(|t| t.field(&f.key));
        if !BASE_KEYS.contains(&f.key.as_str()) && declared.is_none() {
            // Unknown to a known type; a type that could not be read has
            // already said so once.
            if ty.is_some() {
                report.push(f.line, "field_unknown");
            }
            continue;
        }
        match f.key.as_str() {
            "classification" => {
                if f.value.as_text().and_then(Classification::parse).is_none() {
                    report.push(f.line, "classification_invalid");
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
            "translation_of" => match f.value.as_text() {
                Some(id) if Some(id) == doc.id() => {}
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
                report.push(1, "field_missing");
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

fn check_retired(f: &MetaField, index: &HashMap<String, Entry>, report: &mut Report) {
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

fn check_field(f: &MetaField, def: &ValueDef, index: &HashMap<String, Entry>, report: &mut Report) {
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
            None => report.push(1, "section_missing"),
            Some(at) => {
                if last.is_some_and(|l| at < l) {
                    let line = doc.headings.get(at).map_or(1, |h| h.line);
                    report.push(line, "section_order");
                }
                last = Some(at);
            }
        }
    }
}

fn check_blocks(doc: &Document, ty: &Doctype, index: &HashMap<String, Entry>, report: &mut Report) {
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

fn check_attrs(
    doc: &Document,
    b: &Block,
    defs: &[ValueDef],
    index: &HashMap<String, Entry>,
    report: &mut Report,
) {
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
                None => report.push(b.line, "attr_unknown"),
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
            report.push(b.line, "attr_missing");
        }
    }
}

/// A block of this document, optionally of one kind.
fn resolve_block(doc: &Document, id: &str, kind: Option<&str>, line: usize, report: &mut Report) {
    match doc.block(id) {
        None => report.push(line, "ref_broken"),
        Some(target) => {
            if kind.is_some_and(|k| k != target.kind) {
                report.push(line, "ref_kind_mismatch");
            }
        }
    }
}

/// A document of this repository, optionally of one type.
fn resolve_doc(
    id: &str,
    doctype: Option<&str>,
    line: usize,
    index: &HashMap<String, Entry>,
    report: &mut Report,
) {
    match index.get(id) {
        None => report.push(line, "ref_broken"),
        Some(entry) => {
            if doctype.is_some_and(|t| t != entry.doctype) {
                report.push(line, "ref_doctype_mismatch");
            }
        }
    }
}

/// `[[…]]` in the text: in this repository, the document (and block) exist.
fn check_refs(doc: &Document, index: &HashMap<String, Entry>, report: &mut Report) {
    for r in doc.refs.iter().filter(|r| r.repo.is_none()) {
        match index.get(&r.doc) {
            None => report.push(r.line, "ref_broken"),
            Some(entry) => {
                if r.block
                    .as_ref()
                    .is_some_and(|b| !entry.blocks.contains_key(b))
                {
                    report.push(r.line, "ref_broken");
                }
            }
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

#[cfg(test)]
#[path = "lint_tests.rs"]
mod tests;
