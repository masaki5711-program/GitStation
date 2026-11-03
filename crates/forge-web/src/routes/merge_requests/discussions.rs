//! The discussions of a merge request (M3 R3, ADR-0024 §6, §12):
//!
//! - `GET …/merge-requests/{iid}/discussions` (`MergeRequestRead`) — every
//!   thread with its notes, oldest first, and `outdated` (a later version
//!   rewrote the line; computed when read);
//! - `POST …/{iid}/discussions` (`MergeRequestComment`) — start one,
//!   general or inline (`position`: a version, a path, a side and a line of
//!   that version's diff; FG-REVIEW-0004 otherwise);
//! - `POST …/{iid}/discussions/{discussion_id}/notes`
//!   (`MergeRequestComment`) — reply;
//! - `POST …/{iid}/discussions/{discussion_id}/resolve` / `…/unresolve`
//!   (`DiscussionResolve`, decided on the discussion: people only; the
//!   starter, or a Developer who is not the merge request's author).
//!
//! Note bodies are plain text / Markdown, stored and returned as written —
//! the server never renders them (the client renders Markdown safely).
//! A discussion of another merge request (or none) is the same
//! `FG-REPO-0005` as a missing merge request.

use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::{DiscussionCtx, MergeRequestCtx};
use forge_core::{ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{DiscussionView, StoredDiscussion};
use serde::{Deserialize, Serialize};

use super::discussion_loaders::{discussion_ids, position_of};
use super::versions::{merge_request, proof_ids};
use super::{repository, respond, review, review_request};
use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::browse::query;
use crate::routes::write::json;

/// The path after `{iid}` of [`DISCUSSIONS`].
pub const DISCUSSIONS_SUFFIX: &str = "/discussions";
/// `GET` the discussions / `POST` start one.
pub const DISCUSSIONS: &str = "/api/v1/projects/{id}/merge-requests/{iid}/discussions";
/// `POST` reply to one.
pub const NOTES: &str =
    "/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/notes";
/// `POST` resolve one.
pub const RESOLVE: &str =
    "/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/resolve";
/// `POST` unresolve one.
pub const UNRESOLVE: &str =
    "/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/unresolve";

/// The body cap of the discussion routes: the registry's JSON maximum
/// (64 KiB). A note is at most 60 KiB (`forge_review::MAX_NOTE_BYTES`) so
/// it fits with its envelope and a position; only a note whose JSON
/// escaping more than doubles part of it can pass 64 KiB (`FG-WEB-0009`).
pub const MAX_BODY: usize = crate::body::MAX_JSON_BODY_BYTES;

/// Where an inline discussion sits: a line of one side of one version's
/// diff (`GET …/versions/{version}/diff`).
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PositionBody {
    /// The version's number.
    #[schema(minimum = 1)]
    pub version: i64,
    /// The file's path on that side (as the diff lists it).
    pub path: String,
    /// `old` (the version's merge base) or `new` (its head).
    pub side: String,
    /// A line inside a hunk of that file on that side (1-based).
    #[schema(minimum = 1)]
    pub line: i64,
}

/// `POST …/discussions`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StartDiscussion {
    /// The first note: plain text / Markdown, 1 character to 60 KiB.
    pub body: String,
    /// Absent: a general discussion on the merge request.
    pub position: Option<PositionBody>,
}

/// `POST …/discussions/{discussion_id}/notes`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddNote {
    /// Plain text / Markdown, 1 character to 60 KiB.
    pub body: String,
}

/// One note of a discussion.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub author_id: i64,
    /// The author's names (read with the note).
    pub author: super::UserNameBody,
    /// As written (never rendered by the server).
    pub body: String,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// One discussion of a merge request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscussionBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// `general` or `inline`.
    pub kind: String,
    /// Set exactly for an inline discussion.
    #[schema(required = true)]
    pub position: Option<PositionBody>,
    /// A later version rewrote (or moved) the line — or whether it did
    /// could not be computed (`outdated_unknown`): look again.
    pub outdated: bool,
    /// `outdated` could not be computed (git busy or failed, a purged
    /// version, unrecorded bases, too many diffs for one read): it is
    /// shown as `true`.
    pub outdated_unknown: bool,
    #[schema(minimum = 1)]
    pub started_by: i64,
    /// The names of who started it (`started_by`, the first note's author).
    pub author: super::UserNameBody,
    pub resolved: bool,
    #[schema(required = true)]
    pub resolved_by: Option<i64>,
    #[schema(required = true)]
    pub resolved_at: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// Oldest first.
    pub notes: Vec<NoteBody>,
}

/// The discussions of a merge request, oldest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscussionsBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    pub discussions: Vec<DiscussionBody>,
    /// More than 500 discussions or 5 000 notes: the rest are not listed.
    pub truncated: bool,
}

fn body_of(view: DiscussionView) -> DiscussionBody {
    let d = view.discussion;
    DiscussionBody {
        id: d.id.get(),
        kind: d.kind.as_str().to_owned(),
        position: d.position.map(|p| PositionBody {
            version: p.version,
            path: p.path,
            side: p.side.as_str().to_owned(),
            line: p.line,
        }),
        outdated: view.outdated.shown(),
        outdated_unknown: view.outdated.is_unknown(),
        started_by: d.started_by.get(),
        author: d.started_by_name.into(),
        resolved: d.resolved,
        resolved_by: d.resolved_by.map(UserId::get),
        resolved_at: d.resolved_at,
        created_at: d.created_at,
        notes: d
            .notes
            .into_iter()
            .map(|n| NoteBody {
                id: n.id.get(),
                author_id: n.author.get(),
                author: n.author_name.into(),
                body: n.body,
                created_at: n.created_at,
            })
            .collect(),
    }
}

