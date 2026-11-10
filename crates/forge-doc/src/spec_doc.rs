//! Structured specification documents (M3 X4, plan 03 §4.2, ADR-0023
//! Amendment X4): Markdown with a YAML front matter and requirement
//! blocks.
//!
//! ```markdown
//! ---
//! spec: zeva-engine/config
//! title: 設定管理
//! status: approved        # draft / review / approved / deprecated
//! owners: [po@example.internal]
//! ---
//!
//! ::: req ZEVA-CFG-013 priority=must
//! 不正な設定は反映せず、エラーコードを記録して直前の設定で動き続けること。
//! :::
//! ```
//!
//! [`parse_spec_document`] is pure and total: bytes in, a [`SpecDocument`]
//! out — its front matter, its requirements and every rule it breaks
//! ([`SpecProblem`]: a 1-based line and the rule's name; `forge/spec-lint`
//! reports them). A document is read on as far as it can be: one broken
//! block never hides the next.
//!
//! - **Front matter**: the first line is `---`, closed by `---` (or
//!   `...`). Its YAML is read by [`crate::yaml`]'s tree builder (no anchors,
//!   aliases, tags, duplicate keys, a second document; at most
//!   [`MAX_FRONT_MATTER_BYTES`]). Keys: `spec`, `title`, `status`
//!   (required), `owners` (a list of at most [`MAX_OWNERS`]) and
//!   `confidential` (`true` / `false`, M3 X5: plan 06 D-5 — the context
//!   API never hands the document to an AI agent; see
//!   [`SpecDocument::confidential`]); any other key is refused.
//! - **Blocks**: a line `::: req <ID> [key=value …]` opens a requirement,
//!   a line `:::` closes the innermost container. Other containers
//!   (`::: note`) nest and close the same way but are no requirements.
//!   Attributes: `priority` (`must`, `should`, `could`, `wont`) and
//!   `status` (`active`, `deprecated` — an id is never removed, plan 03
//!   §4.2). Inside a fenced code block (```` ``` ```` / `~~~`) nothing is
//!   a block: specifications show the syntax in examples. The blocks are
//!   read by [`crate::document`] (ADR-0026 "one parser"; the mapping onto
//!   these rules is the `blocks` section below).
//! - **Plan 13 front matter** (ADR-0026 §7): `doctype: spec.system` is the
//!   type the older format is; its front matter is read with the same rule
//!   names (`id` is the name, `retired` deprecates, `classification` is the
//!   mark; `plan13_meta_of`). A document of any other `doctype` is no
//!   specification document (doc-lint's, never indexed here).
//! - **Spec document**: a file with a front matter or a requirement block
//!   ([`SpecDocument::is_spec`]); any other Markdown file under a
//!   specification directory (a README) is not indexed.

use std::collections::HashSet;

use crate::document::{parse_document_uncapped, MAX_DOCUMENT_BYTES};
use crate::yaml::{mapping, read_tree, text, Kind, Node};

/// The longest requirement id (`requirement_id_invalid`).
pub const MAX_REQUIREMENT_ID_LEN: usize = 64;
/// A longer front matter is refused (`front_matter_too_large`).
pub const MAX_FRONT_MATTER_BYTES: usize = 8 * 1024;
/// At most this many `owners` (`owners_invalid`).
pub const MAX_OWNERS: usize = 32;
/// The longest owner, `spec` or `title` value (`*_invalid`).
pub const MAX_META_VALUE_BYTES: usize = 256;
/// A longer requirement body is a problem (`body_too_long`); it is kept.
pub const MAX_REQUIREMENT_BODY_BYTES: usize = 16 * 1024;
/// At most this many requirements per document (`too_many_requirements`:
/// the rest are not read).
pub const MAX_REQUIREMENTS_PER_DOCUMENT: usize = 2_000;
/// At most this many problems are kept per document (the rest are
/// counted in [`SpecDocument::problems_dropped`]).
pub const MAX_PROBLEMS_PER_DOCUMENT: usize = 200;

/// The first segment kanflow's work items own (`PBI-43`): never a
/// requirement family, so `@req PBI-43/AC-2` is never mistaken for one.
const RESERVED_FAMILY: &str = "PBI";

