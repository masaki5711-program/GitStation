//! The agent builder's recommendation reasons (plan 16 A11, ADR-0032
//! Amendment A11, ADR-0039 Amendment A11): the local LLM reads the
//! repository's specification documents and says why each part of the
//! rules-only recommendation (`forge_agent::recommend`, A7) fits, in which
//! order to look at them, and which other published parts of the catalog
//! may also fit. Pure — the worker (`forge-cli`) reads git and the
//! database, asks the gateway and records the answer; the route merges it.
//!
//! - [`recommend_input`]: the input (`internal`): the rules' picks, the
//!   catalog's stable parts (manifest facts only: slug, kind, version,
//!   capabilities, requires, files) and at most [`MAX_SPEC_DOCS`]
//!   specification documents that are **not confidential** (the worker
//!   filters them with `forge_checks::confidential`; a document it cannot
//!   classify is never given). Secrets are redacted by [`super::prepare`].
//! - [`policy`]: at most [`MAX_FINDINGS`](super::MAX_FINDINGS) findings,
//!   each naming a part's path `part:<slug>` with a quote from that part
//!   (`require_path`): a finding about anything else — a specification
//!   document, a slug the input does not have — never becomes a reason.
//! - [`merge`]: the rules' selection stays exactly the rules' selection
//!   (same parts, same versions, the base model first); a finding about one
//!   of its parts adds a reason and moves it up (by the finding's
//!   confidence); a finding about another catalog part that is not an
//!   adapter becomes a **suggestion** (never selected: a person ticks it and
//!   sees its capabilities in the Dry-Run). Anything else is dropped and
//!   counted. The LLM never invents a part and never changes the
//!   selection's safety.
//! - [`AskState`] / [`ReasonsState`]: what the worker recorded and what a
//!   reader is told. Anything but a fresh answer for the current tip is the
//!   rules-only result with `degraded` (`FG-AGENT-0011`), never an error
//!   and never a made-up success.

use std::collections::BTreeSet;

use forge_errors::ForgeError;
use serde_json::Value;

use super::{prepare, InputClass, InputLimits, InputPart, OutputPolicy, PreparedInput};

/// The most specification documents one input holds.
pub const MAX_SPEC_DOCS: usize = 6;
/// The most characters of one specification document given.
pub const MAX_SPEC_CHARS: usize = 4_000;
/// The largest specification file read (bytes); a larger one is cut.
pub const MAX_SPEC_BYTES: u64 = 256 * 1024;
/// The most catalog parts one input holds (the rules' picks first).
pub const MAX_CATALOG: usize = 60;
/// The most files of one part listed.
const MAX_PART_FILES: usize = 10;
/// The most suggestions kept.
pub const MAX_SUGGESTED: usize = 5;
/// The longest reason (characters; the builder's `reason` field holds 200).
pub const MAX_REASON_CHARS: usize = 200;
/// The longest summary.
pub const MAX_SUMMARY_CHARS: usize = 600;
/// The label of the catalog parts (the only ones a reason may name).
pub const PART_LABEL: &str = "part";
/// The label of the specification documents.
pub const SPEC_LABEL: &str = "spec";
/// The label of the rules' picks.
pub const RULES_LABEL: &str = "rules";
/// A catalog part's path in the input: `part:<slug>`.
pub const PART_PATH_PREFIX: &str = "part:";
/// The kind of a base model part (never suggested by the model).
pub const ADAPTER_KIND: &str = "adapter";
/// How long an ask may stay pending before a reader calls it unavailable:
/// the interactive deadline (2 min) plus two checked calls and a pass.
pub const PENDING_GRACE_SECS: i64 = 6 * 60;

/// A published stable part version the model may name (manifest facts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub slug: String,
    pub kind: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub requires: Vec<String>,
    pub files: Vec<String>,
}

