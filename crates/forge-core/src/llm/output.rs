//! The model's answer, checked by code (plan 19 §2, §4, §5; ADR-0017 §2).
//!
//! The expected JSON (the prompt asks for it, the check enforces it):
//!
//! ```json
//! {"summary": "…",
//!  "findings": [{"severity": "critical|high|medium|low", "path": "src/a.rs",
//!                "line": 12, "quote": "text copied from the input",
//!                "message": "…", "confidence": 0.8}],
//!  "deep_review": {"recommended": true, "reason": "…"}}
//! ```
//!
//! [`check_output`]:
//! - not that shape → `FG-LLM-0002` (`reason`); the caller retries once
//!   ([`super::generate_checked`]);
//! - a finding below [`OutputPolicy::min_confidence`] is dropped;
//! - a finding whose `quote` is not in the input (in the parts of its
//!   `path` when one has it) is dropped and counted
//!   ([`Advice::dropped_evidence`], `FG-LLM-0006`);
//! - repeats (same path, line and message) collapse into one;
//! - at most [`OutputPolicy::max_findings`] stay, most severe and most
//!   confident first;
//! - a summary that states a verdict (`LGTM`, `safe to merge`) is
//!   replaced and the advice marked degraded (no evidence gate can prove a
//!   summary); findings dropped for evidence also mark it degraded;
//! - every text (quotes too) loses slash commands (`/merge`), mentions
//!   (`@name` → `name`), closing keywords
//!   (`closes #12`), images and links: an advice can notify no one, close
//!   nothing and carry nothing out (ADR-0017 §2);
//! - any other key (`"approve": true`) is ignored: an advice has no action
//!   field, so nothing in it can change state;
//! - with [`OutputPolicy::forbid_mutating_steps`] (the incident diagnosis,
//!   plan 19 §3.2, contract 7) a finding whose message names a step that
//!   changes something (`UPDATE … SET`, `systemctl restart`, 「再起動」), or
//!   whose "next look" (after `次に見るもの:`) is not an allow-listed
//!   read-only command ([`next_look_is_read_only`]), is dropped
//!   ([`Advice::dropped_mutating`]) and such a summary withheld
//!   ([`MUTATING_WITHHELD`]): the diagnosis only says what to look at next;
//! - with [`OutputPolicy::cited_lines_of`] a finding about that path (the
//!   numbered log, `L<n>: …`) must name a given line and quote that line,
//!   and every finding must name its part.

use forge_errors::{ErrorCode, ForgeError};
use serde_json::{Map, Value};

use super::{InputClass, PreparedInput, MAX_FINDINGS};

/// A finding's severity (the baseline's four levels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FindingSeverity {
    Critical,
    High,
    Medium,
    Low,
}

impl FindingSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "critical" => Some(Self::Critical),
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// One finding that passed every check.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub severity: FindingSeverity,
    pub path: Option<String>,
    pub line: Option<u32>,
    /// The evidence, as found in the input.
    pub quote: String,
    pub message: String,
    pub confidence: f64,
}

/// "A deeper review (Claude or a person) is recommended", with the model's
/// reason. Code rules decide it first (plan 19 §3.1); this only adds words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepReview {
    pub reason: String,
}

/// A checked advice: words only, no action.
#[derive(Debug, Clone, PartialEq)]
pub struct Advice {
    pub summary: String,
    pub findings: Vec<Finding>,
    pub deep_review: Option<DeepReview>,
    /// The input's class, carried over (never lowered).
    pub input_class: InputClass,
    /// Findings dropped because their quote is not in the input.
    pub dropped_evidence: usize,
    /// Findings dropped below the confidence floor.
    pub dropped_low_confidence: usize,
    /// Findings dropped over the cap (after the two above).
    pub dropped_over_cap: usize,
    /// Mentions, closing keywords, slash commands, images and links
    /// removed (in the summary, messages, quotes and the deep-review reason).
    pub stripped: usize,
    /// The summary stated a verdict (`LGTM`, `safe to merge`, ...): an
    /// advice never decides, so the summary was replaced by
    /// [`WITHHELD_SUMMARY`].
    pub summary_withheld: bool,
    /// Verdict phrases replaced by [`VERDICT_REMOVED`] in finding
    /// messages, quotes and the deep-review reason.
    pub verdicts_removed: usize,
    /// Findings dropped because their message names a mutating step
    /// ([`OutputPolicy::forbid_mutating_steps`]).
    pub dropped_mutating: usize,
    /// Less trustworthy than its words suggest: a verdict was withheld or
    /// removed, findings were dropped for their evidence, or a mutating
    /// step was refused (shown as such).
    pub degraded: bool,
}

