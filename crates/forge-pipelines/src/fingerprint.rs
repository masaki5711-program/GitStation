//! Masking and failure fingerprints of CI logs (M4 C5; plan 04 P-6, plan
//! 14 §5 C5). Pure: no I/O.
//!
//! **Masking** ([`mask`]) replaces what must never be shown from a job's
//! output, whoever reads it, with [`MASKED`]: forge's own credentials (a
//! personal, runner or job token `fg?_<16 hex>_<secret>`, a web session
//! `gss_…` — a job prints its job token by `env` alone), an HTTP
//! `Authorization` value (any case, quoted or not, `:` or `=`), the
//! password of a URL's userinfo (`/` included), and a PEM private key
//! block. It runs on every page of a log and on every failure before it is
//! fingerprinted or stored, with the log around them as context (up to
//! `MASK_BEFORE_BYTES` before: a secret cut by the edge, or a key whose
//! `BEGIN` line is before the page, is still found). Each masked line stays
//! one line (`[MASKED]` per line): line numbers never move. The base64
//! lines just before an `END … PRIVATE KEY` line are a key's body even when
//! its `BEGIN` line is beyond the context. The values of CI variables are
//! masked by C6 (none exist before). A key body with neither its `BEGIN`
//! line in the context nor its `END` line in page + context is not
//! recognised: keys belong in CI variables (C6), never in a job's output.
//!
//! **Fingerprints** ([`digest`]): the failure lines at the end of a failed
//! job's log — lines naming an error, a failure, a panic, a `FG-` code or a
//! non-zero exit, the last [`KEY_LINES`] of them; or the last non-empty
//! lines when none does; or the job's own error code when there is no log —
//! with what varies between runs replaced (dates, times, durations,
//! UUIDs, hex ids, temporary paths, numbers; the `FG-` codes are kept), and
//! hashed: `fp_` + the first 16 hex digits of their SHA-256. The same
//! failure on another commit, another day or another runner folds into the
//! same fingerprint (`failure_fingerprints`); the job's name is not part of
//! it (one failure in two jobs is one failure).

use std::sync::LazyLock;

use forge_db::ci_outputs::{FailureDigest, FailureLog, MAX_ERROR_CODES, MAX_EXCERPT_BYTES};
use regex::Regex;
use sha2::{Digest, Sha256};

/// What a masked secret is shown as.
pub const MASKED: &str = "[MASKED]";
/// The failure lines a fingerprint and an excerpt are made of, at most.
pub const KEY_LINES: usize = 20;
/// The lines kept when no line names a failure.
const FALLBACK_LINES: usize = 10;

#[allow(clippy::expect_used)] // constant patterns, proved by the tests
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid constant pattern")
}

/// What [`mask`] replaces.
static SECRETS: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        // forge's tokens and web sessions (ADR-0019, ADR-0021, ADR-0029)
        r"\bfg[a-z]_[0-9a-f]{16}_[0-9A-Za-z]{16,}",
        r"|\bgss_[0-9A-Za-z_-]{16,}",
        // an HTTP Authorization value: any case, the name and the value
        // optionally quoted (a JSON or YAML dump), on its line
        r"|(?i-u:[\x22']?authorization[\x22']?[ \t]*[:=][ \t]*[\x22']?",
        r"(?:(?:bearer|basic|token|digest)[ \t]+)?)[^\s'\x22]+",
        // a URL's userinfo password (it may hold `/`)
        r"|[A-Za-z][A-Za-z0-9+.-]*://[^/\s:@]+:[^\s@?#]+@",
        // a PEM private key block (to its END, or to the end of the text)
        r"|-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        // a key's body whose BEGIN line is beyond the context: the base64
        // (and encryption header) lines just before its END line
        r"|(?m:(?:^(?:[A-Za-z0-9+/=]+|Proc-Type:[^\n]*|DEK-Info:[^\n]*)\r?\n)+)",
        r"-----END [A-Z0-9 ]*PRIVATE KEY-----",
    ))
});

/// `text` with every secret replaced by [`MASKED`]. `before` / `after` are
/// the log around `text` (context only, never returned): a secret that
/// starts in `before` or ends in `after` is masked where it overlaps
/// `text`. A secret over several lines becomes one [`MASKED`] per line:
/// `text` keeps its line count.
pub fn mask(before: &str, text: &str, after: &str) -> String {
    let joined = format!("{before}{text}{after}");
    let (start, end) = (before.len(), before.len() + text.len());
    let mut out = String::with_capacity(text.len());
    let mut at = start;
    for m in SECRETS.find_iter(&joined) {
        let (s, e) = (m.start().max(start), m.end().min(end));
        if s >= e || s < at {
            continue;
        }
        out.push_str(&joined[at..s]);
        let mut lines = joined[s..e].split('\n').peekable();
        while let Some(line) = lines.next() {
            if !line.is_empty() {
                out.push_str(MASKED);
            }
            if lines.peek().is_some() {
                out.push('\n');
            }
        }
        at = e;
    }
    if at < end {
        out.push_str(&joined[at..end]);
    }
    out
}

/// A line that reports a failure.
static FAILURE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"\b(?:error|ERROR|Error|FAILED|FAIL|failed|Failed|fatal|FATAL|Fatal|panicked at",
        r"|Traceback|Exception|exception)\b",
        r"|FG-[A-Z]+-[0-9]{4}",
        r"|exit (?:code|status):? [1-9]",
    ))
});

