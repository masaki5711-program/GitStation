//! Deciding an AI advice of the local LLM (M5, plan 19 §4, §5, §8.2,
//! ADR-0039 decision 6). An advice has no read action of its own: it is
//! read with its subject's (`MergeRequestRead`, `DeployRead`,
//! `OpsDiagnosisRead`) and voted on with `AiAdviceFeedback`.
//! [`authorize_advice`] calls [`authorize`] once on the subject's resource
//! and then says who the reader is, so the route's `decide` closure (the
//! one forge-db's `advice_decided` / `record_feedback_decided` take) is
//! this function and nothing else:
//!
//! - [`AdviceReader::Person`] — a web session, or a person's own token not
//!   issued with `is_agent` and not declared an agent audience: the advice
//!   whatever its input class;
//! - [`AdviceReader::Agent`] — an agent's token, a service account, a bot,
//!   a request acting for a person (act-as, ADR-0025), a request that
//!   declared itself an agent audience, and every credential that is no
//!   person's (closed by default): a confidential advice is withheld
//!   ("機密の入力のため表示しません"), never refused, and a vote is never
//!   counted (`authorize()` already refuses `AiAdviceFeedback` to agents).

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{Credential, Principal, Resource};

/// Who reads an AI advice (plan 19 §5): the words of a confidential one
/// reach a person only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceReader {
    /// A person (a web session, a person's token).
    Person,
    /// An AI agent audience or a delegated tool.
    Agent,
}

impl AdviceReader {
    /// The log `audience`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Agent => "agent",
        }
    }
}

/// Who `principal` is for an advice. `declared_agent`: the request
/// declared an AI agent audience (`Forge-Context-Audience: agent`), which
/// a person's token honours as an agent's.
pub fn advice_reader(principal: &Principal, declared_agent: bool) -> AdviceReader {
    if declared_agent || principal.is_agent_audience() {
        return AdviceReader::Agent;
    }
    match principal.credential() {
        Credential::Session | Credential::Token(_) => AdviceReader::Person,
        // Acting for a person is a tool, not the person (ADR-0039
        // decision 6); no other credential is a person reading.
        Credential::ActAs(_)
        | Credential::Runner(_)
        | Credential::Job(_)
        | Credential::AutoMerge
        | Credential::Autofix
        | Credential::Portal(_) => AdviceReader::Agent,
    }
}

/// `authorize(principal, action, resource)` for an advice's subject, then
/// who the reader is. `action` is one of the subject reads or the vote;
/// any other is `FG-SYS-0001 reason=advice_action_invalid` (a programming
/// error, never a pass). A refusal is `authorize()`'s as it is (the
/// subject's 404 for a subject the caller cannot see).
pub fn authorize_advice(
    principal: &Principal,
    action: Action,
    resource: &Resource,
    declared_agent: bool,
) -> Result<AdviceReader, ForgeError> {
    if !matches!(
        action,
        Action::MergeRequestRead
            | Action::DeployRead
            | Action::OpsDiagnosisRead
            | Action::AiAdviceFeedback
    ) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "advice_action_invalid")
            .with_context("action", action.name()));
    }
    authorize(principal, action, resource)?;
    Ok(advice_reader(principal, declared_agent))
}
