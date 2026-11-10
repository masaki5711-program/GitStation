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
//! it expands nothing — and the tree is built by `forge_doc::yaml` with an
//! explicit stack (bounded depth, no recursion on the input).
//!
//! Who may follow a reference, and what it resolves to, is not decided
//! here: the route decides every referenced repository with the reader's
//! permissions (forge-auth `prove_spec_ref`).

use std::fmt;

use forge_doc::yaml::{mapping, problem, read_tree, text, Kind, Node, YamlProblem};

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
pub use forge_doc::yaml::MAX_MAP_KEYS;

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
/// (`FG-REPO-0024`'s catalog entry lists them). The tree's own refusals come
/// from `forge_doc::yaml` (plan 13 stage 2 MR1: the same type).
pub type RepoConfigProblem = YamlProblem;

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

// ── the format ──────────────────────────────────────────────────────────

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
