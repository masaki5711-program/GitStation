//! Merge requests of a project (M3 R1a, ADR-0024):
//! `GET|POST /api/v1/projects/{id}/merge-requests`,
//! `GET|PATCH /api/v1/projects/{id}/merge-requests/{iid}` and
//! `POST …/merge-requests/{iid}/close`, `…/reopen`. Reading and listing are
//! decided like the repository browse API (`MergeRequestRead`,
//! `MergeRequestList`); opening is Developer and above (`MergeRequestCreate`);
//! an update, a close and a reopen are the author's or a Developer's (another's
//! draft flag only a Maintainer's) — ADR-0024 §12.
//!
//! The registry loads the project (the one `ProjectAccessLoader` call of
//! `GET /api/v1/projects/{id}`) and, for a route on a stored merge request,
//! the row by its `iid`; an invisible project and a missing `iid` are the
//! same `FG-REPO-0005` (`authorize()` on the project, or `Unresolved`).
//! Branch names are checked before anything is looked up (`FG-WEB-0006`
//! `param_invalid`: `not_a_branch`, `source_equals_target`,
//! `invalid_ref_name`; `authorize()` refuses them again). forge-review
//! decides again inside the transaction and checks that the branches exist
//! (`FG-REPO-0023`). Every answer is re-read after the commit and never
//! cached by a shared cache.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    LoadedMergeRequest, LoadedProject, MergeRequestCtx, MergeRequestEdit, ProjectCtx,
};
use forge_core::{
    parse_positive_id, MergeRequestState, ProjectId, ProjectRecord, ProjectRepository, RefName,
    UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{EditFields, NewFields, Review, ReviewRequest, StoredMergeRequest, MAX_PAGE};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, id_in, json, no_store};

/// `GET` / `POST` a project's merge requests.
pub const COLLECTION: &str = "/api/v1/projects/{id}/merge-requests";
/// `GET` / `PATCH` one merge request.
pub const ONE: &str = "/api/v1/projects/{id}/merge-requests/{iid}";
/// `POST` close one.
pub const CLOSE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/close";
/// `POST` reopen one.
pub const REOPEN: &str = "/api/v1/projects/{id}/merge-requests/{iid}/reopen";

/// The body cap of the merge request routes (a 64 KiB description,
/// JSON-escaped, and the rest).
pub const MAX_BODY: usize = 64 * 1024;

/// Merge requests per page when `limit` is absent.
pub const DEFAULT_LIMIT: usize = 20;

/// The query parameters of the list (`openapi.json`).
pub const QUERY: &[QueryParam] = &[
    QueryParam {
        name: "state",
        description: "open (default), closed, merged or all",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "Merge requests per page, 1..=100 (default 20)",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "before",
        description: "The previous page's next_before (an iid): merge requests with a smaller iid",
        integer_range: Some((1, i64::MAX)),
    },
];

/// `POST …/merge-requests`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateMergeRequest {
    /// 1..=255 characters.
    pub title: String,
    /// At most 64 KiB; empty when absent.
    #[serde(default)]
    pub description: String,
    /// A branch: `feature/x` or `refs/heads/feature/x`; it must exist.
    pub source_branch: String,
    /// A branch other than the source; it must exist.
    pub target_branch: String,
    #[serde(default)]
    pub draft: bool,
}

/// `PATCH …/merge-requests/{iid}`: every field optional (absent =
/// unchanged).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateMergeRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    /// Only the author or a Maintainer changes another's flag; only while
    /// open.
    pub draft: Option<bool>,
    /// A branch other than the source; it must exist; only while open.
    pub target_branch: Option<String>,
}

// Built only by `body_of`; its doc comment is the schema description.
/// One merge request as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The project's own number of it.
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    pub description: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    pub draft: bool,
    /// The branch name without `refs/heads/`.
    pub source_branch: String,
    pub target_branch: String,
    #[schema(minimum = 1)]
    pub author_id: i64,
    /// The head commit of the latest version (`null` until versions are
    /// recorded).
    pub head_sha: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub closed_by: Option<i64>,
}

/// One page of a project's merge requests, newest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestPage {
    pub merge_requests: Vec<MergeRequestBody>,
    /// Pass as `before` for the next page; `null` on the last page.
    pub next_before: Option<i64>,
}

fn short(name: &RefName) -> String {
    name.branch().unwrap_or(name.as_str()).to_owned()
}

