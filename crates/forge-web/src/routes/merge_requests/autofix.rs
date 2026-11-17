//! What the deterministic autofix did with one merge request (M4 C14;
//! plan 08 "one line: what was fixed automatically / why it was refused"):
//!
//! - `GET …/merge-requests/{iid}/autofix` (`MergeRequestRead`: as the
//!   repository browse API) → `200`: whether the project's autofix is on,
//!   and the merge request's latest attempt (`null`: none) — its trigger,
//!   fixer, state (`queued`, `running`, `applied`, `no_change`, `refused`,
//!   `proposed`, `failed`, `skipped`, `superseded`), a stable reason, the
//!   paths behind a refusal or a proposal, the bot's commit, the error code
//!   and the request id of the decision.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::{MergeRequestCtx, MergeRequestEdit};
use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;

use super::loaders::ids;
use super::{respond, review};
use crate::app::AppState;
use crate::registry::{Dispatched, LoadFuture, Loader};
use crate::routes::write::{bounded, json};

/// The path after `{iid}` of [`AUTOFIX`].
pub const AUTOFIX_SUFFIX: &str = "/autofix";
/// `GET` a merge request's autofix state.
pub const AUTOFIX: &str = "/api/v1/projects/{id}/merge-requests/{iid}/autofix";

/// One path behind a refusal or a proposal.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutofixPathBody {
    pub path: String,
    /// `migration`, `binary`, `protected_path`, `outside_fixer_paths`, … .
    pub reason: String,
}

/// The merge request's latest autofix attempt.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutofixAttemptBody {
    #[schema(minimum = 1)]
    pub attempt_id: i64,
    /// `pipeline_failed` or `conflict`.
    pub trigger: String,
    /// The target branch's fixer (`null` while queued).
    #[schema(required = true)]
    pub fixer: Option<String>,
    /// `queued`, `running`, `applied`, `no_change`, `refused`, `proposed`,
    /// `failed`, `skipped` or `superseded`.
    pub state: String,
    /// A stable word (`applied`, `no_change`, `outside_fixer_paths`,
    /// `migration`, `denied`, `disabled`, `head_moved`, `job_failed`, …).
    #[schema(required = true)]
    pub reason: Option<String>,
    pub paths: Vec<AutofixPathBody>,
    /// The bot's commit on the source branch (`applied`).
    #[schema(required = true)]
    pub commit_sha: Option<String>,
    /// `FG-GIT-0037`, `FG-GIT-0038`, `FG-CI-0031`, `FG-REVIEW-0031`,
    /// `FG-AUTH-0024` (`null`: none).
    #[schema(required = true)]
    pub error_code: Option<String>,
    /// The head it started from.
    pub head_sha: String,
    /// The request id of the decision (its log lines).
    pub request_id: String,
    /// UTC, RFC 3339.
    pub updated_at: String,
}

/// A merge request's autofix.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestAutofixBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// The project's autofix switch.
    pub enabled: bool,
    #[schema(required = true)]
    pub attempt: Option<AutofixAttemptBody>,
}

/// `GET …/autofix`: the merge request as stored.
pub struct AutofixStateOf;

impl Loader<AppState> for AutofixStateOf {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path(), AUTOFIX_SUFFIX)?;
            super::loaders::lookup_ids(state, ids, viewer, MergeRequestEdit::default()).await
        })
    }
}

/// `GET …/autofix` → `200`.
pub async fn autofix_state(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let (project, iid) = *request.proof().id();
    let built = async {
        let iid = iid.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
        })?;
        let review = review(&state)?;
        let stored = bounded(
            &state,
            "merge_request_load",
            review.merge_request(project, iid),
        )
        .await?
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "written_merge_request_missing")
                .with_context("iid", iid)
        })?;
        let settings = bounded(&state, "autofix_read", review.autofix_settings(project)).await?;
        let found = bounded(&state, "autofix_state", review.autofix_state(stored.id)).await?;
        json(MergeRequestAutofixBody {
            iid,
            enabled: settings.enabled,
            attempt: found.map(|a| AutofixAttemptBody {
                attempt_id: a.id,
                trigger: a.trigger.as_str().to_owned(),
                fixer: a.fixer,
                state: a.state.as_str().to_owned(),
                reason: a.reason,
                paths: a
                    .detail
                    .into_iter()
                    .map(|d| AutofixPathBody {
                        path: d.path,
                        reason: d.reason,
                    })
                    .collect(),
                commit_sha: a
                    .commit_sha
                    .filter(|_| a.state == forge_core::autofix::AttemptState::Applied)
                    .map(|c| c.as_str().to_owned()),
                error_code: a.error_code,
                head_sha: a.head_sha.as_str().to_owned(),
                request_id: a.request_id,
                updated_at: a.updated_at,
            }),
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}
