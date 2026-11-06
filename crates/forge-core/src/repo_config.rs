//! `.forge/repo.yml` — a repository's own settings, and the specifications it
//! follows (M3 X1, plan 03 §4.4, ADR-0023 decision 2 and Amendment X1).
//!
//! ```yaml
//! specs:
//!   - repo: specs/zeva-engine   # another repository, pinned to a tag
//!     version: v1.4
//!     scope: [ZEVA-CFG-*]       # the requirements this repository covers
//!   - repo: specs/common
//!     track: main               # or a branch, followed as it moves
//!   - path: spec/               # this repository's own specification
//! review: …                     # ADR-0017 §2.7: reserved, not read here
//! ```
//!
//! [`parse_repo_config`] is pure: bytes in, a [`RepoConfig`] or one
//! [`RepoConfigProblem`] (the first rule broken, with its 1-based line; 0
//! for the whole file) out. It never guesses: an unknown key, a second
//! document, a duplicate key and every YAML feature that could make the
//! file mean something else than it reads (anchors, aliases, tags) are
//! refused, not ignored. Scalars are text as written (`1.10` is the tag
//! `1.10`, never the number 1.1). Only yaml-rust2's event parser is used —
//! it expands nothing — and the tree is built here with an explicit stack
//! (bounded depth, no recursion on the input).
//!
//! Who may follow a reference, and what it resolves to, is not decided
//! here: the route decides every referenced repository with the reader's
//! permissions (forge-auth `prove_spec_ref`).

use std::collections::HashSet;
use std::fmt;

use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::{Marker, ScanError, TScalarStyle};

use crate::namespace_path::LookupPath;
use crate::ref_name::RefName;

/// Where the file is read from (the root of a commit's tree).
pub const REPO_CONFIG_PATH: &str = ".forge/repo.yml";
/// A larger file is refused (`too_large`), never read in part.
pub const MAX_REPO_CONFIG_BYTES: usize = 64 * 1024;
/// At most this many `specs:` entries (`too_many_specs`): each repository
/// entry costs a lookup and a ref resolution per request.
pub const MAX_SPEC_REFS: usize = 32;
/// At most this many `scope:` patterns per entry (`too_many_scopes`).
pub const MAX_SCOPE_PATTERNS: usize = 64;
/// The longest scope pattern (`scope_invalid`).
pub const MAX_SCOPE_PATTERN_LEN: usize = 64;
/// The longest `path:` (`path_invalid`): the browse API's
/// `RepoPath::MAX_BYTES`.
pub const MAX_SPEC_PATH_LEN: usize = 4096;
/// At most this many keys in one mapping (`too_many_keys`).
pub const MAX_MAP_KEYS: usize = 64;
/// The deepest nesting of collections (`too_deep`); the format needs 3.
const MAX_DEPTH: usize = 16;

/// A parsed `.forge/repo.yml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoConfig {
    /// In file order.
    pub specs: Vec<SpecEntry>,
}

/// One `specs:` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecEntry {
    /// The entry's line (1-based).
    pub line: usize,
    pub source: SpecSource,
    /// The requirement id patterns this repository covers (empty: none
    /// stated).
    pub scope: Vec<ScopePattern>,
}

/// What an entry refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecSource {
    /// Another repository (`repo:`), pinned.
    Repo {
        /// The path as written (shown back to the reader: it is the file's
        /// own text).
        written: String,
        /// The path folded for lookup (ADR-0015).
        repo: LookupPath,
        pin: SpecPin,
    },
    /// A directory of this repository (`path:`).
    Path(SpecPath),
}

/// A `path:` value: a tree path by the browse API's rules (`/`-separated,
/// no empty, `.` or `..` segment, no control character, at most 4096
/// bytes; one trailing `/` allowed), and also no leading `/`, no
/// backslash, no leading `:` (git pathspec magic) and no drive letter
/// (`C:`). Kept as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecPath(String);

