//! `fmt` for documents in the plan 13 format (ADR-0026 §4).
//!
//! - **One sentence per line**: a prose line is split after `。`, `！` or
//!   `？` when more text follows — never inside `「」`, `『』`, `（）`,
//!   a code span, a link or a `[[reference]]`, and never in the front
//!   matter, a code fence, an HTML comment, a table, a heading, a block's
//!   opening line or an indented code line. Lines are only split, never
//!   joined (a line break the writer made stays).
//! - **The front matter's keys in order**: the base keys (`doctype`, `id`,
//!   `title`, `owner`, `classification`, `review_cycle`, `related`, `lang`,
//!   `translation_of`, `retired`, `confidential`), then the type's fields in
//!   the type's order, then any other key as written. Each key moves with
//!   its own lines (values and comments above it are kept byte for byte).
//! - **A block's attributes in order**: the id, the type's attributes in
//!   the type's order, `status`, `replaced_by`, then any other as written;
//!   one space between them (a kind the type does not define keeps its
//!   written order). Values are kept as written (quoted or not).
//!
//! [`format_document`] is pure and idempotent (`fmt(fmt(x)) == fmt(x)`).
//! It refuses — never guesses — a file that is not a document in this
//! format: not UTF-8, too large, no readable front matter naming a
//! `doctype` (an older `spec:` document is changed only by an explicit
//! migration, ADR-0026 §7: [`migrate_legacy`], `gstn doc fmt --migrate`).

use crate::doctype::Doctype;
use crate::document::{parse_document, MAX_DOCUMENT_BYTES};
use crate::spec_doc::fence_of;

/// Why a file is not formatted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtRefusal {
    NotUtf8,
    TooLarge,
    /// No front matter naming a `doctype`, or one that cannot be read.
    NotADocument,
    /// `--migrate`: not an older-format file (`spec:` and no `doctype:`).
    NotOlderFormat,
    /// `--migrate`: an older-format file whose front matter breaks a rule
    /// of [`crate::spec_doc`] (`forge/spec-lint` names it): fixed first.
    OlderFormatInvalid,
}

impl FmtRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotUtf8 => "not_utf8",
            Self::TooLarge => "too_large",
            Self::NotADocument => "not_a_document",
            Self::NotOlderFormat => "not_older_format",
            Self::OlderFormatInvalid => "older_format_invalid",
        }
    }
}

/// The base front matter keys, in their order.
const BASE_ORDER: &[&str] = &[
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
    "confidential",
];

/// Format one document (module docs). `doctype` is the document's type
/// when it is known (its field and attribute order); without it the base
/// order still applies and the rest stays as written.
pub fn format_document(bytes: &[u8], doctype: Option<&Doctype>) -> Result<String, FmtRefusal> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(FmtRefusal::TooLarge);
    }
    let parsed = parse_document(bytes);
    if parsed.meta.is_none() || parsed.doctype().is_none() {
        return Err(FmtRefusal::NotADocument);
    }
    let (bom, rest) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => ("\u{feff}", rest),
        None => ("", bytes),
    };
    let text = std::str::from_utf8(rest).map_err(|_| FmtRefusal::NotUtf8)?;
    let crlf = text.contains("\r\n");
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let close = lines
        .iter()
        .skip(1)
        .position(|l| matches!(l.trim_end(), "---" | "..."))
        .map(|i| i + 1)
        .ok_or(FmtRefusal::NotADocument)?;

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 8);
    out.push(lines.first().copied().unwrap_or_default().to_owned());
    let fm = lines.get(1..close).unwrap_or_default();
    let reordered = front_matter(fm, doctype);
    // Reordering must never change what the YAML says (review MR2b): when
    // it would, the front matter stays as written.
    if same_meaning(fm, &reordered) {
        out.extend(reordered);
    } else {
        out.extend(fm.iter().map(|l| (*l).to_owned()));
    }
    out.push(lines.get(close).copied().unwrap_or_default().to_owned());
    body(
        lines.get(close + 1..).unwrap_or_default(),
        doctype,
        &mut out,
    );
    let joined = out.join(if crlf { "\r\n" } else { "\n" });
    Ok(format!("{bom}{joined}"))
}

// ── the front matter ───────────────────────────────────────────────────