/// A requirement id: two or more `-`-separated segments of upper-case ASCII
/// letters and digits, the first starting with a letter (`ZEVA-CFG-012`),
/// at most [`MAX_REQUIREMENT_ID_LEN`] bytes, and not of kanflow's `PBI`
/// family. Stable once given (plan 03 §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequirementId(String);

impl RequirementId {
    /// `None` when `raw` breaks a rule above.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_REQUIREMENT_ID_LEN {
            return None;
        }
        let mut segments = raw.split('-');
        let first = segments.next()?;
        let starts_with_letter = first.bytes().next().is_some_and(|b| b.is_ascii_uppercase());
        if !starts_with_letter || first == RESERVED_FAMILY {
            return None;
        }
        let segment_ok = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        };
        if !segment_ok(first) {
            return None;
        }
        let mut rest = 0usize;
        for s in segments {
            if !segment_ok(s) {
                return None;
            }
            rest += 1;
        }
        (rest >= 1).then(|| Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id without its last segment (`ZEVA-CFG` of `ZEVA-CFG-012`):
    /// the product and area a repository numbers its requirements in.
    pub fn family(&self) -> &str {
        self.0
            .rsplit_once('-')
            .map_or(self.0.as_str(), |(family, _)| family)
    }
}

impl std::fmt::Display for RequirementId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A document's `status:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecStatus {
    Draft,
    Review,
    Approved,
    /// Every requirement of the document is deprecated.
    Deprecated,
}

impl SpecStatus {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "draft" => Self::Draft,
            "review" => Self::Review,
            "approved" => Self::Approved,
            "deprecated" => Self::Deprecated,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Review => "review",
            Self::Approved => "approved",
            Self::Deprecated => "deprecated",
        }
    }
}

/// A requirement's `priority=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Must,
    Should,
    Could,
    Wont,
}

impl Priority {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "must" => Self::Must,
            "should" => Self::Should,
            "could" => Self::Could,
            "wont" => Self::Wont,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Must => "must",
            Self::Should => "should",
            Self::Could => "could",
            Self::Wont => "wont",
        }
    }
}

/// A document's front matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecMeta {
    /// `spec:` — the document's name (unique in a repository).
    pub spec: String,
    /// The line of `spec:`.
    pub spec_line: usize,
    pub title: String,
    pub status: SpecStatus,
    pub owners: Vec<String>,
    /// `confidential: true` (M3 X5, plan 06 D-5).
    pub confidential: bool,
}

/// One requirement block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub id: RequirementId,
    /// The opening line (1-based).
    pub line: usize,
    /// The closing `:::` line (the last line read when unclosed).
    pub end_line: usize,
    pub priority: Option<Priority>,
    /// `status=deprecated`, or the document is deprecated.
    pub deprecated: bool,
    /// The lines between, joined with `\n`, trimmed.
    pub body: String,
}

/// A rule a document breaks: its 1-based line (0 for the whole file) and
/// the rule (`FG-REPO-0028` lists them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecProblem {
    pub line: usize,
    pub rule: &'static str,
}

/// A parsed document (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpecDocument {
    pub meta: Option<SpecMeta>,
    pub requirements: Vec<Requirement>,
    /// In line order, at most [`MAX_PROBLEMS_PER_DOCUMENT`].
    pub problems: Vec<SpecProblem>,
    /// Problems past the cap (counted, not kept).
    pub problems_dropped: usize,
    spec: bool,
    /// The file opens with a front matter (read or not).
    front_matter: bool,
    /// The front matter names a `doctype` other than `spec.system`: a
    /// plan 13 document of another type, no specification.
    other_type: bool,
}

impl SpecDocument {
    /// Whether the file is a specification document (it has a front
    /// matter or a requirement block, or could not be read as text).
    pub fn is_spec(&self) -> bool {
        self.spec
    }

    /// Whether an AI agent must not be given the document (M3 X5, plan 06
    /// D-5): `confidential: true`, and — failing closed — a front matter
    /// that could not be read (the mark may be in it) or a file that is no
    /// text. A document without a front matter carries no mark.
    pub fn confidential(&self) -> bool {
        match &self.meta {
            Some(meta) => meta.confidential,
            None => self.front_matter || self.not_text(),
        }
    }

