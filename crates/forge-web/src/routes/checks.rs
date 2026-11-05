//! The Checks API (M3 C1, ADR-0024 §10, plan 02 §5.4, docs/api/checks.md):
//!
//! - `POST /api/v1/projects/{id}/commits/{sha}/checks` (`CheckWrite`):
//!   report a check run on a commit — the same name again updates its row
//!   (no PATCH). The name's namespace is an input of the decision, so the
//!   name is parsed before it (`FG-REVIEW-0020 reason=name_invalid`); every
//!   other rule of the body is checked after it. `201` created, `200`
//!   updated.
//! - `GET /api/v1/projects/{id}/commits/{sha}/checks` (`CheckRead`): every
//!   run of the commit, by name.
//!
//! A `{sha}` that is no full lowercase commit id, or a commit no branch or
//! tag reaches, is answered exactly like a missing one (`FG-REPO-0023`
//! after the decision; a malformed one before it, as an absent resource).
//!
//! And the grants (administrators, people only):
//!
//! - `GET /api/v1/admin/service-accounts/{id}/check-grants`
//!   (`ServiceAccountRead`): the project and namespace pairs the account
//!   reports.
//! - `PUT|DELETE /api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}`
//!   (`CheckNamespaceGrantManage`): grant or revoke `ci` / `kanflow` on one
//!   project (review fix 1: a grant is for one project only).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    CheckNamespaceGrantCtx, CheckRunCtx, ProjectCtx, ServiceAccountCtx, ServiceAccountView,
};
use forge_checks::{AnnotationInput, Checks, ChecksRequest, ReportInput, StoredCheckRun};
use forge_core::{parse_positive_id, CheckNamespace, ObjectId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, hosting, json, no_store};

/// `POST` / `GET` the check runs of a commit.
pub const CHECKS: &str = "/api/v1/projects/{id}/commits/{sha}/checks";
/// `GET` the grants of a service account.
pub const GRANTS: &str = "/api/v1/admin/service-accounts/{id}/check-grants";
/// `PUT` / `DELETE` one grant: a namespace on one project.
pub const GRANT: &str = "/api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}";
/// The body cap: the largest a JSON route may declare (64 KiB). The whole
/// report — summary, URL and annotations, JSON-escaped — fits in it; a
/// larger body is refused before it is read (docs/api/checks.md).
pub const MAX_BODY: usize = crate::body::MAX_JSON_BODY_BYTES;
/// The version of the list's contract.
pub const SCHEMA_VERSION: u32 = 1;

const ACCOUNTS_PREFIX: &str = "/api/v1/admin/service-accounts/";
const GRANTS_TAIL: &str = "check-grants";

/// One annotation of a report.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationRequest {
    /// A path inside the repository: relative, no `..` segment, 1..=4096
    /// bytes.
    pub path: String,
    #[schema(minimum = 1)]
    pub start_line: i64,
    /// At least `start_line` (default: `start_line`).
    pub end_line: Option<i64>,
    /// `notice`, `warning` or `failure`.
    pub level: String,
    /// 1 character to 4 KiB, plain text.
    pub message: String,
    /// At most 255 bytes.
    pub title: Option<String>,
}

/// `POST …/commits/{sha}/checks`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportCheckRun {
    /// `<namespace>/<name>`: `ci/…` or `kanflow/…` (a granted service
    /// account); `forge/…` and `ext/…` are refused over HTTP in C1.
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// Exactly when `completed`: `success`, `neutral`, `failure`, `error`
    /// or `action_required`.
    pub conclusion: Option<String>,
    /// Plain text / Markdown as written, at most 64 KiB (default empty).
    pub summary: Option<String>,
    /// An http(s) link for people (at most 2 KiB; never fetched by the
    /// server).
    pub details_url: Option<String>,
    /// At most 50; a report replaces the run's annotations.
    #[serde(default)]
    pub annotations: Vec<AnnotationRequest>,
}

/// One annotation as stored.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationBody {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub level: String,
    pub message: String,
    #[schema(required = true)]
    pub title: Option<String>,
}

/// Who reported a run last.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReporterBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(required = true)]
    pub username: Option<String>,
}

