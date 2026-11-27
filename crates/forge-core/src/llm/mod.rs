//! The local LLM's contract (plan 19 §2.1, ADR-0039): what the gateway
//! (`forge-llm`) and the feature crates that build inputs agree on. Pure —
//! no I/O, no database, no clock of its own.
//!
//! - [`Purpose`], [`Priority`]: why a request runs and how it queues
//!   (ADR-0017 §3: interactive > review > autofix > batch).
//! - [`InputClass`] and [`input`]: the data boundary (06 D-5). A request's
//!   class is the maximum of its parts and is never lowered; secrets are
//!   not a class but something that never reaches any LLM — redacted, or
//!   the request is refused (`FG-LLM-0005`).
//! - [`output`]: the answer is checked by code, never trusted — the JSON
//!   schema, evidence that must be in the input (`FG-LLM-0006` per dropped
//!   finding), at most [`MAX_FINDINGS`], no mentions, closing keywords or
//!   links (ADR-0017 §2). An invalid answer is retried once, then
//!   `FG-LLM-0002`.
//! - [`provider`]: the [`LlmProvider`] trait and [`generate_checked`].
//! - [`ModelRef`]: a model pinned as `name:tag@sha256:<digest>` (never
//!   `latest`, plan 17 §6).
//!
//! The LLM decides nothing: an advice has no action field at all, and a
//! failure is never reported as success (ADR-0003).

use std::fmt;
use std::time::Duration;

pub mod agent_recommend;
pub mod asset_draft;
pub mod diagnosis;
pub mod input;
pub mod ops;
pub mod output;
pub mod provider;

pub use input::{prepare, InputLimits, InputPart, PreparedInput};
pub use output::{
    check_output, names_a_mutating_step, next_look_is_read_only, Advice, DeepReview, EditPolicy,
    Finding, FindingSeverity, OutputPolicy, ProposedEdit, MUTATING_WITHHELD, VERDICT_REMOVED,
    WITHHELD_SUMMARY,
};
pub use provider::{generate_checked, ChatFuture, ChatReply, ChatRequest, Generated, LlmProvider};

/// The most findings one advice keeps (plan 19 §3.1, §4).
pub const MAX_FINDINGS: usize = 10;

/// A waiting request rises one priority tier after this long (ADR-0017 §3).
pub const PROMOTE_AFTER: Duration = Duration::from_secs(10 * 60);

/// The share of GPU time reserved for merge request reviews (ADR-0017 §3).
pub const REVIEW_RESERVE_PERCENT: u32 = 20;

/// The longest a `gpu-eval` job's claim waits for interactive inference
/// (plan 19 §1.1: at most 10 minutes): after this the bench gets the GPU
/// anyway. 9 minutes, so a held bench is handed out before the runner
/// health check would call it unclaimed (`FG-CI-0005`, 10 minutes).
pub const GPU_EVAL_MAX_DELAY: Duration = Duration::from_secs(9 * 60);

/// Why a request runs (plan 19 §3; stage 2 adds more).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Purpose {
    /// The merge request first pass (W14-1).
    MrReview,
    /// The incident first diagnosis (W14-5).
    IncidentDiagnosis,
    /// The deployment approval summary (W14-11).
    ApprovalSummary,
    /// A draft fix of an AI asset from an improvement request's case
    /// (plan 17 P17-10, stage 2): proposed on a separate branch only.
    AiAssetDraft,
    /// Reasons for the agent builder's rules-only recommendation from the
    /// repository's specification documents (plan 16 A11, stage 2): words
    /// and an order only, never a change of the selection.
    AgentRecommend,
}

impl Purpose {
    pub const ALL: [Self; 5] = [
        Self::MrReview,
        Self::IncidentDiagnosis,
        Self::ApprovalSummary,
        Self::AiAssetDraft,
        Self::AgentRecommend,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MrReview => "mr_review",
            Self::IncidentDiagnosis => "incident_diagnosis",
            Self::ApprovalSummary => "approval_summary",
            Self::AiAssetDraft => "ai_asset_draft",
            Self::AgentRecommend => "agent_recommend",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == text)
    }

    /// The tier a request of this purpose starts in: a person waits for a
    /// diagnosis or an approval summary; a review runs from an event; an
    /// AI asset draft is a fix (ADR-0017 §3: the autofix tier); a person
    /// opened the agent builder for the recommendation's reasons.
    pub fn priority(self) -> Priority {
        match self {
            Self::MrReview => Priority::Review,
            Self::AiAssetDraft => Priority::Autofix,
            Self::IncidentDiagnosis | Self::ApprovalSummary | Self::AgentRecommend => {
                Priority::Interactive
            }
        }
    }
}

impl fmt::Display for Purpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The queue's tiers, highest first (ADR-0017 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    Interactive,
    Review,
    Autofix,
    Batch,
}

