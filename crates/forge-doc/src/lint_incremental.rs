//! The incremental lint (plan 18 S7, ADR-0037 (b) 3): [`LintCache`]
//! answers exactly what [`super::lint`] answers for the same files, while
//! parsing only the documents it has not seen and re-checking only the
//! documents whose inputs changed.
//!
//! **Content-addressed.** A document is known by a caller's key for its
//! bytes (git's blob id): the parsed document is kept by that key, so a
//! file renamed, copied or unchanged between two trees is never parsed
//! again — and its bytes need not be read ([`LintCache::missing`]). Two
//! different contents never share a key (the key is the content's hash),
//! so a kept parse can never be a stale one.
//!
//! **Reverse references.** A document's own check
//! (`check_document`) depends on its path, its bytes, the type
//! definitions and the index entries it looks up — nothing else. While a
//! document is checked the index records every id it looks up (found or
//! not), and the result is kept with those ids and the entries they had.
//! On the next tree the result is reused only when the path, the key, the
//! type definitions and every one of those entries are the same; a
//! document that adds, removes, renames or changes an id therefore
//! re-checks exactly the documents that looked that id up. The index
//! itself and the duplicate ids are rebuilt from the kept parses on every
//! call (cheap: no parsing), so their answers are always the tree's.
//!
//! **Equal to the whole lint.** The kept check is the sequence of
//! problems the check added, replayed through the same report (the same
//! de-duplication, order and per-file bound), so the reports are the very
//! ones of [`super::lint`]. The property test
//! `incremental_lint_equals_full_lint` holds it over random edits.
//!
//! **Bounded, approximately.** The cache charges an *estimate* of every
//! kept value — a parsed document by its bytes and its parsed items
//! ([`parsed_cost`], calibrated by `the_estimate_covers_a_dense_front_matter`
//! against the size of the parsed value), a kept check by its lookups and
//! problems, the type definitions by their bytes — and, after each call,
//! drops the least recently used ones until the estimate is under its bound
//! (`merge_queue.doc_index_bytes`). The bound is therefore approximate, not
//! an exact count of the heap. What a call uses is the most recent, so a
//! tree larger than the bound still lints (it is parsed again next time);
//! the type definitions of the last call are always kept. Pure: no I/O, no
//! clock — "recent" is a call counter.

use std::collections::HashMap;

use super::{
    build_index, check_document, finish_reports, lint_doctypes, names_type, Entry, FileReport,
    Report, Source,
};
use crate::document::{parse_document, Document, Problem};

/// One `.md` file of the tree: its path, its content's key (the blob id)
/// and its bytes when the cache does not hold the key ([`LintCache::missing`]).
#[derive(Debug, Clone, Copy)]
pub struct KeyedSource<'a> {
    pub path: &'a str,
    pub key: &'a str,
    /// `None`: the cache holds `key` (asked with [`LintCache::missing`]).
    pub bytes: Option<&'a [u8]>,
}

/// A file whose bytes were not given while the cache does not hold its key
/// (the caller asked [`LintCache::missing`] for another tree, or the cache
/// dropped it between the two calls — it never drops during a call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingBytes {
    pub path: String,
}

/// What [`LintCache::lint`] did, for the caller's log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LintStats {
    /// Documents parsed now (their keys were not held).
    pub parsed: usize,
    /// Documents checked now (no kept check, or one of its inputs changed).
    pub checked: usize,
    /// Documents whose kept check was reused.
    pub reused: usize,
    /// Kept values dropped after the call to get under the bound.
    pub evicted: usize,
}

/// A parsed document, by its content's key.
struct Parsed {
    doc: Document,
    names_type: bool,
    cost: usize,
    used: u64,
}

/// The kept check of one document at one path.
struct Checked {
    /// The type definitions' generation it was checked against.
    types: u64,
    /// Every id it looked up, with the entry it had (`None`: absent).
    lookups: Vec<(String, Option<Entry>)>,
    /// The problems the check added, in order (replayed).
    raw: Vec<Problem>,
    cost: usize,
    used: u64,
}

/// The estimated bytes of a value with no content (keys, bookkeeping).
const OVERHEAD: usize = 256;

/// The estimated bytes of one parsed item (a front matter field, a heading,
/// a block, a reference, a problem): its strings' and vectors' headers and
/// allocations beyond the text itself.
const ITEM: usize = 96;

/// The estimated bytes of a document parsed from `len` bytes (approximate:
/// module docs). The text is counted twice (the parsed strings, their
/// allocations' slack) and every parsed item by [`ITEM`], so a dense front
/// matter — many short fields — is not undercharged.
pub(crate) fn parsed_cost(len: usize, doc: &Document) -> usize {
    let items = doc.meta.as_ref().map_or(0, Vec::len)
        + doc.headings.len()
        + doc.blocks.len()
        + doc.refs.len()
        + doc.problems.len();
    len.saturating_mul(2)
        .saturating_add(items.saturating_mul(ITEM))
        .saturating_add(OVERHEAD)
}

