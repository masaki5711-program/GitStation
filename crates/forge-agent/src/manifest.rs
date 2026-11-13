//! `part.toml` and the part directory (plan 16 §2.1, ADR-0032 §2).
//!
//! ```toml
//! slug = "format-on-save"
//! kind = "hook"                 # skill agent rule hook command mcp adapter
//! version = "1.2.3"             # semver
//! requires = ["node-runtime@^1"]
//! capabilities = ["exec"]       # exec network permissions
//! files = [".claude/hooks/format-on-save.js", ".claude/settings.json"]
//! ```
//!
//! The directory holds `part.toml` and exactly the files it lists, each at
//! its install path. Unknown keys are refused (nothing is silently ignored).

use std::collections::{BTreeMap, BTreeSet};

use forge_errors::ErrorCode;
use serde::Deserialize;

use crate::{check_install_path, sha256, Digest, Problem};

/// The manifest's file name at the root of a part directory.
pub const MANIFEST: &str = "part.toml";
/// Most files in one part version, `part.toml` included (plan 16 §3).
pub const MAX_PART_FILES: usize = 200;
/// Most bytes in one part version, `part.toml` included (plan 16 §3).
pub const MAX_PART_BYTES: usize = 1024 * 1024;

const MAX_SLUG: usize = 64;
const MAX_REQUIRES: usize = 32;

/// What a part is (plan 16 §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Skill,
    Agent,
    Rule,
    Hook,
    Command,
    Mcp,
    Adapter,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Skill => "skill",
            Kind::Agent => "agent",
            Kind::Rule => "rule",
            Kind::Hook => "hook",
            Kind::Command => "command",
            Kind::Mcp => "mcp",
            Kind::Adapter => "adapter",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "skill" => Kind::Skill,
            "agent" => Kind::Agent,
            "rule" => Kind::Rule,
            "hook" => Kind::Hook,
            "command" => Kind::Command,
            "mcp" => Kind::Mcp,
            "adapter" => Kind::Adapter,
            _ => return None,
        })
    }
}

/// What a part may do on a developer's PC beyond reading and writing its
/// own text (plan 16 §6, the marketplace's badges). Undeclared use is
/// `FG-AGENT-0005`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Runs other programs (`child_process`, an MCP server's `command`, a
    /// hook command that is not `node`).
    Exec,
    /// Talks to the network (`http`, `net`, `fetch`, an MCP `url`).
    Network,
    /// Widens Claude Code's `permissions` in `settings.json`.
    Permissions,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Exec => "exec",
            Capability::Network => "network",
            Capability::Permissions => "permissions",
        }
    }

    pub fn parse(s: &str) -> Option<Capability> {
        Some(match s {
            "exec" => Capability::Exec,
            "network" => Capability::Network,
            "permissions" => Capability::Permissions,
            _ => return None,
        })
    }
}

/// A checked `part.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub slug: String,
    pub kind: Kind,
    pub version: String,
    /// `slug@range` strings, sorted and unique (resolved by the builder).
    pub requires: Vec<String>,
    pub capabilities: BTreeSet<Capability>,
    /// The install paths, sorted and unique.
    pub files: Vec<String>,
}

/// What an entry of the part directory is; only regular files are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Symlink,
    Other,
}

/// One entry of a part directory as the caller read it: the path relative
/// to the part's root, `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub kind: EntryKind,
    pub content: Vec<u8>,
}

impl TreeEntry {
    pub fn file(path: &str, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.to_owned(),
            kind: EntryKind::File,
            content: content.into(),
        }
    }
}

/// One installed file of a part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartFile {
    pub path: String,
    pub sha256: [u8; 32],
    pub content: Vec<u8>,
}

/// A part that passed the lint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub manifest: Manifest,
    /// Sorted by path.
    pub files: Vec<PartFile>,
    pub digest: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    slug: String,
    kind: String,
    version: String,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    files: Vec<String>,
}

fn invalid(reason: &'static str, subject: Option<String>) -> Problem {
    Problem::new(ErrorCode::PartManifestInvalid, reason, subject)
}

