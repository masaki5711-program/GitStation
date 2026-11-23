//! The search tokenizer (plan 20 K1, ADR-0040): one pure function turns
//! text into the terms PostgreSQL stores in a `tsvector`, and the same
//! function turns a query into a `tsquery`. No database locale, no
//! extension: the rules live here and are unit-tested.
//!
//! 1. **Normalize**: NFKC (full-width ⇄ half-width, compatibility forms),
//!    then lower case.
//! 2. **Split into runs**: a maximal run of Japanese / CJK characters
//!    (kanji, hiragana, katakana, the iteration mark, hangul) or of other
//!    letters and digits (Latin, Indonesian, numbers); everything else
//!    separates runs.
//! 3. **Terms**: a CJK run becomes its 2-grams (`設備名` → `設備`, `備名`)
//!    and, when indexing, also its last character alone (so a 1-character
//!    CJK piece of a query, searched as a prefix, finds every position); a
//!    word run is one term (cut to [`MAX_WORD_CHARS`] characters).
//! 4. **Query**: every term must match (`&`); at most one term is a
//!    prefix (`:*`): the last word when it has 2+ characters, else the
//!    last 1-character CJK piece.
//!
//! The text form of the vector and the query is built here (quoted
//! lexemes, weights A / B / C, positions), so the database never parses
//! user text with a text-search configuration: it casts the literal
//! (`$1::tsvector`, `$1::tsquery`).
//!
//! Changing these rules changes every stored vector: the whole index must
//! be rebuilt (`forge search reindex`, runbook `search-index`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use unicode_normalization::UnicodeNormalization;

/// The shortest query, in characters after normalization, separators not
/// counted (plan 20 Q7: 2; one character is `FG-WEB-0006`).
pub const MIN_QUERY_CHARS: usize = 2;
/// The longest query, in characters as sent.
pub const MAX_QUERY_CHARS: usize = 200;
/// A word longer than this is cut (indexed and queried the same way).
pub const MAX_WORD_CHARS: usize = 64;
/// The most distinct terms one entry stores; past it the entry is marked
/// `truncated` (the vector must stay far below PostgreSQL's 1 MB).
pub const MAX_INDEX_TERMS: usize = 50_000;
/// The largest position PostgreSQL keeps in a `tsvector`.
const MAX_POSITION: u32 = 16_383;
/// Positions kept per term (enough for ranking by cover density).
const POSITIONS_PER_TERM: usize = 4;

/// The weight of a part of an entry (`setweight`): the title ranks first,
/// headings and identifiers next, the body last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Weight {
    /// The title.
    A,
    /// Paths, identifiers, headings.
    B,
    /// The body.
    C,
}

impl Weight {
    fn letter(self) -> char {
        match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
        }
    }
}

/// NFKC, then lower case.
#[must_use]
pub fn normalize(text: &str) -> String {
    text.nfkc().flat_map(char::to_lowercase).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Cjk,
    Word,
    Separator,
}

/// Japanese and the other scripts written without spaces (punctuation
/// such as `・` and `、` separates).
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3005}'                      // 々
        | '\u{3040}'..='\u{309F}'       // hiragana
        | '\u{30A1}'..='\u{30FA}'       // katakana
        | '\u{30FC}'..='\u{30FF}'       // ー and the iteration marks (not ・)
        | '\u{31F0}'..='\u{31FF}'       // katakana phonetic extensions
        | '\u{3400}'..='\u{4DBF}'       // CJK extension A
        | '\u{4E00}'..='\u{9FFF}'       // CJK unified ideographs
        | '\u{AC00}'..='\u{D7AF}'       // hangul syllables
        | '\u{F900}'..='\u{FAFF}'       // CJK compatibility ideographs
        | '\u{20000}'..='\u{2FFFF}'     // CJK extensions B and later
    )
}

fn class(c: char) -> Class {
    if is_cjk(c) {
        Class::Cjk
    } else if c.is_alphanumeric() {
        Class::Word
    } else {
        Class::Separator
    }
}

/// One maximal run of normalized text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) cjk: bool,
    pub(crate) chars: Vec<char>,
}

/// The runs of `normalized` text, in order.
pub(crate) fn pieces(normalized: &str) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut current: Option<Piece> = None;
    for c in normalized.chars() {
        let kind = class(c);
        if kind == Class::Separator {
            if let Some(piece) = current.take() {
                out.push(piece);
            }
            continue;
        }
        let cjk = kind == Class::Cjk;
        match current.as_mut() {
            Some(piece) if piece.cjk == cjk => piece.chars.push(c),
            Some(_) | None => {
                if let Some(piece) = current.take() {
                    out.push(piece);
                }
                current = Some(Piece {
                    cjk,
                    chars: vec![c],
                });
            }
        }
    }
    if let Some(piece) = current {
        out.push(piece);
    }
    out
}