    fn not_text(&self) -> bool {
        self.problems.iter().any(|p| p.rule == "not_utf8")
    }

    fn problem(&mut self, line: usize, rule: &'static str) {
        if self.problems.len() < MAX_PROBLEMS_PER_DOCUMENT {
            self.problems.push(SpecProblem { line, rule });
        } else {
            self.problems_dropped += 1;
        }
    }
}

/// Parse one document (module docs).
pub fn parse_spec_document(bytes: &[u8]) -> SpecDocument {
    parse_with(bytes, blocks)
}

/// How the body's blocks are read (the older scanner is kept for the
/// tests only, `spec_doc_oracle.rs`).
type ReadBlocks = fn(&[&str], usize, &mut SpecDocument) -> bool;

fn parse_with(bytes: &[u8], read_blocks: ReadBlocks) -> SpecDocument {
    let mut doc = SpecDocument::default();
    // Review X4: an editor's UTF-8 byte order mark before the front matter.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let Ok(text) = std::str::from_utf8(bytes) else {
        doc.spec = true;
        doc.problem(0, "not_utf8");
        return doc;
    };
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let body_start = front_matter(&lines, &mut doc);
    if doc.other_type {
        // Another type's document: not this reader's (doc-lint's).
        return SpecDocument::default();
    }
    let saw_requirement = read_blocks(&lines, body_start, &mut doc);
    if saw_requirement && !doc.spec {
        doc.problem(1, "front_matter_missing");
        doc.spec = true;
    }
    let deprecated = doc
        .meta
        .as_ref()
        .is_some_and(|m| m.status == SpecStatus::Deprecated);
    if deprecated {
        for r in &mut doc.requirements {
            r.deprecated = true;
        }
    }
    doc.problems.sort_by_key(|p| p.line);
    doc
}

// ── the front matter ───────────────────────────────────────────────────

/// Read the front matter into `doc`; the index of the first body line.
fn front_matter(lines: &[&str], doc: &mut SpecDocument) -> usize {
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return 0;
    }
    doc.spec = true;
    doc.front_matter = true;
    let Some(close) = lines
        .iter()
        .skip(1)
        .position(|l| matches!(l.trim_end(), "---" | "..."))
        .map(|i| i + 1)
    else {
        doc.problem(1, "front_matter_unclosed");
        return lines.len();
    };
    let yaml = lines.get(1..close).unwrap_or_default().join("\n");
    if yaml.len() > MAX_FRONT_MATTER_BYTES {
        doc.problem(1, "front_matter_too_large");
        return close + 1;
    }
    // The YAML's own line 1 is the document's line 2.
    let shift = |line: usize| line + 1;
    match read_tree(&yaml) {
        Err(p) => doc.problem(shift(p.line), front_matter_rule(p.reason)),
        Ok(None) => doc.problem(1, "spec_missing"),
        Ok(Some(root)) => meta_of(&root, shift, doc),
    }
    close + 1
}