/// One part of the rules-only recommendation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulePick {
    pub slug: String,
    pub kind: String,
    pub version: String,
    /// The rule's reason (`base_model`, `current_lock`, `required_by=…`).
    pub reason: String,
}

/// A specification document's text, as read (not confidential).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecText {
    pub path: String,
    pub text: String,
}

/// A catalog part's path in the input.
pub fn part_path(slug: &str) -> String {
    format!("{PART_PATH_PREFIX}{slug}")
}

/// The slug a finding's path names, if it names a part.
pub fn slug_of_path(path: &str) -> Option<&str> {
    path.strip_prefix(PART_PATH_PREFIX)
        .filter(|s| !s.is_empty())
}

/// The first `max` characters of `text` (on a character boundary).
fn cut(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((at, _)) => text.get(..at).unwrap_or(text),
        None => text,
    }
}

/// The first `max` characters of a document, ending at a line end when
/// one is in it: a secret is not split in half before [`prepare`] redacts
/// (a half-cut value could slip past the redaction rules).
fn cut_lines(text: &str, max: usize) -> &str {
    let head = cut(text, max);
    if head.len() == text.len() {
        return head;
    }
    match head.rfind('\n') {
        Some(at) => head.get(..at).unwrap_or(""),
        None => "",
    }
}

/// One line without control characters (data never breaks the layout).
fn line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn part_text(e: &CatalogEntry) -> String {
    let mut out = format!(
        "slug: {}\nkind: {}\nversion: {}\ncapabilities: {}\nrequires: {}\nfiles:\n",
        line(&e.slug),
        line(&e.kind),
        line(&e.version),
        if e.capabilities.is_empty() {
            "none".to_owned()
        } else {
            line(&e.capabilities.join(", "))
        },
        if e.requires.is_empty() {
            "none".to_owned()
        } else {
            line(&e.requires.join(", "))
        },
    );
    for f in e.files.iter().take(MAX_PART_FILES) {
        out.push_str(&format!("- {}\n", line(f)));
    }
    out
}

/// The catalog parts given: the rules' picks first, then the others by
/// slug; at most [`MAX_CATALOG`].
pub fn catalog_given<'a>(rules: &[RulePick], catalog: &'a [CatalogEntry]) -> Vec<&'a CatalogEntry> {
    let picked: BTreeSet<&str> = rules.iter().map(|r| r.slug.as_str()).collect();
    let mut out: Vec<&CatalogEntry> = catalog
        .iter()
        .filter(|c| picked.contains(c.slug.as_str()))
        .collect();
    let mut others: Vec<&CatalogEntry> = catalog
        .iter()
        .filter(|c| !picked.contains(c.slug.as_str()))
        .collect();
    others.sort_by(|a, b| a.slug.cmp(&b.slug));
    out.extend(others);
    let mut seen = BTreeSet::new();
    out.retain(|c| seen.insert(c.slug.as_str()));
    out.truncate(MAX_CATALOG);
    out
}

/// The input of one ask (module docs). `specs` are the documents the
/// worker already found **not confidential**; at most [`MAX_SPEC_DOCS`] of
/// them are given, each cut to [`MAX_SPEC_CHARS`]. `FG-LLM-0005` when it
/// cannot be sent.
pub fn recommend_input(
    rules: &[RulePick],
    catalog: &[CatalogEntry],
    specs: &[SpecText],
) -> Result<PreparedInput, ForgeError> {
    let mut picks = String::new();
    for r in rules {
        picks.push_str(&format!(
            "{} {} {} ({})\n",
            line(&r.slug),
            line(&r.kind),
            line(&r.version),
            line(&r.reason)
        ));
    }
    let mut parts = vec![InputPart::new(RULES_LABEL, InputClass::Internal, picks)];
    for e in catalog_given(rules, catalog) {
        parts.push(
            InputPart::new(PART_LABEL, InputClass::Internal, part_text(e))
                .with_path(&part_path(&e.slug)),
        );
    }
    for s in specs.iter().take(MAX_SPEC_DOCS) {
        parts.push(
            InputPart::new(
                SPEC_LABEL,
                InputClass::Internal,
                cut_lines(&s.text, MAX_SPEC_CHARS),
            )
            .with_path(&s.path),
        );
    }
    prepare(parts, &InputLimits::default())
}

