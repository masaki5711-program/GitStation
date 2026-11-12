//! "Merge when ready" — one merge request in its target's merge queue (M4
//! C8, ADR-0030 §2, §10, §11):
//!
//! - `POST …/merge-requests/{iid}/merge-queue` (`MergeQueueEnqueue`: as a
//!   merge — people only, the target's push role, a token needs
//!   `merge_execute`): `{"sha", "method"?}` → `201` the entry. Everything
//!   a merge needs must hold now except `ci/pipeline`, which the queue
//!   proves on its train (FG-REVIEW-0007 with the other reasons); a live
//!   entry is `409 FG-CI-0025 reason=already_queued`, a live merge attempt
//!   `409 FG-REVIEW-0011`.
//! - `DELETE …/merge-queue` (`MergeQueueDequeue`): `200` the entry,
//!   `removed`; `409 FG-CI-0025 reason=not_queued` without a live entry.
//! - `GET …/merge-queue` (`MergeRequestRead`): the latest entry (`null`:
//!   never queued) and its place among the live entries.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::{MergeRequestCtx, MergeRequestEdit};
use forge_core::{MergeMethod, ObjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{CredentialRef, QueueAsk, StoredQueueEntry};
use serde::{Deserialize, Serialize};

use super::loaders::{found, ids, project_of};
use super::{respond, review, review_request};
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader};
use crate::routes::write::{absent, bounded, json};

/// The path after `{iid}` of [`QUEUE`].
pub const QUEUE_SUFFIX: &str = "/merge-queue";
/// `POST` add to the queue, `DELETE` take out, `GET` the entry.
pub const QUEUE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/merge-queue";
/// The body cap (a sha and a method).
pub const QUEUE_MAX_BODY: usize = 1024;

/// `POST …/merge-queue`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnqueueMergeRequest {
    /// The head commit the requester reviewed (required): another head is
    /// refused (FG-REVIEW-0008 stale_view).
    pub sha: String,
    /// `merge_commit`, `squash` or `fast_forward` (when the project allows
    /// it); absent: the project's default.
    pub method: Option<String>,
}

/// One merge request's entry in a merge queue (ADR-0030 §12).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueEntryBody {
    #[schema(minimum = 1)]
    pub entry_id: i64,
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `queued`, `testing`, `merged` or `removed`.
    pub state: String,
    /// 1-based among the live entries of the queue (`null` once merged or
    /// removed).
    #[schema(required = true, minimum = 1)]
    pub position: Option<i64>,
    /// The target branch without `refs/heads/`.
    pub target_branch: String,
    pub head_sha: String,
    pub method: String,
    /// Why it runs in a batch of its own: `migration`, `risk_stop`,
    /// `fast_forward` (`null`: may share a batch, or not batched yet).
    #[schema(required = true)]
    pub isolate: Option<String>,
    /// The batch testing it (`testing` only).
    #[schema(required = true, minimum = 1)]
    pub batch_id: Option<i64>,
    /// Its commit in the train (`testing`).
    #[schema(required = true)]
    pub train_sha: Option<String>,
    /// The merge attempt that landed it (`merged`).
    #[schema(required = true, minimum = 1)]
    pub merge_id: Option<i64>,
    /// Why it was removed (`dequeued`, `checks_failed`, `conflict`,
    /// `head_changed`, `stalled`, …).
    #[schema(required = true)]
    pub reason: Option<String>,
    /// The code of a removal by the queue (FG-CI-0023, FG-CI-0024, …).
    #[schema(required = true)]
    pub error_code: Option<String>,
    #[schema(minimum = 1)]
    pub enqueued_by: i64,
    /// The request that queued it.
    pub request_id: String,
    pub created_at: String,
    #[schema(required = true)]
    pub finished_at: Option<String>,
}

/// `GET …/merge-queue`: the latest entry (`null`: never queued).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueEntryStatusBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    #[schema(required = true)]
    pub entry: Option<QueueEntryBody>,
}

/// An entry as answered (`position` from the read, when live).
pub fn entry_body(e: &StoredQueueEntry, position: Option<i64>) -> QueueEntryBody {
    let target = String::new();
    QueueEntryBody {
        entry_id: e.id,
        iid: e.iid,
        state: e.state.as_str().to_owned(),
        position,
        target_branch: target,
        head_sha: e.head_sha.as_str().to_owned(),
        method: e.method.as_str().to_owned(),
        isolate: e.isolate.map(|i| i.as_str().to_owned()),
        batch_id: e.batch_id,
        train_sha: e.train_sha.as_ref().map(|t| t.as_str().to_owned()),
        merge_id: e.merge_id,
        reason: e.reason.clone(),         // clone: the row is borrowed
        error_code: e.error_code.clone(), // clone: as above
        enqueued_by: e.enqueued_by.get(),
        request_id: e.request_id.as_str().to_owned(),
        created_at: e.created_at.clone(),   // clone: as above
        finished_at: e.finished_at.clone(), // clone: as above
    }
}

