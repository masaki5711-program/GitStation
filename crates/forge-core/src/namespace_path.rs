//! Namespace path segments (ADR-0015): the name of a user, group or project.
//! `PathSegment::parse` is the one parser; forge.namespaces.segment has the
//! same rules as a CHECK (test `the_db_check_rejects_what_the_parser_rejects`).
//! [`NamespacePath`] is a whole path of segments; [`LookupPath`] is the
//! request-side form (ADR-0015 "要求のとき", step M1 phase 8 MR1), built
//! only by [`LookupPath::fold`].

use std::fmt;

use forge_errors::{ErrorCode, ForgeError};

/// A path is at most this deep (segments) and this long (characters).
pub const MAX_DEPTH: usize = 20;
pub const MAX_FULL_PATH_LEN: usize = 255;

/// The longest segment (characters).
const MAX_SEGMENT_LEN: usize = 64;

/// Names no top-level namespace (user, top-level group) may take: ADR-0015
/// list plus `edge`, `fleet`, `devices` (edge fleet routes, plan 01 §3.10)
/// and the web root files `favicon.ico`, `robots.txt`.
pub const RESERVED_TOP_LEVEL: &[&str] = &[
    "-",
    "api",
    "admin",
    "assets",
    "login",
    "logout",
    "signup",
    "settings",
    "dashboard",
    "explore",
    "new",
    "search",
    "notifications",
    "docs",
    "help",
    "healthz",
    "readyz",
    "metrics",
    "users",
    "groups",
    "projects",
    "oauth",
    "edge",
    "fleet",
    "devices",
    "favicon.ico",
    "robots.txt",
];

/// Suffixes a segment may not end with (they collide with git / feed routes).
const FORBIDDEN_SUFFIXES: &[&str] = &[".git", ".atom", ".wiki"];

/// A validated path segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PathSegment(String);

fn invalid(reason: &'static str, raw: &str) -> ForgeError {
    ForgeError::new(ErrorCode::NamespacePathInvalid)
        .with_context("reason", reason)
        .with_context("len", raw.chars().count())
}

/// `con prn aux nul com1-9 lpt1-9`, compared on the part before the first `.`.
fn is_device_name(raw: &str) -> bool {
    let stem = raw.split('.').next().unwrap_or_default();
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || ["com", "lpt"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|digit| matches!(digit.as_bytes(), [b'1'..=b'9']))
        })
}

impl PathSegment {
    /// ADR-0015: 1 to 64 characters of `^[a-z0-9](?:[a-z0-9._-]{0,62}[a-z0-9])?$`;
    /// not containing `..`; not ending in `.git`, `.atom` or `.wiki`; not a
    /// Windows device name (con prn aux nul com1-9 lpt1-9), also when only the
    /// part before the first `.` is one. Reserved words are fine here (a
    /// nested `acme/api` is legal): see [`PathSegment::parse_top_level`].
    /// `FG-REPO-0007` (`len`, never the name; `reason` is the first rule
    /// broken: `too_long`, `charset`, `empty`, `edge`, `double_dot`,
    /// `reserved_suffix`, `device_name`).
    pub fn parse(raw: &str) -> Result<Self, ForgeError> {
        let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        let inner = |c: char| alnum(c) || matches!(c, '.' | '_' | '-');
        let len = raw.chars().count();
        if len > MAX_SEGMENT_LEN {
            return Err(invalid("too_long", raw));
        }
        if !raw.chars().all(inner) {
            return Err(invalid("charset", raw));
        }
        let (Some(first), Some(last)) = (raw.chars().next(), raw.chars().last()) else {
            return Err(invalid("empty", raw));
        };
        if !alnum(first) || !alnum(last) {
            return Err(invalid("edge", raw));
        }
        if raw.contains("..") {
            return Err(invalid("double_dot", raw));
        }
        if FORBIDDEN_SUFFIXES.iter().any(|s| raw.ends_with(s)) {
            return Err(invalid("reserved_suffix", raw));
        }
        if is_device_name(raw) {
            return Err(invalid("device_name", raw));
        }
        Ok(Self(raw.to_owned()))
    }