impl SpecPath {
    /// `None` when `raw` breaks a rule above.
    pub fn parse(raw: &str) -> Option<Self> {
        spec_path(raw).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a referenced repository's version is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecPin {
    /// `version: <tag>` — `refs/tags/<tag>`.
    Version(RefName),
    /// `track: <branch>` — `refs/heads/<branch>`, followed as it moves.
    Track(RefName),
}

impl SpecPin {
    /// `version` or `track` (the key it was written with).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Version(_) => "version",
            Self::Track(_) => "track",
        }
    }

    /// The tag or branch name as written.
    pub fn name(&self) -> &str {
        match self {
            Self::Version(r) => r.tag().unwrap_or(r.as_str()),
            Self::Track(r) => r.branch().unwrap_or(r.as_str()),
        }
    }

    /// The full ref name (`refs/tags/…` / `refs/heads/…`).
    pub fn ref_name(&self) -> &RefName {
        match self {
            Self::Version(r) | Self::Track(r) => r,
        }
    }
}

/// A requirement id pattern (`ZEVA-CFG-*`, `ZEVA-CFG-012`, `*`): upper-case
/// ASCII letters, digits and single `-` between them, optionally ending in
/// one `*` (a prefix match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePattern(String);

impl ScopePattern {
    /// `None` when `raw` breaks the rule above.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > MAX_SCOPE_PATTERN_LEN {
            return None;
        }
        let stem = raw.strip_suffix('*').unwrap_or(raw);
        let wildcard = stem.len() != raw.len();
        if stem.is_empty() {
            return wildcard.then(|| Self(raw.to_owned()));
        }
        let charset = stem
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-');
        let edges_ok = !stem.starts_with('-') && (wildcard || !stem.ends_with('-'));
        (charset && edges_ok && !stem.contains("--")).then(|| Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Does requirement id `id` fall under this pattern?
    pub fn matches(&self, id: &str) -> bool {
        match self.0.strip_suffix('*') {
            Some(prefix) => id.starts_with(prefix),
            None => id == self.0,
        }
    }
}

impl fmt::Display for ScopePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a file cannot be used: the first rule broken and its line (1-based;
/// 0 for the whole file). `reason` is one of the rules in the module docs
/// (`FG-REPO-0024`'s catalog entry lists them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoConfigProblem {
    pub line: usize,
    pub reason: &'static str,
}

fn problem(line: usize, reason: &'static str) -> RepoConfigProblem {
    RepoConfigProblem { line, reason }
}