/// What replaces a summary that names a mutating step (a diagnosis).
pub const MUTATING_WITHHELD: &str =
    "[summary withheld: the model proposed a change; mitigation is a person's approved runbook step]";

/// What replaces a summary that stated a verdict.
pub const WITHHELD_SUMMARY: &str =
    "[summary withheld: the model stated a verdict; the AI advises only]";

/// Words of a verdict (lowercase). An advice that says them about the
/// change is not an advice; injected text tries exactly that.
const VERDICT_PHRASES: &[&str] = &[
    "lgtm",
    "looks good to me",
    "safe to merge",
    "ready to merge",
    "ok to merge",
    "okay to merge",
    "i approve",
    "approve this",
    "approve the merge",
    "approval granted",
    "ship it",
    "no issues found",
    "nothing to review",
    "マージしてよい",
    "マージ可能",
    "問題ありません",
    "問題なし",
    "承認してよい",
    "承認して問題",
    "aman untuk digabung",
    "disetujui",
];

/// A word right before a verdict phrase that negates it ("not approved",
/// "tidak disetujui"): no verdict.
const NEGATIONS: &[&str] = &["not", "no", "never", "tidak", "belum", "bukan"];

/// What replaces a verdict phrase inside a message, quote or reason.
pub const VERDICT_REMOVED: &str = "[verdict removed]";

/// Invisible characters that would split a phrase without showing it:
/// zero-width, soft hyphen, word joiners, BOM and bidi controls.
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{115F}' | '\u{1160}' | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}'
        | '\u{FFA0}' | '\u{E0100}'..='\u{E01EF}')
}

/// Whether the phrase that follows `before` is negated: the word right
/// before it (spaces only between) is a negation. A clause boundary
/// (`.,;:!?-`, a newline) in between ends the look-back, so "No, LGTM",
/// "No. Approved" and "Never mind - ship it" are verdicts.
fn negated(before: &str) -> bool {
    let tail = before.trim_end_matches([' ', '\t']);
    if tail.ends_with(|c: char| !c.is_alphanumeric()) {
        return false; // a clause boundary, or nothing at all
    }
    let word = tail
        .rsplit(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("");
    NEGATIONS.contains(&word)
}

/// NFKC (full-width and compatibility forms fold to their plain letters)
/// without invisible characters: the form every advice text is stored and
/// checked in.
fn normalized(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    text.nfkc().filter(|c| !is_invisible(*c)).collect()
}

/// The byte ranges of the verdict phrases in `text` (already
/// [`normalized`]), on word boundaries for the Latin ones, not after a
/// negation.
fn verdict_spans(text: &str) -> Vec<(usize, usize)> {
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        // A case mapping changed lengths: the ranges would not line up, so
        // the whole text counts as one (conservative).
        return if VERDICT_PHRASES.iter().any(|p| lower.contains(p)) {
            vec![(0, text.len())]
        } else {
            Vec::new()
        };
    }
    let mut spans = Vec::new();
    for phrase in VERDICT_PHRASES {
        let latin = phrase.starts_with(|c: char| c.is_ascii_alphabetic());
        let mut from = 0;
        while let Some(at) = lower.get(from..).and_then(|rest| rest.find(phrase)) {
            let start = from + at;
            let end = start + phrase.len();
            from = end;
            let before = lower.get(..start).unwrap_or("");
            let after = lower.get(end..).unwrap_or("");
            if latin
                && (before
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
                    || after.chars().next().is_some_and(char::is_alphanumeric))
            {
                continue;
            }
            if negated(before) {
                continue;
            }
            spans.push((start, end));
        }
    }
    spans.sort_unstable();
    spans
}

fn states_a_verdict(text: &str) -> bool {
    !verdict_spans(text).is_empty()
}