/// The tree builder's refusals, named for a front matter.
fn front_matter_rule(reason: &'static str) -> &'static str {
    match reason {
        "duplicate_key" => "front_matter_duplicate_key",
        "too_many_keys" | "too_deep" => "front_matter_too_large",
        "yaml_anchor" | "yaml_alias" | "yaml_tag" => "front_matter_yaml_feature",
        "multiple_documents" => "front_matter_multiple_documents",
        _ => "front_matter_syntax",
    }
}

fn meta_of(root: &Node, shift: impl Fn(usize) -> usize, doc: &mut SpecDocument) {
    let fields = match mapping(root, "front_matter_not_a_mapping") {
        Ok(f) => f,
        Err(p) => {
            doc.problem(shift(p.line), front_matter_rule_of_shape(p.reason));
            return;
        }
    };
    if let Some(f) = fields.iter().find(|f| f.key == "doctype") {
        if text(f.value, f.line, "doctype").ok() == Some(crate::legacy::SPEC_SYSTEM) {
            plan13_meta_of(&fields, shift, doc);
        } else {
            doc.other_type = true;
        }
        return;
    }
    let mut spec = None;
    let mut title = None;
    let mut status = None;
    let mut owners = Vec::new();
    let mut confidential = false;
    let mut ok = true;
    let mut bad = |doc: &mut SpecDocument, line: usize, rule: &'static str| {
        doc.problem(shift(line), rule);
        ok = false;
    };
    for f in &fields {
        match f.key {
            "spec" => match meta_text(f.value, f.line, "spec_invalid") {
                Ok(v) => spec = Some((v, f.line)),
                Err((l, r)) => bad(doc, l, r),
            },
            "title" => match meta_text(f.value, f.line, "title_invalid") {
                Ok(v) => title = Some(v),
                Err((l, r)) => bad(doc, l, r),
            },
            "status" => match text(f.value, f.line, "status_invalid") {
                Ok(v) => match SpecStatus::parse(v) {
                    Some(s) => status = Some(s),
                    None => bad(doc, f.line, "status_invalid"),
                },
                Err(_) => bad(doc, f.line, "status_invalid"),
            },
            "owners" => match owners_of(f.value) {
                Some(list) => owners = list,
                None => bad(doc, f.line, "owners_invalid"),
            },
            "confidential" => match text(f.value, f.line, "confidential_invalid") {
                Ok("true") => confidential = true,
                Ok("false") => confidential = false,
                _ => bad(doc, f.line, "confidential_invalid"),
            },
            _ => bad(doc, f.line, "front_matter_unknown_key"),
        }
    }
    if !ok {
        return;
    }
    let (Some((spec, spec_line)), Some(title), Some(status)) = (spec, title, status) else {
        // The first missing one, on the front matter's first line.
        let rule = if fields.iter().all(|f| f.key != "spec") {
            "spec_missing"
        } else if fields.iter().all(|f| f.key != "title") {
            "title_missing"
        } else {
            "status_missing"
        };
        doc.problem(1, rule);
        return;
    };
    doc.meta = Some(SpecMeta {
        spec,
        spec_line: shift(spec_line),
        title,
        status,
        owners,
        confidential,
    });
}

/// The front matter of a plan 13 `doctype: spec.system` document
/// (ADR-0026 §7: the type the older format is). Read with the older
/// format's rule names, so spec-lint answers alike for both:
///
/// - `id` is the specification's name (`spec_missing` / `spec_invalid`);
///   `title` as in the older format;
/// - `retired` (any value; its shape is doc-lint's) makes the document
///   [`SpecStatus::Deprecated`]; the plan 13 format keeps no other state in
///   the file, so a document not retired reads [`SpecStatus::Approved`]:
///   the text in force at that commit (its approval is the merge
///   request's);
/// - `classification: confidential` (or the older `confidential: true`,
///   or both keys) marks it confidential, as
///   [`crate::document::Document::classification`] with the type's default
///   `internal`; a value that is no classification is `confidential_invalid`
///   (the front matter is then not used: withheld, fail closed);
/// - the other base keys (`owner`, `review_cycle`, `related`, `lang`,
///   `translation_of`) are accepted (doc-lint checks them); any other key
///   (`spec`, `status`, `owners`, a type's own field) is
///   `front_matter_unknown_key`.
fn plan13_meta_of(
    fields: &[crate::yaml::Field<'_>],
    shift: impl Fn(usize) -> usize,
    doc: &mut SpecDocument,
) {
    use crate::document::Classification;
    let mut id = None;
    let mut title = None;
    let mut retired = false;
    let mut classification: Option<Classification> = None;
    let mut older_mark: Option<bool> = None;
    let mut ok = true;
    let mut bad = |doc: &mut SpecDocument, line: usize, rule: &'static str| {
        doc.problem(shift(line), rule);
        ok = false;
    };
    for f in fields {
        match f.key {
            "doctype" | "owner" | "review_cycle" | "related" | "lang" | "translation_of" => {}
            "id" => match meta_text(f.value, f.line, "spec_invalid") {
                Ok(v) => id = Some((v, f.line)),
                Err((l, r)) => bad(doc, l, r),
            },
            "title" => match meta_text(f.value, f.line, "title_invalid") {
                Ok(v) => title = Some(v),
                Err((l, r)) => bad(doc, l, r),
            },
            "retired" => retired = true,
            "classification" => match text(f.value, f.line, "confidential_invalid")
                .ok()
                .and_then(Classification::parse)
            {
                Some(c) => classification = Some(c),
                None => bad(doc, f.line, "confidential_invalid"),
            },
            "confidential" => match text(f.value, f.line, "confidential_invalid") {
                Ok("true") => older_mark = Some(true),
                Ok("false") => older_mark = Some(false),
                _ => bad(doc, f.line, "confidential_invalid"),
            },
            _ => bad(doc, f.line, "front_matter_unknown_key"),
        }
    }
    if !ok {
        return;
    }
    let (Some((spec, spec_line)), Some(title)) = (id, title) else {
        let rule = if fields.iter().all(|f| f.key != "id") {
            "spec_missing"
        } else {
            "title_missing"
        };
        doc.problem(1, rule);
        return;
    };
    let confidential = match (classification, older_mark) {
        // Both written: fail closed (as Document::classification).
        (Some(_), Some(_)) => true,
        (Some(c), None) => c >= Classification::Confidential,
        (None, Some(mark)) => mark,
        (None, None) => false,
    };
    doc.meta = Some(SpecMeta {
        spec,
        spec_line: shift(spec_line),
        title,
        status: if retired {
            SpecStatus::Deprecated
        } else {
            SpecStatus::Approved
        },
        owners: Vec::new(),
        confidential,
    });
}