/// One check run (docs/api/checks.md).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckRunBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub name: String,
    pub head_sha: String,
    pub status: String,
    #[schema(required = true)]
    pub conclusion: Option<String>,
    pub summary: String,
    #[schema(required = true)]
    pub details_url: Option<String>,
    pub annotations: Vec<AnnotationBody>,
    /// The account that reported it last; `null` for a `forge/` run
    /// (forge's own worker, M3 X2).
    #[schema(required = true)]
    pub reported_by: Option<ReporterBody>,
    /// Counts the run's reports (1 at creation).
    pub revision: i64,
    /// UTC, RFC 3339 with microseconds.
    pub created_at: String,
    pub updated_at: String,
    #[schema(required = true)]
    pub completed_at: Option<String>,
}

/// `GET …/commits/{sha}/checks` (docs/api/checks.md, schema_version 1).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckRunList {
    pub schema_version: u32,
    pub head_sha: String,
    /// By name; at most 100 reported, plus forge's own (`forge/spec-drift`).
    pub check_runs: Vec<CheckRunBody>,
}

/// One grant: a namespace a service account reports on one project.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrantBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// `ci` or `kanflow`.
    pub namespace: String,
}

/// The grants of a service account, by project then namespace.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckGrants {
    pub grants: Vec<GrantBody>,
}

/// A grant change's answer: the grants now, and whether the change wrote
/// anything (`false`: the grant already was, or was not, there).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckGrantChange {
    pub grants: Vec<GrantBody>,
    pub changed: bool,
}

pub(crate) fn run_body(run: &StoredCheckRun) -> CheckRunBody {
    CheckRunBody {
        id: run.id,
        name: run.name.as_str().to_owned(),
        head_sha: run.head_sha.as_str().to_owned(),
        status: run.status.as_str().to_owned(),
        conclusion: run.conclusion.map(|c| c.as_str().to_owned()),
        summary: run.summary.clone(), // clone: the body owns its text
        details_url: run.details_url.clone(), // clone: as above
        annotations: run
            .annotations
            .iter()
            .map(|a| AnnotationBody {
                path: a.path.clone(), // clone: as above
                start_line: a.start_line,
                end_line: a.end_line,
                level: a.level.as_str().to_owned(),
                message: a.message.clone(), // clone: as above
                title: a.title.clone(),     // clone: as above
            })
            .collect(),
        reported_by: run.reported_by.map(|id| ReporterBody {
            id: id.get(),
            username: run.reported_by_username.clone(), // clone: as above
        }),
        revision: run.revision,
        created_at: run.created_at.clone(),     // clone: as above
        updated_at: run.updated_at.clone(),     // clone: as above
        completed_at: run.completed_at.clone(), // clone: as above
    }
}

pub(crate) fn checks(state: &AppState) -> Result<&Checks, ForgeError> {
    state.checks.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "checks_unconfigured")
    })
}

/// The `{id}` and `{sha}` of [`CHECKS`] (`None` for `{sha}` when it is no
/// full lowercase commit id).
fn ids(raw_path: &str) -> Result<(ProjectId, Option<ObjectId>), ForgeError> {
    let (id, rest) = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/commits/"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let sha = rest
        .strip_suffix("/checks")
        .filter(|s| !s.contains('/'))
        .and_then(|s| ObjectId::parse(s).ok());
    Ok((ProjectId::new(id), sha))
}

fn found_project<R>(ctx: R, record: forge_core::ProjectRecord) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: Some(record),
        group: None,
    }
}

fn found<R>(ctx: R) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
    }
}

/// The project of the path for `viewer` (`None`: absent).
async fn project_record(
    state: &AppState,
    id: ProjectId,
    viewer: Option<UserId>,
) -> Result<Option<forge_core::ProjectRecord>, ForgeError> {
    let Some(record) = projects::load_bounded(state, id, viewer).await? else {
        return Ok(None);
    };
    if record.id != id {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_project")
            .with_context("project_id", id.get())
            .with_context("loaded_project_id", record.id.get()));
    }
    Ok(Some(record))
}