/// `text` with each verdict phrase replaced by [`VERDICT_REMOVED`]; and
/// whether any was.
fn without_verdicts(text: &str) -> (String, bool) {
    let spans = verdict_spans(text);
    if spans.is_empty() {
        return (text.to_owned(), false);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in spans {
        if start < at {
            continue; // overlapping phrases: the first already covered it
        }
        out.push_str(text.get(at..start).unwrap_or(""));
        out.push_str(VERDICT_REMOVED);
        at = end;
    }
    out.push_str(text.get(at..).unwrap_or(""));
    (out, true)
}

impl Advice {
    /// `FG-LLM-0006` (info) to log when findings were dropped for their
    /// evidence; `None` when none was.
    pub fn evidence_note(&self) -> Option<ForgeError> {
        (self.dropped_evidence > 0).then(|| {
            ForgeError::new(ErrorCode::LlmEvidenceInvalid)
                .with_context("dropped", self.dropped_evidence)
                .with_context("kept", self.findings.len())
        })
    }
}

/// The bounds of an answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputPolicy {
    pub max_findings: usize,
    /// Findings below this confidence are not shown (plan 19 §3.1).
    pub min_confidence: f64,
    pub max_summary_chars: usize,
    pub max_message_chars: usize,
    pub max_quote_chars: usize,
    /// Drop findings (and withhold a summary) that name a step changing
    /// something (module docs). Off by default.
    pub forbid_mutating_steps: bool,
    /// A finding about this path must cite one of its numbered lines
    /// (`L<n>: `) by `line` and quote that line only (module docs).
    pub cited_lines_of: Option<&'static str>,
}

impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            max_findings: MAX_FINDINGS,
            min_confidence: 0.5,
            max_summary_chars: 1_200,
            max_message_chars: 1_000,
            max_quote_chars: 500,
            forbid_mutating_steps: false,
            cited_lines_of: None,
        }
    }
}

fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::LlmOutputInvalid).with_context("reason", reason)
}

/// Check `raw` against the schema and `input` (module docs).
pub fn check_output(
    raw: &str,
    input: &PreparedInput,
    policy: &OutputPolicy,
) -> Result<Advice, ForgeError> {
    let value: Value = serde_json::from_str(strip_fence(raw)).map_err(|_| invalid("not_json"))?;
    let Value::Object(top) = value else {
        return Err(invalid("not_object"));
    };
    let mut stripped = 0usize;
    let mut verdicts_removed = 0usize;
    let summary = match top.get("summary") {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            let (s, n) = clean(s, policy.max_summary_chars);
            stripped += n;
            s
        }
        Some(_) => return Err(invalid("summary_type")),
        None => return Err(invalid("summary_missing")),
    };
    // No evidence gate can prove a summary; a verdict in it is refused.
    let summary_withheld = states_a_verdict(&summary);
    let summary_mutating =
        !summary_withheld && policy.forbid_mutating_steps && names_a_mutating_step(&summary);
    let summary = if summary_withheld {
        WITHHELD_SUMMARY.to_owned()
    } else if summary_mutating {
        MUTATING_WITHHELD.to_owned()
    } else {
        summary
    };
    let raw_findings = match top.get("findings") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(invalid("findings_type")),
    };
    if raw_findings.len() > 100 {
        return Err(invalid("findings_too_many"));
    }
    let deep_review = match top.get("deep_review") {
        None | Some(Value::Null) => None,
        Some(Value::Object(d)) => match (d.get("recommended"), d.get("reason")) {
            (Some(Value::Bool(false)) | None, _) => None,
            (Some(Value::Bool(true)), Some(Value::String(r))) if !r.trim().is_empty() => {
                let (reason, n) = clean(r, policy.max_message_chars);
                stripped += n;
                let (reason, removed) = without_verdicts(&reason);
                verdicts_removed += usize::from(removed);
                Some(DeepReview { reason })
            }
            _ => return Err(invalid("deep_review_type")),
        },
        Some(_) => return Err(invalid("deep_review_type")),
    };

    let mut kept: Vec<Finding> = Vec::new();
    let mut dropped_evidence = 0usize;
    let mut dropped_low_confidence = 0usize;
    let mut dropped_mutating = 0usize;
    for item in &raw_findings {
        let Value::Object(f) = item else {
            return Err(invalid("finding_type"));
        };
        let finding = parse_finding(f, policy)?;
        if finding.confidence < policy.min_confidence {
            dropped_low_confidence += 1;
            continue;
        }
        if policy.forbid_mutating_steps
            && (names_a_mutating_step(&finding.message)
                || !next_look_is_read_only(&finding.message))
        {
            dropped_mutating += 1;
            continue;
        }
        if !evidence_in_input(&finding, input, policy) {
            dropped_evidence += 1;
            continue;
        }
        let (message, n) = clean(&finding.message, policy.max_message_chars);
        stripped += n;
        let (message, removed) = without_verdicts(&message);
        verdicts_removed += usize::from(removed);
        if message.trim().is_empty() {
            dropped_evidence += 1;
            continue;
        }
        // The quote passed the evidence check as given; what is stored and
        // shown is cleaned like every other text.
        let (quote, n) = clean(&finding.quote, policy.max_quote_chars);
        stripped += n;
        let (quote, removed) = without_verdicts(&quote);
        verdicts_removed += usize::from(removed);
        let finding = Finding {
            message,
            quote,
            ..finding
        };
        let repeat = kept.iter().any(|k| {
            k.path == finding.path && k.line == finding.line && k.message == finding.message
        });
        if !repeat {
            kept.push(finding);
        }
    }
    kept.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then(b.confidence.total_cmp(&a.confidence))
    });
    let dropped_over_cap = kept.len().saturating_sub(policy.max_findings);
    kept.truncate(policy.max_findings);
    Ok(Advice {
        summary,
        findings: kept,
        deep_review,
        input_class: input.class(),
        dropped_evidence,
        dropped_low_confidence,
        dropped_over_cap,
        stripped,
        summary_withheld,
        verdicts_removed,
        dropped_mutating,
        degraded: summary_withheld
            || summary_mutating
            || verdicts_removed > 0
            || dropped_evidence > 0
            || dropped_mutating > 0,
    })
}