/// The entry with its merge request's target branch.
fn with_target(mut body: QueueEntryBody, target: &forge_core::RefName) -> QueueEntryBody {
    body.target_branch = target.branch().unwrap_or(target.as_str()).to_owned();
    body
}

/// The merge request about to be queued (or dequeued), with its target's
/// protection (decided like a merge).
async fn queue_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<crate::registry::Lookup<MergeRequestCtx>, ForgeError> {
    let (id, iid) = ids(parts.uri.path(), QUEUE_SUFFIX)?;
    let Some((project, record)) = project_of(state, id, viewer).await? else {
        return Ok(absent());
    };
    let review = review(state)?;
    let stored = bounded(state, "merge_request_load", review.merge_request(id, iid)).await?;
    let Some(stored) = stored else {
        return Ok(absent());
    };
    let protection = bounded(
        state,
        "merge_target_protection",
        review.target_protection(id, &stored.target_branch),
    )
    .await?;
    let mr = forge_review::loaded_merge_request(&stored);
    Ok(found(
        MergeRequestCtx::merging(project, mr, protection),
        record,
    ))
}

/// `POST …/merge-queue`: the merge request and its target's protection.
pub struct MergeRequestToQueue;

impl BodyLoader<AppState, EnqueueMergeRequest> for MergeRequestToQueue {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a EnqueueMergeRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(queue_lookup(state, parts, viewer))
    }
}

/// `DELETE …/merge-queue`: as `POST`.
pub struct MergeRequestToDequeue;

impl Loader<AppState> for MergeRequestToDequeue {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(queue_lookup(state, parts, viewer))
    }
}

/// `GET …/merge-queue`: the merge request as stored.
pub struct QueueEntryOf;

impl Loader<AppState> for QueueEntryOf {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path(), QUEUE_SUFFIX)?;
            super::loaders::lookup_ids(state, ids, viewer, MergeRequestEdit::default()).await
        })
    }
}

fn ask_of(body: &EnqueueMergeRequest) -> Result<QueueAsk, ForgeError> {
    let sha = ObjectId::parse(&body.sha).map_err(|_| param_invalid("sha", "not_an_object_id"))?;
    let method = body
        .method
        .as_deref()
        .map(|m| MergeMethod::parse(m).ok_or_else(|| param_invalid("method", "unknown_value")))
        .transpose()?;
    Ok(QueueAsk { sha, method })
}

/// `POST …/merge-queue` → `201` the entry (`queued`).
pub async fn enqueue(
    state: AppState,
    request: Dispatched<MergeRequestCtx, EnqueueMergeRequest>,
) -> Response {
    let repo = super::repository(&request);
    let session = request.session().map(|s| s.session_id);
    let w = request.into_write();
    let built = async {
        let credential = match (w.credential_id, session) {
            (Some(token), _) => CredentialRef::Token(token),
            (None, Some(session)) => CredentialRef::Session(session),
            (None, None) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "enqueue_without_credential"))
            }
        };
        let ask = ask_of(&w.input)?;
        let review = review(&state)?;
        let entry = review
            .enqueue(&review_request(&w), credential, &w.proof, repo?, &ask)
            .await?;
        let position = review
            .merge_queue_entry(entry.merge_request)
            .await?
            .and_then(|(_, p)| p);
        let target = review
            .merge_request(entry.project_id, entry.iid)
            .await?
            .map(|m| m.target_branch);
        let body = entry_body(&entry, position);
        json(match &target {
            Some(t) => with_target(body, t),
            None => body,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `DELETE …/merge-queue` → `200` the entry (`removed`).
pub async fn dequeue(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let review = review(&state)?;
        let entry = review.dequeue(&review_request(&w), &w.proof).await?;
        let target = review
            .merge_request(entry.project_id, entry.iid)
            .await?
            .map(|m| m.target_branch);
        let body = entry_body(&entry, None);
        json(match &target {
            Some(t) => with_target(body, t),
            None => body,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET …/merge-queue` → `200` the latest entry.
pub async fn entry(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
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
        let found = bounded(
            &state,
            "merge_queue_entry",
            review.merge_queue_entry(stored.id),
        )
        .await?;
        json(QueueEntryStatusBody {
            iid,
            entry: found
                .map(|(e, position)| with_target(entry_body(&e, position), &stored.target_branch)),
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}