/// `POST …/checks`: the project, the commit and name of the report, and
/// whether the caller holds a grant of the name's namespace.
pub struct ReportOf;

impl BodyLoader<AppState, ReportCheckRun> for ReportOf {
    type Ctx = CheckRunCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a ReportCheckRun,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, CheckRunCtx> {
        Box::pin(async move {
            let (id, sha) = ids(parts.uri.path())?;
            let name = forge_checks::parse_name(&body.name)?;
            let Some(sha) = sha else {
                return Ok(absent());
            };
            let Some(record) = project_record(state, id, viewer).await? else {
                return Ok(absent());
            };
            let granted = match viewer {
                Some(user) => {
                    bounded(
                        state,
                        "check_grant_load",
                        checks(state)?.granted(user, id, name.namespace()),
                    )
                    .await?
                }
                None => false,
            };
            let ctx = CheckRunCtx::new(projects::loaded(&record), sha, name, granted);
            Ok(found_project(ctx, record))
        })
    }
}

/// `GET …/checks`: the project (the commit is checked after the decision).
pub struct ChecksOf;

impl Loader<AppState> for ChecksOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let (id, _) = ids(parts.uri.path())?;
            let Some(record) = project_record(state, id, viewer).await? else {
                return Ok(absent());
            };
            Ok(found_project(
                ProjectCtx::new(projects::loaded(&record)),
                record,
            ))
        })
    }
}

fn request_of<R: forge_auth::ResourceCtx, B>(w: &WriteParts<R, B>) -> ChecksRequest {
    ChecksRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn repository<R: forge_auth::ResourceCtx, B>(
    request: &Dispatched<R, B>,
) -> Result<forge_core::ProjectRepository, ForgeError> {
    request.project().map(|r| r.repository).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "checks_without_project_row")
    })
}