/// The terms of one piece, in text order. `index`: the stored side (a CJK
/// run also keeps its last character alone).
fn piece_terms(piece: &Piece, index: bool) -> Vec<String> {
    if !piece.cjk {
        return vec![piece.chars.iter().take(MAX_WORD_CHARS).collect()];
    }
    match piece.chars.as_slice() {
        [] => Vec::new(),
        [one] => vec![one.to_string()],
        chars => {
            let mut terms: Vec<String> = chars
                .windows(2)
                .map(|pair| pair.iter().collect::<String>())
                .collect();
            if index {
                if let Some(last) = chars.last() {
                    terms.push(last.to_string());
                }
            }
            terms
        }
    }
}

/// The stored terms of `text`, in text order (duplicates kept): what
/// [`IndexText`] builds its vector from. Pure and deterministic.
#[must_use]
pub fn index_terms(text: &str) -> Vec<String> {
    pieces(&normalize(text))
        .iter()
        .flat_map(|p| piece_terms(p, true))
        .collect()
}

/// Quote a lexeme for the `tsvector` / `tsquery` text form: the terms hold
/// letters and digits only, but quoting is unconditional.
fn quote(lexeme: &str, out: &mut String) {
    out.push('\'');
    for c in lexeme.chars() {
        if c == '\'' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
}

/// The parts of one entry, weighted, turned into a `tsvector` literal.
#[derive(Debug, Default)]
pub struct IndexText {
    /// term → its first positions with their weights.
    terms: BTreeMap<String, Vec<(u32, Weight)>>,
    position: u32,
    truncated: bool,
}

/// A built vector: the literal for `$1::tsvector` and whether terms were
/// left out ([`MAX_INDEX_TERMS`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedVector {
    pub literal: String,
    pub terms: usize,
    pub truncated: bool,
}

impl IndexText {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `text` with `weight`, after what was added before.
    #[must_use]
    pub fn with(mut self, weight: Weight, text: &str) -> Self {
        self.push(weight, text);
        self
    }

    /// Add `text` with `weight`, after what was added before.
    pub fn push(&mut self, weight: Weight, text: &str) {
        for term in index_terms(text) {
            self.position = self.position.saturating_add(1).min(MAX_POSITION);
            let at = self.position;
            if let Some(positions) = self.terms.get_mut(&term) {
                if positions.len() < POSITIONS_PER_TERM {
                    positions.push((at, weight));
                }
                continue;
            }
            if self.terms.len() >= MAX_INDEX_TERMS {
                self.truncated = true;
                continue;
            }
            self.terms.insert(term, vec![(at, weight)]);
        }
    }

    /// The `tsvector` literal (lexemes sorted, each with its positions and
    /// weights).
    #[must_use]
    pub fn build(&self) -> IndexedVector {
        let mut literal = String::new();
        for (term, positions) in &self.terms {
            if !literal.is_empty() {
                literal.push(' ');
            }
            quote(term, &mut literal);
            literal.push(':');
            for (i, (at, weight)) in positions.iter().enumerate() {
                if i > 0 {
                    literal.push(',');
                }
                let _ = write!(literal, "{at}{}", weight.letter());
            }
        }
        IndexedVector {
            literal,
            terms: self.terms.len(),
            truncated: self.truncated,
        }
    }
}

/// Why a query was refused (`FG-WEB-0006 reason=param_invalid param=q
/// rule=<as_str>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRejection {
    /// Nothing searchable (empty, or separators only).
    Empty,
    /// Fewer than [`MIN_QUERY_CHARS`] searchable characters.
    TooShort,
    /// More than [`MAX_QUERY_CHARS`] characters.
    TooLong,
}

impl QueryRejection {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooShort => "too_short",
            Self::TooLong => "too_long",
        }
    }
}

/// One term of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    pub text: String,
    /// Matches any stored term that starts with `text`.
    pub prefix: bool,
}

/// A parsed query: its terms (all must match) and the normalized pieces
/// the snippet highlights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    terms: Vec<QueryTerm>,
    needles: Vec<String>,
}