/// A model may wrap JSON in a ```json fence.
fn strip_fence(raw: &str) -> &str {
    let t = raw.trim();
    t.strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .and_then(|r| r.strip_suffix("```"))
        .map_or(t, str::trim)
}

fn parse_finding(f: &Map<String, Value>, policy: &OutputPolicy) -> Result<Finding, ForgeError> {
    let severity = match f.get("severity") {
        Some(Value::String(s)) => {
            FindingSeverity::parse(s).ok_or_else(|| invalid("finding_severity"))?
        }
        _ => return Err(invalid("finding_severity")),
    };
    let path = match f.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(p))
            if !p.trim().is_empty() && p.len() <= 1_000 && !p.chars().any(char::is_control) =>
        {
            Some(p.trim().to_owned())
        }
        Some(_) => return Err(invalid("finding_path")),
    };
    let line = match f.get("line") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n >= 1 => Some(n),
            _ => return Err(invalid("finding_line")),
        },
        Some(_) => return Err(invalid("finding_line")),
    };
    let quote = match f.get("quote") {
        Some(Value::String(q)) if !q.trim().is_empty() => {
            q.chars().take(policy.max_quote_chars).collect()
        }
        _ => return Err(invalid("finding_quote")),
    };
    let message = match f.get("message") {
        Some(Value::String(m)) if !m.trim().is_empty() => m.clone(),
        _ => return Err(invalid("finding_message")),
    };
    let confidence = match f.get("confidence") {
        Some(Value::Number(n)) => match n.as_f64() {
            Some(c) if (0.0..=1.0).contains(&c) => c,
            _ => return Err(invalid("finding_confidence")),
        },
        _ => return Err(invalid("finding_confidence")),
    };
    Ok(Finding {
        severity,
        path,
        line,
        quote,
        message,
        confidence,
    })
}

