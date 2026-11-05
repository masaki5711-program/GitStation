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
//!   `...`). Its YAML is read by `repo_config`'s tree builder (no anchors,
//!   aliases, tags, duplicate keys, a second document; at most
//!   [`MAX_FRONT_MATTER_BYTES`]). Keys: `spec`, `title`, `status`
//!   (required) and `owners` (a list of at most [`MAX_OWNERS`]); any other
//!   key is refused.
//! - **Blocks**: a line `::: req <ID> [key=value …]` opens a requirement,
//!   a line `:::` closes the innermost container. Other containers
//!   (`::: note`) nest and close the same way but are no requirements.
//!   Attributes: `priority` (`must`, `should`, `could`, `wont`) and
//!   `status` (`active`, `deprecated` — an id is never removed, plan 03
//!   §4.2). Inside a fenced code block (```` ``` ```` / `~~~`) nothing is
//!   a block: specifications show the syntax in examples.
//! - **Spec document**: a file with a front matter or a requirement block
//!   ([`SpecDocument::is_spec`]); any other Markdown file under a
//!   specification directory (a README) is not indexed.

use crate::repo_config::{mapping, read_tree, text, Kind, Node};

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
/// The deepest nesting of containers (`too_deep`).
const MAX_CONTAINER_DEPTH: usize = 16;

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
}

impl SpecDocument {
    /// Whether the file is a specification document (it has a front
    /// matter or a requirement block, or could not be read as text).
    pub fn is_spec(&self) -> bool {
        self.spec
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
    let mut doc = SpecDocument::default();
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
    let saw_requirement = blocks(&lines, body_start, &mut doc);
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
    let mut spec = None;
    let mut title = None;
    let mut status = None;
    let mut owners = Vec::new();
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

/// A container being read.
enum Open {
    /// A requirement: its id (when valid), opening line, attributes and
    /// body lines.
    Req {
        id: Option<RequirementId>,
        line: usize,
        priority: Option<Priority>,
        deprecated: bool,
        body: Vec<String>,
    },
    /// Any other container (`::: note`), or a nested `::: req`.
    Other,
}

/// A code fence: its character and length.
struct Fence(u8, usize);

/// The fence a line opens (or closes), when it is one: up to three spaces,
/// then three or more backticks or tildes.
fn fence_of(line: &str) -> Option<Fence> {
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

/// Read the blocks into `doc`; whether any `::: req` line was seen.
fn blocks(lines: &[&str], start: usize, doc: &mut SpecDocument) -> bool {
    let mut saw_requirement = false;
    let mut stack: Vec<Open> = Vec::new();
    let mut fence: Option<Fence> = None;
    let mut last_line = start;
    for (i, raw) in lines.iter().enumerate().skip(start) {
        let line = i + 1;
        last_line = line;
        if let Some(open) = &fence {
            let closes = fence_of(raw).is_some_and(|f| {
                f.0 == open.0 && f.1 >= open.1 && raw.trim().bytes().all(|b| b == open.0)
            });
            if closes {
                fence = None;
            }
            push_body(&mut stack, raw);
            continue;
        }
        if let Some(f) = fence_of(raw) {
            fence = Some(f);
            push_body(&mut stack, raw);
            continue;
        }
        match container_of(raw) {
            None => push_body(&mut stack, raw),
            Some(None) => match stack.pop() {
                None => doc.problem(line, "close_unmatched"),
                Some(Open::Other) => {}
                Some(Open::Req {
                    id,
                    line: opened,
                    priority,
                    deprecated,
                    body,
                }) => finish(doc, (id, opened, line, true), priority, deprecated, &body),
            },
            Some(Some((name, attrs))) => {
                if stack.len() >= MAX_CONTAINER_DEPTH {
                    doc.problem(line, "too_deep");
                    stack.push(Open::Other);
                    continue;
                }
                if name != "req" {
                    stack.push(Open::Other);
                    continue;
                }
                saw_requirement = true;
                if stack.iter().any(|o| matches!(o, Open::Req { .. })) {
                    doc.problem(line, "block_nested");
                    stack.push(Open::Other);
                    continue;
                }
                stack.push(open_req(doc, line, attrs));
            }
        }
    }
    // Unclosed containers: a requirement is still kept (its problem says
    // where it opened).
    while let Some(open) = stack.pop() {
        if let Open::Req {
            id,
            line,
            priority,
            deprecated,
            body,
        } = open
        {
            doc.problem(line, "block_unclosed");
            finish(
                doc,
                (id, line, last_line, false),
                priority,
                deprecated,
                &body,
            );
        }
    }
    saw_requirement
}

fn push_body(stack: &mut [Open], raw: &str) {
    // The innermost requirement takes the line (a nested container's text
    // is its text too).
    if let Some(Open::Req { body, .. }) = stack
        .iter_mut()
        .rev()
        .find(|o| matches!(o, Open::Req { .. }))
    {
        body.push(raw.to_owned());
    }
}

fn open_req(doc: &mut SpecDocument, line: usize, attrs: &str) -> Open {
    let mut tokens = attrs.split_whitespace();
    let id = match tokens.next() {
        None => {
            doc.problem(line, "requirement_id_missing");
            None
        }
        Some(raw) => {
            let id = RequirementId::parse(raw);
            if id.is_none() {
                doc.problem(line, "requirement_id_invalid");
            }
            id
        }
    };
    let mut priority = None;
    let mut status = None;
    for token in tokens {
        let Some((key, value)) = token.split_once('=') else {
            doc.problem(line, "attribute_invalid");
            continue;
        };
        match key {
            "priority" => {
                if priority.is_some() {
                    doc.problem(line, "attribute_duplicate");
                    continue;
                }
                match Priority::parse(value) {
                    Some(p) => priority = Some(p),
                    None => doc.problem(line, "priority_invalid"),
                }
            }
            "status" => {
                if status.is_some() {
                    doc.problem(line, "attribute_duplicate");
                    continue;
                }
                match value {
                    "active" => status = Some(false),
                    "deprecated" => status = Some(true),
                    _ => doc.problem(line, "requirement_status_invalid"),
                }
            }
            _ => doc.problem(line, "attribute_unknown"),
        }
    }
    Open::Req {
        id,
        line,
        priority,
        deprecated: status.unwrap_or(false),
        body: Vec::new(),
    }
}

/// Keep a read block: `(id, line, end_line, closed)`; an unclosed block's
/// one problem is `block_unclosed` (its body may simply not have started).
fn finish(
    doc: &mut SpecDocument,
    (id, line, end_line, closed): (Option<RequirementId>, usize, usize, bool),
    priority: Option<Priority>,
    deprecated: bool,
    body: &[String],
) {
    let body = body.join("\n").trim().to_owned();
    if body.is_empty() && closed {
        doc.problem(line, "body_empty");
    } else if body.len() > MAX_REQUIREMENT_BODY_BYTES {
        doc.problem(line, "body_too_long");
    }
    let Some(id) = id else {
        return;
    };
    if doc.requirements.len() >= MAX_REQUIREMENTS_PER_DOCUMENT {
        doc.problem(line, "too_many_requirements");
        return;
    }
    doc.requirements.push(Requirement {
        id,
        line,
        end_line,
        priority,
        deprecated,
        body,
    });
}

#[cfg(test)]
#[path = "spec_doc_tests.rs"]
mod tests;