/// The front matter's top-level keys, each with the comment and blank
/// lines above it and its own continuation lines, and the comment and
/// blank lines after the last one.
type Chunks<'a> = (Vec<(Option<String>, Vec<&'a str>)>, Vec<&'a str>);

fn chunks<'a>(lines: &[&'a str]) -> Chunks<'a> {
    let mut chunks: Vec<(Option<String>, Vec<&str>)> = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    for line in lines {
        let top = !line.starts_with([' ', '\t']) && !line.trim().is_empty();
        let comment = line.trim_start().starts_with('#');
        let key = if top && !comment { key_of(line) } else { None };
        if let Some(key) = key {
            let mut own = std::mem::take(&mut pending);
            own.push(line);
            chunks.push((Some(key), own));
        } else if (top && comment) || line.trim().is_empty() {
            // A comment or a blank line: it belongs to the next key.
            pending.push(line);
        } else if let Some((_, own)) = chunks.last_mut() {
            // A continuation: an indented value, or a sequence item / a
            // flow collection's end written at column 0 (review MR2b).
            own.append(&mut pending);
            own.push(line);
        } else {
            pending.push(line);
        }
    }
    (chunks, pending)
}

/// Reorder the top-level keys; each key takes the comment and blank lines
/// above it and its own continuation lines.
fn front_matter(lines: &[&str], doctype: Option<&Doctype>) -> Vec<String> {
    let (mut chunks, pending) = chunks(lines);
    let rank = |key: &Option<String>| -> (usize, usize) {
        let Some(k) = key else {
            return (2, 0);
        };
        if let Some(i) = BASE_ORDER.iter().position(|b| b == k) {
            return (0, i);
        }
        match doctype.and_then(|t| t.fields.iter().position(|f| &f.key == k)) {
            Some(i) => (1, i),
            None => (2, 0),
        }
    };
    // A stable sort keeps the written order among equals.
    chunks.sort_by_key(|(k, _)| rank(k));
    let mut out: Vec<String> = chunks
        .into_iter()
        .flat_map(|(_, own)| own.into_iter().map(str::to_owned))
        .collect();
    out.extend(pending.into_iter().map(str::to_owned));
    out
}

/// A top-level key line's key (`key:`, `"key":`): `None` for anything
/// else at column 0 (`- item`, `]`, `}`), which continues the key above.
fn key_of(line: &str) -> Option<String> {
    if line.starts_with(['-', '[', ']', '{', '}', '?', '&', '*', '!', '|', '>', '#']) {
        return None;
    }
    let (key, _) = line.split_once(':')?;
    let key = key.trim().trim_matches(['"', '\'']);
    let ok = key
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b' '));
    ok.then(|| key.to_owned())
}

/// Whether two front matters mean the same: the same top-level keys with
/// the same values (lines and the top-level order aside).
fn same_meaning(before: &[&str], after: &[String]) -> bool {
    use crate::yaml::{read_tree, Kind, Node};
    fn eq(a: &Node, b: &Node) -> bool {
        match (&a.kind, &b.kind) {
            (Kind::Null, Kind::Null) => true,
            (Kind::Text(x), Kind::Text(y)) => x == y,
            (Kind::List(x), Kind::List(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eq(p, q))
            }
            (Kind::Map(x), Kind::Map(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y)
                        .all(|((k1, v1), (k2, v2))| eq(k1, k2) && eq(v1, v2))
            }
            _ => false,
        }
    }
    let key = |n: &Node| match &n.kind {
        Kind::Text(t) => t.clone(),
        Kind::Null | Kind::List(_) | Kind::Map(_) => String::new(),
    };
    let (Ok(Some(a)), Ok(Some(b))) = (read_tree(&before.join("\n")), read_tree(&after.join("\n")))
    else {
        return false;
    };
    let (Kind::Map(a), Kind::Map(b)) = (&a.kind, &b.kind) else {
        return false;
    };
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, v)| {
        b.iter()
            .find(|(k2, _)| key(k2) == key(k))
            .is_some_and(|(_, v2)| eq(v, v2))
    })
}

// ── the body ───────────────────────────────────────────────────────────

