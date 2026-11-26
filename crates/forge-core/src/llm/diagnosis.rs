//! The incident first diagnosis's facts (plan 19 §3.2, W14-5): pure — the
//! operator command `forge diagnose` (forge-cli) reads the sources, this
//! module selects, bounds and labels them as input parts.
//!
//! - [`select_log_lines`]: the lines of a log stream (forge's JSON lines,
//!   as `journalctl -o cat` prints them) about one `request_id` or one
//!   `error_code`, numbered by their position in the stream, the newest
//!   [`MAX_LOG_LINES`] kept, each cut at [`MAX_LINE_CHARS`].
//! - [`runbook_section`]: the section of a runbook that names the code
//!   (else its head), cut at [`MAX_SECTION_CHARS`].
//! - [`incident_records`]: the incident-memory lines (`records.jsonl`)
//!   that name the code.
//! - [`parts`]: the input parts. The log lines are one part (path
//!   [`LOG_PATH`], `L<n>: …` per line: a finding about the log must cite
//!   one of them, `OutputPolicy::cited_lines_of`). Log lines that look
//!   like personal data (an e-mail address, a name, login or address
//!   field) or are not forge's JSON make that part `confidential` (people
//!   only, plan 19 §11 question 6); so does such a line in an incident
//!   record or the doctor's report; everything else is `internal`. Secrets are redacted afterwards by `prepare`, before
//!   anything is printed or sent.
//! - [`policy`]: three candidates at most, evidence required, no mutating
//!   step (contract 7).

use serde_json::Value;

use super::{InputClass, InputPart, OutputPolicy};

/// The log lines one diagnosis keeps (the newest).
pub const MAX_LOG_LINES: usize = 200;
/// One log line's characters in the input.
pub const MAX_LINE_CHARS: usize = 2_000;
/// One runbook section's characters.
pub const MAX_SECTION_CHARS: usize = 8_000;
/// Incident records per code.
pub const MAX_INCIDENTS: usize = 5;
/// One incident record's characters.
pub const MAX_INCIDENT_CHARS: usize = 2_000;
/// Error codes taken from the log lines of a request (first seen first).
pub const MAX_CODES: usize = 3;
/// Cause candidates one diagnosis keeps (plan 19 §3.2).
pub const MAX_CANDIDATES: usize = 3;
/// The path of the log part (a finding about it cites a line).
pub const LOG_PATH: &str = "log";
/// The path of the recent changes part.
pub const CHANGES_PATH: &str = "changes";
/// The path of the `forge doctor --json` part.
pub const DOCTOR_PATH: &str = "doctor";

/// What is diagnosed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    RequestId(String),
    /// A registered code (`FG-AREA-NNNN`).
    ErrorCode(String),
}

impl Target {
    /// `request_id=<id>` / `error_code=<code>`.
    pub fn describe(&self) -> String {
        match self {
            Self::RequestId(id) => format!("request_id={id}"),
            Self::ErrorCode(code) => format!("error_code={code}"),
        }
    }
}

/// The log lines about the target.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogSelection {
    /// (line number in the stream, 1-based; the line, cut).
    pub lines: Vec<(usize, String)>,
    /// Lines of the stream read.
    pub scanned: usize,
    /// Lines about the target (more than kept: the oldest were left out).
    pub matched: usize,
    /// FG codes the kept lines carry (`error_code`), first seen first, at
    /// most [`MAX_CODES`].
    pub codes: Vec<String>,
    /// Some kept line looks like personal data.
    pub personal: bool,
}

/// Whether `text` is an FG code's shape (`FG-AREA-NNNN`).
pub fn is_code_shape(text: &str) -> bool {
    let mut it = text.split('-');
    matches!(
        (it.next(), it.next(), it.next(), it.next()),
        (Some("FG"), Some(area), Some(num), None)
            if !area.is_empty()
                && area.chars().all(|c| c.is_ascii_uppercase())
                && num.len() == 4
                && num.chars().all(|c| c.is_ascii_digit())
    )
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The `request_id` of a JSON log line: the event's own, or its span's.
fn request_ids(v: &Value) -> Vec<&str> {
    let mut ids: Vec<&str> = str_field(v, "request_id").into_iter().collect();
    if let Some(span) = v.get("span") {
        ids.extend(str_field(span, "request_id"));
    }
    if let Some(Value::Array(spans)) = v.get("spans") {
        ids.extend(spans.iter().filter_map(|s| str_field(s, "request_id")));
    }
    ids
}

/// Whether a line not in JSON names `needle` as a whole token.
fn names_token(line: &str, needle: &str) -> bool {
    line.match_indices(needle).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + needle.len()..].chars().next();
        let edge = |c: Option<char>| {
            c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        };
        edge(before) && edge(after)
    })
}