// ── handlers ──

/// The caller's key (`user:<id>`) for the browse limits.
fn key_of(user: Option<UserId>) -> Result<String, ForgeError> {
    user.map(|u| format!("user:{}", u.get())).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "discussion_route_without_user")
    })
}

/// A discussion a write just committed, with `outdated` as the list shows
/// it. Never an error (review round 1): the write is stored, so a failure
/// here must not make the client retry and duplicate it — the thread is
/// shown `outdated_unknown` and the failure is logged once.
async fn shown(
    state: &AppState,
    (project, iid): (ProjectId, i64),
    repo: forge_core::ProjectRepository,
    key: &str,
    request_id: &forge_core::RequestId,
    discussion: StoredDiscussion,
) -> DiscussionBody {
    let read = match review(state) {
        Ok(review) => merge_request(state, project, iid)
            .await
            .map(|mr| (review, mr)),
        Err(e) => Err(e),
    };
    let view = match read {
        Ok((review, mr)) => review.view(&mr, repo, key, request_id, discussion).await,
        Err(e) => {
            tracing::warn!(
                target: "forge_web",
                event = "forge.review.discussion_outdated_unknown",
                request_id = %request_id,
                project_id = project.get(),
                iid,
                reason = "merge_request_unreadable",
                error_code = e.code().id(),
                "discussion outdated state could not be computed after the write"
            );
            let outdated = if discussion.position.is_some() {
                forge_review::Outdated::Unknown
            } else {
                forge_review::Outdated::No
            };
            DiscussionView {
                discussion,
                outdated,
            }
        }
    };
    body_of(view)
}

/// `GET …/discussions` → `200` the discussions.
pub async fn list(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    let repo = repository(&request);
    let key = key_of(request.principal().user());
    let request_id = request.ctx().request_id.clone(); // clone: owned before the first await
                                                       // Owned before the first await: the request (its body) is not `Sync`.
    let params = query(request.raw_query(), &[]).map(drop);
    let built = async {
        let (project, iid) = ids?;
        params?;
        let mr = merge_request(&state, project, iid).await?;
        let list = review(&state)?
            .discussions(&mr, repo?, &key?, &request_id)
            .await?;
        json(DiscussionsBody {
            iid,
            discussions: list.discussions.into_iter().map(body_of).collect(),
            truncated: list.truncated,
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `POST …/discussions` → `201` the new discussion.
pub async fn start(
    state: AppState,
    request: Dispatched<MergeRequestCtx, StartDiscussion>,
) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let (project, iid) = match *w.proof.id() {
            (project, Some(iid)) => (project, iid),
            (_, None) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "proof_without_iid"))
            }
        };
        let key = key_of(w.proof.principal().user())?;
        let position = w.input.position.as_ref().map(position_of).transpose()?;
        let repo = repo?;
        let stored = review(&state)?
            .start_discussion(
                &review_request(&w),
                &w.proof,
                repo,
                &w.input.body,
                position.as_ref(),
            )
            .await?;
        json(
            shown(
                &state,
                (project, iid),
                repo,
                &key,
                &w.ctx.request_id,
                stored,
            )
            .await,
        )
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `POST …/discussions/{discussion_id}/notes` → `201` the discussion with
/// the new note.
pub async fn reply(state: AppState, request: Dispatched<MergeRequestCtx, AddNote>) -> Response {
    let repo = repository(&request);
    let discussion = discussion_ids(request.raw_path(), "/notes").map(|(_, _, d)| d);
    let w = request.into_write();
    let built = async {
        let (project, iid) = match *w.proof.id() {
            (project, Some(iid)) => (project, iid),
            (_, None) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "proof_without_iid"))
            }
        };
        let key = key_of(w.proof.principal().user())?;
        let repo = repo?;
        let stored = review(&state)?
            .reply(&review_request(&w), &w.proof, discussion?, &w.input.body)
            .await?;
        json(
            shown(
                &state,
                (project, iid),
                repo,
                &key,
                &w.ctx.request_id,
                stored,
            )
            .await,
        )
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

async fn set_resolved(
    state: AppState,
    request: Dispatched<DiscussionCtx>,
    resolved: bool,
) -> Response {
    let repo = request.project().map(|r| r.repository).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "discussion_without_project_row")
    });
    let w = request.into_write();
    let built = async {
        let (project, iid) = match *w.proof.id() {
            (project, Some(iid), _) => (project, iid),
            (_, None, _) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "proof_without_iid"))
            }
        };
        let key = key_of(w.proof.principal().user())?;
        let repo = repo?;
        let request = forge_review::ReviewRequest {
            request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
            credential_id: w.credential_id,
        };
        let (_, stored) = review(&state)?
            .resolve(&request, &w.proof, resolved)
            .await?;
        json(
            shown(
                &state,
                (project, iid),
                repo,
                &key,
                &w.ctx.request_id,
                stored,
            )
            .await,
        )
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/discussions/{discussion_id}/resolve` → `200` the discussion
/// (also when it was already resolved).
pub async fn resolve(state: AppState, request: Dispatched<DiscussionCtx>) -> Response {
    set_resolved(state, request, true).await
}

/// `POST …/discussions/{discussion_id}/unresolve` → `200` the discussion
/// (also when it was not resolved).
pub async fn unresolve(state: AppState, request: Dispatched<DiscussionCtx>) -> Response {
    set_resolved(state, request, false).await
}
