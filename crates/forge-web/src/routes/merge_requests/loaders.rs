//! The loaders of the merge request routes: the project (with its
//! repository) and, for a route on a stored merge request, the row by its
//! `iid` — an invisible project and a missing `iid` are the same
//! `FG-REPO-0005`. Branch names are checked here, before any lookup.

use axum::http::request::Parts;
use forge_auth::{
    LoadedMergeRequest, LoadedProject, MergeRequestCtx, MergeRequestEdit, ProjectCtx,
};
use forge_core::{parse_positive_id, MergeRequestState, ProjectId, ProjectRecord, RefName, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::{review, CreateMergeRequest, PurgeVersionRefs, UpdateMergeRequest, PURGE_SUFFIX};
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, LoadFuture, Loader, Lookup};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, id_in};

/// A branch named in a request: `name` or `refs/heads/name`; any other
/// namespace is `not_a_branch` (ADR-0024 §1), a name git refuses
/// `invalid_ref_name` — `FG-WEB-0006` before anything is looked up.
pub(super) fn branch_param(param: &'static str, raw: &str) -> Result<RefName, ForgeError> {
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
pub(super) fn ids(raw_path: &str, suffix: &str) -> Result<(ProjectId, i64), ForgeError> {
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

/// `POST …/merge-requests/{iid}/version-refs/purge` (M3 R1b): the merge
/// request as stored (the body names versions, decided by the command).
pub struct MergeRequestToPurge;

impl BodyLoader<AppState, PurgeVersionRefs> for MergeRequestToPurge {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a PurgeVersionRefs,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(stored_lookup(
            state,
            parts,
            PURGE_SUFFIX,
            viewer,
            MergeRequestEdit::default(),
        ))
    }
}