fn respond(
    ctx: &crate::Ctx,
    status: StatusCode,
    body: Result<serde_json::Value, ForgeError>,
) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `POST …/commits/{sha}/checks` → `201` created / `200` updated: the run
/// as committed.
pub async fn report(state: AppState, request: Dispatched<CheckRunCtx, ReportCheckRun>) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let input = ReportInput {
            status: w.input.status.clone(), // clone: the input stays borrowed by the parts
            conclusion: w.input.conclusion.clone(), // clone: as above
            summary: w.input.summary.clone(), // clone: as above
            details_url: w.input.details_url.clone(), // clone: as above
            annotations: w
                .input
                .annotations
                .iter()
                .map(|a| AnnotationInput {
                    path: a.path.clone(), // clone: as above
                    start_line: a.start_line,
                    end_line: a.end_line,
                    level: a.level.clone(),     // clone: as above
                    message: a.message.clone(), // clone: as above
                    title: a.title.clone(),     // clone: as above
                })
                .collect(),
        };
        let reported = checks(&state)?
            .report(&request_of(&w), &w.proof, repo?, &input)
            .await?;
        let status = if reported.created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok((status, json(run_body(&reported.run))?))
    }
    .await;
    match built {
        Ok((status, body)) => no_store(status, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `GET …/commits/{sha}/checks` → `200` every run of the commit by name.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let repo = repository(&request);
    let raw_path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        // A malformed commit id is answered like a missing commit.
        let Some(sha) = ids(&raw_path)?.1 else {
            return Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                .with_context("reason", "ref_not_found")
                .with_context("rule", "sha_invalid"));
        };
        let runs = checks(&state)?.list(&w.proof, repo?, &sha).await?;
        json(CheckRunList {
            schema_version: SCHEMA_VERSION,
            head_sha: sha.as_str().to_owned(),
            check_runs: runs.iter().map(run_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

// ── the grants ──

/// The `{id}` and the tail after it of a path under the service accounts.
fn account_ids(path: &str) -> Result<(UserId, &str), ForgeError> {
    let rest = path
        .strip_prefix(ACCOUNTS_PREFIX)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let (raw, tail) = rest.split_once('/').map_or((rest, ""), |(id, t)| (id, t));
    let id = parse_positive_id(raw).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    Ok((UserId::new(id), tail))
}

async fn account_view(
    state: &AppState,
    id: UserId,
) -> Result<Option<ServiceAccountView>, ForgeError> {
    Ok(bounded(
        state,
        "service_account_load",
        hosting(state)?.service_account_target(id),
    )
    .await?
    .map(|t| ServiceAccountView {
        kind: t.kind,
        active: t.active,
    }))
}

/// The `{namespace}` of [`GRANT`] (`FG-REVIEW-0020
/// reason=namespace_not_grantable` for any but `ci` / `kanflow`).
fn grant_namespace(raw: &str) -> Result<CheckNamespace, ForgeError> {
    CheckNamespace::ALL
        .into_iter()
        .find(|n| n.as_str() == raw && matches!(n, CheckNamespace::Ci | CheckNamespace::Kanflow))
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::CheckPayloadInvalid)
                .with_context("reason", "namespace_not_grantable")
                .with_context("len", raw.len())
        })
}

/// `GET …/{id}/check-grants`.
pub struct GrantsOf;

impl Loader<AppState> for GrantsOf {
    type Ctx = ServiceAccountCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, ServiceAccountCtx> {
        Box::pin(async move {
            let (id, tail) = account_ids(parts.uri.path())?;
            if tail != GRANTS_TAIL {
                return Err(param_invalid("id", "not_positive_decimal"));
            }
            Ok(match account_view(state, id).await? {
                Some(view) => found(ServiceAccountCtx::new(id, view)),
                None => absent(),
            })
        })
    }
}

/// `PUT|DELETE …/{id}/check-grants/{project_id}/{namespace}`.
pub struct GrantOf;

impl Loader<AppState> for GrantOf {
    type Ctx = CheckNamespaceGrantCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, CheckNamespaceGrantCtx> {
        Box::pin(async move {
            let (id, tail) = account_ids(parts.uri.path())?;
            let (project, raw) = tail
                .strip_prefix("check-grants/")
                .and_then(|rest| rest.split_once('/'))
                .filter(|(_, raw)| !raw.contains('/'))
                .ok_or_else(|| param_invalid("namespace", "missing"))?;
            let project = parse_positive_id(project)
                .map(ProjectId::new)
                .ok_or_else(|| param_invalid("project_id", "not_positive_decimal"))?;
            let namespace = grant_namespace(raw)?;
            Ok(match account_view(state, id).await? {
                Some(view) => found(CheckNamespaceGrantCtx::new(id, view, project, namespace)),
                None => absent(),
            })
        })
    }
}

async fn grants_of(state: &AppState, id: UserId) -> Result<Vec<GrantBody>, ForgeError> {
    let grants = bounded(state, "check_grants_load", checks(state)?.grants_of(id)).await?;
    Ok(grants
        .iter()
        .map(|g| GrantBody {
            project_id: g.project.get(),
            namespace: g.namespace.as_str().to_owned(),
        })
        .collect())
}

/// `GET …/{id}/check-grants` → `200`.
pub async fn grants(state: AppState, request: Dispatched<ServiceAccountCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        json(CheckGrants {
            grants: grants_of(&state, *w.proof.id()).await?,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

async fn change(
    state: AppState,
    request: Dispatched<CheckNamespaceGrantCtx>,
    grant: bool,
) -> Response {
    let w = request.into_write();
    let built = async {
        let outcome = checks(&state)?
            .change_grant(&request_of(&w), &w.proof, grant)
            .await?;
        json(CheckGrantChange {
            grants: grants_of(&state, w.proof.id().0).await?,
            changed: outcome == forge_checks::GrantOutcome::Changed,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PUT …/{id}/check-grants/{project_id}/{namespace}` → `200` the grants
/// now (also when the grant was there already).
pub async fn grant(state: AppState, request: Dispatched<CheckNamespaceGrantCtx>) -> Response {
    change(state, request, true).await
}

/// `DELETE …/{id}/check-grants/{project_id}/{namespace}` → `200` the grants
/// now (also when there was no such grant).
pub async fn revoke(state: AppState, request: Dispatched<CheckNamespaceGrantCtx>) -> Response {
    change(state, request, false).await
}