/// Whitespace runs as one space (a model re-indents what it quotes).
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The quote is in the input: in the parts of the finding's path when any
/// part has that path, otherwise in any part. A quote of fewer than 8
/// characters proves nothing and does not count.
fn evidence_in_input(finding: &Finding, input: &PreparedInput, policy: &OutputPolicy) -> bool {
    let quote = squash(&finding.quote);
    if quote.chars().count() < 8 {
        return false;
    }
    let parts = input.parts();
    if policy.cited_lines_of.is_some() && finding.path.is_none() {
        // A diagnosis names the part of every candidate.
        return false;
    }
    if let (Some(numbered), Some(path)) = (policy.cited_lines_of, finding.path.as_deref()) {
        if numbered == path {
            // Only the cited line of that part counts: a line the input
            // does not have, or a quote from another line, is no evidence.
            let Some(n) = finding.line else {
                return false;
            };
            let prefix = format!("L{n}: ");
            return parts
                .iter()
                .filter(|part| part.path.as_deref() == Some(path))
                .flat_map(|part| part.text.lines())
                .filter_map(|line| line.strip_prefix(prefix.as_str()))
                .any(|line| squash(line).contains(&quote));
        }
    }
    let of_path: Vec<_> = match &finding.path {
        Some(p) => parts
            .iter()
            .filter(|part| part.path.as_deref() == Some(p.as_str()))
            .collect(),
        None => Vec::new(),
    };
    let scope: Vec<_> = if of_path.is_empty() {
        if finding.path.is_some() {
            // A path the input does not have: the finding is outside it.
            return false;
        }
        parts.iter().collect()
    } else {
        of_path
    };
    scope.iter().any(|part| squash(&part.text).contains(&quote))
}

/// Whole words that change something when run (lowercase).
const MUTATING_WORDS: &[&str] = &[
    "rm", "rmdir", "mv", "chmod", "chown", "chgrp", "sudo", "kill", "pkill", "killall", "reboot",
    "shutdown", "restart", "redeploy", "rollback", "truncate", "forgectl",
];

/// Phrases that change something (lowercase; matched on the squashed,
/// lowercased text).
const MUTATING_PHRASES: &[&str] = &[
    "delete from",
    "insert into",
    "drop table",
    "drop index",
    "drop schema",
    "drop database",
    "drop role",
    "alter table",
    "alter role",
    "alter system",
    "grant ",
    "revoke ",
    "vacuum full",
    "reindex ",
    "roll back",
    "systemctl start",
    "systemctl stop",
    "systemctl reload",
    "systemctl kill",
    "systemctl enable",
    "systemctl disable",
    "systemctl mask",
    "service restart",
    "git push",
    "git reset",
    "git update-ref",
    "git gc",
    "git prune",
    "git branch -d",
    "forge db migrate",
    "forge restore",
    "forge backup",
    "forge search reindex",
    "ollama pull",
    "ollama rm",
    "ollama stop",
    "podman rm",
    "podman stop",
    "podman kill",
    "docker rm",
    "docker stop",
    "docker kill",
    "-x post",
    "-x put",
    "-x patch",
    "-x delete",
    "--request post",
    "--request put",
    "--request patch",
    "--request delete",
    "llm.paused",
    "再起動",
    "削除して",
    "削除する",
    "削除し、",
    "消して",
    "停止して",
    "止めて",
    "書き換え",
    "変更して",
    "変更する",
    "適用して",
    "移行して",
    "戻して",
    "ロールバック",
    "切り替えて",
    "有効にして",
    "無効にして",
];

/// `forge admin` subcommands that change data (`forge admin token list`
/// only reads).
const MUTATING_ADMIN: &[&str] = &[
    "issue",
    "revoke",
    "disable",
    "enable",
    "rename",
    "reprovision",
    "record-push",
    "maintain",
    "allow-import",
    "purge",
    "move",
    "new-epoch",
    "register",
    "generate",
    "bootstrap-admin",
    "link-identity",
    "admin-source",
];

/// Whether `text` names a step that changes something: a mutating SQL
/// statement, a shell or service command, a forge operator command that
/// writes, or a Japanese imperative to change, stop or restart. It is
/// conservative: a cause worded like a step is dropped too. The incident
/// diagnosis only says what to look at next (plan 19 §3.2, contract 7).
pub fn names_a_mutating_step(text: &str) -> bool {
    let lower = squash(&normalized(text)).to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .filter(|w| !w.is_empty())
        .collect();
    if words.iter().any(|w| MUTATING_WORDS.contains(w)) {
        return true;
    }
    // `UPDATE <table> SET`: a statement, not the noun "update".
    let update_set = words
        .iter()
        .enumerate()
        .filter(|(_, w)| **w == "update")
        .any(|(at, _)| words.iter().skip(at + 1).take(4).any(|w| *w == "set"));
    if update_set || MUTATING_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    lower.split("forge admin").skip(1).any(|rest| {
        rest.split_whitespace()
            .take(3)
            .any(|w| MUTATING_ADMIN.contains(&w))
    })
}