impl SearchQuery {
    /// Parse a query as typed. Pure.
    pub fn parse(raw: &str) -> Result<Self, QueryRejection> {
        if raw.chars().count() > MAX_QUERY_CHARS {
            return Err(QueryRejection::TooLong);
        }
        let pieces = pieces(&normalize(raw));
        let chars: usize = pieces.iter().map(|p| p.chars.len()).sum();
        if chars == 0 {
            return Err(QueryRejection::Empty);
        }
        if chars < MIN_QUERY_CHARS {
            return Err(QueryRejection::TooShort);
        }
        // At most one prefix term per query (a prefix scans many GIN keys):
        // the last word when it has 2+ characters, otherwise the last
        // one-character CJK piece. Every other term is exact.
        let prefix_at = pieces
            .iter()
            .rposition(|p| !p.cjk)
            .filter(|i| pieces.get(*i).is_some_and(|p| p.chars.len() >= 2))
            .or_else(|| pieces.iter().rposition(|p| p.cjk && p.chars.len() == 1));
        let mut terms: Vec<QueryTerm> = Vec::new();
        for (i, piece) in pieces.iter().enumerate() {
            let prefix_piece = Some(i) == prefix_at;
            for text in piece_terms(piece, false) {
                let term = QueryTerm {
                    text,
                    prefix: prefix_piece,
                };
                if !terms.contains(&term) {
                    terms.push(term);
                }
            }
        }
        let mut needles: Vec<String> = Vec::new();
        for p in &pieces {
            let needle: String = if p.cjk {
                p.chars.iter().collect()
            } else {
                p.chars.iter().take(MAX_WORD_CHARS).collect()
            };
            if !needles.contains(&needle) && needles.len() < MAX_NEEDLES {
                needles.push(needle);
            }
        }
        Ok(Self { terms, needles })
    }

    #[must_use]
    pub fn terms(&self) -> &[QueryTerm] {
        &self.terms
    }

    /// The `tsquery` literal for `$1::tsquery`: every term, `&`-joined.
    #[must_use]
    pub fn tsquery_literal(&self) -> String {
        let mut out = String::new();
        for (i, term) in self.terms.iter().enumerate() {
            if i > 0 {
                out.push_str(" & ");
            }
            quote(&term.text, &mut out);
            if term.prefix {
                out.push_str(":*");
            }
        }
        out
    }

    /// A short, stable fingerprint of the normalized query (binds a cursor
    /// to its query without putting the words in it): the first 16 bytes
    /// of the SHA-256 of [`SearchQuery::tsquery_literal`], in hex.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.tsquery_literal().as_bytes());
        digest.iter().take(16).fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }
}

/// A piece of an entry's text around the first match, with the matched
/// ranges. Offsets count Unicode scalar values (code points) of `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// `[start, end)` ranges in `text`, sorted, not overlapping.
    pub highlights: Vec<(usize, usize)>,
    /// Text was cut before / after the snippet.
    pub cut_before: bool,
    pub cut_after: bool,
}

/// The most text a snippet scans for matches (the reader hands over at
/// most this much of a body; the CPU per hit stays bounded).
pub const SNIPPET_SCAN_CHARS: usize = 16_384;
/// The most distinct query pieces a snippet looks for.
pub const MAX_NEEDLES: usize = 8;

/// The snippet of `text` for `query`: at most `max_chars` characters
/// around the first match (the start of the text without one). Pure.
#[must_use]
pub fn snippet(text: &str, query: &SearchQuery, max_chars: usize) -> Snippet {
    let original: Vec<char> = text.chars().take(SNIPPET_SCAN_CHARS).collect();
    // Normalize character by character, remembering where each normalized
    // character came from.
    let mut folded: Vec<char> = Vec::with_capacity(original.len());
    let mut origin: Vec<usize> = Vec::with_capacity(original.len());
    for (i, c) in original.iter().enumerate() {
        for n in std::iter::once(*c).nfkc().flat_map(char::to_lowercase) {
            folded.push(n);
            origin.push(i);
        }
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for needle in &query.needles {
        let needle: Vec<char> = needle.chars().collect();
        if needle.is_empty() || needle.len() > folded.len() {
            continue;
        }
        let mut at = 0usize;
        while let Some(window) = folded.get(at..at + needle.len()) {
            let found = (window == needle.as_slice())
                .then(|| origin.get(at).zip(origin.get(at + needle.len() - 1)))
                .flatten();
            if let Some((start, last)) = found {
                ranges.push((*start, last + 1));
                at += needle.len();
            } else {
                at += 1;
            }
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let total = original.len();
    let max_chars = max_chars.max(1);
    let start = merged
        .first()
        .map_or(0, |(s, _)| s.saturating_sub(max_chars / 4));
    let end = (start + max_chars).min(total);
    let start = end.saturating_sub(max_chars).min(start);
    let highlights = merged
        .iter()
        .filter(|(s, e)| *e > start && *s < end)
        .map(|(s, e)| ((*s).max(start) - start, (*e).min(end) - start))
        .collect();
    Snippet {
        text: original[start..end].iter().collect(),
        highlights,
        cut_before: start > 0,
        cut_after: end < text.chars().count(),
    }
}

#[cfg(test)]
#[path = "search_text_tests.rs"]
mod tests;