/// The output policy of an ask (module docs).
pub fn policy() -> OutputPolicy {
    OutputPolicy {
        max_summary_chars: MAX_SUMMARY_CHARS,
        max_message_chars: MAX_REASON_CHARS,
        max_quote_chars: 300,
        require_path: true,
        ..OutputPolicy::default()
    }
}

/// A stored finding, as the merge reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReasonFinding {
    pub path: Option<String>,
    pub message: String,
    pub confidence: f64,
}

/// The findings of a stored advice (`forge.llm_advice.findings`); an item
/// of another shape is skipped (counted by [`merge`] as dropped).
pub fn findings_of(stored: &Value) -> (Vec<ReasonFinding>, usize) {
    let Some(items) = stored.as_array() else {
        return (Vec::new(), 0);
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for item in items {
        match (
            item.get("message").and_then(Value::as_str),
            item.get("confidence").and_then(Value::as_f64),
        ) {
            (Some(message), Some(confidence)) => out.push(ReasonFinding {
                path: item.get("path").and_then(Value::as_str).map(str::to_owned),
                message: message.to_owned(),
                confidence,
            }),
            _ => skipped += 1,
        }
    }
    (out, skipped)
}

/// One part of the merged recommendation (the rules' pick, maybe with the
/// model's reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonedPart {
    pub pick: RulePick,
    pub llm_reason: Option<String>,
}

/// A catalog part the model suggests (never selected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggested {
    pub slug: String,
    pub kind: String,
    pub version: String,
    /// The part's declared capabilities (shown beside the AI's reason).
    pub capabilities: Vec<String>,
    pub reason: String,
}

/// The merge (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reasoned {
    /// Exactly the rules' picks: the base model first, then the parts with
    /// a reason (most confident first), then the rest in the rules' order.
    pub parts: Vec<ReasonedPart>,
    pub suggested: Vec<Suggested>,
    /// Findings that named no catalog part, an adapter, a repeat, or past
    /// [`MAX_SUGGESTED`].
    pub dropped: usize,
}

/// Merge the model's findings into the rules' recommendation (module
/// docs). `catalog`: the stable versions the recommendation was made from;
/// only the parts the input gave ([`catalog_given`]) may be named.
pub fn merge(rules: &[RulePick], catalog: &[CatalogEntry], findings: &[ReasonFinding]) -> Reasoned {
    let given = catalog_given(rules, catalog);
    let mut ordered: Vec<&ReasonFinding> = findings.iter().collect();
    ordered.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let mut reasons: Vec<(String, String)> = Vec::new();
    let mut suggested: Vec<Suggested> = Vec::new();
    let mut dropped = 0usize;
    for f in ordered {
        let Some(slug) = f.path.as_deref().and_then(slug_of_path) else {
            dropped += 1;
            continue;
        };
        let reason = line(f.message.trim());
        let reason = cut(&reason, MAX_REASON_CHARS).to_owned();
        if reason.is_empty()
            || reasons.iter().any(|(s, _)| s == slug)
            || suggested.iter().any(|s| s.slug == slug)
        {
            dropped += 1;
            continue;
        }
        if rules.iter().any(|r| r.slug == slug) {
            reasons.push((slug.to_owned(), reason));
            continue;
        }
        match given.iter().find(|c| c.slug == slug) {
            Some(c) if c.kind != ADAPTER_KIND && suggested.len() < MAX_SUGGESTED => {
                suggested.push(Suggested {
                    slug: c.slug.clone(),                 // clone: the answer owns it
                    kind: c.kind.clone(),                 // clone: as above
                    version: c.version.clone(),           // clone: as above
                    capabilities: c.capabilities.clone(), // clone: as above
                    reason,
                });
            }
            Some(_) | None => dropped += 1,
        }
    }
    let reason_of = |slug: &str| {
        reasons
            .iter()
            .find(|(s, _)| s == slug)
            .map(|(_, r)| r.clone()) // clone: the part owns it
    };
    let rank = |slug: &str| reasons.iter().position(|(s, _)| s == slug);
    let mut parts: Vec<ReasonedPart> = rules
        .iter()
        .map(|r| ReasonedPart {
            pick: r.clone(), // clone: the answer owns its picks
            llm_reason: reason_of(&r.slug),
        })
        .collect();
    // Stable: the base model first, then by the model's rank, then the
    // rules' own order.
    parts.sort_by_key(|p| {
        (
            p.pick.kind != ADAPTER_KIND,
            rank(&p.pick.slug).unwrap_or(usize::MAX),
        )
    });
    Reasoned {
        parts,
        suggested,
        dropped,
    }
}