fn front_matter_rule_of_shape(reason: &'static str) -> &'static str {
    match reason {
        "front_matter_not_a_mapping" => "front_matter_not_a_mapping",
        "duplicate_key" => "front_matter_duplicate_key",
        _ => "front_matter_syntax",
    }
}

/// A short single-line text value.
fn meta_text(
    node: &Node,
    line: usize,
    rule: &'static str,
) -> Result<String, (usize, &'static str)> {
    let value = text(node, line, rule).map_err(|_| (line, rule))?;
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_META_VALUE_BYTES || value.chars().any(char::is_control)
    {
        return Err((line, rule));
    }
    Ok(value.to_owned())
}

fn owners_of(node: &Node) -> Option<Vec<String>> {
    let Kind::List(items) = &node.kind else {
        return None;
    };
    if items.len() > MAX_OWNERS {
        return None;
    }
    items
        .iter()
        .map(|item| meta_text(item, item.line, "owners_invalid").ok())
        .collect()
}

// ── the blocks ─────────────────────────────────────────────────────────
//
// ADR-0026 "one parser": which lines are block markers, which code fences
// hide them, how containers nest and close, and each block's body and
// lines come from [`crate::document::parse_document`]. What stays here is
// the older format's own reading of a `::: req` opening line (its id and
// `priority` / `status` attributes, with the older rule names) and the
// mapping of the newer parser's answer onto the older rules, so that
// spec-lint, spec-trace and the spec API answer as before.
//
// The newer parser is stricter on a requirement's opening line (it refuses
// a line whose attributes it cannot read, and a block with an id inside
// another block with an id), and the older format knew neither rule. So the
// text is read twice: once to learn which lines the newer parser takes as
// block openings, then again with each such line rewritten to a canonical
// form the older format means by it — `::: req <id> [priority=…]
// [status=deprecated]` for a requirement (`invalid` standing for an id the
// older format refuses: still a requirement container, never indexed), and
// `::: note` for any other container (never a block with an id). Opening
// lines are never part of a body, so the rewrite changes no body.
//
// Kept differences (the newer parser's; listed in `crate::legacy`): a `:::`
// line inside an HTML comment is no marker; a document over
// [`crate::document::MAX_DOCUMENT_BYTES`] is `file_too_large` (forge-checks
// never reads one over 256 KiB); past
// [`crate::document::MAX_BLOCKS_PER_DOCUMENT`] blocks (plain containers
// included) the rest are not read.

/// The rules of the newer parser that mark a line as a block opening it
/// did not keep as a block.
const OPENING_RULES: &[&str] = &[
    "block_kind_invalid",
    "attr_invalid",
    "attr_duplicate",
    "block_nested",
    "too_deep",
    "too_many_blocks",
];

/// The deepest nesting of containers (`too_deep`, as
/// [`crate::document`]'s).
#[cfg(test)]
const MAX_CONTAINER_DEPTH: usize = 16;

/// A code fence: its character and length (shared with [`crate::document`]).
pub(crate) struct Fence(pub(crate) u8, pub(crate) usize);

/// The fence a line opens (or closes), when it is one: up to three spaces,
/// then three or more backticks or tildes.
pub(crate) fn fence_of(line: &str) -> Option<Fence> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = line.get(indent..)?;
    let c = *rest.as_bytes().first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = rest.bytes().take_while(|b| *b == c).count();
    // CommonMark: a backtick fence's info string holds no backtick (review
    // X4: such a line is text, so it never hides the blocks after it).
    if c == b'`' && rest.get(n..).is_some_and(|info| info.contains('`')) {
        return None;
    }
    (n >= 3).then_some(Fence(c, n))
}

