//! The string-only half of the web dispatcher (ADR-0015 "判定の順序" step 1,
//! M1 phase 8 MR1): [`parse_web_path`] sorts a raw request path into one of
//! the three URL shapes — namespace home, namespace + `/-/` + route word,
//! Smart HTTP — or an instance route (`/-/projects/{id}/…`), without the
//! database. It returns the route word only; the scope (group or project)
//! is chosen after resolution (forge-web).
//!
//! Every refusal is `FG-WEB-0001` (404) with `reason` = `path_rejected` (a
//! grammar rule), `unknown_route` (a word, reserved top-level name or
//! instance route the dispatcher does not serve) or `service_unsupported`
//! (`info/refs` without exactly one known `service`; until phase 9 registers
//! its own 400 code), plus `part` and `rule` naming where and which rule.
//! The input is never in the error: log it through [`log_safe_path`].
//!
//! Shared vectors: `tests/fixtures/url-vectors.json` (repository root).

use std::fmt;

use forge_errors::{ErrorCode, ForgeError};

use crate::namespace_path::{LookupPath, RESERVED_TOP_LEVEL};

/// The longest raw path (bytes) the parser looks at; longer is
/// `path_rejected` (`rule=too_long`) before any splitting or decoding.
pub const MAX_RAW_PATH_LEN: usize = 8192;

/// How much of a raw path [`log_safe_path`] keeps (characters).
pub const LOG_PATH_MAX_CHARS: usize = 256;

/// The separator segment before a route word (`/acme/x/-/blob/…`).
const SEPARATOR: &str = "-";

/// The suffix of the last namespace segment of a Smart HTTP path.
const DOT_GIT: &str = ".git";

/// The words after `/-/` (ADR-0015 "ルートの系統"). Compared exactly: `Blob`
/// is not `blob`. Words of later milestones (`pulls`, `pipelines`, `wiki`,
/// …) are added with the ADR that gives them an Action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RouteWord {
    Tree,
    Blob,
    Raw,
    Blame,
    Commits,
    Commit,
    Compare,
    Branches,
    Tags,
    Archive,
    Settings,
    Members,
}

impl RouteWord {
    /// Every word.
    pub const ALL: &'static [RouteWord] = &[
        Self::Tree,
        Self::Blob,
        Self::Raw,
        Self::Blame,
        Self::Commits,
        Self::Commit,
        Self::Compare,
        Self::Branches,
        Self::Tags,
        Self::Archive,
        Self::Settings,
        Self::Members,
    ];

    /// The URL word (also the log / manifest form).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Raw => "raw",
            Self::Blame => "blame",
            Self::Commits => "commits",
            Self::Commit => "commit",
            Self::Compare => "compare",
            Self::Branches => "branches",
            Self::Tags => "tags",
            Self::Archive => "archive",
            Self::Settings => "settings",
            Self::Members => "members",
        }
    }

    /// The word spelled exactly `raw` (case-sensitive), `None` otherwise.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|w| w.as_str() == raw)
    }
}

/// The four Smart HTTP endpoints (ADR-0015, ADR-0007). The `service` of an
/// advertisement is part of the route, so the git subcommand comes from the
/// matched route and the query is never parsed again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GitRoute {
    /// `GET {path}.git/info/refs?service=git-upload-pack`
    InfoRefsUploadPack,
    /// `GET {path}.git/info/refs?service=git-receive-pack`
    InfoRefsReceivePack,
    /// `POST {path}.git/git-upload-pack`
    UploadPack,
    /// `POST {path}.git/git-receive-pack`
    ReceivePack,
}

impl GitRoute {
    pub const ALL: &'static [GitRoute] = &[
        Self::InfoRefsUploadPack,
        Self::InfoRefsReceivePack,
        Self::UploadPack,
        Self::ReceivePack,
    ];

    /// The log / manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InfoRefsUploadPack => "info_refs_upload_pack",
            Self::InfoRefsReceivePack => "info_refs_receive_pack",
            Self::UploadPack => "upload_pack",
            Self::ReceivePack => "receive_pack",
        }
    }

    /// The git service the route runs (`git-upload-pack` / `git-receive-pack`).
    pub fn service(self) -> &'static str {
        match self {
            Self::InfoRefsUploadPack | Self::UploadPack => "git-upload-pack",
            Self::InfoRefsReceivePack | Self::ReceivePack => "git-receive-pack",
        }
    }

    /// `true` for the `info/refs` advertisements (GET), `false` for the POSTs.
    pub fn is_advertisement(self) -> bool {
        matches!(self, Self::InfoRefsUploadPack | Self::InfoRefsReceivePack)
    }

    /// `true` when the route can change the repository (receive-pack).
    pub fn is_push(self) -> bool {
        matches!(self, Self::InfoRefsReceivePack | Self::ReceivePack)
    }
}

