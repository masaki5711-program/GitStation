//! `GET …/merge-requests/{iid}/ai-review` (M5 L5-5, plan 19 §3.1, ADR-0039
//! Amendment L5-5): the local LLM's first pass, as advice — the project's
//! `llm.review`, the `forge/ai-review` run on the current head and the
//! newest advice (summary, findings with their evidence, the deep-review
//! mark), labelled "AI の助言（提案）" with its model, prompt version and
//! time. Read like the merge request (`MergeRequestRead`). The advice's
//! words are decided on the advice's own subject
//! (`forge_auth::authorize_advice`): a confidential advice is `withheld`
//! from an agent token, a delegated (act-as) request and a request that
//! declares `Forge-Context-Audience: agent`. Reading writes nothing; the
//! advice never decides anything.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::MergeRequestCtx;
use forge_core::checks::CheckRun;
use forge_review::ai_review::{AdviceState, AiReviewView, ADVICE_LABEL, WITHHELD_JA};
use serde::Serialize;

use super::review;
use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::write::{bounded, json, no_store};

/// The path after `{iid}` of [`AI_REVIEW`].
pub const AI_REVIEW_SUFFIX: &str = "/ai-review";
/// `GET` the AI first pass of a merge request.
pub const AI_REVIEW: &str = "/api/v1/projects/{id}/merge-requests/{iid}/ai-review";
/// What the screens say when there is no advice (plan 19 §0.3 5).
pub const NO_ADVICE_JA: &str = "AI の助言は今ありません";

/// The `forge/ai-review` run on the current head.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiReviewCheckBody {
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// `success` (no finding), `neutral` (findings, low confidence, or not
    /// reviewed), `failure` (a `required` project's critical / high
    /// finding), `error` (the LLM failed: never a success).
    #[schema(required = true)]
    pub conclusion: Option<String>,
}

/// One finding, with the evidence it was checked against.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiReviewFindingBody {
    /// `critical`, `high`, `medium` or `low`.
    pub severity: String,
    #[schema(required = true)]
    pub path: Option<String>,
    #[schema(required = true)]
    pub line: Option<i64>,
    /// Copied from the input (code checked it is there).
    pub quote: String,
    pub message: String,
    pub confidence: f64,
}

/// The newest advice.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiReviewAdviceBody {
    /// `none` (no advice: off, the LLM disabled or down, not answered yet),
    /// `shown`, or `withheld` (confidential, read by an agent).
    pub state: String,
    /// "AI の助言（提案） / AI advice (suggestion)".
    pub label: String,
    /// For `none` and `withheld`: what the screen says instead.
    #[schema(required = true)]
    pub note: Option<String>,
    /// Made for another head than the current one.
    pub stale: bool,
    /// The head the advice was made for.
    #[schema(required = true)]
    pub head_sha: Option<String>,
    #[schema(required = true)]
    pub summary: Option<String>,
    pub findings: Vec<AiReviewFindingBody>,
    /// Why a deeper review (Claude or a person) is recommended: code rules
    /// first, then the model's words (`null`: not recommended).
    #[schema(required = true)]
    pub deep_review: Option<String>,
    /// Less trustworthy than its words (a verdict withheld, findings
    /// dropped for missing evidence).
    #[schema(required = true)]
    pub degraded: Option<bool>,
    /// `internal` or `confidential` (people only).
    #[schema(required = true)]
    pub input_class: Option<String>,
    #[schema(required = true)]
    pub advice_id: Option<i64>,
    /// `name:tag`.
    #[schema(required = true)]
    pub model: Option<String>,
    #[schema(required = true)]
    pub prompt_version: Option<i32>,
    /// UTC.
    #[schema(required = true)]
    pub created_at: Option<String>,
}

/// `GET …/merge-requests/{iid}/ai-review`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiReviewBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// The project's `llm.review`: `off`, `advisory` or `required`.
    pub mode: String,
    /// The merge request's head now.
    #[schema(required = true)]
    pub head_sha: Option<String>,
    /// `forge/ai-review` on that head (`null`: none).
    #[schema(required = true)]
    pub check: Option<AiReviewCheckBody>,
    pub advice: AiReviewAdviceBody,
}

fn check_body(run: CheckRun) -> AiReviewCheckBody {
    AiReviewCheckBody {
        status: run.status.as_str().to_owned(),
        conclusion: run.conclusion.map(|c| c.as_str().to_owned()),
    }
}

fn findings(raw: &serde_json::Value) -> Vec<AiReviewFindingBody> {
    let Some(list) = raw.as_array() else {
        return Vec::new();
    };
    list.iter()
        .map(|f| AiReviewFindingBody {
            severity: f["severity"].as_str().unwrap_or("low").to_owned(),
            path: f["path"].as_str().map(str::to_owned),
            line: f["line"].as_i64(),
            quote: f["quote"].as_str().unwrap_or("").to_owned(),
            message: f["message"].as_str().unwrap_or("").to_owned(),
            confidence: f["confidence"].as_f64().unwrap_or(0.0),
        })
        .collect()
}

/// The body of a view (pure).
pub fn ai_review_body(view: AiReviewView) -> AiReviewBody {
    let empty = |state: &str, note: Option<&str>| AiReviewAdviceBody {
        state: state.to_owned(),
        label: ADVICE_LABEL.to_owned(),
        note: note.map(str::to_owned),
        stale: false,
        head_sha: None,
        summary: None,
        findings: Vec::new(),
        deep_review: None,
        degraded: None,
        input_class: None,
        advice_id: None,
        model: None,
        prompt_version: None,
        created_at: None,
    };
    let advice = match view.advice {
        AdviceState::None => empty("none", Some(NO_ADVICE_JA)),
        AdviceState::Shown {
            label,
            advice,
            stale,
        } => AiReviewAdviceBody {
            note: None,
            stale,
            head_sha: label.head_sha,
            summary: Some(advice.summary),
            findings: findings(&advice.findings),
            deep_review: advice.deep_review,
            degraded: Some(advice.degraded),
            input_class: Some(advice.input_class.as_str().to_owned()),
            advice_id: Some(label.id),
            model: Some(label.model),
            prompt_version: Some(label.prompt_version),
            created_at: Some(label.created_at),
            ..empty("shown", None)
        },
        AdviceState::Withheld {
            label,
            input_class,
            stale,
        } => AiReviewAdviceBody {
            stale,
            head_sha: label.head_sha,
            input_class: Some(input_class.as_str().to_owned()),
            advice_id: Some(label.id),
            model: Some(label.model),
            prompt_version: Some(label.prompt_version),
            created_at: Some(label.created_at),
            ..empty("withheld", Some(WITHHELD_JA))
        },
    };
    AiReviewBody {
        iid: view.iid,
        mode: view.mode.as_str().to_owned(),
        head_sha: view.head_sha,
        check: view.check.map(check_body),
        advice,
    }
}

/// `GET …/merge-requests/{iid}/ai-review` → `200` (module docs).
pub async fn ai_review(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let audience = crate::routes::context::audience(&request);
    let w = request.into_write();
    let built = async {
        let declared_agent = audience?.is_agent();
        let view = bounded(
            &state,
            "merge_request_ai_review",
            review(&state)?.ai_review(&w.proof, declared_agent),
        )
        .await?;
        json(ai_review_body(view))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