fn body(lines: &[&str], doctype: Option<&Doctype>, out: &mut Vec<String>) {
    let mut fence: Option<crate::spec_doc::Fence> = None;
    let mut in_comment = false;
    let mut in_table = false;
    for (i, raw) in lines.iter().enumerate() {
        if let Some(open) = &fence {
            let closes = fence_of(raw).is_some_and(|f| {
                f.0 == open.0 && f.1 >= open.1 && raw.trim().bytes().all(|b| b == open.0)
            });
            if closes {
                fence = None;
            }
            out.push((*raw).to_owned());
            continue;
        }
        if in_comment {
            if raw.contains("-->") {
                in_comment = after_comments(raw);
            }
            out.push((*raw).to_owned());
            continue;
        }
        if let Some(f) = fence_of(raw) {
            fence = Some(f);
            out.push((*raw).to_owned());
            continue;
        }
        if raw.contains("<!--") {
            in_comment = after_comments(raw);
            out.push((*raw).to_owned());
            continue;
        }
        let t = raw.trim_start();
        if t.starts_with(":::") {
            out.push(block_header(raw, doctype));
            continue;
        }
        let indent = raw.len() - t.len();
        // A table runs from its header (the line before a delimiter row) to
        // the next blank line, with or without leading pipes.
        if t.is_empty() {
            in_table = false;
        } else if is_delimiter_row(raw)
            || lines.get(i + 1).is_some_and(|next| is_delimiter_row(next))
        {
            in_table = true;
        }
        let untouched = t.is_empty()
            || in_table
            || t.starts_with('<')
            || t.starts_with('|')
            || t.starts_with('#')
            || raw.starts_with('\t')
            || indent >= 4;
        if untouched {
            out.push((*raw).to_owned());
            continue;
        }
        out.extend(split_sentences(raw));
    }
}

/// Whether an HTML comment is still open at the end of `raw`.
fn after_comments(raw: &str) -> bool {
    let mut open = false;
    let mut rest = raw;
    loop {
        let next = if open {
            rest.find("-->")
        } else {
            rest.find("<!--")
        };
        match next {
            None => return open,
            Some(i) => {
                rest = rest.get(i + if open { 3 } else { 4 }..).unwrap_or_default();
                open = !open;
            }
        }
    }
}

/// A block's opening line with its attributes in order (a line that is
/// no valid opening is kept as written).
fn block_header(raw: &str, doctype: Option<&Doctype>) -> String {
    let indent = &raw[..raw.len() - raw.trim_start().len()];
    let t = raw.trim();
    let Some(rest) = t.strip_prefix(":::") else {
        return raw.to_owned();
    };
    let marker_extra = rest.len() - rest.trim_start_matches(':').len();
    let rest = rest.trim_start_matches(':').trim();
    if rest.is_empty() {
        return raw.to_owned();
    }
    let (kind, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let Some(tokens) = raw_tokens(tail.trim()) else {
        return raw.to_owned();
    };
    // Without the kind's definition the attributes stay as written (only
    // the spacing is made even): there is no order to follow.
    let Some(def) = doctype.and_then(|t| t.block(kind)) else {
        let mut line = format!("{indent}:::{} {kind}", ":".repeat(marker_extra));
        for (_, tok) in tokens {
            line.push(' ');
            line.push_str(tok);
        }
        return line;
    };
    let order: Vec<&str> = def.attrs.iter().map(|a| a.key.as_str()).collect();
    let rank = |key: Option<&str>| -> (usize, usize) {
        match key {
            None => (0, 0),
            Some(k) => match order.iter().position(|o| *o == k) {
                Some(i) => (1, i),
                None => match k {
                    "status" => (2, 0),
                    "replaced_by" => (2, 1),
                    _ => (3, 0),
                },
            },
        }
    };
    let mut tokens = tokens;
    tokens.sort_by_key(|(k, _)| rank(*k));
    let mut line = format!("{indent}:::{} {kind}", ":".repeat(marker_extra));
    for (_, tok) in tokens {
        line.push(' ');
        line.push_str(tok);
    }
    line
}

/// The tokens of a block header as written: `(key, raw)`; the id has no
/// key. `None` when the header is not well formed.
fn raw_tokens(tail: &str) -> Option<Vec<(Option<&str>, &str)>> {
    let mut out = Vec::new();
    let mut rest = tail;
    let mut first = true;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Some(out);
        }
        let start = rest;
        let word_end = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '"')
            .unwrap_or(rest.len());
        let key = rest.get(..word_end)?;
        let after = rest.get(word_end..)?;
        if let Some(value) = after.strip_prefix('=') {
            let used = if value.starts_with('"') {
                quoted_len(value)?
            } else {
                value.find(char::is_whitespace).unwrap_or(value.len())
            };
            let total = word_end + 1 + used;
            out.push((Some(key), start.get(..total)?));
            rest = start.get(total..)?;
        } else if first && !key.is_empty() && !after.starts_with('"') {
            out.push((None, key));
            rest = after;
        } else {
            return None;
        }
        first = false;
    }
}