/// Field names that hold personal data in forge's log events.
const PERSONAL_FIELDS: &[&str] = &[
    "\"email\"",
    "\"display_name\"",
    "\"full_name\"",
    "\"phone\"",
    "\"author_email\"",
    "\"committer_email\"",
    "\"author_name\"",
    "\"username\"",
    "\"user_name\"",
    "\"login\"",
    "\"staff_id\"",
    "\"remote_ip\"",
    "\"client_ip\"",
    "\"ip\"",
    "\"remote_addr\"",
    "\"peer_addr\"",
    "\"x_forwarded_for\"",
];

/// Whether a log line looks like personal data: a name field or an
/// e-mail address (`local@domain.tld`).
pub fn looks_personal(line: &str) -> bool {
    if PERSONAL_FIELDS.iter().any(|f| line.contains(f)) {
        return true;
    }
    line.match_indices('@').any(|(at, _)| {
        let local = line[..at]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-'))
            .count();
        let domain: String = line[at + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
            .collect();
        let domain = domain.trim_end_matches('.');
        local > 0
            && domain.contains('.')
            && domain
                .rsplit('.')
                .next()
                .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
    })
}

fn cut(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().nth(max).is_some() {
        out.push_str(" …[cut]");
    }
    out
}

/// The lines of `stream` about `target` (module docs).
pub fn select_log_lines(stream: &str, target: &Target) -> LogSelection {
    let mut sel = LogSelection::default();
    let mut kept: std::collections::VecDeque<(usize, String)> = std::collections::VecDeque::new();
    for (i, raw) in stream.lines().enumerate() {
        sel.scanned += 1;
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Option<Value> = serde_json::from_str(line.trim()).ok();
        let about = match (&parsed, target) {
            (Some(v @ Value::Object(_)), Target::RequestId(id)) => {
                request_ids(v).iter().any(|r| r == id)
            }
            (Some(v @ Value::Object(_)), Target::ErrorCode(code)) => {
                str_field(v, "error_code") == Some(code.as_str())
            }
            (_, Target::RequestId(needle) | Target::ErrorCode(needle)) => names_token(line, needle),
        };
        if !about {
            continue;
        }
        sel.matched += 1;
        kept.push_back((i + 1, cut(line, MAX_LINE_CHARS)));
        if kept.len() > MAX_LOG_LINES {
            kept.pop_front();
        }
    }
    for (_, line) in &kept {
        // A line that is not forge's JSON (a panic, another program) is
        // not known to be free of personal data: people only.
        let json = serde_json::from_str::<Value>(line.trim()).is_ok_and(|v| v.is_object());
        if !json || looks_personal(line) {
            sel.personal = true;
        }
        let code = serde_json::from_str::<Value>(line.trim())
            .ok()
            .and_then(|v| str_field(&v, "error_code").map(str::to_owned));
        if let Some(code) = code {
            if is_code_shape(&code) && !sel.codes.contains(&code) && sel.codes.len() < MAX_CODES {
                sel.codes.push(code);
            }
        }
    }
    sel.lines = kept.into_iter().collect();
    sel
}

/// The section of `markdown` that names `code`: from the nearest heading
/// above the first mention to the next heading of the same or a higher
/// level. Without a mention, the head of the runbook. (heading, text).
pub fn runbook_section(markdown: &str, code: &str) -> (String, String) {
    let lines: Vec<&str> = markdown.lines().collect();
    let level = |l: &str| {
        let hashes = l.chars().take_while(|c| *c == '#').count();
        (hashes > 0 && l.chars().nth(hashes) == Some(' ')).then_some(hashes)
    };
    let Some(mention) = lines.iter().position(|l| l.contains(code)) else {
        let heading = lines
            .iter()
            .find(|l| level(l).is_some())
            .map_or_else(String::new, |l| l.trim_start_matches('#').trim().to_owned());
        return (heading, cut(markdown, MAX_SECTION_CHARS));
    };
    let start = (0..=mention)
        .rev()
        .find(|&i| lines.get(i).and_then(|l| level(l)).is_some())
        .unwrap_or(0);
    let start_level = lines.get(start).and_then(|l| level(l)).unwrap_or(1);
    let end = (start + 1..lines.len())
        .find(|&i| {
            lines
                .get(i)
                .and_then(|l| level(l))
                .is_some_and(|lv| lv <= start_level)
        })
        .unwrap_or(lines.len());
    let heading = lines
        .get(start)
        .map_or_else(String::new, |l| l.trim_start_matches('#').trim().to_owned());
    let body = lines.get(start..end).unwrap_or(&[]).join("\n");
    (heading, cut(&body, MAX_SECTION_CHARS))
}

/// One incident-memory record that names the code: (id, text). The id is
/// the record's `id` field, else `line-<n>`.
pub fn incident_records(jsonl: &str, code: &str) -> Vec<(String, String)> {
    jsonl
        .lines()
        .enumerate()
        .filter(|(_, l)| names_token(l, code))
        .take(MAX_INCIDENTS)
        .map(|(i, l)| {
            let id = serde_json::from_str::<Value>(l.trim())
                .ok()
                .and_then(|v| str_field(&v, "id").map(str::to_owned))
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= 64
                        && id
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                })
                .unwrap_or_else(|| format!("line-{}", i + 1));
            (id, cut(l.trim(), MAX_INCIDENT_CHARS))
        })
        .collect()
}