/// The markers before a diagnosis's "next look" (after NFKC: `：` is `:`).
const NEXT_MARKERS: &[&str] = &["次に見るもの:", "次に見るもの :", "next:", "next look:"];

/// Commands a "next look" may start with: they only read.
const READ_ONLY_COMMANDS: &[&str] = &[
    "journalctl",
    "jq",
    "grep",
    "rg",
    "awk",
    "cut",
    "sort",
    "uniq",
    "head",
    "tail",
    "wc",
    "cat",
    "less",
    "ls",
    "stat",
    "df",
    "du",
    "findmnt",
    "free",
    "uptime",
    "ps",
    "ss",
    "date",
    "nvidia-smi",
    "sha256sum",
    "select",
    "with",
    "explain",
    "forge",
    "systemctl",
    "git",
    "curl",
    "ollama",
    "podman",
    "psql",
];

/// The read-only subcommands of the commands that also write.
fn read_only_use(cmd: &str, rest: &[&str]) -> bool {
    let first = rest.first().copied().unwrap_or("");
    let has = |w: &str| rest.contains(&w);
    match cmd {
        "forge" => match first {
            "doctor" | "explain" | "diagnose" => true,
            "errors" => rest.get(1) == Some(&"list"),
            "db" => rest.get(1) == Some(&"status"),
            "admin" => {
                (matches!(rest.get(1).copied(), Some("token" | "runner"))
                    && rest.get(2) == Some(&"list"))
                    || rest.get(1) == Some(&"repo-path")
            }
            _ => false,
        },
        "systemctl" => matches!(
            first,
            "status" | "show" | "is-active" | "is-failed" | "list-units"
        ),
        "git" => matches!(
            first,
            "log"
                | "show"
                | "status"
                | "cat-file"
                | "rev-parse"
                | "for-each-ref"
                | "ls-tree"
                | "fsck"
                | "count-objects"
                | "diff"
        ),
        "ollama" => matches!(first, "list" | "ps" | "show"),
        "podman" => matches!(first, "ps" | "info" | "images" | "inspect" | "logs"),
        "journalctl" => !rest
            .iter()
            .any(|w| w.starts_with("--vacuum") || *w == "--rotate" || *w == "--flush"),
        // GET only, to this host or a placeholder (`<llm.endpoint>`): no
        // body, no method, no output file, nothing sent elsewhere.
        "curl" => {
            !rest.iter().any(|w| {
                w.starts_with("-x")
                    || w.starts_with("--request")
                    || w.starts_with("-d")
                    || w.starts_with("--data")
                    || w.starts_with("-f")
                    || w.starts_with("--form")
                    || w.starts_with("-t")
                    || w.starts_with("--upload")
                    || w.starts_with("-o")
                    || w.starts_with("--output")
            }) && rest.iter().filter(|w| w.contains("://")).all(|u| {
                u.contains("://127.0.0.1") || u.contains("://localhost") || u.contains("://[::1]")
            })
        }
        "psql" => has("-c") && !has("-f"),
        _ => true,
    }
}

/// SQL that is not a plain read, even inside a SELECT.
const SQL_WRITES: &[&str] = &[
    " into ",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "set_config",
    "nextval",
    "setval",
    "lo_",
    "dblink",
    "pg_read_file",
    "pg_read_binary_file",
    " copy ",
    " for update",
    " refresh ",
    " create ",
    " call ",
    " do ",
];