/// Parse `.forge/repo.yml` (module docs).
pub fn parse_repo_config(bytes: &[u8]) -> Result<RepoConfig, RepoConfigProblem> {
    if bytes.len() > MAX_REPO_CONFIG_BYTES {
        return Err(problem(0, "too_large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| problem(0, "not_utf8"))?;
    let Some(root) = read_tree(text)? else {
        return Ok(RepoConfig::default());
    };
    config_of(&root)
}

// ── the tree (scalars as text; no anchors, aliases or tags) ────────────

#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) line: usize,
    pub(crate) kind: Kind,
}

#[derive(Debug)]
pub(crate) enum Kind {
    Text(String),
    /// An empty value, or a plain `~` / `null` (YAML's null): never text.
    Null,
    List(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

/// A collection being filled: its line, its items, and for a mapping the
/// key waiting for its value.
enum Open {
    List(usize, Vec<Node>),
    /// Its line, its entries, the key waiting for its value, and the keys
    /// so far (duplicates are refused in every mapping, read or not).
    Map(usize, Vec<(Node, Node)>, Option<Node>, HashSet<String>),
}

fn syntax(e: &ScanError) -> RepoConfigProblem {
    problem(line_of(e.marker()), "syntax")
}

fn line_of(mark: &Marker) -> usize {
    mark.line().max(1)
}

/// Add a finished node to the innermost open collection (`None`: it is the
/// document's root).
fn attach(stack: &mut [Open], node: Node) -> Result<Option<Node>, RepoConfigProblem> {
    match stack.last_mut() {
        None => Ok(Some(node)),
        Some(Open::List(_, items)) => {
            items.push(node);
            Ok(None)
        }
        Some(Open::Map(_, entries, pending, keys)) => {
            match pending.take() {
                Some(key) => entries.push((key, node)),
                None => {
                    if !matches!(node.kind, Kind::Text(_)) {
                        return Err(problem(node.line, "key_not_text"));
                    }
                    // Bounded while it is built: the duplicate check and
                    // every later scan stay small.
                    if entries.len() >= MAX_MAP_KEYS {
                        return Err(problem(node.line, "too_many_keys"));
                    }
                    if let Kind::Text(name) = &node.kind {
                        if !keys.insert(name.clone()) {
                            // clone: the set owns its keys
                            return Err(problem(node.line, "duplicate_key"));
                        }
                    }
                    *pending = Some(node);
                }
            }
            Ok(None)
        }
    }
}

/// The document's root node (`None`: an empty stream). Also the spec
/// documents' front matter (`spec_doc`, M3 X4): the same refusals.
pub(crate) fn read_tree(text: &str) -> Result<Option<Node>, RepoConfigProblem> {
    let mut parser = Parser::new_from_str(text);
    let mut stack: Vec<Open> = Vec::new();
    let mut root: Option<Node> = None;
    let mut documents = 0usize;
    loop {
        let (event, mark) = parser.next_token().map_err(|e| syntax(&e))?;
        let line = line_of(&mark);
        let finished = match event {
            Event::StreamStart | Event::Nothing | Event::DocumentEnd => None,
            Event::StreamEnd => break,
            Event::DocumentStart => {
                documents += 1;
                if documents > 1 {
                    return Err(problem(line, "multiple_documents"));
                }
                None
            }
            Event::Alias(_) => return Err(problem(line, "yaml_alias")),
            Event::Scalar(value, style, anchor, tag) => {
                refuse_extras(line, anchor, tag.is_some())?;
                let null = style == TScalarStyle::Plain
                    && matches!(value.as_str(), "" | "~" | "null" | "Null" | "NULL");
                Some(Node {
                    line,
                    kind: if null { Kind::Null } else { Kind::Text(value) },
                })
            }
            Event::SequenceStart(anchor, tag) => {
                refuse_extras(line, anchor, tag.is_some())?;
                open(&mut stack, Open::List(line, Vec::new()))?;
                None
            }
            Event::MappingStart(anchor, tag) => {
                refuse_extras(line, anchor, tag.is_some())?;
                open(
                    &mut stack,
                    Open::Map(line, Vec::new(), None, HashSet::new()),
                )?;
                None
            }
            Event::SequenceEnd | Event::MappingEnd => match stack.pop() {
                Some(Open::List(at, items)) => Some(Node {
                    line: at,
                    kind: Kind::List(items),
                }),
                Some(Open::Map(at, entries, None, _)) => Some(Node {
                    line: at,
                    kind: Kind::Map(entries),
                }),
                // A key without a value, or an end with nothing open: the
                // parser never emits these for a well-formed stream.
                Some(Open::Map(_, _, Some(_), _)) | None => return Err(problem(line, "syntax")),
            },
        };
        if let Some(node) = finished {
            if let Some(done) = attach(&mut stack, node)? {
                root = Some(done);
            }
        }
    }
    Ok(root)
}

fn refuse_extras(line: usize, anchor: usize, tagged: bool) -> Result<(), RepoConfigProblem> {
    if anchor != 0 {
        return Err(problem(line, "yaml_anchor"));
    }
    if tagged {
        return Err(problem(line, "yaml_tag"));
    }
    Ok(())
}

fn open(stack: &mut Vec<Open>, collection: Open) -> Result<(), RepoConfigProblem> {
    if stack.len() >= MAX_DEPTH {
        let line = match &collection {
            Open::List(l, _) | Open::Map(l, _, _, _) => *l,
        };
        return Err(problem(line, "too_deep"));
    }
    stack.push(collection);
    Ok(())
}

// ── the format ──────────────────────────────────────────────────────────

/// The entries of a mapping with unique text keys, or the problem.
pub(crate) fn mapping<'a>(
    node: &'a Node,
    not_a_mapping: &'static str,
) -> Result<Vec<Field<'a>>, RepoConfigProblem> {
    let Kind::Map(entries) = &node.kind else {
        return Err(problem(node.line, not_a_mapping));
    };
    let mut out: Vec<Field<'a>> = Vec::with_capacity(entries.len());
    let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
    for (key, value) in entries {
        let Kind::Text(name) = &key.kind else {
            return Err(problem(key.line, "key_not_text"));
        };
        if !seen.insert(name.as_str()) {
            return Err(problem(key.line, "duplicate_key"));
        }
        out.push(Field {
            key: name.as_str(),
            line: key.line,
            value,
        });
    }
    Ok(out)
}

/// One key of a mapping: its text, its line and its value.
pub(crate) struct Field<'a> {
    pub(crate) key: &'a str,
    pub(crate) line: usize,
    pub(crate) value: &'a Node,
}

/// A value that must be text: `empty` names the field's own rule for an
/// empty or null value (reported at `at`, the key's line).
pub(crate) fn text<'a>(
    node: &'a Node,
    at: usize,
    empty: &'static str,
) -> Result<&'a str, RepoConfigProblem> {
    match &node.kind {
        Kind::Text(t) => Ok(t),
        Kind::Null => Err(problem(at, empty)),
        Kind::List(_) | Kind::Map(_) => Err(problem(node.line, "not_text")),
    }
}