fn body_of(mr: &StoredMergeRequest) -> MergeRequestBody {
    MergeRequestBody {
        project_id: mr.project_id.get(),
        iid: mr.iid,
        title: mr.title.clone(), // clone: the row is borrowed
        description: mr.description.clone(),
        state: mr.state.as_str().to_owned(),
        draft: mr.draft,
        source_branch: short(&mr.source_branch),
        target_branch: short(&mr.target_branch),
        author_id: mr.author.get(),
        head_sha: mr.head_sha.clone(),
        created_at: mr.created_at.clone(),
        updated_at: mr.updated_at.clone(),
        closed_at: mr.closed_at.clone(),
        closed_by: mr.closed_by.map(UserId::get),
    }
}

/// The review commands; `FG-SYS-0001 reason=review_unconfigured` when the
/// server was built without them (`forge serve` always sets them).
fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

/// A branch named in a request: `name` or `refs/heads/name`; any other
/// namespace is `not_a_branch` (ADR-0024 §1), a name git refuses
/// `invalid_ref_name` — `FG-WEB-0006` before anything is looked up.
fn branch_param(param: &'static str, raw: &str) -> Result<RefName, ForgeError> {
    let full = if raw.starts_with("refs/") {
        if !raw.starts_with("refs/heads/") {
            return Err(param_invalid(param, "not_a_branch"));
        }
        raw.to_owned()
    } else {
        format!("refs/heads/{raw}")
    };
    RefName::parse(&full).map_err(|_| param_invalid(param, "invalid_ref_name"))
}

/// The ids of `/api/v1/projects/{id}/merge-requests/{iid}[suffix]` (raw path).
fn ids(raw_path: &str, suffix: &str) -> Result<(ProjectId, i64), ForgeError> {
    let (id, rest) = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/merge-requests/"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let iid = rest
        .strip_suffix(suffix)
        .filter(|iid| !iid.contains('/'))
        .and_then(parse_positive_id)
        .ok_or_else(|| param_invalid("iid", "not_positive_decimal"))?;
    Ok((ProjectId::new(id), iid))
}

fn collection_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    id_in(raw_path, PREFIX, "/merge-requests")
        .map(ProjectId::new)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))
}

/// The project for `viewer` (its row too: the handler needs the
/// repository), `None` when it does not exist.
async fn project_of(
    state: &AppState,
    id: ProjectId,
    viewer: Option<UserId>,
) -> Result<Option<(LoadedProject, ProjectRecord)>, ForgeError> {
    let Some(record) = projects::load_bounded(state, id, viewer).await? else {
        return Ok(None);
    };
    if record.id != id {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_project")
            .with_context("project_id", id.get())
            .with_context("loaded_project_id", record.id.get()));
    }
    Ok(Some((projects::loaded(&record), record)))
}

fn found<R>(ctx: R, record: ProjectRecord) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: Some(record),
        group: None,
    }
}

/// A route on stored merge request `{iid}` (`edit` for an update): the
/// project and the row; either missing is the same `FG-REPO-0005`.
async fn stored_lookup(
    state: &AppState,
    parts: &Parts,
    suffix: &str,
    viewer: Option<UserId>,
    edit: MergeRequestEdit,
) -> Result<Lookup<MergeRequestCtx>, ForgeError> {
    let (id, iid) = ids(parts.uri.path(), suffix)?;
    let Some((project, record)) = project_of(state, id, viewer).await? else {
        return Ok(absent());
    };
    let stored = bounded(
        state,
        "merge_request_load",
        review(state)?.merge_request(id, iid),
    )
    .await?;
    let Some(stored) = stored else {
        return Ok(absent());
    };
    let mr = forge_review::loaded_merge_request(&stored);
    Ok(found(MergeRequestCtx::edited(project, mr, edit), record))
}

/// `GET …/merge-requests`: the project.
pub struct MergeRequestsOf;

impl Loader<AppState> for MergeRequestsOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let id = collection_id(parts.uri.path())?;
            Ok(match project_of(state, id, viewer).await? {
                Some((project, record)) => found(ProjectCtx::new(project), record),
                None => absent(),
            })
        })
    }
}

/// `POST …/merge-requests`: the project and the merge request about to
/// be opened by the caller.
pub struct NewMergeRequest;