/// A registered code's catalog text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogFact {
    pub code: String,
    pub cause: String,
    pub diagnosis: String,
    pub fix: String,
    pub runbook: Option<String>,
}

/// A runbook section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunbookFact {
    pub name: String,
    pub heading: String,
    pub text: String,
}

/// Everything gathered for one diagnosis (forge-cli reads the sources).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Facts {
    pub log: LogSelection,
    pub catalog: Vec<CatalogFact>,
    pub runbooks: Vec<RunbookFact>,
    /// (incident id, record).
    pub incidents: Vec<(String, String)>,
    /// The recent changes (deployments, migrations, configuration), as text.
    pub changes: Option<String>,
    /// `forge doctor --json`.
    pub doctor: Option<String>,
}

/// The input parts of `facts` (module docs). Empty sources give no part.
pub fn parts(facts: &Facts) -> Vec<InputPart> {
    let mut out = Vec::new();
    if !facts.log.lines.is_empty() {
        let text = facts
            .log
            .lines
            .iter()
            .map(|(n, l)| format!("L{n}: {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        let class = if facts.log.personal {
            InputClass::Confidential
        } else {
            InputClass::Internal
        };
        out.push(InputPart::new("log", class, text).with_path(LOG_PATH));
    }
    for c in &facts.catalog {
        let runbook = c.runbook.as_deref().unwrap_or("-");
        out.push(
            InputPart::new(
                "catalog",
                InputClass::Internal,
                format!(
                    "code: {}\ncause: {}\ndiagnosis: {}\nfix: {}\nrunbook: {runbook}",
                    c.code, c.cause, c.diagnosis, c.fix
                ),
            )
            .with_path(&format!("catalog:{}", c.code)),
        );
    }
    for r in &facts.runbooks {
        out.push(
            InputPart::new(
                "runbook",
                InputClass::Internal,
                format!("section: {}\n{}", r.heading, r.text),
            )
            .with_path(&format!("runbook:{}", r.name)),
        );
    }
    for (id, text) in &facts.incidents {
        out.push(
            InputPart::new("incident", class_of_text(text), text.as_str())
                .with_path(&format!("incident:{id}")),
        );
    }
    if let Some(changes) = &facts.changes {
        out.push(
            InputPart::new("changes", InputClass::Internal, changes.as_str())
                .with_path(CHANGES_PATH),
        );
    }
    if let Some(doctor) = &facts.doctor {
        out.push(
            InputPart::new(
                "doctor",
                class_of_text(doctor),
                cut(doctor, MAX_SECTION_CHARS),
            )
            .with_path(DOCTOR_PATH),
        );
    }
    out
}

/// `confidential` when some line of `text` looks personal, else `internal`.
fn class_of_text(text: &str) -> InputClass {
    if text.lines().any(looks_personal) {
        InputClass::Confidential
    } else {
        InputClass::Internal
    }
}

/// The output policy of a diagnosis (module docs).
pub fn policy() -> OutputPolicy {
    OutputPolicy {
        max_findings: MAX_CANDIDATES,
        min_confidence: 0.3,
        max_summary_chars: 600,
        forbid_mutating_steps: true,
        cited_lines_of: Some(LOG_PATH),
        ..OutputPolicy::default()
    }
}

#[cfg(test)]
#[path = "diagnosis_tests.rs"]
mod tests;
