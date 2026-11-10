//! Documents in the plan 13 format (ADR-0026 §1–§3): Markdown named by the
//! document number, with a YAML front matter that names its type
//! (`doctype`) and blocks that can be referred to by id.
//!
//! ```markdown
//! ---
//! doctype: safety.risk-assessment
//! id: RA-2026-014
//! title: プレス機 No.3 リスクアセスメント
//! owner: safety-team
//! ---
//!
//! ::: hazard H-01 severity=4 likelihood=2
//! スライドの下降中に、ガードの開口部から手が入る。
//! :::
//!
//! ::: control C-01 for=H-01 type=engineering note="和文の値は引用符の中"
//! インターロックを付ける（[[URS-L3-001#REQ-L3-012]]）。
//! :::
//! ```
//!
//! [`parse_document`] is pure and total: bytes in, a [`Document`] out —
//! its front matter (as written), headings, blocks, references and the
//! rules the *text* breaks. Whether the values fit the document's type, and
//! whether references resolve, is the lint's ([`crate::lint`]): it needs the
//! type definitions and the other documents.
//!
//! - **Front matter**: the first line is `---`, closed by `---` or `...`;
//!   read by [`crate::yaml`]'s strict tree (no anchors, aliases, tags,
//!   duplicate keys). At most [`MAX_FRONT_MATTER_BYTES`].
//! - **Blocks**: `::: <kind> [<ID>] [<key>=<value> …]` opens, `:::` closes
//!   the innermost. A value is a word without spaces or `"`, or `"…"` with
//!   only `\"` and `\\` escaped. A block without an id is a plain container
//!   (`::: note`). A block with an id inside another block with an id is
//!   refused (`block_nested`). Nothing inside a code fence is a block.
//! - **References**: `[[ID]]`, `[[ID#BLOCK]]`, `[[repo/path:ID]]` and
//!   `[[repo/path:ID#BLOCK]]`, outside code fences, code spans and HTML
//!   comments.
//! - **HTML**: comments (`<!-- … -->`, also over several lines) are allowed
//!   and hidden; any other raw HTML is refused (`html_not_allowed`, plan 13
//!   §4.1). Autolinks (`<https://…>`) are links, not HTML.

use crate::spec_doc::fence_of;
use crate::yaml::{mapping, read_tree, Kind, Node};

/// A larger file is refused (`too_large`), never read in part.
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// A longer front matter is refused (`front_matter_too_large`).
pub const MAX_FRONT_MATTER_BYTES: usize = 16 * 1024;
/// At most this many blocks are kept (`too_many_blocks`: the rest are not).
pub const MAX_BLOCKS_PER_DOCUMENT: usize = 2_000;
/// At most this many references are kept (`too_many_references`).
pub const MAX_REFS_PER_DOCUMENT: usize = 5_000;
/// At most this many problems are kept per document (the rest are counted).
pub const MAX_PROBLEMS_PER_DOCUMENT: usize = 200;
/// The longest id in a reference (`ref_invalid`).
pub const MAX_REF_ID_LEN: usize = 128;
/// The deepest nesting of blocks (`too_deep`).
const MAX_BLOCK_DEPTH: usize = 16;

/// A document's classification (plan 06 D-5, ADR-0026 §6): how it may be
/// handed to an AI. Not access control (that is the repository's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Classification {
    Public,
    Internal,
    Confidential,
}

impl Classification {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "public" => Self::Public,
            "internal" => Self::Internal,
            "confidential" => Self::Confidential,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Confidential => "confidential",
        }
    }
}

/// A front matter value, as written (scalars are text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Empty, `~` or `null`.
    Null,
    Text(String),
    List(Vec<(usize, Value)>),
    Map(Vec<(String, usize, Value)>),
}

impl Value {
    fn of(node: &Node) -> Self {
        match &node.kind {
            Kind::Null => Self::Null,
            Kind::Text(t) => Self::Text(t.clone()),
            Kind::List(items) => Self::List(items.iter().map(|n| (n.line, Self::of(n))).collect()),
            Kind::Map(entries) => Self::Map(
                entries
                    .iter()
                    .map(|(k, v)| {
                        let key = match &k.kind {
                            Kind::Text(t) => t.clone(),
                            // The tree refuses keys that are not text.
                            Kind::Null | Kind::List(_) | Kind::Map(_) => String::new(),
                        };
                        (key, k.line, Self::of(v))
                    })
                    .collect(),
            ),
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            Self::Null | Self::List(_) | Self::Map(_) => None,
        }
    }
}