impl BodyLoader<AppState, CreateMergeRequest> for NewMergeRequest {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a CreateMergeRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let id = collection_id(parts.uri.path())?;
            let source = branch_param("source_branch", &body.source_branch)?;
            let target = branch_param("target_branch", &body.target_branch)?;
            if source == target {
                return Err(param_invalid("target_branch", "source_equals_target"));
            }
            // Nobody signed in: decided by `authorize()` (FG-AUTH-0001).
            let Some(author) = viewer else {
                return Ok(absent());
            };
            Ok(match project_of(state, id, viewer).await? {
                Some((project, record)) => found(
                    MergeRequestCtx::new(
                        project,
                        LoadedMergeRequest {
                            iid: None,
                            author,
                            state: MergeRequestState::Open,
                            draft: body.draft,
                            source_branch: source,
                            target_branch: target,
                        },
                    ),
                    record,
                ),
                None => absent(),
            })
        })
    }
}

/// `GET …/merge-requests/{iid}`.
pub struct MergeRequestOf;

impl Loader<AppState> for MergeRequestOf {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(stored_lookup(
            state,
            parts,
            "",
            viewer,
            MergeRequestEdit::default(),
        ))
    }
}

/// `PATCH …/merge-requests/{iid}`: the row and what the body changes.
pub struct MergeRequestToUpdate;

impl BodyLoader<AppState, UpdateMergeRequest> for MergeRequestToUpdate {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a UpdateMergeRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let target = body
                .target_branch
                .as_deref()
                .map(|raw| branch_param("target_branch", raw))
                .transpose()?;
            let edit = MergeRequestEdit {
                target_branch: target,
                draft: body.draft,
            };
            stored_lookup(state, parts, "", viewer, edit).await
        })
    }
}

/// `POST …/merge-requests/{iid}/close`.
pub struct MergeRequestToClose;

impl Loader<AppState> for MergeRequestToClose {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(stored_lookup(
            state,
            parts,
            "/close",
            viewer,
            MergeRequestEdit::default(),
        ))
    }
}

/// `POST …/merge-requests/{iid}/reopen`.
pub struct MergeRequestToReopen;

impl Loader<AppState> for MergeRequestToReopen {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(stored_lookup(
            state,
            parts,
            "/reopen",
            viewer,
            MergeRequestEdit::default(),
        ))
    }
}

// ── handlers ──

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

fn review_request<B>(w: &WriteParts<MergeRequestCtx, B>) -> ReviewRequest {
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// The repository of the loaded project row (read with the access facts).
fn repository<B>(
    request: &Dispatched<MergeRequestCtx, B>,
) -> Result<ProjectRepository, ForgeError> {
    request.project().map(|r| r.repository).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "merge_request_without_project_row")
            .with_context("project_id", request.proof().id().0.get())
    })
}

/// The merge request re-read after a committed write.
async fn written(state: &AppState, project: ProjectId, iid: i64) -> Result<Value, ForgeError> {
    let stored = bounded(
        state,
        "merge_request_load",
        review(state)?.merge_request(project, iid),
    )
    .await?
    .ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "written_merge_request_missing")
            .with_context("iid", iid)
    })?;
    json(body_of(&stored))
}

/// `state`, `limit` and `before`, strictly: `FG-WEB-0006
/// reason=param_invalid`.
fn page_query(
    query: Option<&str>,
) -> Result<(Option<MergeRequestState>, usize, Option<i64>), ForgeError> {
    let (mut state, mut limit, mut before): (Option<&str>, Option<&str>, Option<&str>) =
        (None, None, None);
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "state" => (&mut state, "state"),
            "limit" => (&mut limit, "limit"),
            "before" => (&mut before, "before"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let state = match state {
        None => Some(MergeRequestState::Open),
        Some("all") => None,
        Some(raw) => Some(
            MergeRequestState::parse(raw).ok_or_else(|| param_invalid("state", "unknown_value"))?,
        ),
    };
    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_PAGE)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    let before = match before {
        None => None,
        Some(raw) => Some(
            parse_positive_id(raw)
                .ok_or_else(|| param_invalid("before", "not_positive_decimal"))?,
        ),
    };
    Ok((state, limit, before))
}

