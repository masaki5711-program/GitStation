//! The document's view (plan 13 §7.3 "文書の JSON", stage 3): one shape
//! that the web screen, the API, MCP and a later export all read, built
//! from the one parser ([`crate::document`]). Pure: the caller (the server)
//! resolves references, because only it knows the repository's index, the
//! reader's permissions and every document's classification.
//!
//! - The front matter as written (the base keys picked out, the rest as
//!   `fields`), the classification the AI must respect.
//! - The body split at level-2 headings into sections; each section is a
//!   sequence of Markdown text and blocks (kind, id, attributes, body).
//! - Every `[[…]]` reference with how it resolved: a document (and block)
//!   of this repository, broken, another repository's (resolved later), or
//!   **hidden** — the target is more confidential than this document, so
//!   only its id is shown (Q14, 2026-11-10).
//!
//! The view is versioned ([`VIEW_VERSION`]): a change that removes or
//! renames a field raises it.

use crate::document::{Block, Classification, Document, Reference, TypeDefault, Value};

/// The view's schema version.
pub const VIEW_VERSION: u32 = 1;

/// A front matter value in the view.
pub type FieldValue = Value;

/// One front matter field the base keys do not cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldView {
    pub key: String,
    pub value: FieldValue,
}

/// A piece of a section, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Markdown between blocks (trimmed of blank lines at both ends; never
    /// empty). Lines kept as written: [`join_cjk_lines`] is the display's.
    Text { markdown: String, line: usize },
    /// A block with an id, or a plain container (`::: note`).
    Block(BlockView),
}

/// A block in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockView {
    pub kind: String,
    pub id: Option<String>,
    /// `(key, value)` as written, in the written order.
    pub attrs: Vec<(String, String)>,
    pub deprecated: bool,
    pub body: String,
    pub line: usize,
}

/// A level-2 section (`None` heading: the text before the first one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionView {
    pub heading: Option<String>,
    pub line: usize,
    pub pieces: Vec<Piece>,
}

/// How a reference resolved (the caller decides; see [`Resolver`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A document of this repository the reader may see, no more
    /// confidential than this one: its title (and the block's kind).
    Found {
        title: String,
        block_kind: Option<String>,
    },
    /// The target is more confidential than this document: id only, no
    /// title or text (Q14).
    Hidden,
    /// No such document or block (or one the reader may not see — the same
    /// answer, so the view never tells them apart).
    Broken,
    /// Another repository's (`[[repo:ID]]`): resolved by the caller later.
    External,
}

/// A reference in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefView {
    pub repo: Option<String>,
    pub doc: String,
    pub block: Option<String>,
    pub line: usize,
    pub target: Target,
}

/// What the caller knows about other documents.
pub trait Resolver {
    /// The target document's title, classification and its blocks' kinds,
    /// when the reader may see it (`None`: absent or not visible).
    fn document(&self, id: &str) -> Option<TargetDoc>;
}

/// A document a reference may point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDoc {
    pub title: String,
    pub classification: Classification,
    /// `(block id, kind)`.
    pub blocks: Vec<(String, String)>,
}

/// The whole view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentView {
    pub version: u32,
    pub doctype: Option<String>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub owner: Option<String>,
    pub classification: Classification,
    pub fields: Vec<FieldView>,
    pub sections: Vec<SectionView>,
    pub refs: Vec<RefView>,
}

/// The base keys picked out of the front matter (the rest are `fields`).
const PICKED: &[&str] = &[
    "doctype",
    "id",
    "title",
    "owner",
    "classification",
    "confidential",
];

/// Build the view of `doc` (module docs). `ty` is what the caller knows of
/// the document's type (its default classification).
pub fn view(doc: &Document, ty: TypeDefault, resolver: &dyn Resolver) -> DocumentView {
    let classification = doc.classification(ty);
    let fields = doc
        .meta
        .iter()
        .flatten()
        .filter(|f| !PICKED.contains(&f.key.as_str()))
        .map(|f| FieldView {
            key: f.key.clone(),
            value: f.value.clone(),
        })
        .collect();
    let refs = doc
        .refs
        .iter()
        .map(|r| RefView {
            repo: r.repo.clone(),
            doc: r.doc.clone(),
            block: r.block.clone(),
            line: r.line,
            target: resolve(r, classification, resolver),
        })
        .collect();
    DocumentView {
        version: VIEW_VERSION,
        doctype: doc.doctype().map(str::to_owned),
        id: doc.id().map(str::to_owned),
        title: doc.meta_text("title").map(str::to_owned),
        owner: doc.meta_text("owner").map(str::to_owned),
        classification,
        fields,
        sections: Vec::new(),
        refs,
    }
}

/// [`view`] with the sections, which need the document's text.
pub fn view_with_body(
    text: &str,
    doc: &Document,
    ty: TypeDefault,
    resolver: &dyn Resolver,
) -> DocumentView {
    let mut v = view(doc, ty, resolver);
    v.sections = sections(text, doc);
    v
}