/// The length of `"…"` (with `\"` and `\\`) at the start of `s`.
fn quoted_len(s: &str) -> Option<usize> {
    let mut chars = s.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next()?;
            }
            '"' => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// The continuation prefix of a list item or a quote (`- ` → two spaces,
/// `> ` → `> `), and the line's own prefix length.
fn continuation(raw: &str) -> (String, usize) {
    let indent = raw.len() - raw.trim_start().len();
    let t = raw.trim_start();
    // A quote, nested or not (`> > `): every marker continues.
    let bytes = raw.as_bytes();
    let mut j = indent;
    while bytes.get(j) == Some(&b'>') {
        j += 1;
        if bytes.get(j) == Some(&b' ') {
            j += 1;
        }
    }
    if j > indent {
        let prefix = raw.get(..j).unwrap_or_default();
        return (prefix.to_owned(), j);
    }
    for marker in ["- ", "* ", "+ "] {
        if t.starts_with(marker) {
            return (" ".repeat(indent + 2), indent + 2);
        }
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        if let Some(after) = t.get(digits..) {
            if after.starts_with(". ") || after.starts_with(") ") {
                return (" ".repeat(indent + digits + 2), indent + digits + 2);
            }
        }
    }
    (raw[..indent].to_owned(), indent)
}

/// Whether `t` (trimmed at its start) would open a block at a line start.
fn starts_block(t: &str) -> bool {
    let marker = [
        "- ", "* ", "+ ", "#", ">", "|", ":::", "```", "~~~", "---", "===", "___", "***", "<",
    ]
    .iter()
    .any(|m| t.starts_with(m))
        || matches!(t, "-" | "*" | "+");
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let ordered = (1..=9).contains(&digits)
        && t.get(digits..)
            .is_some_and(|a| a.starts_with(". ") || a.starts_with(") "));
    marker || ordered
}

/// A GFM table's delimiter row (`|---|:-:|`, `--|--`).
fn is_delimiter_row(line: &str) -> bool {
    let t = line.trim();
    t.contains('|')
        && t.contains('-')
        && t.bytes()
            .all(|b| matches!(b, b'|' | b'-' | b':' | b' ' | b'\t'))
}

/// Split one prose line after each sentence end (module docs).
fn split_sentences(raw: &str) -> Vec<String> {
    let (cont, own) = continuation(raw);
    let Some(text) = raw.get(own..) else {
        return vec![raw.to_owned()];
    };
    let mut cuts: Vec<usize> = Vec::new();
    let mut depth = 0usize; // 「」『』（）
    let mut code: Option<usize> = None; // the backtick run that opened a span
    let mut link = 0usize; // [ … ] and [[ … ]]
    let mut paren_link = 0usize; // ( … ) right after a link's ]
    let bytes = text.as_bytes();
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        if let Some(run) = code {
            // Only a whole run of the same length closes the span.
            if c == '`' && prev != Some('`') {
                let n = bytes
                    .get(i..)
                    .map_or(0, |b| b.iter().take_while(|x| **x == b'`').count());
                if n == run {
                    code = None;
                }
            }
            prev = Some(c);
            continue;
        }
        match c {
            '`' if prev != Some('`') => {
                code = Some(
                    bytes
                        .get(i..)
                        .map_or(1, |b| b.iter().take_while(|x| **x == b'`').count()),
                );
            }
            '「' | '『' | '（' => depth += 1,
            '」' | '』' | '）' => depth = depth.saturating_sub(1),
            '[' => link += 1,
            ']' => link = link.saturating_sub(1),
            '(' if prev == Some(']') => paren_link += 1,
            ')' if paren_link > 0 => paren_link -= 1,
            '。' | '！' | '？' if depth == 0 && link == 0 && paren_link == 0 => {
                let end = i + c.len_utf8();
                // No cut before text that would become another block once it
                // starts a line (a list item, a heading, a rule …).
                let rest = text.get(end..).unwrap_or_default().trim_start();
                if !rest.is_empty() && !starts_block(rest) {
                    cuts.push(end);
                }
            }
            _ => {}
        }
        prev = Some(c);
    }
    if cuts.is_empty() {
        return vec![raw.to_owned()];
    }
    let mut out = Vec::with_capacity(cuts.len() + 1);
    let mut start = 0usize;
    for (n, cut) in cuts.iter().chain(std::iter::once(&text.len())).enumerate() {
        let piece = text.get(start..*cut).unwrap_or_default();
        // The last piece keeps its trailing spaces (a hard line break).
        let last = n == cuts.len();
        let piece = match (n == 0, last) {
            (true, _) => piece.trim_end(),
            (false, true) => piece.trim_start(),
            (false, false) => piece.trim(),
        };
        let prefix = if n == 0 {
            raw.get(..own).unwrap_or_default()
        } else {
            cont.as_str()
        };
        out.push(format!("{prefix}{piece}"));
        start = *cut;
    }
    out
}