/// Routes under the instance prefix `/-/` (no namespace).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InstanceRoute {
    /// `/-/projects/{id}/…` (ADR-0015 "固定リンク"): `rest` starts with the
    /// project id, a positive decimal `i64` without leading zeros.
    ProjectPermalink,
}

impl InstanceRoute {
    pub const ALL: &'static [InstanceRoute] = &[Self::ProjectPermalink];

    /// The log / manifest name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectPermalink => "project_permalink",
        }
    }

    /// The word after `/-/`.
    fn word(self) -> &'static str {
        match self {
            Self::ProjectPermalink => "projects",
        }
    }
}

/// The segments after a route word, each percent-decoded exactly once and
/// validated, never case-folded (refs and file names are case-sensitive).
/// Built only by [`TailSegments::decode`]. A segment is never empty, `.` or
/// `..`, and never holds `/`, `\` or a control character.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TailSegments(Vec<String>);

/// `FG-WEB-0001` `reason=path_rejected` with where (`part`) and which rule.
fn rejected(part: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RouteNotFound)
        .with_context("reason", "path_rejected")
        .with_context("part", part)
        .with_context("rule", rule)
}

/// `FG-WEB-0001` `reason=unknown_route`.
fn unknown_route(part: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RouteNotFound)
        .with_context("reason", "unknown_route")
        .with_context("part", part)
}

impl TailSegments {
    /// Decode raw (percent-encoded) segments. `FG-WEB-0001`
    /// `reason=path_rejected`, `part=tail`, `rule`: `empty` (`//`, trailing
    /// `/`), `malformed_escape` (`%` not followed by two hex digits),
    /// `invalid_utf8` (after decoding), `encoded_separator` (`%2F`, `%5C`,
    /// either case), `separator` (a raw `\`), `control` (NUL and every other
    /// control character, C1 included), `dot_segment` (`.` or `..`, also
    /// when encoded).
    pub fn decode(raw_segments: &[&str]) -> Result<Self, ForgeError> {
        raw_segments
            .iter()
            .map(|raw| decode_segment(raw))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, String> {
        self.0.iter()
    }
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// One tail segment, decoded once (strict: a malformed escape is refused,
/// not passed through as text).
fn decode_segment(raw: &str) -> Result<String, ForgeError> {
    const PART: &str = "tail";
    if raw.is_empty() {
        return Err(rejected(PART, "empty"));
    }
    let mut bytes = Vec::with_capacity(raw.len());
    let mut iter = raw.bytes();
    while let Some(b) = iter.next() {
        if b != b'%' {
            bytes.push(b);
            continue;
        }
        let (Some(hi), Some(lo)) = (
            iter.next().and_then(hex_value),
            iter.next().and_then(hex_value),
        ) else {
            return Err(rejected(PART, "malformed_escape"));
        };
        bytes.push((hi << 4) | lo);
    }
    let decoded = String::from_utf8(bytes).map_err(|_| rejected(PART, "invalid_utf8"))?;
    if decoded.contains(['/', '\\']) {
        let rule = if raw.contains('\\') {
            "separator"
        } else {
            "encoded_separator"
        };
        return Err(rejected(PART, rule));
    }
    if decoded.chars().any(char::is_control) {
        return Err(rejected(PART, "control"));
    }
    if decoded == "." || decoded == ".." {
        return Err(rejected(PART, "dot_segment"));
    }
    Ok(decoded)
}

/// What a request path is, by its string alone (ADR-0015 "URL の文字列の形").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedPath {
    /// `/acme/platform/forge-web`: no `-` segment, no `.git` segment.
    NamespaceHome(LookupPath),
    /// `/acme/forge-web/-/blob/main/README.md`; the scope is decided after
    /// resolution (`settings` / `members` exist for groups and projects).
    Scoped {
        path: LookupPath,
        word: RouteWord,
        rest: TailSegments,
    },
    /// `/acme/forge-web.git/info/refs?service=…` and the two POSTs; `path`
    /// is the namespace with the `.git` removed once.
    Git { path: LookupPath, route: GitRoute },
    /// `/-/projects/{id}/…`.
    Instance {
        route: InstanceRoute,
        rest: TailSegments,
    },
}

impl ParsedPath {
    /// The shape, for logs: `home`, `scoped`, `git` or `instance`.
    pub fn shape(&self) -> &'static str {
        match self {
            Self::NamespaceHome(_) => "home",
            Self::Scoped { .. } => "scoped",
            Self::Git { .. } => "git",
            Self::Instance { .. } => "instance",
        }
    }