fn resolve(r: &Reference, mine: Classification, resolver: &dyn Resolver) -> Target {
    if r.repo.is_some() {
        return Target::External;
    }
    let Some(target) = resolver.document(&r.doc) else {
        return Target::Broken;
    };
    if target.classification > mine {
        return Target::Hidden;
    }
    match &r.block {
        None => Target::Found {
            title: target.title,
            block_kind: None,
        },
        Some(b) => match target.blocks.iter().find(|(id, _)| id == b) {
            Some((_, kind)) => Target::Found {
                title: target.title,
                block_kind: Some(kind.clone()),
            },
            None => Target::Broken,
        },
    }
}

/// One document that refers to a target (plan 13 §8.2 "参照している側を逆に
/// 引く").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrer {
    /// The referring document: an index into the `docs` given to
    /// [`referrers`].
    pub doc: usize,
    /// Its references to the target, in the written order: `(line, the
    /// block named)`.
    pub refs: Vec<(usize, Option<String>)>,
}

/// What [`referrers`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Referrers {
    /// In the order of `docs`.
    pub found: Vec<Referrer>,
    /// The documents whose references hit the parser's cap
    /// ([`crate::document::MAX_REFS_PER_DOCUMENT`]): they may refer to the
    /// target in what was not kept, so the answer is incomplete.
    pub capped: Vec<usize>,
}

/// The documents of `docs` that refer to `target` (to its block `block`
/// only, when given). Only this repository's references count (`[[repo:…]]`
/// is another repository's); references inside code are no references (the
/// parser keeps none). A document's references to itself — the target and
/// its translations, which carry its `id` — are not referrers.
pub fn referrers(docs: &[&Document], target: &str, block: Option<&str>) -> Referrers {
    let mut out = Referrers::default();
    for (i, d) in docs.iter().enumerate() {
        if d.refs.len() >= crate::document::MAX_REFS_PER_DOCUMENT {
            out.capped.push(i);
        }
        if d.id() == Some(target) {
            continue;
        }
        let refs: Vec<(usize, Option<String>)> = d
            .refs
            .iter()
            .filter(|r| r.repo.is_none() && r.doc == target)
            .filter(|r| block.is_none_or(|b| r.block.as_deref() == Some(b)))
            .map(|r| (r.line, r.block.clone()))
            .collect();
        if !refs.is_empty() {
            out.found.push(Referrer { doc: i, refs });
        }
    }
    out
}

/// The body's sections: text between top-level blocks, and the blocks.
fn sections(text: &str, doc: &Document) -> Vec<SectionView> {
    let lines: Vec<&str> = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let start = body_start(&lines, doc);
    // Top-level blocks only (a block inside another is part of its body).
    let mut top: Vec<&Block> = Vec::new();
    for b in &doc.blocks {
        let inside = top
            .last()
            .is_some_and(|t| b.line > t.line && b.line <= t.end_line);
        if !inside {
            top.push(b);
        }
    }
    let heading_at: Vec<(usize, &str)> = doc
        .headings
        .iter()
        .filter(|h| h.level == 2)
        .map(|h| (h.line, h.text.as_str()))
        .collect();
    let mut out: Vec<SectionView> = vec![SectionView {
        heading: None,
        line: start + 1,
        pieces: Vec::new(),
    }];
    let mut text_from = start + 1; // 1-based first line of pending text
    let mut blocks = top.iter().peekable();
    let mut line = start + 1;
    while line <= lines.len() {
        if let Some(b) = blocks.peek().filter(|b| b.line == line) {
            flush(&mut out, &lines, text_from, line - 1);
            if let Some(s) = out.last_mut() {
                s.pieces.push(Piece::Block(block_view(b)));
            }
            line = b.end_line.max(b.line) + 1;
            text_from = line;
            blocks.next();
            continue;
        }
        if let Some((_, heading)) = heading_at.iter().find(|(l, _)| *l == line) {
            flush(&mut out, &lines, text_from, line - 1);
            out.push(SectionView {
                heading: Some((*heading).to_owned()),
                line,
                pieces: Vec::new(),
            });
            text_from = line + 1;
        }
        line += 1;
    }
    flush(&mut out, &lines, text_from, lines.len());
    // The part before the first heading is dropped when it holds nothing.
    if out
        .first()
        .is_some_and(|s| s.heading.is_none() && s.pieces.is_empty())
    {
        out.remove(0);
    }
    out
}

/// The first body line's index (0-based): after the front matter.
fn body_start(lines: &[&str], doc: &Document) -> usize {
    if !doc.has_front_matter {
        return 0;
    }
    lines
        .iter()
        .skip(1)
        .position(|l| matches!(l.trim_end(), "---" | "..."))
        .map_or(lines.len(), |i| i + 2)
}

