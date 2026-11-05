//! High-confidence secret patterns for the push checks (M3 X8, ADR-0008
//! amendment X8): the content of one blob in, the first rule it trips out.
//! Never the matched value — callers report the rule, the path and the
//! commit only, so the secret reaches neither the pusher's terminal nor a
//! log line.
//!
//! The rules are fixed and few on purpose (a false positive refuses a
//! push): a PEM / OpenSSH / PGP private key header, AWS access key ids,
//! GitHub, GitLab, Slack and Google API tokens, and GitStation's own
//! credentials (`fgp_` personal access tokens, `gss_` session values,
//! `whsec_` webhook secrets). Token rules need a boundary before the
//! prefix and an exact (or minimum) length of the right characters after
//! it; a body made of very few distinct characters is a placeholder
//! (`fgp_…_1111…`, the synthetic fixtures), not a secret.
//!
//! A deliberate false positive (a test fixture's synthetic key) is allowed
//! by writing [`ALLOW_MARKER`] anywhere in the same file: reviewed with
//! the change, visible in history, counted in the push's log line.
//!
//! A blob with a NUL byte in its first 8000 bytes is binary (git's own
//! rule) and is not scanned.

/// Anywhere in a file, this marks its findings as known false positives.
pub const ALLOW_MARKER: &[u8] = b"forge:allow-secret";

/// git's binary heuristic: a NUL in the first 8000 bytes.
const BINARY_PROBE: usize = 8000;

/// Distinct characters a token body needs to count as random.
const MIN_DISTINCT: usize = 6;

/// What one blob holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScan {
    /// Nothing found (or binary content, not scanned).
    Clean,
    /// Binary content: not scanned.
    Binary,
    /// The first rule that matched.
    Found(&'static str),
    /// A rule matched, and the file carries [`ALLOW_MARKER`].
    Allowed(&'static str),
}

/// One token shape: literal prefixes, then the body.
struct TokenRule {
    name: &'static str,
    prefixes: &'static [&'static [u8]],
    body: Body,
}

#[derive(Clone, Copy)]
enum Body {
    /// Exactly `len` characters of the class, then a boundary.
    Exact(Class, usize),
    /// At least `min` characters of the class.
    AtLeast(Class, usize),
    /// `<16 lower hex>_<64 lower hex>` (GitStation's credentials).
    ForgeToken,
}

#[derive(Clone, Copy)]
enum Class {
    /// `[A-Z0-9]`
    UpperAlnum,
    /// `[A-Za-z0-9]`
    Alnum,
    /// `[A-Za-z0-9_]`
    AlnumUnderscore,
    /// `[A-Za-z0-9_-]`
    Base64Url,
    /// `[A-Za-z0-9-]`
    AlnumDash,
}

impl Class {
    fn has(self, b: u8) -> bool {
        match self {
            Self::UpperAlnum => b.is_ascii_uppercase() || b.is_ascii_digit(),
            Self::Alnum => b.is_ascii_alphanumeric(),
            Self::AlnumUnderscore => b.is_ascii_alphanumeric() || b == b'_',
            Self::Base64Url => b.is_ascii_alphanumeric() || b == b'_' || b == b'-',
            Self::AlnumDash => b.is_ascii_alphanumeric() || b == b'-',
        }
    }
}