/// `GET …/merge-requests` → `200` one page.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let query = page_query(request.raw_query());
    let id = *request.proof().id();
    let built = async {
        let (wanted, limit, before) = query?;
        let rows = bounded(
            &state,
            "merge_request_page",
            review(&state)?.merge_requests(id, wanted, before, limit),
        )
        .await?;
        let next_before = if rows.len() == limit {
            rows.last().map(|r| r.iid)
        } else {
            None
        };
        json(MergeRequestPage {
            merge_requests: rows.iter().map(body_of).collect(),
            next_before,
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `GET …/merge-requests/{iid}` → `200` the merge request as decided on.
pub async fn view(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let (project, iid) = *request.proof().id();
    let built = async {
        let iid = iid.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
        })?;
        written(&state, project, iid).await
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `POST …/merge-requests` → `201` the merge request.
pub async fn create(
    state: AppState,
    request: Dispatched<MergeRequestCtx, CreateMergeRequest>,
) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let fields = NewFields {
            title: w.input.title.clone(), // clone: the input stays borrowed by the parts
            description: w.input.description.clone(),
        };
        let opened = review(&state)?
            .open(&review_request(&w), &w.proof, repo?, &fields)
            .await?;
        written(&state, opened.project_id, opened.iid).await
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `PATCH …/merge-requests/{iid}` → `200` the merge request as it is now
/// (also when nothing changed).
pub async fn update(
    state: AppState,
    request: Dispatched<MergeRequestCtx, UpdateMergeRequest>,
) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let fields = EditFields {
            title: w.input.title.clone(), // clone: the input stays borrowed by the parts
            description: w.input.description.clone(),
        };
        let (forge_review::EditOutcome::Changed(_) | forge_review::EditOutcome::Unchanged) =
            review(&state)?
                .edit(&review_request(&w), &w.proof, repo?, &fields)
                .await?;
        stored_answer(&state, &w).await
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/merge-requests/{iid}/close` → `200` the merge request (also
/// when it was already closed).
pub async fn close(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let (forge_review::StateOutcome::Changed | forge_review::StateOutcome::Unchanged) =
            review(&state)?.close(&review_request(&w), &w.proof).await?;
        stored_answer(&state, &w).await
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/merge-requests/{iid}/reopen` → `200` the merge request (also
/// when it was already open).
pub async fn reopen(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let (forge_review::StateOutcome::Changed | forge_review::StateOutcome::Unchanged) =
            review(&state)?
                .reopen(&review_request(&w), &w.proof, repo?)
                .await?;
        stored_answer(&state, &w).await
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// The merge request of a write's proof, re-read.
async fn stored_answer<B>(
    state: &AppState,
    w: &WriteParts<MergeRequestCtx, B>,
) -> Result<Value, ForgeError> {
    let (project, iid) = *w.proof.id();
    let iid = iid.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
    })?;
    written(state, project, iid).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(err: &ForgeError) -> String {
        err.to_string()
    }

    #[test]
    fn branch_params_are_branches_only() {
        assert_eq!(
            branch_param("source_branch", "feature/x")
                .expect("short")
                .as_str(),
            "refs/heads/feature/x"
        );
        assert_eq!(
            branch_param("source_branch", "refs/heads/main")
                .expect("full")
                .as_str(),
            "refs/heads/main"
        );
        for (raw, why) in [
            ("refs/tags/v1", "not_a_branch"),
            ("refs/merge-requests/1/head", "not_a_branch"),
            ("-rf", "invalid_ref_name"),
            ("a..b", "invalid_ref_name"),
            ("", "invalid_ref_name"),
        ] {
            let err = branch_param("target_branch", raw).expect_err(raw);
            assert_eq!(err.code(), ErrorCode::RequestInvalid, "{raw}");
            assert!(rule(&err).contains(why), "{raw}: {err}");
        }
    }

    #[test]
    fn ids_and_queries_are_strict() {
        let p = "/api/v1/projects/7/merge-requests/3";
        assert_eq!(ids(p, "").expect("ids"), (ProjectId::new(7), 3));
        assert_eq!(
            ids("/api/v1/projects/7/merge-requests/3/close", "/close").expect("ids"),
            (ProjectId::new(7), 3)
        );
        for bad in [
            "/api/v1/projects/7/merge-requests/0",
            "/api/v1/projects/7/merge-requests/03",
            "/api/v1/projects/7/merge-requests/3/x",
            "/api/v1/projects/x/merge-requests/3",
        ] {
            assert!(ids(bad, "").is_err(), "{bad}");
        }
        assert_eq!(
            page_query(None).expect("defaults"),
            (Some(MergeRequestState::Open), DEFAULT_LIMIT, None)
        );
        assert_eq!(
            page_query(Some("state=all&limit=5&before=9")).expect("all"),
            (None, 5, Some(9))
        );
        for bad in [
            "state=draft",
            "limit=101",
            "limit=0",
            "before=-1",
            "x=1",
            "limit=1&limit=2",
        ] {
            assert!(page_query(Some(bad)).is_err(), "{bad}");
        }
    }
}