/// One front matter key: its name, its line and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaField {
    pub key: String,
    pub line: usize,
    pub value: Value,
}

/// A heading (`#` … `######`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub line: usize,
}

/// One `key=value` of a block's opening line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub key: String,
    pub value: String,
}

/// A block (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: String,
    /// `None`: a plain container (`::: note`).
    pub id: Option<String>,
    pub attrs: Vec<Attr>,
    /// The opening line (1-based).
    pub line: usize,
    /// The closing `:::` (the last line when unclosed).
    pub end_line: usize,
    /// The level-2 heading the block is under (an index into
    /// [`Document::headings`]).
    pub section: Option<usize>,
    /// Its lines (and those of the plain containers inside it), joined
    /// with `\n` and trimmed. Empty for a plain container.
    pub body: String,
}

impl Block {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.key == key)
            .map(|a| a.value.as_str())
    }

    /// `status=deprecated` (ADR-0026 §2).
    pub fn deprecated(&self) -> bool {
        self.attr("status") == Some("deprecated")
    }
}

/// A reference written `[[…]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// `[[repo/path:ID]]`: another repository (resolved through
    /// `.forge/repo.yml`, not here).
    pub repo: Option<String>,
    pub doc: String,
    pub block: Option<String>,
    pub line: usize,
}

/// A rule the text breaks: its 1-based line (0 for the whole file) and the
/// rule's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    pub line: usize,
    pub rule: &'static str,
}

/// A parsed document (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Document {
    /// `None`: no front matter, or one that could not be read.
    pub meta: Option<Vec<MetaField>>,
    /// The file opens with `---` (read or not).
    pub has_front_matter: bool,
    pub headings: Vec<Heading>,
    pub blocks: Vec<Block>,
    pub refs: Vec<Reference>,
    pub problems: Vec<Problem>,
    pub problems_dropped: usize,
}

impl Document {
    pub fn meta_field(&self, key: &str) -> Option<&MetaField> {
        self.meta.as_ref()?.iter().find(|f| f.key == key)
    }

    pub fn meta_text(&self, key: &str) -> Option<&str> {
        self.meta_field(key)?.value.as_text()
    }

    /// The type the document names (`doctype:`).
    pub fn doctype(&self) -> Option<&str> {
        self.meta_text("doctype")
    }

    /// The document number (`id:`).
    pub fn id(&self) -> Option<&str> {
        self.meta_text("id")
    }

    pub fn block(&self, id: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id.as_deref() == Some(id))
    }

    /// The classification an AI must respect (ADR-0026 §6), given the
    /// type's default. Fails closed: a front matter that could not be read,
    /// a value that is no classification, or both `classification` and the
    /// older `confidential` written, is `Confidential`.
    pub fn classification(&self, default: Option<Classification>) -> Classification {
        let Some(_) = &self.meta else {
            return if self.has_front_matter || self.not_text() {
                Classification::Confidential
            } else {
                default.unwrap_or(Classification::Internal)
            };
        };
        let new = self.meta_field("classification");
        let old = self.meta_field("confidential");
        match (new, old) {
            (Some(_), Some(_)) => Classification::Confidential,
            (Some(f), None) => f
                .value
                .as_text()
                .and_then(Classification::parse)
                .unwrap_or(Classification::Confidential),
            (None, Some(f)) => match f.value.as_text() {
                Some("false") => Classification::Internal,
                _ => Classification::Confidential,
            },
            (None, None) => default.unwrap_or(Classification::Internal),
        }
    }

    fn not_text(&self) -> bool {
        self.problems
            .iter()
            .any(|p| matches!(p.rule, "not_utf8" | "too_large"))
    }

    fn problem(&mut self, line: usize, rule: &'static str) {
        if self.problems.len() < MAX_PROBLEMS_PER_DOCUMENT {
            self.problems.push(Problem { line, rule });
        } else {
            self.problems_dropped += 1;
        }
    }
}

