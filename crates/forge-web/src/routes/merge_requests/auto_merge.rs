//! What auto-merge did with one merge request (M4 C9; plan 08 "one line:
//! why it stopped and what to press next"):
//!
//! - `GET …/merge-requests/{iid}/auto-merge` (`MergeRequestRead`: as the
//!   repository browse API) → `200`: the project's mode, the emergency stop
//!   in effect, and the merge request's last auto-merge state (`null`: none
//!   — its project does not use auto-merge, or the worker has not looked
//!   yet): `queued` / `waiting` / `held` / `stopped`, the stop class, a
//!   stable reason, the forge/risk rule or check behind it, and the error
//!   code (`FG-REVIEW-0026` / `FG-REVIEW-0027`).

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

/// The path after `{iid}` of [`AUTO_MERGE`].
pub const AUTO_MERGE_SUFFIX: &str = "/auto-merge";
/// `GET` a merge request's auto-merge state.
pub const AUTO_MERGE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/auto-merge";

/// The merge request's last auto-merge state.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoMergeStateBody {
    /// `queued`, `waiting`, `held` or `stopped`.
    pub state: String,
    /// The stop class (`stopped` only): `program_breakage`, `data_loss`,
    /// `leak`, `developer_pc`, `not_assessed`, `emergency_stop`.
    #[schema(required = true)]
    pub class: Option<String>,
    /// A stable word: `risk_stop`, `required_check_failed`,
    /// `pipeline_failed`, `merge_queue_checks_failed`, `instance_stopped`,
    /// `project_stopped`, `draft`, `risk_pending`, `approvals_missing`,
    /// `protected_path`, `dequeued_by_person`, `queued`, … .
    pub reason: String,
    /// The forge/risk rule or the check behind it.
    #[schema(required = true)]
    pub detail: Option<String>,
    /// `FG-REVIEW-0026` (a stop) or `FG-REVIEW-0027` (an emergency stop).
    #[schema(required = true)]
    pub error_code: Option<String>,
    /// The head it was decided on.
    pub head_sha: String,
    /// The request id of the decision (the log line of a stop).
    pub request_id: String,
    /// UTC, RFC 3339: when it last changed / was last looked at.
    pub changed_at: String,
    pub checked_at: String,
}

/// A merge request's auto-merge.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestAutoMergeBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// The project's `merge.auto`.
    pub mode: String,
    /// The auto-merge principal acts in this mode.
    pub acts: bool,
    /// The emergency stop in effect: `instance`, `project` (`null`: none).
    #[schema(required = true)]
    pub stop: Option<String>,
    #[schema(required = true)]
    pub auto_merge: Option<AutoMergeStateBody>,
}

/// `GET …/auto-merge`: the merge request as stored.
pub struct AutoMergeStateOf;

impl Loader<AppState> for AutoMergeStateOf {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path(), AUTO_MERGE_SUFFIX)?;
            super::loaders::lookup_ids(state, ids, viewer, MergeRequestEdit::default()).await
        })
    }
}

/// `GET …/auto-merge` → `200`.
pub async fn auto_merge_state(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
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
        let settings = bounded(
            &state,
            "auto_merge_read",
            review.auto_merge_settings(project),
        )
        .await?;
        let found = bounded(
            &state,
            "auto_merge_state",
            review.auto_merge_state(stored.id),
        )
        .await?;
        let current_head = stored.head_sha.as_deref();
        json(MergeRequestAutoMergeBody {
            iid,
            mode: settings.mode.as_str().to_owned(),
            acts: settings.mode.acts(),
            stop: settings.stop().map(|s| s.as_str().to_owned()),
            // A state of another head is history: the worker looks again.
            auto_merge: found
                .filter(|s| settings.mode.acts() && Some(s.head_sha.as_str()) == current_head)
                .map(|s| AutoMergeStateBody {
                    state: s.state,
                    class: s.class.map(|c| c.as_str().to_owned()),
                    reason: s.reason,
                    detail: s.detail,
                    error_code: s.error_code,
                    head_sha: s.head_sha.as_str().to_owned(),
                    request_id: s.request_id,
                    changed_at: s.changed_at,
                    checked_at: s.checked_at,
                }),
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}
