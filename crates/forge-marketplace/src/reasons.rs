//! The local LLM's reasons for the builder's recommendation, as a reader
//! gets them (plan 16 A11, ADR-0032 Amendment A11).
//!
//! The gateway runs in the worker; the route runs in `forge serve`. So the
//! route never waits for a model: [`reasons`] records the read
//! (`forge_db::llm::agent_recommend::upsert_ask`, which also asks again for
//! a new tip) and returns what is there now:
//!
//! - `ready`: the worker's advice for exactly this tip, read through the
//!   advice's own row (`forge_db::llm::advice_decided`, decided on its
//!   subject — this project — with the reader's kind; a confidential advice
//!   is withheld from an agent, though no confidential document is ever
//!   sent);
//! - anything else (`pending`, `unavailable`, `paused`, `refused`,
//!   `withheld`): no reasons; the caller answers the rules-only result with
//!   `degraded` and, for a failure, `FG-AGENT-0011` and the stored cause.
//!
//! A swept advice or one about another subject or tip is never shown
//! (`pending`: the worker asks again).

use forge_core::llm::agent_recommend::{reasons_state, ReasonsState};
use forge_core::llm::{Purpose, SubjectKind};
use forge_core::ProjectId;
use forge_db::llm::agent_recommend::upsert_ask;
use forge_db::llm::{advice_decided, AdviceView, ReaderKind};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

/// A shown advice's words and label.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownReasons {
    pub advice_id: i64,
    pub summary: String,
    /// The stored findings (`forge_core::llm::agent_recommend::findings_of`).
    pub findings: serde_json::Value,
    pub degraded: bool,
    pub model: String,
    pub prompt_version: i32,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub created_at: String,
}

/// What a reader is told (module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Reasons {
    pub state: ReasonsState,
    /// `FG-AGENT-0011` when the LLM did not answer (`unavailable`,
    /// `paused`, `refused`).
    pub error_code: Option<ErrorCode>,
    /// The worker's recorded cause (an FG code), if any.
    pub cause: Option<String>,
    /// `Some` exactly when `state` is `Ready`.
    pub advice: Option<ShownReasons>,
}

impl Reasons {
    fn without(state: ReasonsState, cause: Option<String>) -> Self {
        let failed = matches!(
            state,
            ReasonsState::Unavailable | ReasonsState::Paused | ReasonsState::Refused
        );
        Self {
            state,
            error_code: failed.then_some(ErrorCode::RecommendDegraded),
            cause,
            advice: None,
        }
    }
}

/// Record a read of `project`'s recommendation at `tip` and return what
/// the reader is told (module docs).
pub async fn reasons(
    db: &Db,
    project: ProjectId,
    tip: Option<&str>,
    request_id: &str,
    reader: ReaderKind,
) -> Result<Reasons, ForgeError> {
    let ask = upsert_ask(db, project, tip, request_id).await?;
    let state = reasons_state(Some(&ask), tip);
    let (ReasonsState::Ready, Some(advice_id)) = (state, ask.advice_id) else {
        return Ok(Reasons::without(state, ask.error_code));
    };
    let viewed = advice_decided(db, advice_id, |subject| {
        if subject.kind == SubjectKind::Project && subject.id == Some(project.get()) {
            Ok(reader)
        } else {
            Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "advice_subject_mismatch")
                .with_context("advice_id", advice_id))
        }
    })
    .await?;
    Ok(match viewed {
        Some(AdviceView::Shown(a))
            if a.purpose == Purpose::AgentRecommend && a.head_sha.as_deref() == tip =>
        {
            Reasons {
                state: ReasonsState::Ready,
                error_code: None,
                cause: None,
                advice: Some(ShownReasons {
                    advice_id: a.id,
                    summary: a.summary,
                    findings: a.findings,
                    degraded: a.degraded,
                    model: a.model,
                    prompt_version: a.prompt_version,
                    created_at: a.created_at,
                }),
            }
        }
        Some(AdviceView::Withheld { .. }) => Reasons::without(ReasonsState::Withheld, None),
        // Swept, or not this tip's: asked again by the worker.
        Some(AdviceView::Shown(_)) | None => Reasons::without(ReasonsState::Pending, None),
    })
}