/// Whether every command of the "next look" in `text` only reads: each
/// segment (split at `|`, `;`, `&&`, `||`) starts with an allowed command
/// used read-only, no redirection into a file outside SQL, no writing SQL.
/// Text without a marker names no command and passes (the deny-list still
/// applies to it).
pub fn next_look_is_read_only(text: &str) -> bool {
    let lower = squash(&normalized(text)).to_lowercase();
    let Some(after) = NEXT_MARKERS
        .iter()
        .filter_map(|m| lower.find(m).map(|at| &lower[at + m.len()..]))
        .next()
    else {
        return true;
    };
    let after = after.replace("&&", ";").replace("||", ";");
    after.split(['|', ';']).all(|segment| {
        let words: Vec<&str> = segment
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | '(' | ')')))
            .filter(|w| !w.is_empty())
            .collect();
        let Some((cmd, rest)) = words.split_first() else {
            return true;
        };
        // Words after the command that are not ASCII end it (「で確かめる」).
        let rest: Vec<&str> = rest.iter().copied().take_while(|w| w.is_ascii()).collect();
        let sql = matches!(*cmd, "select" | "with" | "explain" | "psql");
        let seg = format!(" {segment} ");
        READ_ONLY_COMMANDS.contains(cmd)
            && read_only_use(cmd, &rest)
            && (sql || !segment.contains('>'))
            && !(sql && SQL_WRITES.iter().any(|w| seg.contains(w)))
            && !(*cmd == "explain" && seg.contains("analyze"))
    })
}

/// Closing keywords followed by an issue reference.
const CLOSING: &[&str] = &[
    "closes", "close", "closed", "fixes", "fix", "fixed", "resolves", "resolve", "resolved",
];

/// A word that is a slash command of a forge or a chat (`/merge`,
/// `/approve`, `/close`): a slash and letters only (a path has another `/`
/// or a `.`).
fn is_slash_command(word: &str) -> bool {
    let word = word.trim_matches(|c: char| {
        matches!(
            c,
            '`' | '"'
                | '\''
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '*'
                | '_'
                | '~'
                | ','
                | '.'
                | ':'
                | ';'
                | '!'
                | '?'
        )
    });
    word.strip_prefix('/').is_some_and(|rest| {
        let rest = rest.trim_end_matches([':', ',', '.', ';', '!', '?']);
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '_' || c == '-')
    })
}

/// The text without mentions, closing keywords, slash commands, images and
/// links, its whitespace runs as one space, cut at `max` characters; and
/// how many were removed.
fn clean(text: &str, max: usize) -> (String, usize) {
    let mut n = 0usize;
    // (word, a line break before it): whitespace runs become one space,
    // line breaks stay one line break (a clause boundary for the verdict
    // check, see `negated`).
    let mut out: Vec<(String, bool)> = Vec::new();
    // NFKC without invisible characters first, so a full-width `／ｍｅｒｇｅ`
    // or a zero-width split `L\u{200B}GTM` is seen as what it shows.
    let text = normalized(text);
    let words: Vec<(&str, bool)> = text
        .lines()
        .flat_map(|line| {
            line.split_whitespace()
                .enumerate()
                .map(|(k, w)| (w, k == 0))
        })
        .collect();
    let mut pending_break = false;
    let mut i = 0;
    while let Some(&(word, line_start)) = words.get(i) {
        i += 1;
        pending_break |= line_start;
        let lower = word.to_ascii_lowercase();
        let bare = lower.trim_end_matches([':', ',', '.']);
        let kept = if CLOSING.contains(&bare)
            && words
                .get(i)
                .is_some_and(|(next, _)| next.trim_start_matches(['(', '[']).contains('#'))
        {
            // `closes #12`, `fixes group/project#3`: both words go.
            i += 1;
            n += 1;
            None
        } else if is_slash_command(word) {
            n += 1;
            None
        } else if lower.contains("://") || lower.starts_with("www.") || lower.starts_with("data:") {
            n += 1;
            Some("[link removed]".to_owned())
        } else if word.contains("![") || lower.contains("<img") {
            n += 1;
            Some("[image removed]".to_owned())
        } else if word.contains('@') {
            // `@alice` → `alice`; an e-mail address keeps no `@` either.
            n += 1;
            Some(word.replace('@', ""))
        } else {
            Some(word.to_owned())
        };
        if let Some(w) = kept {
            out.push((w, pending_break && !out.is_empty()));
            pending_break = false;
        }
    }
    let mut joined = String::new();
    for (k, (w, line_break)) in out.iter().enumerate() {
        if k > 0 {
            joined.push(if *line_break { '\n' } else { ' ' });
        }
        joined.push_str(w);
    }
    let cut: String = joined.chars().take(max).collect();
    (cut.trim().to_owned(), n)
}