const TOKEN_RULES: &[TokenRule] = &[
    TokenRule {
        name: "aws_access_key_id",
        prefixes: &[b"AKIA", b"ASIA"],
        body: Body::Exact(Class::UpperAlnum, 16),
    },
    TokenRule {
        name: "github_token",
        prefixes: &[b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"],
        body: Body::Exact(Class::Alnum, 36),
    },
    TokenRule {
        name: "github_token",
        prefixes: &[b"github_pat_"],
        body: Body::Exact(Class::AlnumUnderscore, 82),
    },
    TokenRule {
        name: "gitlab_token",
        prefixes: &[b"glpat-"],
        body: Body::AtLeast(Class::Base64Url, 20),
    },
    TokenRule {
        name: "slack_token",
        prefixes: &[b"xoxb-", b"xoxa-", b"xoxp-", b"xoxr-", b"xoxs-"],
        body: Body::AtLeast(Class::AlnumDash, 10),
    },
    TokenRule {
        name: "google_api_key",
        prefixes: &[b"AIza"],
        body: Body::Exact(Class::Base64Url, 35),
    },
    TokenRule {
        name: "gitstation_token",
        prefixes: &[b"fgp_", b"gss_"],
        body: Body::ForgeToken,
    },
    TokenRule {
        name: "gitstation_webhook_secret",
        prefixes: &[b"whsec_"],
        body: Body::Exact(Class::Base64Url, 43),
    },
];

/// Scan one blob's content.
pub fn scan(content: &[u8]) -> SecretScan {
    let probe = content
        .get(..BINARY_PROBE.min(content.len()))
        .unwrap_or(content);
    if probe.contains(&0) {
        return SecretScan::Binary;
    }
    let Some(rule) = first_rule(content) else {
        return SecretScan::Clean;
    };
    if contains(content, ALLOW_MARKER) {
        SecretScan::Allowed(rule)
    } else {
        SecretScan::Found(rule)
    }
}

fn first_rule(content: &[u8]) -> Option<&'static str> {
    for line in content.split(|&b| b == b'\n') {
        if let Some(rule) = private_key(line) {
            return Some(rule);
        }
        if let Some(rule) = token(line) {
            return Some(rule);
        }
    }
    None
}

/// `-----BEGIN [RSA |DSA |EC |OPENSSH |ENCRYPTED |PGP ]PRIVATE KEY[ BLOCK]-----`
fn private_key(line: &[u8]) -> Option<&'static str> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    let mut from = 0;
    while let Some(at) = find(line.get(from..)?, BEGIN) {
        let start = from + at + BEGIN.len();
        let rest = line.get(start..)?;
        // The label: upper-case words and spaces up to the closing dashes.
        let label_len = rest
            .iter()
            .take_while(|b| b.is_ascii_uppercase() || **b == b' ')
            .count();
        let label = rest.get(..label_len)?;
        let closed = rest.get(label_len..)?.starts_with(b"-----");
        if closed && (label.ends_with(b"PRIVATE KEY") || label.ends_with(b"PRIVATE KEY BLOCK")) {
            return Some("private_key");
        }
        from = start;
    }
    None
}

fn token(line: &[u8]) -> Option<&'static str> {
    for (i, &b) in line.iter().enumerate() {
        if !matches!(b, b'A' | b'g' | b'x' | b'f' | b'w') {
            continue;
        }
        // A token starts at a boundary (not inside a longer word).
        if i > 0 && line.get(i - 1).is_some_and(|p| p.is_ascii_alphanumeric()) {
            continue;
        }
        let rest = line.get(i..).unwrap_or_default();
        for rule in TOKEN_RULES {
            for prefix in rule.prefixes {
                if let Some(body) = rest.strip_prefix(*prefix) {
                    if body_matches(rule.body, body) {
                        return Some(rule.name);
                    }
                }
            }
        }
    }
    None
}

fn body_matches(body: Body, rest: &[u8]) -> bool {
    match body {
        Body::Exact(class, len) => {
            let run = rest.iter().take_while(|b| class.has(**b)).count();
            run == len && random(rest.get(..len).unwrap_or_default())
        }
        Body::AtLeast(class, min) => {
            let run = rest.iter().take_while(|b| class.has(**b)).count();
            run >= min && random(rest.get(..run).unwrap_or_default())
        }
        Body::ForgeToken => {
            let hex = |s: &[u8]| {
                s.iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            };
            let (Some(id), Some(sep), Some(secret)) =
                (rest.get(..16), rest.get(16), rest.get(17..81))
            else {
                return false;
            };
            let ends = rest.get(81).is_none_or(|b| !b.is_ascii_alphanumeric());
            hex(id) && *sep == b'_' && hex(secret) && ends && random(secret)
        }
    }
}

/// Enough distinct characters to be random (a placeholder such as
/// `1111…` or `EXAMPLE` padding is not).
fn random(body: &[u8]) -> bool {
    let mut seen = [false; 256];
    let mut distinct = 0;
    for &b in body {
        if let Some(slot) = seen.get_mut(usize::from(b)) {
            if !*slot {
                *slot = true;
                distinct += 1;
            }
        }
    }
    distinct >= MIN_DISTINCT && !contains(body, b"EXAMPLE")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

#[cfg(test)]
#[path = "secret_scan_tests.rs"]
mod tests;