/// The incremental, content-addressed lint (module docs).
pub struct LintCache {
    max_bytes: usize,
    bytes: usize,
    clock: u64,
    parsed: HashMap<String, Parsed>,
    /// By `(path, key)`.
    checked: HashMap<(String, String), Checked>,
    /// The type definitions of the last call, and their generation.
    types: Vec<(String, Vec<u8>)>,
    types_gen: u64,
}

impl std::fmt::Debug for LintCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LintCache")
            .field("max_bytes", &self.max_bytes)
            .field("bytes", &self.bytes)
            .field("parsed", &self.parsed.len())
            .field("checked", &self.checked.len())
            .finish_non_exhaustive()
    }
}

impl LintCache {
    /// A cache that keeps at most about `max_bytes` between calls.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: 0,
            clock: 0,
            parsed: HashMap::new(),
            checked: HashMap::new(),
            types: Vec::new(),
            types_gen: 0,
        }
    }

    /// The estimated bytes kept now.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The documents (by content) kept now.
    pub fn documents(&self) -> usize {
        self.parsed.len()
    }

    /// Whether the content `key` is held (its bytes need not be read).
    pub fn holds(&self, key: &str) -> bool {
        self.parsed.contains_key(key)
    }

    /// The keys of `keys` whose bytes the next [`LintCache::lint`] needs.
    pub fn missing<'k>(&self, keys: impl IntoIterator<Item = &'k str>) -> Vec<&'k str> {
        keys.into_iter().filter(|k| !self.holds(k)).collect()
    }

    /// Forget everything (a caller that cannot trust the cache any more).
    pub fn clear(&mut self) {
        self.parsed.clear();
        self.checked.clear();
        self.types.clear();
        self.bytes = 0;
    }

    /// The lint of `doctypes` and `documents` — the very answer of
    /// [`super::lint`] on the same files (module docs). A document without
    /// bytes whose key is not held is [`MissingBytes`] (nothing is kept
    /// from such a call's documents beyond what it parsed).
    pub fn lint(
        &mut self,
        doctypes: &[Source<'_>],
        documents: &[KeyedSource<'_>],
    ) -> Result<(Vec<FileReport>, LintStats), MissingBytes> {
        self.clock = self.clock.saturating_add(1);
        let now = self.clock;
        let mut stats = LintStats::default();
        let (mut reports, types) = lint_doctypes(doctypes);
        self.note_types(doctypes);

        // Parse what is not held; mark what is used.
        for src in documents {
            if let Some(kept) = self.parsed.get_mut(src.key) {
                kept.used = now;
                continue;
            }
            let Some(bytes) = src.bytes else {
                return Err(MissingBytes {
                    path: src.path.to_owned(),
                });
            };
            let doc = parse_document(bytes);
            let names_type = names_type(&doc, bytes);
            let cost = parsed_cost(bytes.len(), &doc);
            self.bytes = self.bytes.saturating_add(cost);
            self.parsed.insert(
                src.key.to_owned(),
                Parsed {
                    doc,
                    names_type,
                    cost,
                    used: now,
                },
            );
            stats.parsed += 1;
        }

        let mut docs: Vec<(&str, &str, &Document)> = Vec::new();
        for src in documents {
            let Some(kept) = self.parsed.get(src.key) else {
                return Err(MissingBytes {
                    path: src.path.to_owned(),
                });
            };
            if kept.names_type {
                docs.push((src.path, src.key, &kept.doc));
            }
        }
        docs.sort_by(|a, b| a.0.cmp(b.0));

        let by_path: Vec<(&str, &Document)> = docs.iter().map(|(p, _, d)| (*p, *d)).collect();
        let (index, duplicates) = build_index(&by_path);
        for (path, line, first) in duplicates {
            reports
                .entry(path.to_owned())
                .or_default()
                .push_on(line, "id_duplicate", first);
        }

        let mut fresh: Vec<((String, String), Checked)> = Vec::new();
        for (path, key, doc) in &docs {
            let report = reports.entry((*path).to_owned()).or_default();
            for p in &doc.problems {
                report.add(p.clone());
            }
            report.dropped += doc.problems_dropped;
            let slot = ((*path).to_owned(), (*key).to_owned());
            let reusable = self.checked.get_mut(&slot).filter(|kept| {
                kept.types == self.types_gen
                    && kept
                        .lookups
                        .iter()
                        .all(|(id, entry)| index.entries.get(id) == entry.as_ref())
            });
            if let Some(kept) = reusable {
                kept.used = now;
                for p in &kept.raw {
                    report.add(p.clone());
                }
                stats.reused += 1;
                continue;
            }
            let mut recording = Report {
                raw: Some(Vec::new()),
                ..Report::default()
            };
            index.record();
            check_document(path, doc, &types, &index, &mut recording);
            let looked_up = index.recorded();
            let raw = recording.raw.take().unwrap_or_default();
            for p in &raw {
                report.add(p.clone());
            }
            stats.checked += 1;
            // A recording that lost a lookup is never kept: it could be
            // reused when it should not.
            if let Some(ids) = looked_up {
                let mut lookups: Vec<(String, Option<Entry>)> = Vec::with_capacity(ids.len());
                for id in ids {
                    if lookups.iter().any(|(seen, _)| *seen == id) {
                        continue;
                    }
                    let entry = index.entries.get(&id).cloned();
                    lookups.push((id, entry));
                }
                let cost = checked_cost(&slot, &lookups, &raw);
                fresh.push((
                    slot,
                    Checked {
                        types: self.types_gen,
                        lookups,
                        raw,
                        cost,
                        used: now,
                    },
                ));
            }
        }
        drop(docs);
        for (slot, checked) in fresh {
            self.bytes = self.bytes.saturating_add(checked.cost);
            if let Some(old) = self.checked.insert(slot, checked) {
                self.bytes = self.bytes.saturating_sub(old.cost);
            }
        }
        stats.evicted = self.evict();
        Ok((finish_reports(reports), stats))
    }

    /// A new generation when the type definitions differ from the last
    /// call's (every kept check is then stale).
    fn note_types(&mut self, doctypes: &[Source<'_>]) {
        let same = self.types.len() == doctypes.len()
            && self
                .types
                .iter()
                .zip(doctypes)
                .all(|((path, bytes), src)| path == src.path && bytes.as_slice() == src.bytes);
        if !same {
            self.bytes = self.bytes.saturating_sub(self.types_cost());
            self.types = doctypes
                .iter()
                .map(|src| (src.path.to_owned(), src.bytes.to_vec()))
                .collect();
            self.bytes = self.bytes.saturating_add(self.types_cost());
            self.types_gen = self.types_gen.saturating_add(1);
        }
    }

    /// The estimated bytes of the kept type definitions.
    fn types_cost(&self) -> usize {
        self.types
            .iter()
            .map(|(path, bytes)| {
                path.len()
                    .saturating_add(bytes.len())
                    .saturating_add(OVERHEAD)
            })
            .fold(0, usize::saturating_add)
    }

    /// Drop the least recently used values until under the bound; how many.
    fn evict(&mut self) -> usize {
        if self.bytes <= self.max_bytes {
            return 0;
        }
        enum Slot {
            Parsed(String),
            Checked((String, String)),
        }
        let mut all: Vec<(u64, usize, Slot)> = self
            .parsed
            .iter()
            .map(|(k, v)| (v.used, v.cost, Slot::Parsed(k.clone())))
            .chain(
                self.checked
                    .iter()
                    .map(|(k, v)| (v.used, v.cost, Slot::Checked(k.clone()))),
            )
            .collect();
        all.sort_by_key(|(used, _, _)| *used);
        let mut evicted = 0;
        for (_, cost, slot) in all {
            if self.bytes <= self.max_bytes {
                break;
            }
            match slot {
                Slot::Parsed(k) => {
                    self.parsed.remove(&k);
                }
                Slot::Checked(k) => {
                    self.checked.remove(&k);
                }
            }
            self.bytes = self.bytes.saturating_sub(cost);
            evicted += 1;
        }
        evicted
    }
}

fn checked_cost(
    (path, key): &(String, String),
    lookups: &[(String, Option<Entry>)],
    raw: &[Problem],
) -> usize {
    let lookups: usize = lookups
        .iter()
        .map(|(id, entry)| {
            id.len()
                .saturating_add(64)
                .saturating_add(entry.as_ref().map_or(0, |e| {
                    e.doctype.len().saturating_add(
                        e.blocks
                            .iter()
                            .map(|(k, v)| k.len().saturating_add(v.len()).saturating_add(48))
                            .sum(),
                    )
                }))
        })
        .sum();
    let raw: usize = raw
        .iter()
        .map(|p| 48usize.saturating_add(p.subject.as_ref().map_or(0, String::len)))
        .sum();
    OVERHEAD
        .saturating_add(path.len())
        .saturating_add(key.len())
        .saturating_add(lookups)
        .saturating_add(raw)
}

#[cfg(test)]
#[path = "lint_incremental_tests.rs"]
mod tests;