/// Parse one document (module docs).
pub fn parse_document(bytes: &[u8]) -> Document {
    let mut doc = Document::default();
    if bytes.len() > MAX_DOCUMENT_BYTES {
        doc.problem(0, "too_large");
        return doc;
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let Ok(text) = std::str::from_utf8(bytes) else {
        doc.problem(0, "not_utf8");
        return doc;
    };
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let start = front_matter(&lines, &mut doc);
    body(&lines, start, &mut doc);
    doc.problems.sort();
    doc
}

// ── the front matter ───────────────────────────────────────────────────

fn front_matter(lines: &[&str], doc: &mut Document) -> usize {
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return 0;
    }
    doc.has_front_matter = true;
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
        Ok(None) => doc.problem(1, "front_matter_empty"),
        Ok(Some(root)) => match mapping(&root, "front_matter_not_mapping") {
            Err(p) => doc.problem(shift(p.line), front_matter_rule(p.reason)),
            Ok(fields) => {
                doc.meta = Some(
                    fields
                        .iter()
                        .map(|f| MetaField {
                            key: f.key.to_owned(),
                            line: shift(f.line),
                            value: shift_value(Value::of(f.value), 1),
                        })
                        .collect(),
                );
            }
        },
    }
    close + 1
}

/// Move the lines inside a value by `by` (the front matter's offset).
fn shift_value(v: Value, by: usize) -> Value {
    match v {
        Value::Null | Value::Text(_) => v,
        Value::List(items) => Value::List(
            items
                .into_iter()
                .map(|(l, v)| (l + by, shift_value(v, by)))
                .collect(),
        ),
        Value::Map(entries) => Value::Map(
            entries
                .into_iter()
                .map(|(k, l, v)| (k, l + by, shift_value(v, by)))
                .collect(),
        ),
    }
}

/// The tree's reason as the document's rule (`front_matter_*`).
fn front_matter_rule(reason: &'static str) -> &'static str {
    match reason {
        "duplicate_key" => "front_matter_duplicate_key",
        "yaml_alias" | "yaml_anchor" | "yaml_tag" => "front_matter_yaml_feature",
        "multiple_documents" => "front_matter_multiple_documents",
        "too_deep" | "too_many_keys" => "front_matter_too_large",
        "front_matter_not_mapping" => "front_matter_not_mapping",
        _ => "front_matter_syntax",
    }
}

// ── the body ───────────────────────────────────────────────────────────

/// A block being read: its index in `doc.blocks` (`None`: past the cap)
/// and whether it has an id.
struct Open {
    index: Option<usize>,
    has_id: bool,
    body: Vec<String>,
}

fn body(lines: &[&str], start: usize, doc: &mut Document) {
    let mut stack: Vec<Open> = Vec::new();
    let mut fence: Option<crate::spec_doc::Fence> = None;
    let mut fence_line = 0usize;
    let mut in_comment = false;
    let mut section: Option<usize> = None;
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
        if !in_comment {
            if let Some(f) = fence_of(raw) {
                fence = Some(f);
                fence_line = line;
                push_body(&mut stack, raw);
                continue;
            }
        }
        // The text outside comments and code spans: what references, HTML
        // and headings are read from.
        let (visible, still_in_comment) = visible_text(raw, in_comment);
        in_comment = still_in_comment;
        scan_inline(&visible, line, doc);
        match container_of(&visible) {
            None => {
                if let Some(h) = heading_of(&visible, line) {
                    if h.level == 2 {
                        section = Some(doc.headings.len());
                    }
                    doc.headings.push(h);
                }
                push_body(&mut stack, raw);
            }
            Some(None) => match stack.pop() {
                None => doc.problem(line, "close_unmatched"),
                Some(open) => close(doc, open, line),
            },
            Some(Some(rest)) => {
                if stack.len() >= MAX_BLOCK_DEPTH {
                    doc.problem(line, "too_deep");
                    stack.push(Open {
                        index: None,
                        has_id: false,
                        body: Vec::new(),
                    });
                    continue;
                }
                let opened = open_block(doc, line, rest, section, &stack);
                stack.push(opened);
            }
        }
    }
    if fence.is_some() {
        doc.problem(fence_line, "unclosed_fence");
    }
    if in_comment {
        doc.problem(last_line, "unclosed_comment");
    }
    while let Some(open) = stack.pop() {
        if let Some(b) = open.index.and_then(|i| doc.blocks.get(i)) {
            let at = b.line;
            doc.problem(at, "block_unclosed");
        }
        close(doc, open, last_line);
    }
}

fn push_body(stack: &mut [Open], raw: &str) {
    // The innermost block with an id takes the line (a plain container's
    // text is its text too).
    if let Some(open) = stack.iter_mut().rev().find(|o| o.has_id) {
        open.body.push(raw.to_owned());
    }
}

