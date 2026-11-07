//! A project's push mirror to GitLab (M3 O2, plan 11 §5 O2, ADR-0024
//! Amendment O2, runbook `gitlab-mirror`). Administrators only, people only
//! (an agent's token, a service account and a bot are refused); a token
//! needs the `admin` scope too.
//!
//! - `GET /api/v1/projects/{id}/push-mirror` (`PushMirrorRead`): the
//!   mirror (its full URL: the token is never in it) and its status; `404
//!   FG-REPO-0005` when there is none.
//! - `PUT …/push-mirror` (`PushMirrorManage`): create (`201`) or change
//!   (`200`) it. The URL passes the operator's mirror policy and the
//!   credential file must be usable now, after the decision (`400
//!   FG-INT-0021`, one sentence; the reason, the origin and the credential
//!   name only in the log). A new or re-enabled mirror is pushed at once.
//! - `DELETE …/push-mirror` (`PushMirrorManage`): `204`.
//! - `POST …/push-mirror/sync` (`PushMirrorManage`): queue one push now,
//!   `202 {"queued": bool}` (`false`: this request's push was already
//!   queued); `404 FG-REPO-0005` without an enabled mirror.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::ProjectCtx;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::mirror::{MirrorInput, MirrorRequest, PushMirror, PushMirrors, SetOutcome};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` / `PUT` / `DELETE` a project's mirror.
pub const ONE: &str = "/api/v1/projects/{id}/push-mirror";
/// `POST`: push now.
pub const SYNC: &str = "/api/v1/projects/{id}/push-mirror/sync";
/// The largest body (a URL of 2 KiB and a credential name).
pub const MAX_BODY: usize = 8 * 1024;

fn enabled_by_default() -> bool {
    true
}

/// `PUT …/push-mirror`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetPushMirror {
    /// An absolute `https` URL (`http` only where the operator allows it)
    /// of at most 2048 bytes, without user information, query or fragment,
    /// whose host and port `mirror.allowed_hosts` names.
    pub url: String,
    /// The name of a credential file in `mirror.credentials_dir` (1–64 of
    /// `[a-z0-9._-]`, starting with a letter or a digit): one
    /// `<username>:<token>` line, mode 0600.
    pub credential_name: String,
    /// `false` keeps the setting without pushing (default `true`).
    #[serde(default = "enabled_by_default")]
    #[schema(default = true)]
    pub enabled: bool,
}

/// A mirror's progress.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushMirrorStatusBody {
    /// The newest push waiting to be mirrored (`0`: none since it was set).
    pub pending_seq: i64,
    /// Waiting since (UTC, RFC 3339); `null` when up to date.
    #[schema(required = true)]
    pub pending_since: Option<String>,
    /// The newest push mirrored.
    pub mirrored_seq: i64,
    #[schema(required = true)]
    pub last_attempt_at: Option<String>,
    #[schema(required = true)]
    pub last_success_at: Option<String>,
    /// Failed attempts in a row.
    pub failures: i32,
    /// `FG-INT-0013` after a failed attempt.
    #[schema(required = true)]
    pub last_error_code: Option<String>,
    /// `auth`, `network`, `timeout`, `credential_unreadable`, … (runbook
    /// `gitlab-mirror`).
    #[schema(required = true)]
    pub last_reason: Option<String>,
}

/// A project's mirror (never the credential's content).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushMirrorBody {
    #[schema(minimum = 1)]
    pub repo_id: i64,
    /// The destination as stored (normalized).
    pub url: String,
    /// `scheme://host:port` of the URL (what logs and audit rows show).
    pub origin: String,
    pub credential_name: String,
    pub enabled: bool,
    #[schema(minimum = 1)]
    pub created_by: i64,
    pub created_at: String,
    pub updated_at: String,
    pub status: PushMirrorStatusBody,
}

/// A push was asked for.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushMirrorSyncQueued {
    /// `true`: a job was queued; `false`: this request's job was already
    /// there.
    pub queued: bool,
}

fn body(m: PushMirror) -> PushMirrorBody {
    let s = m.status;
    PushMirrorBody {
        repo_id: m.repo.get(),
        url: m.url,
        origin: m.origin,
        credential_name: m.credential_name,
        enabled: m.enabled,
        created_by: m.created_by.get(),
        created_at: m.created_at,
        updated_at: m.updated_at,
        status: PushMirrorStatusBody {
            pending_seq: s.pending_seq,
            pending_since: s.pending_since,
            mirrored_seq: s.mirrored_seq,
            last_attempt_at: s.last_attempt_at,
            last_success_at: s.last_success_at,
            failures: s.failures,
            last_error_code: s.last_error_code,
            last_reason: s.last_reason,
        },
    }
}

