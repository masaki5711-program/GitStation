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
//!
//! `POST …/ai-review/feedback` (M5 L5-6, plan 19 §4, §8.2): a person's
//! 「役に立った / 誤り」 on one finding (`AiAdviceFeedback`: whoever reads
//! the merge request, Reporter and above, people only), one per person and
//! finding — a new verdict replaces theirs. Decided again on the advice's
//! own subject (`Review::ai_review_feedback`). The read says whether the
//! reader may vote (`can_feedback`) and what they said (`my_feedback`).
//!
//! `POST …/ai-review/rerun` (plan 19 §8.2, `AiReviewRerun`: Developer and
//! above, people only — an agent's token, act-as and a request declaring
//! `Forge-Context-Audience: agent` are refused): the worker asks the
//! current head again on its next pass. `queued` (recorded and counted,
//! `remaining` of 3 an hour), `pending` (the head still waits: nothing
//! written); closed, without a head or `llm.review = off`: 409
//! `FG-LLM-0008`; a fourth in an hour: 429 `FG-WEB-0010` with
//! `Retry-After`. The read says whether the reader may (`can_rerun`).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::MergeRequestCtx;
use forge_core::checks::CheckRun;
use forge_core::llm::Verdict;
use forge_core::UserId;
use forge_review::ai_review::{AdviceState, AiReviewView, ADVICE_LABEL, WITHHELD_JA};
use serde::{Deserialize, Serialize};

use super::loaders::MergeRequestRead;
use super::review;
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader};
use crate::routes::write::{bounded, json, no_store};

/// The path after `{iid}` of [`AI_REVIEW`].
pub const AI_REVIEW_SUFFIX: &str = "/ai-review";
/// `GET` the AI first pass of a merge request.
pub const AI_REVIEW: &str = "/api/v1/projects/{id}/merge-requests/{iid}/ai-review";
/// The path after `{iid}` of [`AI_FEEDBACK`].
pub const AI_FEEDBACK_SUFFIX: &str = "/ai-review/feedback";
/// `POST` a person's verdict on one finding of the AI first pass.
pub const AI_FEEDBACK: &str = "/api/v1/projects/{id}/merge-requests/{iid}/ai-review/feedback";
/// The vote's body is three small fields.
pub const AI_FEEDBACK_MAX_BODY: usize = 1024;
/// `POST …/ai-review/feedback`: the merge request, loaded as for a read.
pub const AI_FEEDBACK_OF: MergeRequestRead = MergeRequestRead(AI_FEEDBACK_SUFFIX);
/// The path after `{iid}` of [`AI_RERUN`].
pub const AI_RERUN_SUFFIX: &str = "/ai-review/rerun";
/// `POST`: ask the AI first pass of the current head again.
pub const AI_RERUN: &str = "/api/v1/projects/{id}/merge-requests/{iid}/ai-review/rerun";
/// `POST …/ai-review/rerun`: the merge request, loaded as for a read.
pub const AI_RERUN_OF: MergeRequestRead = MergeRequestRead(AI_RERUN_SUFFIX);
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

/// One of the reader's own verdicts.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiReviewMyFeedbackBody {
    /// The finding (0-based, as in `findings`).
    #[schema(minimum = 0, maximum = 255)]
    pub finding_index: i64,
    /// `useful` or `wrong`.
    pub verdict: String,
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
    /// The reader's own verdicts on the findings (M5 L5-6; empty for
    /// `none`, `withheld` and a reader who may not vote).
    pub my_feedback: Vec<AiReviewMyFeedbackBody>,
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
    /// The reader may vote 「役に立った / 誤り」 on the findings (a person
    /// who reads the merge request, Reporter and above, never an agent or
    /// a read-only token). A hint: the vote decides again.
    pub can_feedback: bool,
    /// The reader may ask the first pass of the current head again now
    /// (plan 19 §8.2: a Developer and above, a person; the merge request
    /// open, `llm.review` on, its run completed and no rerun waiting). A
    /// hint: the rerun decides again and allows 3 an hour.
    pub can_rerun: bool,
    /// Someone asked the current head again and the worker has not picked
    /// it up yet (about 30 s): the screen says so and reads again.
    pub rerun_asked: bool,
}

/// `POST …/ai-review/feedback`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAdviceVote {
    /// The advice (`advice.advice_id` of the read).
    #[schema(minimum = 1)]
    pub advice_id: i64,
    /// The finding (0-based, as in `advice.findings`).
    #[schema(minimum = 0, maximum = 255)]
    pub finding_index: i64,
    /// `useful` (役に立った) or `wrong` (誤り).
    pub verdict: String,
}