/// Add the text of lines `from..=to` (1-based) to the last section.
fn flush(out: &mut [SectionView], lines: &[&str], from: usize, to: usize) {
    if from == 0 || from > to {
        return;
    }
    let chunk = lines.get(from - 1..to).unwrap_or_default();
    let first = chunk.iter().position(|l| !l.trim().is_empty());
    let last = chunk.iter().rposition(|l| !l.trim().is_empty());
    let (Some(first), Some(last)) = (first, last) else {
        return;
    };
    let markdown = chunk.get(first..=last).unwrap_or_default().join("\n");
    if let Some(s) = out.last_mut() {
        s.pieces.push(Piece::Text {
            markdown,
            line: from + first,
        });
    }
}

fn block_view(b: &Block) -> BlockView {
    BlockView {
        kind: b.kind.clone(),
        id: b.id.clone(),
        attrs: b
            .attrs
            .iter()
            .map(|a| (a.key.clone(), a.value.clone()))
            .collect(),
        deprecated: b.deprecated(),
        body: b.body.clone(),
        line: b.line,
    }
}

/// For display (plan 13 §4.1): join the lines of a paragraph the way
/// Japanese prose needs. One sentence per line is how documents are stored;
/// Markdown would show a line break between two Japanese characters as a
/// space. A break between two characters of which either is CJK (or
/// full-width punctuation) is removed; any other break becomes one space.
/// Blank lines (paragraph ends) are kept. Code fences and table rows are not
/// touched; callers pass prose only.
pub fn join_cjk_lines(text: &str) -> String {
    let mut lines = text.split('\n');
    let mut out = lines.next().unwrap_or_default().to_owned();
    let mut prev = out.clone();
    for next in lines {
        let hard = prev.ends_with("  ") || prev.ends_with('\\');
        if prev.trim().is_empty() || next.trim().is_empty() || hard {
            out.push('\n');
            out.push_str(next);
        } else {
            let before = prev.trim_end().chars().last();
            let after = next.trim_start().chars().next();
            let len = out.trim_end().len();
            out.truncate(len);
            if !(before.is_some_and(is_cjk) || after.is_some_and(is_cjk)) {
                out.push(' ');
            }
            out.push_str(next.trim_start());
        }
        prev = next.to_owned();
    }
    out
}

/// For display: a section's Markdown (a [`Piece::Text`] or a block's body)
/// with [`join_cjk_lines`] applied to its prose only. A paragraph's lines,
/// and a list item with its continuation lines, are joined; code fences
/// (kept byte for byte), headings, table rows, block quotes, thematic
/// breaks and setext underlines are left as they are, one per line, so the
/// Markdown keeps its structure. The web screen, MCP and an export draw
/// this; the view keeps the text as written.
pub fn display_markdown(markdown: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    let mut fence: Option<&str> = None;
    let flush = |run: &mut Vec<&str>, out: &mut Vec<String>| {
        if !run.is_empty() {
            out.push(join_cjk_lines(&run.join("\n")));
            run.clear();
        }
    };
    for line in markdown.split('\n') {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            out.push(line.to_owned());
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        let opens_fence = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        if let Some(marker) = opens_fence {
            flush(&mut run, &mut out);
            out.push(line.to_owned());
            fence = Some(marker);
            continue;
        }
        if trimmed.is_empty() || is_structural(trimmed) {
            flush(&mut run, &mut out);
            out.push(line.to_owned());
            continue;
        }
        if is_list_item(trimmed) {
            flush(&mut run, &mut out);
        }
        run.push(line);
    }
    flush(&mut run, &mut out);
    out.join("\n")
}

/// A line Markdown gives its own meaning (never joined with another).
fn is_structural(trimmed: &str) -> bool {
    let only = |c: char| {
        let t = trimmed.trim_end();
        t.len() >= 3 && t.chars().all(|x| x == c || x == ' ')
    };
    trimmed.starts_with('#')
        || trimmed.starts_with('|')
        || trimmed.starts_with('>')
        || trimmed.starts_with("<!--")
        || only('-')
        || only('*')
        || only('_')
        || (trimmed.trim_end().chars().all(|c| c == '=') && !trimmed.trim_end().is_empty())
}

/// `- `, `* `, `+ `, `1. ` or `1) ` (a list item's first line).
fn is_list_item(trimmed: &str) -> bool {
    if ["- ", "* ", "+ "].iter().any(|m| trimmed.starts_with(m)) {
        return true;
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    (1..=9).contains(&digits)
        && trimmed
            .get(digits..)
            .is_some_and(|rest| rest.starts_with(". ") || rest.starts_with(") "))
}

/// CJK ideographs, kana, full-width forms and CJK punctuation.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F   // CJK symbols and punctuation
        | 0x3040..=0x30FF // hiragana, katakana
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xF900..=0xFAFF // compatibility ideographs
        | 0xFF00..=0xFFEF // half-width and full-width forms
        | 0x20000..=0x2FA1F)
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