/// What the worker recorded for a project's ask
/// (`forge.agent_recommend_asks.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskState {
    /// Asked by a reader; no answer for it yet.
    Pending,
    /// An advice for `head_sha` was written.
    Ready,
    /// The LLM did not answer (down, timed out, the digest stopped it, an
    /// invalid answer twice): the error code is recorded.
    Unavailable,
    /// The gateway is paused: nothing was queued.
    Paused,
    /// Nothing was sent: no specification document that may be given, an
    /// input that cannot be sent (`FG-LLM-0005`), or the documents could
    /// not be classified (fail closed).
    Refused,
}

impl AskState {
    pub const ALL: [Self; 5] = [
        Self::Pending,
        Self::Ready,
        Self::Unavailable,
        Self::Paused,
        Self::Refused,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Paused => "paused",
            Self::Refused => "refused",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == text)
    }
}

/// What a reader is told about the reasons (`llm.state` of the answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonsState {
    /// The reasons are merged in (`source: "rules+llm"`).
    Ready,
    /// Asked; the answer is not there yet.
    Pending,
    Unavailable,
    Paused,
    Refused,
    /// A confidential advice asked for by an agent (never expected: no
    /// confidential document is sent).
    Withheld,
}

impl ReasonsState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Pending => "pending",
            Self::Unavailable => "unavailable",
            Self::Paused => "paused",
            Self::Refused => "refused",
            Self::Withheld => "withheld",
        }
    }
}

/// A stored ask, as the reader sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRecord {
    pub state: AskState,
    /// The tip the answer is for (`None`: not answered yet).
    pub head_sha: Option<String>,
    pub error_code: Option<String>,
    pub advice_id: Option<i64>,
    /// Seconds since the ask became pending.
    pub pending_secs: i64,
}

/// What the reader is told for `ask` at the current `tip` (`None`: an
/// empty repository), before the advice is read. `Ready` means "read the
/// advice"; every other state is the rules-only result with `degraded`.
pub fn reasons_state(ask: Option<&AskRecord>, tip: Option<&str>) -> ReasonsState {
    let Some(ask) = ask else {
        return ReasonsState::Pending;
    };
    let current = tip.is_some() && ask.head_sha.as_deref() == tip;
    match ask.state {
        AskState::Ready if current && ask.advice_id.is_some() => ReasonsState::Ready,
        // An older tip's answer: asked again for this one.
        AskState::Ready | AskState::Pending if ask.pending_secs > PENDING_GRACE_SECS => {
            ReasonsState::Unavailable
        }
        AskState::Ready | AskState::Pending => ReasonsState::Pending,
        AskState::Unavailable => ReasonsState::Unavailable,
        AskState::Paused => ReasonsState::Paused,
        AskState::Refused => ReasonsState::Refused,
    }
}

#[cfg(test)]
#[path = "agent_recommend_tests.rs"]
mod tests;