    /// `parse`, then the reserved list: `FG-REPO-0008` (`reason=reserved`).
    /// A syntax failure wins over reservation (`FG-REPO-0007`).
    pub fn parse_top_level(raw: &str) -> Result<Self, ForgeError> {
        let segment = Self::parse(raw)?;
        if RESERVED_TOP_LEVEL.contains(&segment.as_str()) {
            return Err(ForgeError::new(ErrorCode::NamespacePathReserved)
                .with_context("reason", "reserved"));
        }
        Ok(segment)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PathSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A whole namespace path (`acme/platform/forge-web`): 1 to [`MAX_DEPTH`]
/// valid segments, at most [`MAX_FULL_PATH_LEN`] characters joined with `/`.
/// Reserved top-level words are not checked here (a request for one never
/// reaches a lookup: [`crate::web_path::parse_web_path`] refuses it).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NamespacePath(Vec<PathSegment>);

impl NamespacePath {
    /// `FG-REPO-0007` with `reason=empty` (no segment), `too_deep` (more than
    /// [`MAX_DEPTH`]) or `path_too_long` (more than [`MAX_FULL_PATH_LEN`]
    /// characters) — the reasons forge-db uses for the same limits — plus
    /// `depth` and `len`.
    pub fn from_segments(segments: Vec<PathSegment>) -> Result<Self, ForgeError> {
        let len = full_len(&segments);
        let limit_error = |reason: &'static str| {
            ForgeError::new(ErrorCode::NamespacePathInvalid)
                .with_context("reason", reason)
                .with_context("depth", segments.len())
                .with_context("len", len)
        };
        if segments.is_empty() {
            return Err(limit_error("empty"));
        }
        if segments.len() > MAX_DEPTH {
            return Err(limit_error("too_deep"));
        }
        if len > MAX_FULL_PATH_LEN {
            return Err(limit_error("path_too_long"));
        }
        Ok(Self(segments))
    }

    /// A stored `full_path` (`a/b/c`): every segment through
    /// [`PathSegment::parse`], then [`NamespacePath::from_segments`]. For
    /// paths read back from the database (the resolver builds a moved
    /// namespace's current path with it); `FG-REPO-0007` on anything else.
    pub fn parse(full_path: &str) -> Result<Self, ForgeError> {
        let segments = full_path
            .split('/')
            .map(PathSegment::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_segments(segments)
    }

    pub fn segments(&self) -> &[PathSegment] {
        &self.0
    }

    /// The number of segments (1 = top level).
    pub fn depth(&self) -> usize {
        self.0.len()
    }

    /// The first (top-level) segment.
    pub fn top_level(&self) -> &str {
        // Never empty (from_segments refuses that).
        self.0.first().map(PathSegment::as_str).unwrap_or_default()
    }

    /// The segments joined with `/` (the `forge.namespaces.full_path` form).
    pub fn full_path(&self) -> String {
        let mut out = String::with_capacity(full_len(&self.0));
        for (i, segment) in self.0.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(segment.as_str());
        }
        out
    }
}

impl fmt::Display for NamespacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full_path())
    }
}

/// Characters of the segments joined with `/` (segments are ASCII).
fn full_len(segments: &[PathSegment]) -> usize {
    segments.iter().map(|s| s.0.len()).sum::<usize>() + segments.len().saturating_sub(1)
}

/// The namespace part of a request path, folded for lookup (ADR-0015 "要求の
/// とき"). Built only by [`LookupPath::fold`]: a lookup never sees a
/// percent-escape, a non-ASCII character or a Unicode case fold.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LookupPath(NamespacePath);

/// `FG-WEB-0001` `reason=path_rejected` for the namespace part. `rule` names
/// the first rule broken; the input itself is never in the error.
fn namespace_rejected(rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RouteNotFound)
        .with_context("reason", "path_rejected")
        .with_context("part", "namespace")
        .with_context("rule", rule)
}

impl LookupPath {
    /// The raw (still percent-encoded) namespace segments of a request path,
    /// checked and folded. Refused — `FG-WEB-0001` `reason=path_rejected`,
    /// `part=namespace`, `rule` the first rule broken — when there is no
    /// segment or more than [`MAX_DEPTH`] (`empty`, `too_deep`), a segment is
    /// empty (`//`, a trailing `/`: never repaired; `empty`), holds a
    /// non-ASCII character (`non_ascii`), an ASCII control character
    /// (`control`) or a `%` (`percent`: a valid name never needs encoding).
    /// Then each segment is lowercased with `to_ascii_lowercase` only
    /// (`to_lowercase` would turn U+212A KELVIN SIGN into `k`) and must pass
    /// [`PathSegment::parse`] (`rule` = its reason: `charset`, `edge`, …),
    /// and the joined path must stay within [`MAX_FULL_PATH_LEN`]
    /// (`path_too_long`).
    pub fn fold(raw_segments: &[&str]) -> Result<Self, ForgeError> {
        if raw_segments.is_empty() {
            return Err(namespace_rejected("empty"));
        }
        if raw_segments.len() > MAX_DEPTH {
            return Err(namespace_rejected("too_deep"));
        }
        let mut segments = Vec::with_capacity(raw_segments.len());
        for raw in raw_segments {
            if raw.is_empty() {
                return Err(namespace_rejected("empty"));
            }
            if !raw.is_ascii() {
                return Err(namespace_rejected("non_ascii"));
            }
            if raw.bytes().any(|b| b.is_ascii_control()) {
                return Err(namespace_rejected("control"));
            }
            if raw.contains('%') {
                return Err(namespace_rejected("percent"));
            }
            let folded = raw.to_ascii_lowercase();
            let segment = PathSegment::parse(&folded)
                .map_err(|e| namespace_rejected(known_reason(&e).unwrap_or("segment")))?;
            segments.push(segment);
        }
        NamespacePath::from_segments(segments)
            .map(Self)
            .map_err(|e| namespace_rejected(known_reason(&e).unwrap_or("path")))
    }

    pub fn as_namespace_path(&self) -> &NamespacePath {
        &self.0
    }

    pub fn segments(&self) -> &[PathSegment] {
        self.0.segments()
    }

    pub fn depth(&self) -> usize {
        self.0.depth()
    }

    /// The first (top-level) segment.
    pub fn top_level(&self) -> &str {
        self.0.top_level()
    }

    /// The `full_path` to look up (exact match, `COLLATE "C"`).
    pub fn full_path(&self) -> String {
        self.0.full_path()
    }
}

impl fmt::Display for LookupPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The `reason` of an FG-REPO-0007 from this module, as the `'static` word
/// it is, so it can be copied into an FG-WEB-0001 without carrying input.
fn known_reason(err: &ForgeError) -> Option<&'static str> {
    const REASONS: &[&str] = &[
        "empty",
        "too_long",
        "charset",
        "edge",
        "double_dot",
        "reserved_suffix",
        "device_name",
        "too_deep",
        "path_too_long",
    ];
    let value = err
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")?
        .1
        .as_str();
    REASONS.iter().copied().find(|r| *r == value)
}