pub(crate) fn mirrors(state: &AppState) -> Result<&PushMirrors, ForgeError> {
    state.push_mirrors.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "push_mirrors_unconfigured")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    One,
    Sync,
}

/// The path's project id and tail.
fn ids(raw_path: &str) -> Result<(ProjectId, Tail), ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once("/push-mirror").ok_or_else(bad)?;
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)?;
    let tail = match tail {
        "" => Tail::One,
        "/sync" => Tail::Sync,
        _ => return Err(bad()),
    };
    Ok((project, tail))
}

/// `…/push-mirror[/sync]`: the project.
pub struct MirrorOf {
    tail: Tail,
}

/// `GET|PUT|DELETE …/push-mirror`.
pub const MIRROR: MirrorOf = MirrorOf { tail: Tail::One };
/// `POST …/push-mirror/sync`.
pub const MIRROR_SYNC: MirrorOf = MirrorOf { tail: Tail::Sync };

impl MirrorOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let (id, tail) = ids(parts.uri.path())?;
        if tail != self.tail {
            return Err(param_invalid("id", "not_positive_decimal"));
        }
        let Some(record) = projects::load_bounded(state, id, viewer).await? else {
            return Ok(absent());
        };
        if record.id != id {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "loaded_another_project")
                .with_context("project_id", id.get())
                .with_context("loaded_project_id", record.id.get()));
        }
        Ok(Lookup::Found {
            ctx: ProjectCtx::new(projects::loaded(&record)),
            canonical: None,
            tombstone: None,
            project: Some(record),
            group: None,
        })
    }
}

impl Loader<AppState> for MirrorOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, SetPushMirror> for MirrorOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a SetPushMirror,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

fn request_of<B>(w: &WriteParts<ProjectCtx, B>) -> MirrorRequest {
    MirrorRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn respond(
    ctx: &crate::Ctx,
    status: StatusCode,
    built: Result<serde_json::Value, ForgeError>,
) -> Response {
    match built {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/push-mirror` → `200`.
pub async fn read(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let got = bounded(&state, "push_mirror_read", mirrors(&state)?.get(&w.proof)).await?;
        json(body(got))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PUT …/push-mirror` → `201` (created) or `200`.
pub async fn set(state: AppState, request: Dispatched<ProjectCtx, SetPushMirror>) -> Response {
    let w = request.into_write();
    let built = async {
        let input = MirrorInput {
            url: w.input.url.clone(), // clone: the body stays with the request
            credential_name: w.input.credential_name.clone(), // clone: as above
            enabled: w.input.enabled,
        };
        let (outcome, stored) = mirrors(&state)?
            .set(&request_of(&w), &w.proof, &input)
            .await?;
        let status = match outcome {
            SetOutcome::Created => StatusCode::CREATED,
            SetOutcome::Updated | SetOutcome::Unchanged => StatusCode::OK,
        };
        Ok((status, json(body(stored))?))
    }
    .await;
    match built {
        Ok((status, value)) => no_store(status, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE …/push-mirror` → `204`.
pub async fn delete(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let done = async { mirrors(&state)?.remove(&request_of(&w), &w.proof).await }.await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/push-mirror/sync` → `202`.
pub async fn sync(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let queued = mirrors(&state)?.sync(&request_of(&w), &w.proof).await?;
        json(PushMirrorSyncQueued { queued })
    }
    .await;
    respond(&w.ctx, StatusCode::ACCEPTED, built)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_paths_name_the_project_and_the_tail() {
        assert_eq!(
            ids("/api/v1/projects/7/push-mirror").expect("one"),
            (ProjectId::new(7), Tail::One)
        );
        assert_eq!(
            ids("/api/v1/projects/7/push-mirror/sync").expect("sync"),
            (ProjectId::new(7), Tail::Sync)
        );
        for bad in [
            "/api/v1/projects/0/push-mirror",
            "/api/v1/projects/x/push-mirror",
            "/api/v1/projects/7/push-mirrorx",
            "/api/v1/projects/7/push-mirror/other",
        ] {
            assert!(ids(bad).is_err(), "{bad}");
        }
    }
}