fn config_of(root: &Node) -> Result<RepoConfig, RepoConfigProblem> {
    let top = mapping(root, "not_a_mapping")?;
    let mut config = RepoConfig::default();
    for field in top {
        match field.key {
            "specs" => config.specs = specs_of(field.value)?,
            // ADR-0017 §2.7 (`review.ai_approval.*`): reserved for the MR
            // that reads it; accepted unread so the file can carry it.
            "review" => {}
            _ => return Err(problem(field.line, "unknown_key")),
        }
    }
    Ok(config)
}

fn specs_of(node: &Node) -> Result<Vec<SpecEntry>, RepoConfigProblem> {
    let Kind::List(items) = &node.kind else {
        return Err(problem(node.line, "specs_not_a_list"));
    };
    if items.len() > MAX_SPEC_REFS {
        return Err(problem(node.line, "too_many_specs"));
    }
    let mut out: Vec<SpecEntry> = Vec::with_capacity(items.len());
    for item in items {
        let entry = entry_of(item)?;
        let duplicate = out.iter().any(|seen| match (&seen.source, &entry.source) {
            (SpecSource::Repo { repo: a, .. }, SpecSource::Repo { repo: b, .. }) => a == b,
            (SpecSource::Path(a), SpecSource::Path(b)) => {
                a.as_str().trim_end_matches('/') == b.as_str().trim_end_matches('/')
            }
            (SpecSource::Repo { .. }, SpecSource::Path(_))
            | (SpecSource::Path(_), SpecSource::Repo { .. }) => false,
        });
        if duplicate {
            let reason = match entry.source {
                SpecSource::Repo { .. } => "duplicate_repo",
                SpecSource::Path(_) => "duplicate_path",
            };
            return Err(problem(entry.line, reason));
        }
        out.push(entry);
    }
    Ok(out)
}