    /// The namespace path, for the three namespace shapes.
    pub fn lookup_path(&self) -> Option<&LookupPath> {
        match self {
            Self::NamespaceHome(path) | Self::Scoped { path, .. } | Self::Git { path, .. } => {
                Some(path)
            }
            Self::Instance { .. } => None,
        }
    }
}

/// Sort a raw request path (`req.uri().path()`, still percent-encoded —
/// never axum's decoded `Path`) and its raw query (without `?`) into a
/// [`ParsedPath`]. Rules, in order (ADR-0015 and its phase 8 amendment):
///
/// 1. The path starts with `/`, is at most [`MAX_RAW_PATH_LEN`] bytes and
///    has no empty segment (`/`, `//`, a trailing `/` are refused, never
///    repaired).
/// 2. `/-/<word>/…` is an instance route (`/-/projects/{id}/…`); any other
///    word, or `/-` alone, is `unknown_route`.
/// 3. Otherwise the segments are scanned left to right up to the first
///    segment that is exactly `-` or ends with `.git` (ASCII case-insensitive):
///    - `-` first: the segments before it are the namespace; the next
///      segment is a [`RouteWord`] (exact) else `unknown_route`; the rest is
///      [`TailSegments::decode`]d;
///    - `.git` first: that segment is the last namespace segment, `.git`
///      removed once; what follows must be exactly `info/refs` (with exactly
///      one `service=git-upload-pack` / `service=git-receive-pack` in the
///      query, else `service_unsupported`), `git-upload-pack` or
///      `git-receive-pack`; anything else — nothing, `/-/…`, dumb HTTP — is
///      `path_rejected` (so `.git` is legal only there);
///    - neither: the whole path is a namespace home.
/// 4. The namespace goes through [`LookupPath::fold`]; a folded top-level
///    segment in [`RESERVED_TOP_LEVEL`] (`api`, `admin`, `favicon.ico`, …)
///    is `unknown_route` — those are the server's own routes.
///
/// The query is looked at only for `info/refs`.
///
/// ```
/// use forge_core::web_path::{parse_web_path, GitRoute, ParsedPath, RouteWord};
///
/// let parsed = parse_web_path("/Acme/forge-web/-/blob/main/README.md", None).unwrap();
/// let ParsedPath::Scoped { path, word, rest } = parsed else { panic!() };
/// assert_eq!(path.full_path(), "acme/forge-web");
/// assert_eq!(word, RouteWord::Blob);
/// assert_eq!(rest.as_slice(), ["main", "README.md"]);
///
/// let git = parse_web_path("/acme/x.git/info/refs", Some("service=git-upload-pack")).unwrap();
/// assert!(matches!(git, ParsedPath::Git { route: GitRoute::InfoRefsUploadPack, .. }));
///
/// assert!(parse_web_path("/acme/x/", None).is_err()); // never normalised
/// ```
pub fn parse_web_path(raw_path: &str, query: Option<&str>) -> Result<ParsedPath, ForgeError> {
    if raw_path.len() > MAX_RAW_PATH_LEN {
        return Err(rejected("path", "too_long"));
    }
    let Some(body) = raw_path.strip_prefix('/') else {
        return Err(rejected("path", "no_leading_slash"));
    };
    let segments: Vec<&str> = body.split('/').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(rejected("path", "empty_segment"));
    }
    if let Some(after) = segments.strip_prefix(&[SEPARATOR]) {
        return parse_instance(after);
    }
    let split = segments
        .iter()
        .position(|s| *s == SEPARATOR || ends_with_dot_git(s));
    let Some(at) = split else {
        return Ok(ParsedPath::NamespaceHome(fold_namespace(&segments)?));
    };
    let (namespace, after) = segments.split_at(at);
    let Some((marker, after)) = after.split_first() else {
        // Unreachable: `at` is an index into `segments`.
        return Err(rejected("path", "empty_segment"));
    };
    if *marker == SEPARATOR {
        let path = fold_namespace(namespace)?;
        let Some((word, rest)) = after.split_first() else {
            return Err(unknown_route("word"));
        };
        let word = RouteWord::parse(word).ok_or_else(|| unknown_route("word"))?;
        let rest = TailSegments::decode(rest)?;
        return Ok(ParsedPath::Scoped { path, word, rest });
    }
    let route = match after {
        ["info", "refs"] => advertisement(query)?,
        ["git-upload-pack"] => GitRoute::UploadPack,
        ["git-receive-pack"] => GitRoute::ReceivePack,
        _ => return Err(rejected("git", "dot_git_placement")),
    };
    let last = marker
        .get(..marker.len() - DOT_GIT.len())
        .ok_or_else(|| rejected("git", "dot_git_placement"))?;
    let mut namespace = namespace.to_vec();
    namespace.push(last);
    let path = fold_namespace(&namespace)?;
    Ok(ParsedPath::Git { path, route })
}

