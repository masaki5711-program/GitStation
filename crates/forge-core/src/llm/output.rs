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
//!   field, so nothing in it can change state.

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
    /// Less trustworthy than its words suggest: the summary was withheld or
    /// findings were dropped for their evidence (shown as such).
    pub degraded: bool,
}

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

fn states_a_verdict(text: &str) -> bool {
    let lower = text.to_lowercase();
    VERDICT_PHRASES.iter().any(|p| lower.contains(p))
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
}

impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            max_findings: MAX_FINDINGS,
            min_confidence: 0.5,
            max_summary_chars: 1_200,
            max_message_chars: 1_000,
            max_quote_chars: 500,
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
    let summary = if summary_withheld {
        WITHHELD_SUMMARY.to_owned()
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
                Some(DeepReview { reason })
            }
            _ => return Err(invalid("deep_review_type")),
        },
        Some(_) => return Err(invalid("deep_review_type")),
    };

    let mut kept: Vec<Finding> = Vec::new();
    let mut dropped_evidence = 0usize;
    let mut dropped_low_confidence = 0usize;
    for item in &raw_findings {
        let Value::Object(f) = item else {
            return Err(invalid("finding_type"));
        };
        let finding = parse_finding(f, policy)?;
        if finding.confidence < policy.min_confidence {
            dropped_low_confidence += 1;
            continue;
        }
        if !evidence_in_input(&finding, input) {
            dropped_evidence += 1;
            continue;
        }
        let (message, n) = clean(&finding.message, policy.max_message_chars);
        stripped += n;
        if message.trim().is_empty() {
            dropped_evidence += 1;
            continue;
        }
        // The quote passed the evidence check as given; what is stored and
        // shown is cleaned like every other text.
        let (quote, n) = clean(&finding.quote, policy.max_quote_chars);
        stripped += n;
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
        degraded: summary_withheld || dropped_evidence > 0,
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
fn evidence_in_input(finding: &Finding, input: &PreparedInput) -> bool {
    let quote = squash(&finding.quote);
    if quote.chars().count() < 8 {
        return false;
    }
    let parts = input.parts();
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

/// Closing keywords followed by an issue reference.
const CLOSING: &[&str] = &[
    "closes", "close", "closed", "fixes", "fix", "fixed", "resolves", "resolve", "resolved",
];

/// A word that is a slash command of a forge or a chat (`/merge`,
/// `/approve`, `/close`): a slash and letters only (a path has another `/`
/// or a `.`).
fn is_slash_command(word: &str) -> bool {
    word.strip_prefix('/').is_some_and(|rest| {
        let rest = rest.trim_end_matches([':', ',', '.', ';', '!']);
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
    let mut out: Vec<String> = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    while let Some(&word) = words.get(i) {
        i += 1;
        let lower = word.to_ascii_lowercase();
        let bare = lower.trim_end_matches([':', ',', '.']);
        if CLOSING.contains(&bare)
            && words
                .get(i)
                .is_some_and(|next| next.trim_start_matches(['(', '[']).contains('#'))
        {
            // `closes #12`, `fixes group/project#3`: both words go.
            i += 1;
            n += 1;
            continue;
        }
        if is_slash_command(word) {
            n += 1;
            continue;
        }
        if lower.contains("://") || lower.starts_with("www.") || lower.starts_with("data:") {
            out.push("[link removed]".to_owned());
            n += 1;
            continue;
        }
        if word.contains("![") || lower.contains("<img") {
            out.push("[image removed]".to_owned());
            n += 1;
            continue;
        }
        if word.contains('@') {
            // `@alice` → `alice`; an e-mail address keeps no `@` either.
            n += 1;
            out.push(word.replace('@', ""));
            continue;
        }
        out.push(word.to_owned());
    }
    let joined = out.join(" ");
    let cut: String = joined.chars().take(max).collect();
    (cut.trim().to_owned(), n)
}