/// What a vote did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAdviceVoteBody {
    #[schema(minimum = 1)]
    pub advice_id: i64,
    #[schema(minimum = 0, maximum = 255)]
    pub finding_index: i64,
    /// `useful` or `wrong`: the verdict now.
    pub verdict: String,
    /// `recorded` (new), `changed` (the person's earlier verdict replaced)
    /// or `unchanged` (the same verdict was there).
    pub outcome: String,
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
        my_feedback: Vec::new(),
    };
    let my_feedback: Vec<AiReviewMyFeedbackBody> = view
        .my_feedback
        .iter()
        .map(|(index, verdict)| AiReviewMyFeedbackBody {
            finding_index: i64::from(*index),
            verdict: verdict.as_str().to_owned(),
        })
        .collect();
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
            my_feedback,
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
        can_feedback: view.can_feedback,
        can_rerun: view.can_rerun,
        rerun_asked: view.rerun_asked,
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

impl BodyLoader<AppState, AiAdviceVote> for MergeRequestRead {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a AiAdviceVote,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Loader::load(self, state, parts, viewer)
    }
}

/// The vote's fields, checked (`FG-WEB-0006 reason=param_invalid`).
fn vote_of(input: &AiAdviceVote) -> Result<(i64, u8, Verdict), forge_errors::ForgeError> {
    if input.advice_id < 1 {
        return Err(param_invalid("advice_id", "positive"));
    }
    let finding =
        u8::try_from(input.finding_index).map_err(|_| param_invalid("finding_index", "0..=255"))?;
    let verdict = Verdict::parse(&input.verdict)
        .ok_or_else(|| param_invalid("verdict", "useful_or_wrong"))?;
    Ok((input.advice_id, finding, verdict))
}

/// `POST …/ai-review/feedback` → `200` what the vote did (module docs).
pub async fn ai_review_feedback(
    state: AppState,
    request: Dispatched<MergeRequestCtx, AiAdviceVote>,
) -> Response {
    let audience = crate::routes::context::audience(&request);
    let w = request.into_write();
    let built = async {
        let declared_agent = audience?.is_agent();
        let (advice_id, finding, verdict) = vote_of(&w.input)?;
        let outcome = bounded(
            &state,
            "merge_request_ai_feedback",
            review(&state)?.ai_review_feedback(
                &w.proof,
                declared_agent,
                advice_id,
                finding,
                verdict,
            ),
        )
        .await?;
        json(AiAdviceVoteBody {
            advice_id,
            finding_index: i64::from(finding),
            verdict: verdict.as_str().to_owned(),
            outcome: outcome.as_str().to_owned(),
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/ai-review/rerun`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiReviewRerunBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `queued` (the worker asks the head again on its next pass, within
    /// about 30 s) or `pending` (the head still waits for its pass: nothing
    /// was recorded or counted).
    pub outcome: String,
    /// The head that will be reviewed.
    pub head_sha: String,
    /// Reruns left in this hour after a `queued` one (`null`: `pending`).
    #[schema(required = true, minimum = 0, maximum = 2)]
    pub remaining: Option<i64>,
}

/// `POST …/ai-review/rerun` → `200` (module docs).
pub async fn ai_review_rerun(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let audience = crate::routes::context::audience(&request);
    let w = request.into_write();
    let built = async {
        let declared_agent = audience?.is_agent();
        let req = forge_review::ReviewRequest {
            request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
            credential_id: w.credential_id,
        };
        let iid = w.proof.id().1.ok_or_else(|| {
            forge_errors::ForgeError::new(forge_errors::ErrorCode::Internal)
                .with_context("reason", "proof_without_iid")
        })?;
        // The change has its own timeout (`ChangeTx`).
        let rerun = review(&state)?
            .ai_review_rerun(&req, &w.proof, declared_agent)
            .await?;
        let body = match rerun {
            forge_review::AiReviewRerun::Queued {
                head_sha,
                remaining,
            } => AiReviewRerunBody {
                iid,
                outcome: "queued".to_owned(),
                head_sha,
                remaining: Some(remaining),
            },
            forge_review::AiReviewRerun::Pending { head_sha } => AiReviewRerunBody {
                iid,
                outcome: "pending".to_owned(),
                head_sha,
                remaining: None,
            },
        };
        json(body)
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
