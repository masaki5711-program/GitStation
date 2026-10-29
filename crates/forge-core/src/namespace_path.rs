//! Namespace path segments (ADR-0015): the name of a user, group or project.
//! `PathSegment::parse` is the one parser; forge.namespaces.segment has the
//! same rules as a CHECK (test `the_db_check_rejects_what_the_parser_rejects`).

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