fn entry_of(node: &Node) -> Result<SpecEntry, RepoConfigProblem> {
    let fields = mapping(node, "entry_not_a_mapping")?;
    let mut repo = None;
    let mut path = None;
    let mut version = None;
    let mut track = None;
    let mut scope = Vec::new();
    for field in &fields {
        // A value's problem is reported on its key's line (an empty
        // value's node starts on the next line).
        let at = field.line;
        match field.key {
            "repo" => repo = Some((text(field.value, at, "repo_invalid")?, at)),
            "path" => path = Some((text(field.value, at, "path_invalid")?, at)),
            "version" => version = Some((text(field.value, at, "version_invalid")?, at)),
            "track" => track = Some((text(field.value, at, "track_invalid")?, at)),
            "scope" => scope = scope_of(field.value)?,
            _ => return Err(problem(at, "unknown_key")),
        }
    }
    let line = node.line;
    let source = match (repo, path) {
        (Some(_), Some(_)) => return Err(problem(line, "repo_and_path")),
        (None, None) => return Err(problem(line, "neither_repo_nor_path")),
        (None, Some((p, at))) => {
            if version.is_some() || track.is_some() {
                return Err(problem(line, "version_on_path"));
            }
            SpecSource::Path(SpecPath::parse(p).ok_or(problem(at, "path_invalid"))?)
        }
        (Some((r, at)), None) => {
            let pin = match (version, track) {
                (Some(_), Some(_)) => return Err(problem(line, "version_and_track")),
                (None, None) => return Err(problem(line, "no_version")),
                (Some((v, vat)), None) => SpecPin::Version(
                    pinned("refs/tags/", v).ok_or(problem(vat, "version_invalid"))?,
                ),
                (None, Some((t, tat))) => {
                    SpecPin::Track(pinned("refs/heads/", t).ok_or(problem(tat, "track_invalid"))?)
                }
            };
            let segments: Vec<&str> = r.split('/').collect();
            let lookup = LookupPath::fold(&segments).map_err(|_| problem(at, "repo_invalid"))?;
            SpecSource::Repo {
                written: r.to_owned(),
                repo: lookup,
                pin,
            }
        }
    };
    Ok(SpecEntry {
        line,
        source,
        scope,
    })
}

/// `refs/tags/<name>` / `refs/heads/<name>` when `name` is a short name git
/// accepts (never a full `refs/…` name; no component may start with `-`).
fn pinned(prefix: &str, name: &str) -> Option<RefName> {
    if name.is_empty() || name.starts_with("refs/") {
        return None;
    }
    RefName::parse(&format!("{prefix}{name}")).ok()
}

/// [`SpecPath`]'s rules.
fn spec_path(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_SPEC_PATH_LEN || raw.starts_with('/') {
        return None;
    }
    // Git pathspec magic (`:(glob)…`), a drive letter (`C:` / `c:x`) and
    // Windows separators are never a tree path here.
    let bytes = raw.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if raw.starts_with(':') || drive || raw.contains('\\') {
        return None;
    }
    let body = raw.strip_suffix('/').unwrap_or(raw);
    // The browse API's rules (forge_git::browse::RepoPath).
    let ok = body.split('/').all(|seg| {
        !seg.is_empty() && seg != "." && seg != ".." && !seg.chars().any(char::is_control)
    });
    ok.then(|| raw.to_owned())
}

fn scope_of(node: &Node) -> Result<Vec<ScopePattern>, RepoConfigProblem> {
    let Kind::List(items) = &node.kind else {
        return Err(problem(node.line, "scope_not_a_list"));
    };
    if items.len() > MAX_SCOPE_PATTERNS {
        return Err(problem(node.line, "too_many_scopes"));
    }
    items
        .iter()
        .map(|item| {
            let raw = text(item, item.line, "scope_invalid")?;
            ScopePattern::parse(raw).ok_or(problem(item.line, "scope_invalid"))
        })
        .collect()
}

#[cfg(test)]
#[path = "repo_config_tests.rs"]
mod tests;