// ── the migration of the older format (ADR-0026 §7) ────────────────────

/// What a migration leaves to a person (reported, never guessed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationNote {
    /// No `owner`: the older `owners` are people, not a group, so none is
    /// written; a `TODO` comment in the front matter says what to fill in,
    /// and the lint reports `field_missing owner` until it is.
    OwnerMissing,
    /// `status: deprecated` became `retired: {}`: the day it was deprecated
    /// is not in the file (a `TODO` comment asks for `date:`).
    RetiredDateUnknown,
    /// `status: draft / review / approved` was dropped: the plan 13 format
    /// does not keep a state in the file (the merge request's approval is).
    StatusDropped,
}

impl MigrationNote {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerMissing => "owner_missing",
            Self::RetiredDateUnknown => "retired_date_unknown",
            Self::StatusDropped => "status_dropped",
        }
    }
}

/// A migrated and formatted document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migrated {
    pub text: String,
    pub notes: Vec<MigrationNote>,
}

/// The comment a migration leaves for the owner.
const OWNER_TODO: &str = "# TODO(gstn doc fmt --migrate): owner: <the owning group>";
/// The comment a migration leaves for the retirement date.
const RETIRED_TODO: &str =
    "# TODO(gstn doc fmt --migrate): retired: date: YYYY-MM-DD (the day it was deprecated)";