fn close(doc: &mut Document, open: Open, end_line: usize) {
    if let Some(b) = open.index.and_then(|i| doc.blocks.get_mut(i)) {
        b.end_line = end_line;
        b.body = open.body.join("\n").trim().to_owned();
    }
}

/// A block line: `None` when it is none, `Some(None)` for a close,
/// `Some(Some(rest))` for an opening (the text after `:::`).
fn container_of(visible: &str) -> Option<Option<&str>> {
    let t = visible.trim();
    let rest = t.strip_prefix(":::")?;
    // `::::` and longer are the same fence-like marker (as in spec_doc).
    let rest = rest.trim_start_matches(':').trim();
    Some((!rest.is_empty()).then_some(rest))
}

fn open_block(
    doc: &mut Document,
    line: usize,
    rest: &str,
    section: Option<usize>,
    stack: &[Open],
) -> Open {
    let (kind, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let unused = Open {
        index: None,
        has_id: false,
        body: Vec::new(),
    };
    if !crate::doctype::word_ok(kind, false) {
        doc.problem(line, "block_kind_invalid");
        return unused;
    }
    let (id, attrs) = match header_tokens(tail.trim()) {
        Ok(parsed) => parsed,
        Err(rule) => {
            doc.problem(line, rule);
            return unused;
        }
    };
    let has_id = id.is_some();
    if has_id && stack.iter().any(|o| o.has_id) {
        doc.problem(line, "block_nested");
        return unused;
    }
    if doc.blocks.len() >= MAX_BLOCKS_PER_DOCUMENT {
        doc.problem(line, "too_many_blocks");
        return Open {
            index: None,
            has_id,
            body: Vec::new(),
        };
    }
    doc.blocks.push(Block {
        kind: kind.to_owned(),
        id,
        attrs,
        line,
        end_line: line,
        section,
        body: String::new(),
    });
    Open {
        index: Some(doc.blocks.len() - 1),
        has_id,
        body: Vec::new(),
    }
}

/// The id (the first bare word) and the `key=value` attributes of a block's
/// opening line, or the rule it breaks.
fn header_tokens(tail: &str) -> Result<(Option<String>, Vec<Attr>), &'static str> {
    let mut id = None;
    let mut attrs: Vec<Attr> = Vec::new();
    let mut rest = tail;
    let mut first = true;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let word_end = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '"')
            .unwrap_or(rest.len());
        let (word, after) = rest.split_at(word_end);
        if let Some(after_eq) = after.strip_prefix('=') {
            if !crate::doctype::word_ok(word, true) {
                return Err("attr_invalid");
            }
            let (value, next) = attr_value(after_eq)?;
            if attrs.iter().any(|a| a.key == word) {
                return Err("attr_duplicate");
            }
            attrs.push(Attr {
                key: word.to_owned(),
                value,
            });
            rest = next;
        } else if first && !word.is_empty() && !after.starts_with('"') {
            id = Some(word.to_owned());
            rest = after;
        } else {
            return Err("attr_invalid");
        }
        first = false;
    }
    Ok((id, attrs))
}

/// One value after `=`: a word, or `"…"` with `\"` and `\\` escaped.
fn attr_value(s: &str) -> Result<(String, &str), &'static str> {
    if let Some(quoted) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = quoted.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some((_, e @ ('"' | '\\'))) => out.push(e),
                    _ => return Err("attr_invalid"),
                },
                '"' => {
                    let next = quoted.get(i + 1..).unwrap_or_default();
                    // The value ends at the quote: a space or the end next.
                    if !(next.is_empty() || next.starts_with(char::is_whitespace)) {
                        return Err("attr_invalid");
                    }
                    if out.is_empty() {
                        return Err("attr_invalid");
                    }
                    return Ok((out, next));
                }
                _ => out.push(c),
            }
        }
        return Err("attr_invalid");
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    let (word, next) = s.split_at(end);
    if word.is_empty() || word.contains('"') {
        return Err("attr_invalid");
    }
    Ok((word.to_owned(), next))
}

/// An ATX heading (`## 手順`): up to three spaces, 1–6 `#`, a space.
fn heading_of(visible: &str, line: usize) -> Option<Heading> {
    let indent = visible.len() - visible.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let t = visible.get(indent..)?;
    let level = t.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = t.get(level..)?;
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    // A closing run of `#` is not part of the text (CommonMark).
    let text = rest.trim();
    let text = text
        .strip_suffix(|c: char| c == '#')
        .map_or(text, |s| s.trim_end_matches('#'))
        .trim_end();
    Some(Heading {
        level: u8::try_from(level).ok()?,
        text: text.to_owned(),
        line,
    })
}