/// Check a part directory and build the [`Part`]; every problem found, or
/// the part with its digest.
pub fn parse_part(entries: &[TreeEntry]) -> Result<Part, Vec<Problem>> {
    let mut problems = Vec::new();
    if entries.len() > MAX_PART_FILES {
        problems.push(invalid("too_many_files", Some(entries.len().to_string())));
        return Err(problems);
    }
    let total: usize = entries.iter().map(|e| e.content.len()).sum();
    if total > MAX_PART_BYTES {
        problems.push(invalid("too_large", Some(total.to_string())));
        return Err(problems);
    }
    let mut by_path: BTreeMap<&str, &TreeEntry> = BTreeMap::new();
    for e in entries {
        if e.kind != EntryKind::File {
            problems.push(invalid("not_a_regular_file", Some(e.path.clone())));
            continue;
        }
        if by_path.insert(e.path.as_str(), e).is_some() {
            problems.push(invalid("duplicate_file", Some(e.path.clone())));
        }
    }
    let Some(manifest_entry) = by_path.remove(MANIFEST) else {
        problems.push(invalid("manifest_missing", None));
        return Err(problems);
    };
    let manifest = match parse_manifest(&manifest_entry.content) {
        Ok(m) => m,
        Err(mut more) => {
            problems.append(&mut more);
            return Err(problems);
        }
    };
    let mut files = Vec::with_capacity(manifest.files.len());
    for path in &manifest.files {
        match by_path.remove(path.as_str()) {
            Some(e) => files.push(PartFile {
                path: path.clone(),
                sha256: sha256(&e.content),
                content: e.content.clone(),
            }),
            None => problems.push(invalid("file_missing", Some(path.clone()))),
        }
    }
    for extra in by_path.keys() {
        problems.push(invalid("file_not_declared", Some((*extra).to_owned())));
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let hashes: Vec<(String, [u8; 32])> =
        files.iter().map(|f| (f.path.clone(), f.sha256)).collect();
    let digest = crate::digest(&manifest, &hashes);
    Ok(Part {
        manifest,
        files,
        digest,
    })
}

/// Parse and lint `part.toml` alone.
pub(crate) fn parse_manifest(bytes: &[u8]) -> Result<Manifest, Vec<Problem>> {
    let text = std::str::from_utf8(bytes).map_err(|_| vec![invalid("manifest_not_utf8", None)])?;
    let raw: RawManifest = toml::from_str(text).map_err(|e| {
        // The parser's message names the key or line, never other content.
        vec![invalid(
            "manifest_syntax",
            Some(e.message().chars().take(200).collect()),
        )]
    })?;
    manifest_from_parts(
        &raw.slug,
        &raw.kind,
        &raw.version,
        &raw.requires,
        &raw.capabilities,
        &raw.files,
    )
}

/// The manifest lint, shared with the lock (whose part entries carry the
/// same fields).
pub(crate) fn manifest_from_parts(
    slug: &str,
    kind: &str,
    version: &str,
    requires: &[String],
    capabilities: &[String],
    files: &[String],
) -> Result<Manifest, Vec<Problem>> {
    let mut problems = Vec::new();
    if !valid_slug(slug) {
        problems.push(invalid(
            "slug_invalid",
            Some(slug.chars().take(80).collect()),
        ));
    }
    let kind_parsed = Kind::parse(kind);
    if kind_parsed.is_none() {
        problems.push(invalid(
            "kind_unknown",
            Some(kind.chars().take(40).collect()),
        ));
    }
    if !valid_semver(version) {
        problems.push(invalid(
            "version_invalid",
            Some(version.chars().take(80).collect()),
        ));
    }
    if requires.len() > MAX_REQUIRES {
        problems.push(invalid("too_many_requires", None));
    }
    for r in requires {
        if !valid_require(r) {
            problems.push(invalid(
                "require_invalid",
                Some(r.chars().take(80).collect()),
            ));
        }
    }
    let mut caps = BTreeSet::new();
    for c in capabilities {
        match Capability::parse(c) {
            Some(cap) => {
                caps.insert(cap);
            }
            None => problems.push(invalid(
                "capability_unknown",
                Some(c.chars().take(40).collect()),
            )),
        }
    }
    if files.is_empty() {
        problems.push(invalid("no_files", None));
    }
    let mut seen = BTreeSet::new();
    for f in files {
        if let Err(reason) = check_install_path(f) {
            problems.push(invalid(reason, Some(f.chars().take(260).collect())));
        } else if !seen.insert(f.clone()) {
            problems.push(invalid("duplicate_file", Some(f.clone())));
        }
        if f == ".mcp.json" && kind_parsed != Some(Kind::Mcp) {
            problems.push(invalid("mcp_file_outside_mcp_part", Some(f.clone())));
        }
        if f.starts_with(".claude/hooks/") && !hook_file_allowed(f) {
            problems.push(invalid("hook_not_node", Some(f.clone())));
        }
    }
    let Some(kind) = kind_parsed else {
        return Err(problems);
    };
    if !problems.is_empty() {
        return Err(problems);
    }
    let mut requires: Vec<String> = requires.to_vec();
    requires.sort();
    requires.dedup();
    Ok(Manifest {
        slug: slug.to_owned(),
        kind,
        version: version.to_owned(),
        requires,
        capabilities: caps,
        files: seen.into_iter().collect(),
    })
}

/// Hooks are Node.js (plan 09 §3.1): scripts under `.claude/hooks/` are
/// `.js` / `.cjs` / `.mjs`; data and docs beside them are `.json` / `.md`.
fn hook_file_allowed(path: &str) -> bool {
    [".js", ".cjs", ".mjs", ".json", ".md"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_SLUG
        && b.first()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && !s.ends_with('-')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Semantic Versioning 2.0.0 without build metadata: `M.m.p` or
/// `M.m.p-pre.release`, no leading zeros in numbers.
pub(crate) fn valid_semver(s: &str) -> bool {
    if s.len() > 64 {
        return false;
    }
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (s, None),
    };
    let nums: Vec<&str> = core.split('.').collect();
    if nums.len() != 3 || !nums.iter().all(|n| numeric_identifier(n)) {
        return false;
    }
    match pre {
        None => true,
        Some(p) => p.split('.').all(|id| {
            !id.is_empty()
                && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                && (!id.bytes().all(|c| c.is_ascii_digit()) || numeric_identifier(id))
        }),
    }
}

fn numeric_identifier(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) && (n == "0" || !n.starts_with('0'))
}

/// `slug` or `slug@range` (range: semver operators and versions; resolved
/// by the builder, A7).
fn valid_require(s: &str) -> bool {
    let (slug, range) = match s.split_once('@') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    valid_slug(slug)
        && range.is_none_or(|r| {
            !r.is_empty()
                && r.len() <= 64
                && r.bytes().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(
                            c,
                            b'.' | b'^' | b'~' | b'<' | b'>' | b'=' | b'*' | b',' | b' ' | b'-'
                        )
                })
        })
}