/// Rewrite an older-format specification (`spec:` front matter, M3 X4)
/// into the plan 13 format, then [`format_document`] it (ADR-0026 §7;
/// `gstn doc fmt --migrate`). Pure, and refuses — never guesses — anything
/// else: a file that is not in the older format, or whose older front
/// matter breaks a rule of [`crate::spec_doc`], is
/// [`FmtRefusal::NotOlderFormat`] / [`FmtRefusal::OlderFormatInvalid`]. A
/// migrated file is in the newer format, so a second migration refuses it
/// (`gstn` then formats it as any document: running twice changes nothing).
///
/// - `spec: X` → `doctype: spec.system` and `id: X` (the value as written);
///   `title` is kept;
/// - `owners: [..]` is removed and **no `owner` is written**: they are
///   people's addresses, not a group (ADR-0026 §7). A `TODO` comment names
///   the people the older file listed; the lint reports `field_missing
///   owner` until a person fills it ([`MigrationNote::OwnerMissing`]);
/// - `status: deprecated` → `retired: {}` with a `TODO` comment for its
///   `date` — no date is invented: the migration's day is not the day it
///   was deprecated, and forge-doc has no clock
///   ([`MigrationNote::RetiredDateUnknown`]); any other `status` is
///   dropped ([`MigrationNote::StatusDropped`]);
/// - `confidential: true` / `false` → `classification: confidential` /
///   `internal` (ADR-0026 §6); without it nothing is written (the type's
///   default applies, never lower than `internal`);
/// - comments and blank lines of the front matter are kept (an end-of-line
///   comment of a removed key goes with it); the body is not touched
///   before the format.
pub fn migrate_legacy(bytes: &[u8], doctype: Option<&Doctype>) -> Result<Migrated, FmtRefusal> {
    use crate::legacy::{is_legacy, SPEC_SYSTEM};
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(FmtRefusal::TooLarge);
    }
    let (bom, rest) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => ("\u{feff}", rest),
        None => ("", bytes),
    };
    let text = std::str::from_utf8(rest).map_err(|_| FmtRefusal::NotUtf8)?;
    let parsed = parse_document(bytes);
    if !is_legacy(&parsed) {
        return Err(FmtRefusal::NotOlderFormat);
    }
    let older = crate::spec_doc::parse_spec_document(bytes);
    let meta = older.meta.as_ref().ok_or(FmtRefusal::OlderFormatInvalid)?;
    let crlf = text.contains("\r\n");
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let close = lines
        .iter()
        .skip(1)
        .position(|l| matches!(l.trim_end(), "---" | "..."))
        .map(|i| i + 1)
        .ok_or(FmtRefusal::OlderFormatInvalid)?;
    let (chunks, trailing) = chunks(lines.get(1..close).unwrap_or_default());

    let mut notes = Vec::new();
    let mut fm: Vec<String> = vec![format!("doctype: {SPEC_SYSTEM}")];
    let mut added: Vec<String> = Vec::new();
    for (key, own) in &chunks {
        let Some(key) = key.as_deref() else {
            return Err(FmtRefusal::OlderFormatInvalid);
        };
        // The comment and blank lines above the key, and the key's lines.
        let at = own
            .iter()
            .position(|l| key_of(l).as_deref() == Some(key))
            .ok_or(FmtRefusal::OlderFormatInvalid)?;
        let (above, keyed) = own.split_at(at);
        fm.extend(above.iter().map(|l| (*l).to_owned()));
        match key {
            "spec" => {
                let first = keyed.first().ok_or(FmtRefusal::OlderFormatInvalid)?;
                let (_, value) = first
                    .split_once(':')
                    .ok_or(FmtRefusal::OlderFormatInvalid)?;
                fm.push(format!("id:{value}"));
                fm.extend(keyed.iter().skip(1).map(|l| (*l).to_owned()));
            }
            "title" => fm.extend(keyed.iter().map(|l| (*l).to_owned())),
            "status" | "owners" | "confidential" => {}
            _ => return Err(FmtRefusal::OlderFormatInvalid),
        }
    }
    if let Some(f) = parsed.meta_field("confidential") {
        let class = match f.value.as_text() {
            Some("true") => "confidential",
            Some("false") => "internal",
            _ => return Err(FmtRefusal::OlderFormatInvalid),
        };
        added.push(format!("classification: {class}"));
    }
    let deprecated = meta.status == crate::spec_doc::SpecStatus::Deprecated;
    if deprecated {
        added.push("retired: {}".to_owned());
        notes.push(MigrationNote::RetiredDateUnknown);
    } else {
        notes.push(MigrationNote::StatusDropped);
    }
    fm.extend(added);
    fm.extend(trailing.iter().map(|l| (*l).to_owned()));
    fm.push(OWNER_TODO.to_owned());
    if !meta.owners.is_empty() {
        fm.push(format!(
            "# (the older file named people: {})",
            meta.owners.join(", ")
        ));
    }
    notes.insert(0, MigrationNote::OwnerMissing);
    if deprecated {
        fm.push(RETIRED_TODO.to_owned());
    }

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 8);
    out.push(lines.first().copied().unwrap_or_default().to_owned());
    out.extend(fm);
    out.extend(
        lines
            .get(close..)
            .unwrap_or_default()
            .iter()
            .map(|l| (*l).to_owned()),
    );
    let migrated = format!("{bom}{}", out.join(if crlf { "\r\n" } else { "\n" }));

    // Never guess: the result must say what the older file said.
    let check = parse_document(migrated.as_bytes());
    let classification = parsed.meta_field("confidential").map(|_| {
        if meta.confidential {
            "confidential"
        } else {
            "internal"
        }
    });
    let same = check.doctype() == Some(SPEC_SYSTEM)
        && check.id() == Some(meta.spec.as_str())
        && check.meta_text("title").map(str::trim) == Some(meta.title.as_str())
        && check.meta_text("classification") == classification
        && check.meta_field("retired").is_some() == deprecated
        && ["spec", "status", "owners", "confidential", "owner"]
            .iter()
            .all(|k| check.meta_field(k).is_none())
        && check.blocks.len() == parsed.blocks.len();
    if !same {
        return Err(FmtRefusal::OlderFormatInvalid);
    }
    let text = format_document(migrated.as_bytes(), doctype)?;
    Ok(Migrated { text, notes })
}

#[cfg(test)]
#[path = "fmt_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "fmt_migrate_tests.rs"]
mod migrate_tests;