/// `x.git`, `x.GIT`: the suffix is ASCII, so the byte slice is safe.
fn ends_with_dot_git(segment: &str) -> bool {
    segment.len() >= DOT_GIT.len()
        && segment
            .get(segment.len() - DOT_GIT.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(DOT_GIT))
}

/// [`LookupPath::fold`], then the reserved top-level words.
fn fold_namespace(raw: &[&str]) -> Result<LookupPath, ForgeError> {
    let path = LookupPath::fold(raw)?;
    if RESERVED_TOP_LEVEL.contains(&path.top_level()) {
        return Err(unknown_route("reserved_top_level"));
    }
    Ok(path)
}

/// `info/refs`: exactly one `service` key, value compared byte for byte
/// (no decoding, no case folding). Other keys are ignored.
fn advertisement(query: Option<&str>) -> Result<GitRoute, ForgeError> {
    let unsupported = |rule: &'static str| {
        ForgeError::new(ErrorCode::RouteNotFound)
            .with_context("reason", "service_unsupported")
            .with_context("part", "query")
            .with_context("rule", rule)
    };
    let mut services = query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| pair.split('=').next() == Some("service"));
    let Some(service) = services.next() else {
        return Err(unsupported("missing"));
    };
    if services.next().is_some() {
        return Err(unsupported("duplicate"));
    }
    match service.strip_prefix("service=") {
        Some("git-upload-pack") => Ok(GitRoute::InfoRefsUploadPack),
        Some("git-receive-pack") => Ok(GitRoute::InfoRefsReceivePack),
        _ => Err(unsupported("unknown")),
    }
}

/// `/-/…`.
fn parse_instance(after: &[&str]) -> Result<ParsedPath, ForgeError> {
    let Some((word, rest)) = after.split_first() else {
        return Err(unknown_route("instance"));
    };
    let route = InstanceRoute::ALL
        .iter()
        .copied()
        .find(|r| r.word() == *word)
        .ok_or_else(|| unknown_route("instance"))?;
    match route {
        InstanceRoute::ProjectPermalink => {
            let Some(id) = rest.first() else {
                return Err(rejected("instance", "missing_id"));
            };
            if !is_positive_i64(id) {
                return Err(rejected("instance", "invalid_id"));
            }
        }
    }
    let rest = TailSegments::decode(rest)?;
    Ok(ParsedPath::Instance { route, rest })
}

/// `1`..=`9223372036854775807`, ASCII digits, no leading zero.
fn is_positive_i64(raw: &str) -> bool {
    !raw.starts_with('0')
        && raw.bytes().all(|b| b.is_ascii_digit())
        && raw.parse::<i64>().is_ok_and(|n| n > 0)
}

/// A raw request path made safe for one log field: the first
/// [`LOG_PATH_MAX_CHARS`] characters, with `\` doubled and every control
/// character and invisible / bidi formatting character escaped as
/// `\u{…}`; a cut path ends in `…(+N)` with the number of characters left
/// out. Never parses or decodes.
pub fn log_safe_path(raw: &str) -> String {
    LogSafePath(raw).to_string()
}

/// [`log_safe_path`] as a `Display` value (no allocation until formatted).
#[derive(Debug, Clone, Copy)]
pub struct LogSafePath<'a>(pub &'a str);

impl fmt::Display for LogSafePath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut shown = 0usize;
        for c in self.0.chars().take(LOG_PATH_MAX_CHARS) {
            shown += 1;
            if c == '\\' {
                f.write_str("\\\\")?;
            } else if c.is_control() || is_invisible_format(c) {
                write!(f, "\\u{{{:x}}}", u32::from(c))?;
            } else {
                write!(f, "{c}")?;
            }
        }
        let left = self.0.chars().count() - shown;
        if left > 0 {
            write!(f, "…(+{left})")?;
        }
        Ok(())
    }
}

/// Zero-width, bidi-control and other invisible formatting characters that
/// could disguise a log line.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
    )
}
