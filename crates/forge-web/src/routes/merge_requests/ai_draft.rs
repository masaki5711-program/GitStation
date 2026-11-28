//! `GET …/merge-requests/{iid}/ai-draft` (plan 17 P17-10, ADR-0039
//! Amendment P17-10b): whether the local LLM proposed a fix of an AI asset
//! beside this draft improvement merge request — the branch `ai-draft/<id>`
//! and its commit, the asset paths it changes, how many of the model's
//! edits the checks dropped, and whether the branch is still at that
//! commit. Labelled "AI の下書き（提案）". Read like the merge request
//! (`MergeRequestRead`); the facts are code-made (the worker's record of
//! its own commit), never the model's words. Reading writes nothing.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::MergeRequestCtx;
use forge_review::ai_draft::{AiDraftView, AI_DRAFT_LABEL};
use serde::Serialize;

use super::loaders::MergeRequestRead;
use super::review;
use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::write::{bounded, json, no_store};

/// The path after `{iid}` of [`AI_DRAFT`].
pub const AI_DRAFT_SUFFIX: &str = "/ai-draft";
/// `GET` the AI asset draft of a merge request.
pub const AI_DRAFT: &str = "/api/v1/projects/{id}/merge-requests/{iid}/ai-draft";
/// `GET …/ai-draft`: the merge request, loaded as for a read.
pub const AI_DRAFT_OF: MergeRequestRead = MergeRequestRead(AI_DRAFT_SUFFIX);

/// The proposed draft.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiDraftDetailBody {
    /// The branch's short name (`ai-draft/<id>`).
    pub branch: String,
    /// The draft commit (one commit over `parent_sha`).
    pub commit_sha: String,
    /// The merge request's head the draft was made over.
    pub parent_sha: String,
    /// The asset paths the draft changes (at most 3).
    pub files: Vec<String>,
    /// The model's edits the output checks dropped (not in the commit).
    #[schema(minimum = 0)]
    pub dropped_edits: i64,
    /// The advice the draft came from.
    #[schema(minimum = 1)]
    pub advice_id: i64,
    /// UTC.
    pub proposed_at: String,
    /// `current` (the branch is at the draft commit), `moved` (someone
    /// pushed to it since) or `deleted` (the branch is gone).
    pub branch_state: String,
    /// The branch's head now (`null`: deleted).
    #[schema(required = true)]
    pub branch_head: Option<String>,
}

/// `GET …/merge-requests/{iid}/ai-draft`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiDraftBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// "AI の下書き（提案） / AI draft (suggestion)".
    pub label: String,
    /// The merge request comes from `improve/<id>` (only those get a
    /// draft).
    pub improvement: bool,
    /// `none` (no draft was proposed), `proposed`, or `unrecorded` (the
    /// draft branch exists but no proposal is recorded: see `branch`).
    pub state: String,
    /// For `unrecorded`: the draft branch's short name (`null` otherwise).
    #[schema(required = true)]
    pub branch: Option<String>,
    /// The draft (`null` for `none`).
    #[schema(required = true)]
    pub draft: Option<AiDraftDetailBody>,
}

/// The body of a view (pure).
pub fn ai_draft_body(view: AiDraftView) -> AiDraftBody {
    let short = |b: &str| b.strip_prefix("refs/heads/").unwrap_or(b).to_owned();
    let draft = view.draft.map(|shown| {
        let d = shown.draft;
        AiDraftDetailBody {
            branch: short(&d.branch),
            commit_sha: d.commit_sha,
            parent_sha: d.parent_sha,
            files: d.files,
            dropped_edits: d.dropped_edits,
            advice_id: d.advice_id,
            proposed_at: d.proposed_at,
            branch_state: shown.branch_state.as_str().to_owned(),
            branch_head: shown.branch_head,
        }
    });
    AiDraftBody {
        iid: view.iid,
        label: AI_DRAFT_LABEL.to_owned(),
        improvement: view.improvement,
        state: match (&draft, &view.unrecorded_branch) {
            (Some(_), _) => "proposed",
            (None, Some(_)) => "unrecorded",
            (None, None) => "none",
        }
        .to_owned(),
        branch: view.unrecorded_branch.as_deref().map(short),
        draft,
    }
}

/// `GET …/merge-requests/{iid}/ai-draft` → `200` (module docs).
pub async fn ai_draft(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let view = bounded(
            &state,
            "merge_request_ai_draft",
            review(&state)?.ai_draft(&w.proof),
        )
        .await?;
        json(ai_draft_body(view))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