/// A container line: `None` when it is none; `Some(None)` for a close;
/// `Some(Some((name, rest)))` for an opening.
fn container_of(line: &str) -> Option<Option<(&str, &str)>> {
    let t = line.trim();
    if !t.starts_with(":::") {
        return None;
    }
    let rest = t.trim_start_matches(':').trim_start();
    if rest.is_empty() {
        return Some(None);
    }
    let (name, attrs) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some(Some((name, attrs.trim())))
}

/// Whether `raw` opens a requirement in the older format (`::: req …`).
fn opens_req(raw: &str) -> bool {
    matches!(container_of(raw), Some(Some(("req", _))))
}

/// A `::: req` opening line as the older format reads it.
struct ReqHeader {
    id: Option<RequirementId>,
    priority: Option<Priority>,
    deprecated: bool,
    /// The rules the line breaks, in the order they are found.
    problems: Vec<&'static str>,
}

/// Read the text after `::: req` (the id, then `key=value` attributes).
fn req_header(attrs: &str) -> ReqHeader {
    let mut problems = Vec::new();
    let mut tokens = attrs.split_whitespace();
    let id = match tokens.next() {
        None => {
            problems.push("requirement_id_missing");
            None
        }
        Some(raw) => {
            let id = RequirementId::parse(raw);
            if id.is_none() {
                problems.push("requirement_id_invalid");
            }
            id
        }
    };
    let mut priority = None;
    let mut status = None;
    for token in tokens {
        let Some((key, value)) = token.split_once('=') else {
            problems.push("attribute_invalid");
            continue;
        };
        match key {
            "priority" => {
                if priority.is_some() {
                    problems.push("attribute_duplicate");
                    continue;
                }
                match Priority::parse(value) {
                    Some(p) => priority = Some(p),
                    None => problems.push("priority_invalid"),
                }
            }
            "status" => {
                if status.is_some() {
                    problems.push("attribute_duplicate");
                    continue;
                }
                match value {
                    "active" => status = Some(false),
                    "deprecated" => status = Some(true),
                    _ => problems.push("requirement_status_invalid"),
                }
            }
            _ => problems.push("attribute_unknown"),
        }
    }
    ReqHeader {
        id,
        priority,
        deprecated: status.unwrap_or(false),
        problems,
    }
}

/// The canonical opening line the older format means by `raw`, an opening
/// line of the newer parser (module comment above).
fn canonical_opening(raw: &str) -> Option<String> {
    match container_of(raw)? {
        Some(("req", attrs)) => {
            let head = req_header(attrs);
            let mut line = format!(
                "::: req {}",
                head.id.as_ref().map_or("invalid", RequirementId::as_str)
            );
            if let Some(p) = head.priority {
                line.push_str(" priority=");
                line.push_str(p.as_str());
            }
            if head.deprecated {
                line.push_str(" status=deprecated");
            }
            Some(line)
        }
        Some(_) => Some("::: note".to_owned()),
        None => None,
    }
}

