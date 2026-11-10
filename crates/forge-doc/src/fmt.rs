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
//! migration, ADR-0026 §7).

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
}

impl FmtRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotUtf8 => "not_utf8",
            Self::TooLarge => "too_large",
            Self::NotADocument => "not_a_document",
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
    out.extend(front_matter(
        lines.get(1..close).unwrap_or_default(),
        doctype,
    ));
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

/// Reorder the top-level keys; each key takes the comment and blank lines
/// above it and its own continuation lines.
fn front_matter(lines: &[&str], doctype: Option<&Doctype>) -> Vec<String> {
    let mut chunks: Vec<(Option<String>, Vec<&str>)> = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    for line in lines {
        let top = !line.starts_with([' ', '\t']) && !line.trim().is_empty();
        let comment = line.trim_start().starts_with('#');
        if top && !comment {
            let key = line.split_once(':').map(|(k, _)| k.trim().to_owned());
            let mut own = std::mem::take(&mut pending);
            own.push(line);
            chunks.push((key, own));
        } else if top || line.trim().is_empty() {
            // A comment or a blank line: it belongs to the next key.
            pending.push(line);
        } else if let Some((_, own)) = chunks.last_mut() {
            // A continuation (an indented value).
            own.append(&mut pending);
            own.push(line);
        } else {
            pending.push(line);
        }
    }
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

// ── the body ───────────────────────────────────────────────────────────

fn body(lines: &[&str], doctype: Option<&Doctype>, out: &mut Vec<String>) {
    let mut fence: Option<crate::spec_doc::Fence> = None;
    let mut in_comment = false;
    for raw in lines {
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
        let untouched = t.is_empty()
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
    if t.starts_with("> ") {
        return (format!("{}> ", &raw[..indent]), indent + 2);
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
            if c == '`' {
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
                if text.get(end..).is_some_and(|r| !r.trim().is_empty()) {
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
        let piece = if n == 0 {
            piece.trim_end()
        } else {
            piece.trim()
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

#[cfg(test)]
#[path = "fmt_tests.rs"]
mod tests;