/// The line without HTML comments and code spans (each replaced by a
/// space), and whether a comment is still open at its end.
fn visible_text(raw: &str, mut in_comment: bool) -> (String, bool) {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        if in_comment {
            match rest.find("-->") {
                Some(end) => {
                    rest = rest.get(end + 3..).unwrap_or_default();
                    in_comment = false;
                    out.push(' ');
                }
                None => return (out, true),
            }
            continue;
        }
        let comment = rest.find("<!--");
        let code = rest.find('`');
        match (comment, code) {
            (None, None) => {
                out.push_str(rest);
                return (out, false);
            }
            (Some(c), b) if b.is_none_or(|b| c < b) => {
                out.push_str(rest.get(..c).unwrap_or_default());
                rest = rest.get(c + 4..).unwrap_or_default();
                in_comment = true;
            }
            (_, Some(b)) => {
                out.push_str(rest.get(..b).unwrap_or_default());
                let after = rest.get(b..).unwrap_or_default();
                let run = after.bytes().take_while(|x| *x == b'`').count();
                let ticks = after.get(..run).unwrap_or_default();
                let inner = after.get(run..).unwrap_or_default();
                match inner.find(ticks) {
                    // A code span: hidden.
                    Some(end) => {
                        out.push(' ');
                        rest = inner.get(end + run..).unwrap_or_default();
                    }
                    // No closing run: the backticks are text.
                    None => {
                        out.push_str(ticks);
                        rest = inner;
                    }
                }
            }
            (Some(_), None) => {
                // Unreachable by the guard above; keep the text.
                out.push_str(rest);
                return (out, false);
            }
        }
    }
}

/// References and raw HTML on one visible line.
fn scan_inline(visible: &str, line: usize, doc: &mut Document) {
    let mut rest = visible;
    while let Some(start) = rest.find("[[") {
        let after = rest.get(start + 2..).unwrap_or_default();
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = after.get(..end).unwrap_or_default();
        match reference_of(inner, line) {
            Some(r) => {
                if doc.refs.len() < MAX_REFS_PER_DOCUMENT {
                    doc.refs.push(r);
                } else {
                    doc.problem(line, "too_many_references");
                }
            }
            None => doc.problem(line, "ref_invalid"),
        }
        rest = after.get(end + 2..).unwrap_or_default();
    }
    if has_html(visible) {
        doc.problem(line, "html_not_allowed");
    }
}

/// `[repo/path:]ID[#BLOCK]` (`None`: not a reference's shape).
pub fn reference_of(inner: &str, line: usize) -> Option<Reference> {
    let (target, block) = match inner.split_once('#') {
        Some((t, b)) => (t, Some(b)),
        None => (inner, None),
    };
    let (repo, doc) = match target.split_once(':') {
        Some((r, d)) => (Some(r), d),
        None => (None, target),
    };
    if !ref_id_ok(doc) || block.is_some_and(|b| !ref_id_ok(b)) {
        return None;
    }
    if let Some(r) = repo {
        let ok = !r.is_empty()
            && r.len() <= 512
            && r.split('/').all(|seg| {
                !seg.is_empty()
                    && seg
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            });
        if !ok {
            return None;
        }
    }
    Some(Reference {
        repo: repo.map(str::to_owned),
        doc: doc.to_owned(),
        block: block.map(str::to_owned),
        line,
    })
}

/// A document or block id in a reference: ASCII letters, digits, `.`,
/// `_` and `-`, starting with a letter or digit.
fn ref_id_ok(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_REF_ID_LEN
        && raw
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Raw HTML: `<` then a letter, `/` or `!` (comments are already hidden),
/// unless it is an autolink (`<https://…>`, `<mailto:…>`).
fn has_html(visible: &str) -> bool {
    let bytes = visible.as_bytes();
    bytes.iter().enumerate().any(|(i, b)| {
        if *b != b'<' {
            return false;
        }
        let next = bytes.get(i + 1).copied().unwrap_or(b' ');
        if !(next.is_ascii_alphabetic() || next == b'/' || next == b'!') {
            return false;
        }
        let tail = visible.get(i + 1..).unwrap_or_default();
        let autolink = ["http://", "https://", "mailto:"].iter().any(|p| {
            tail.len() >= p.len()
                && tail
                    .get(..p.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(p))
        });
        !autolink
    })
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;