/// Read the blocks into `doc` (module comment above); whether any
/// `::: req` line was seen.
fn blocks(lines: &[&str], _start: usize, doc: &mut SpecDocument) -> bool {
    let text = lines.join("\n");
    if text.len() > MAX_DOCUMENT_BYTES {
        // The newer parser reads none of it (never a part); forge-checks
        // never hands over a file this large (`file_too_large` there too).
        doc.spec = true;
        doc.problem(0, "file_too_large");
        return false;
    }
    let first = parse_document_uncapped(text.as_bytes());
    let openings: HashSet<usize> = first
        .blocks
        .iter()
        .map(|b| b.line)
        .chain(
            first
                .problems
                .iter()
                .filter(|p| OPENING_RULES.contains(&p.rule))
                .map(|p| p.line),
        )
        .collect();
    let normalized: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let rewritten = openings
                .contains(&(i + 1))
                .then(|| canonical_opening(raw))
                .flatten();
            rewritten.unwrap_or_else(|| (*raw).to_owned())
        })
        .collect();
    let read = parse_document_uncapped(normalized.join("\n").as_bytes());
    let raw_line = |line: usize| line.checked_sub(1).and_then(|i| lines.get(i)).copied();
    let opens_req_at = |line: usize| raw_line(line).is_some_and(opens_req);

    // Each problem with the time the older scanner found it (the line it
    // was reading; `end` and after: at the end of the file), so the cap on
    // problems keeps the same ones.
    let end = lines.len() + 1;
    let mut found: Vec<(usize, SpecProblem)> = Vec::new();
    let mut saw_requirement = false;
    let mut unclosed: HashSet<usize> = HashSet::new();
    for p in &read.problems {
        let at = |rule| (p.line, SpecProblem { line: p.line, rule });
        match p.rule {
            "close_unmatched" | "too_deep" => found.push(at(p.rule)),
            "block_nested" if opens_req_at(p.line) => {
                saw_requirement = true;
                found.push(at("block_nested"));
            }
            "unclosed_fence" => {
                found.push((
                    end,
                    SpecProblem {
                        line: p.line,
                        rule: "unclosed_fence",
                    },
                ));
                // A requirement block the fence hides still makes the file
                // a specification document (review X4).
                saw_requirement |= lines.iter().skip(p.line).any(|raw| opens_req(raw));
            }
            "block_unclosed" => {
                unclosed.insert(p.line);
            }
            // Past the newer parser's block cap: the requirement is not
            // read (its body is unknown), as past the older one's.
            "too_many_blocks" => {
                if let Some(Some(("req", attrs))) = raw_line(p.line).and_then(container_of) {
                    saw_requirement = true;
                    let head = req_header(attrs);
                    for rule in head.problems {
                        found.push(at(rule));
                    }
                    if head.id.is_some() {
                        found.push(at("too_many_requirements"));
                    }
                }
            }
            _ => {}
        }
    }
    let mut kept = 0usize;
    for b in read.blocks.iter().filter(|b| b.kind == "req") {
        let Some(Some(("req", attrs))) = raw_line(b.line).and_then(container_of) else {
            continue;
        };
        saw_requirement = true;
        let head = req_header(attrs);
        for rule in head.problems {
            found.push((b.line, SpecProblem { line: b.line, rule }));
        }
        // Requirements never nest, so at most one is open at the end.
        let closed = !unclosed.contains(&b.line);
        let time = if closed { b.end_line } else { end + 1 };
        let mut at = |rule| found.push((time, SpecProblem { line: b.line, rule }));
        if !closed {
            at("block_unclosed");
        }
        if b.body.is_empty() && closed {
            at("body_empty");
        } else if b.body.len() > MAX_REQUIREMENT_BODY_BYTES {
            at("body_too_long");
        }
        let Some(requirement) = crate::legacy::requirement(b, false) else {
            continue;
        };
        if kept >= MAX_REQUIREMENTS_PER_DOCUMENT {
            at("too_many_requirements");
            continue;
        }
        kept += 1;
        doc.requirements.push(requirement);
    }
    found.sort_by_key(|(time, _)| *time);
    for (_, p) in found {
        doc.problem(p.line, p.rule);
    }
    saw_requirement
}

#[cfg(test)]
#[path = "spec_doc_oracle.rs"]
mod oracle;

#[cfg(test)]
#[path = "spec_doc_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "spec_doc_plan13_tests.rs"]
mod plan13_tests;