/// A `FG-` error code.
static ERROR_CODE: LazyLock<Regex> = LazyLock::new(|| re(r"\bFG-[A-Z]+-[0-9]{4}\b"));

/// Terminal colour and cursor sequences.
static ANSI: LazyLock<Regex> = LazyLock::new(|| re(r"\x1b\[[0-9;?]*[A-Za-z]"));

/// What varies between runs, in the order it is replaced.
static VARIABLE: LazyLock<[(Regex, &'static str); 6]> = LazyLock::new(|| {
    [
        (
            re(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?"),
            "<time>",
        ),
        (re(r"\b\d{1,2}:\d{2}:\d{2}(?:[.,]\d+)?\b"), "<time>"),
        (
            re(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b"),
            "<uuid>",
        ),
        (
            re(r"(?:/tmp|/var/tmp|/private/var/folders|[A-Za-z]:\\[^\s]*\\Temp)[/\\][^\s:'\x22)]*"),
            "<tmp>",
        ),
        (re(r"\b[0-9a-f]{7,}\b"), "<hex>"),
        (
            re(r"\b\d+(?:\.\d+)?\s?(?:ns|us|µs|ms|s|sec|secs|seconds|m|min|h)\b"),
            "<duration>",
        ),
    ]
});

/// Numbers, keeping the `FG-` codes.
static NUMBER: LazyLock<Regex> = LazyLock::new(|| re(r"FG-[A-Z]+-[0-9]{4}|\d+"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));

/// One line as the fingerprint sees it.
pub fn normalize(line: &str) -> String {
    let mut s = ANSI.replace_all(line, "").into_owned();
    for (pattern, with) in VARIABLE.iter() {
        s = pattern.replace_all(&s, *with).into_owned();
    }
    s = NUMBER
        .replace_all(&s, |c: &regex::Captures<'_>| {
            let m = c.get(0).map_or("", |m| m.as_str());
            if m.starts_with("FG-") {
                m.to_owned()
            } else {
                "<n>".to_owned()
            }
        })
        .into_owned();
    SPACES.replace_all(s.trim(), " ").into_owned()
}

fn hash(key: &str) -> String {
    let sum = Sha256::digest(key.as_bytes());
    let hex: String = sum.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("fp_{hex}")
}

/// The first `max` bytes of `text`, on a char boundary.
fn bounded(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    &text[..at]
}

/// The fingerprint, error codes, masked excerpt and its lines of a failed
/// job's log (module docs). Deterministic: the same log, the same digest.
pub fn digest(log: &FailureLog) -> FailureDigest {
    let masked = mask(&log.before, &log.tail, "");
    let masked_lines: Vec<&str> = masked.split('\n').collect();
    // Lines are numbered on the log as stored (`mask` keeps the line
    // count); what is kept of each is its masked text (a line `mask` did
    // not return is shown masked, never as stored).
    let lines: Vec<(usize, String)> = log
        .tail
        .split('\n')
        .enumerate()
        .filter(|(_, raw)| !ANSI.replace_all(raw, "").trim().is_empty())
        .map(|(i, _)| {
            let line = masked_lines.get(i).copied().unwrap_or(MASKED);
            (
                i,
                ANSI.replace_all(line.trim_end_matches('\r'), "")
                    .into_owned(),
            )
        })
        .collect();
    let failing: Vec<&(usize, String)> = lines
        .iter()
        .filter(|(_, l)| FAILURE_LINE.is_match(l))
        .collect();
    let key_lines: Vec<&(usize, String)> = if failing.is_empty() {
        lines.iter().rev().take(FALLBACK_LINES).rev().collect()
    } else {
        failing
            .iter()
            .rev()
            .take(KEY_LINES)
            .rev()
            .copied()
            .collect()
    };
    let mut error_codes: Vec<String> = Vec::new();
    let codes = log
        .job_error_code
        .iter()
        .map(String::as_str)
        .chain(ERROR_CODE.find_iter(&masked).map(|m| m.as_str()));
    for code in codes {
        if error_codes.len() >= MAX_ERROR_CODES {
            break;
        }
        if !error_codes.iter().any(|c| c == code) {
            error_codes.push(code.to_owned());
        }
    }
    if key_lines.is_empty() {
        let code = log.job_error_code.as_deref().unwrap_or("none");
        return FailureDigest {
            fingerprint: hash(&format!("no_log\n{code}")),
            error_codes,
            excerpt: String::new(),
            lines: None,
        };
    }
    let key = key_lines
        .iter()
        .map(|(_, l)| normalize(l))
        .collect::<Vec<_>>()
        .join("\n");
    let excerpt = key_lines
        .iter()
        .map(|(_, l)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let line_of = |index: usize| {
        let n = log
            .first_line
            .saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
        i32::try_from(n).unwrap_or(i32::MAX)
    };
    let first = key_lines.first().map_or(0, |(i, _)| *i);
    let last = key_lines.last().map_or(0, |(i, _)| *i);
    FailureDigest {
        fingerprint: hash(&key),
        error_codes,
        excerpt: bounded(&excerpt, MAX_EXCERPT_BYTES).to_owned(),
        lines: Some((line_of(first), line_of(last))),
    }
}

#[cfg(test)]
#[path = "fingerprint_tests.rs"]
mod tests;
