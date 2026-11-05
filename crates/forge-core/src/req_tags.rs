//! `@req` tags (M3 X4, plan 03 §4.3, plan 06 D-7): a test names what it
//! covers in a comment or its name, in any language.
//!
//! ```text
//! // @req ZEVA-CFG-012
//! # @req PBI-123/AC-2, PBI-123/AC-3
//! ```
//!
//! [`scan_line`] finds every tag in one line. A tag is the word `@req`
//! (lower case, not preceded by a letter, digit, `_`, `.`, `@` or `/` — so
//! `me@req.example` is none — and followed by a space or a tab: `@required`
//! is none), then one id, then more ids each after a `,`. An id is a
//! requirement id ([`RequirementId`]) or an acceptance criterion of a
//! kanflow work item ([`CriterionId`], `PBI-<n>/AC-<m>`). Punctuation
//! closing a sentence or a comment (`.`, `)`, `` ` ``, `"`, …) ends an id.
//! Anything else after `@req` is [`TagScan::Malformed`] (kept, cut to
//! [`MAX_REPORTED_TOKEN_BYTES`]: the trace says which line to fix).

use crate::spec_doc::RequirementId;
use crate::work_item::WorkItemRef;

/// The word a tag starts with.
pub const REQ_TAG: &str = "@req";
/// A malformed token is reported at most this long.
pub const MAX_REPORTED_TOKEN_BYTES: usize = 128;
/// At most this many ids per tag (the rest of the line is prose).
pub const MAX_IDS_PER_TAG: usize = 32;
/// The largest acceptance criterion number.
const MAX_CRITERION: u32 = 9_999;

/// An acceptance criterion of a kanflow work item: `PBI-<n>/AC-<m>`, both
/// numbers canonical (no leading zero), `m` from 1 to 9999.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CriterionId {
    work_item: WorkItemRef,
    number: u32,
}

impl CriterionId {
    /// `None` when `raw` is not canonical.
    pub fn parse(raw: &str) -> Option<Self> {
        let (item, criterion) = raw.split_once('/')?;
        let work_item = WorkItemRef::parse(item)?;
        let digits = criterion.strip_prefix("AC-")?;
        let canonical = !digits.is_empty()
            && digits.len() <= 4
            && digits.bytes().all(|b| b.is_ascii_digit())
            && !digits.starts_with('0');
        if !canonical {
            return None;
        }
        let number: u32 = digits.parse().ok()?;
        (1..=MAX_CRITERION)
            .contains(&number)
            .then_some(Self { work_item, number })
    }

    /// The work item it belongs to (`PBI-43`).
    pub fn work_item(&self) -> &WorkItemRef {
        &self.work_item
    }

    pub fn number(&self) -> u32 {
        self.number
    }
}

impl std::fmt::Display for CriterionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/AC-{}", self.work_item, self.number)
    }
}

/// What a tag names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TagTarget {
    Requirement(RequirementId),
    Criterion(CriterionId),
}

impl TagTarget {
    /// A criterion when `raw` has a `/`, a requirement otherwise.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.contains('/') {
            CriterionId::parse(raw).map(Self::Criterion)
        } else {
            RequirementId::parse(raw).map(Self::Requirement)
        }
    }
}

impl std::fmt::Display for TagTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Requirement(r) => r.fmt(f),
            Self::Criterion(c) => c.fmt(f),
        }
    }
}

/// One id after a tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagScan {
    Target(TagTarget),
    /// Not an id (as written, cut to [`MAX_REPORTED_TOKEN_BYTES`]; empty
    /// when nothing followed the tag).
    Malformed(String),
}

/// A byte that may not come right before `@req`.
fn joins_before(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'/')
}

/// The characters that end an id (closing a sentence, a comment or a
/// quotation).
fn closes(c: char) -> bool {
    matches!(
        c,
        '.' | ';' | ':' | ')' | ']' | '}' | '`' | '"' | '\'' | '*' | '>' | '!' | '?'
    )
}

fn cut(raw: &str) -> String {
    let mut end = raw.len().min(MAX_REPORTED_TOKEN_BYTES);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw.get(..end).unwrap_or_default().to_owned()
}

/// One id token at the start of `rest` (after any spaces): its scan and
/// what follows it.
fn token(rest: &str) -> (TagScan, &str) {
    let rest = rest.trim_start_matches([' ', '\t']);
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(rest.len());
    let (word, after) = rest.split_at(end);
    let id = word.trim_end_matches(closes);
    let scan = match TagTarget::parse(id) {
        Some(t) => TagScan::Target(t),
        None => TagScan::Malformed(cut(id)),
    };
    (scan, after)
}

/// Every tag's ids in `line`, in order (module docs).
pub fn scan_line(line: &str) -> Vec<TagScan> {
    let mut found = Vec::new();
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(at) = line.get(from..).and_then(|s| s.find(REQ_TAG)) {
        let start = from + at;
        let end = start + REQ_TAG.len();
        from = end;
        let before_ok = start == 0 || bytes.get(start - 1).is_some_and(|b| !joins_before(*b));
        let after_ok =
            bytes.get(end).is_some_and(|b| *b == b' ' || *b == b'\t') || end == bytes.len();
        if !before_ok || !after_ok {
            continue;
        }
        let mut rest = line.get(end..).unwrap_or_default();
        for n in 0..MAX_IDS_PER_TAG {
            let (scan, after) = token(rest);
            found.push(scan);
            rest = after;
            let next = rest.trim_start_matches([' ', '\t']);
            match next.strip_prefix(',') {
                Some(more) if n + 1 < MAX_IDS_PER_TAG => rest = more,
                _ => break,
            }
        }
        from = line.len() - rest.len();
    }
    found
}

#[cfg(test)]
#[path = "req_tags_tests.rs"]
mod tests;
