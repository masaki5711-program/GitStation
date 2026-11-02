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

/// The body cap of the discussion routes: the registry's JSON maximum (as
/// the merge request routes: a note near its 64 KiB cap that JSON escaping
/// pushes past it is refused as too large, `FG-WEB-0009`).
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
    /// The first note: plain text / Markdown, 1 character to 64 KiB.
    pub body: String,
    /// Absent: a general discussion on the merge request.
    pub position: Option<PositionBody>,
}

/// `POST …/discussions/{discussion_id}/notes`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddNote {
    /// Plain text / Markdown, 1 character to 64 KiB.
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
    /// A later version rewrote (or moved) the line: look again.
    pub outdated: bool,
    #[schema(minimum = 1)]
    pub started_by: i64,
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
        outdated: view.outdated,
        started_by: d.started_by.get(),
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

/// `discussion` with `outdated`, as the list shows it.
async fn shown(
    state: &AppState,
    project: ProjectId,
    iid: i64,
    repo: Result<forge_core::ProjectRepository, ForgeError>,
    key: &str,
    discussion: StoredDiscussion,
) -> Result<DiscussionBody, ForgeError> {
    let mr = merge_request(state, project, iid).await?;
    let view = review(state)?.view(&mr, repo?, key, discussion).await?;
    Ok(body_of(view))
}

/// `GET …/discussions` → `200` the discussions.
pub async fn list(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let ids = proof_ids(&request);
    let repo = repository(&request);
    let key = key_of(request.principal().user());
    // Owned before the first await: the request (its body) is not `Sync`.
    let params = query(request.raw_query(), &[]).map(drop);
    let built = async {
        let (project, iid) = ids?;
        params?;
        let mr = merge_request(&state, project, iid).await?;
        let list = review(&state)?.discussions(&mr, repo?, &key?).await?;
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
        json(shown(&state, project, iid, Ok(repo), &key, stored).await?)
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
        let stored = review(&state)?
            .reply(&review_request(&w), &w.proof, discussion?, &w.input.body)
            .await?;
        json(shown(&state, project, iid, repo, &key, stored).await?)
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
        let request = forge_review::ReviewRequest {
            request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
            credential_id: w.credential_id,
        };
        let (_, stored) = review(&state)?
            .resolve(&request, &w.proof, resolved)
            .await?;
        json(shown(&state, project, iid, repo, &key, stored).await?)
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