impl Priority {
    pub const ALL: [Self; 4] = [Self::Interactive, Self::Review, Self::Autofix, Self::Batch];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Review => "review",
            Self::Autofix => "autofix",
            Self::Batch => "batch",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == text)
    }

    /// 0 = highest.
    pub fn rank(self) -> u8 {
        match self {
            Self::Interactive => 0,
            Self::Review => 1,
            Self::Autofix => 2,
            Self::Batch => 3,
        }
    }

    /// One tier up (a request that waited [`PROMOTE_AFTER`]); interactive
    /// stays.
    pub fn promoted(self) -> Self {
        match self {
            Self::Interactive | Self::Review => Self::Interactive,
            Self::Autofix => Self::Review,
            Self::Batch => Self::Autofix,
        }
    }

    /// How long a request of this tier may wait before it is given up
    /// (`FG-LLM-0004`, plan 19 §8.1: interactive 2 min, a review 30 min,
    /// batch 6 h).
    pub fn deadline(self) -> Duration {
        match self {
            Self::Interactive => Duration::from_secs(2 * 60),
            Self::Review | Self::Autofix => Duration::from_secs(30 * 60),
            Self::Batch => Duration::from_secs(6 * 60 * 60),
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The data class of an input, lowest first (06 D-5). Secrets are not a
/// class: they never reach any LLM ([`input::prepare`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InputClass {
    Public,
    /// Code, PBIs, designs, redacted logs.
    Internal,
    /// Personal or customer data, confidential specifications: the local
    /// LLM only, and the advice is shown to people only (never to an agent
    /// token or a delegated tool).
    Confidential,
}

impl InputClass {
    pub const ALL: [Self; 3] = [Self::Public, Self::Internal, Self::Confidential];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Confidential => "confidential",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == text)
    }

    /// The class of an input made of parts of these classes: the highest
    /// (an empty input is `Public`).
    pub fn of_parts(classes: impl IntoIterator<Item = Self>) -> Self {
        classes.into_iter().max().unwrap_or(Self::Public)
    }

    /// Whether an agent token or a delegated (act-as) tool may read an
    /// advice of this class (plan 19 §5).
    pub fn shown_to_agents(self) -> bool {
        self != Self::Confidential
    }
}

impl fmt::Display for InputClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A model pinned by digest: `name:tag@sha256:<64 hex>` (plan 17 §6, plan
/// 19 §0.3 7). `latest` and a missing digest are refused, so a model never
/// changes silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    /// `name:tag` as the provider names it.
    pub name: String,
    /// 64 lowercase hex characters (without `sha256:`).
    pub digest: String,
}

impl ModelRef {
    /// Parse `name:tag@sha256:<hex>` (or `name:tag@<hex>`). The reason on
    /// failure: `invalid`, `tag_missing`, `latest_tag`, `digest_missing`,
    /// `digest_invalid`.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let text = text.trim();
        let Some((name, digest)) = text.split_once('@') else {
            return Err("digest_missing");
        };
        let Some((model, tag)) = name.rsplit_once(':') else {
            return Err("tag_missing");
        };
        let ok_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
        if model.is_empty()
            || tag.is_empty()
            || name.len() > 200
            || !model.chars().all(ok_char)
            || !tag.chars().all(ok_char)
        {
            return Err("invalid");
        }
        if tag.eq_ignore_ascii_case("latest") {
            return Err("latest_tag");
        }
        let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("digest_invalid");
        }
        Ok(Self {
            name: name.to_owned(),
            digest: hex.to_ascii_lowercase(),
        })
    }

    /// Whether a digest the provider reports (with or without `sha256:`)
    /// is this one.
    pub fn digest_matches(&self, reported: &str) -> bool {
        let hex = reported.trim();
        let hex = hex.strip_prefix("sha256:").unwrap_or(hex);
        hex.eq_ignore_ascii_case(&self.digest)
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@sha256:{}", self.name, self.digest)
    }
}

/// What an advice is about. Who may read it is decided on this subject
/// (its merge request, deployment or incident), never on another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    MergeRequest,
    Deployment,
    /// A diagnosis of an error code or request (no row of its own).
    Incident,
    /// A project (the agent builder's recommendation, plan 16 A11).
    Project,
}

impl SubjectKind {
    pub const ALL: [Self; 4] = [
        Self::MergeRequest,
        Self::Deployment,
        Self::Incident,
        Self::Project,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MergeRequest => "merge_request",
            Self::Deployment => "deployment",
            Self::Incident => "incident",
            Self::Project => "project",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// A project's `llm.review` (plan 19 §3.1; no setting = `Off`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReviewMode {
    /// No first pass, no check row.
    #[default]
    Off,
    /// `forge/ai-review` is written as advice; never blocks a merge.
    Advisory,
    /// The project requires it (a Maintainer's choice, ADR-0003).
    Required,
}

impl ReviewMode {
    pub const ALL: [Self; 3] = [Self::Off, Self::Advisory, Self::Required];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Advisory => "advisory",
            Self::Required => "required",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == text)
    }
}

/// A person's verdict on an advice or one of its findings (plan 19 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    Useful,
    Wrong,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Wrong => "wrong",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "useful" => Some(Self::Useful),
            "wrong" => Some(Self::Wrong),
            _ => None,
        }
    }
}

/// Whether a `gpu-eval` job pending for `pending_for` may be claimed now
/// (plan 19 §1.1): at once when no interactive inference is queued or
/// running, otherwise after [`GPU_EVAL_MAX_DELAY`] (never starved).
pub fn gpu_eval_may_claim(interactive_active: bool, pending_for: Duration) -> bool {
    !interactive_active || pending_for >= GPU_EVAL_MAX_DELAY
}

#[cfg(test)]
mod tests;
