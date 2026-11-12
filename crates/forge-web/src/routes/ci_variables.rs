//! A project's CI variables — the secrets its jobs name in `secrets:` (M4
//! C6, plan 14 §4.2; ADR-0029 decision 8). `CiVariableManage`: Maintainer
//! and above, people only (an agent's token, a service account, a bot, an
//! act-as request, a runner and a job are refused).
//!
//! - `GET /api/v1/projects/{id}/ci/variables`: names and flags (never a
//!   value).
//! - `PUT …/ci/variables/{name}`: `{"value", "protected_only"?,
//!   "masked"?}` (both default `true`) → `201` created / `200` replaced,
//!   the metadata only. A bad name or value is `400 FG-WEB-0006` with
//!   `param` and `reason` (never the value); no usable key `503
//!   FG-CI-0016`.
//! - `DELETE …/ci/variables/{name}`: `204`; `404 FG-REPO-0005` without
//!   such a variable.
//!
//! A value is never answered by any route after it was written: it goes to
//! a runner with the claim of a job of a protected push pipeline only.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::ProjectCtx;
use forge_core::ci_secret::VariableValue;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_pipelines::secrets::{Variable, VariableInput};
use forge_pipelines::{ChangeRequest, Pipelines};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` a project's variables.
pub const LIST: &str = "/api/v1/projects/{id}/ci/variables";
/// `PUT` / `DELETE` one.
pub const ONE: &str = "/api/v1/projects/{id}/ci/variables/{name}";
/// The body cap (a value of 4 KiB, JSON-escaped).
pub const MAX_BODY: usize = 16 * 1024;

const TAIL: &str = "ci/variables";

/// `PUT …/ci/variables/{name}`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutCiVariable {
    /// The value: one line of 1..=4096 bytes, no control character, no
    /// white space at either end, not starting with a quote; at least 8
    /// bytes when masked. Never answered back.
    pub value: String,
    /// Give it only to pipelines of protected branches (default `true`;
    /// today every value goes only to a push pipeline of a protected
    /// branch, ADR-0029).
    #[serde(default)]
    pub protected_only: Option<bool>,
    /// Mask it in job logs (default `true`).
    #[serde(default)]
    pub masked: Option<bool>,
}

impl std::fmt::Debug for PutCiVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PutCiVariable")
            .field("protected_only", &self.protected_only)
            .field("masked", &self.masked)
            .finish_non_exhaustive()
    }
}

/// A variable's metadata (never its value).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CiVariableBody {
    pub name: String,
    pub protected_only: bool,
    pub masked: bool,
    /// The value's length in bytes.
    #[schema(minimum = 1)]
    pub value_bytes: i32,
    #[schema(minimum = 1)]
    pub created_by: i64,
    #[schema(minimum = 1)]
    pub updated_by: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// `GET …/ci/variables`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CiVariableList {
    pub variables: Vec<CiVariableBody>,
}

fn variable_body(v: Variable) -> CiVariableBody {
    CiVariableBody {
        name: v.name,
        protected_only: v.protected_only,
        masked: v.masked,
        value_bytes: v.value_bytes,
        created_by: v.created_by,
        updated_by: v.updated_by,
        created_at: v.created_at,
        updated_at: v.updated_at,
    }
}

fn pipelines(state: &AppState) -> Result<&Pipelines, ForgeError> {
    state.pipelines.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "pipelines_unconfigured")
    })
}

/// The path's project and, for one variable, its name (checked by
/// `forge_pipelines::secrets::check_name` in the command).
fn path_of(raw_path: &str) -> Result<(ProjectId, Option<String>), ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)?;
    if tail == TAIL {
        return Ok((project, None));
    }
    let name = tail
        .strip_prefix(TAIL)
        .and_then(|t| t.strip_prefix('/'))
        .filter(|n| !n.is_empty() && !n.contains('/'))
        .ok_or_else(|| param_invalid("name", "invalid_name"))?;
    Ok((project, Some(name.to_owned())))
}

/// `…/ci/variables[/{name}]`: the project.
pub struct VariablesOf {
    one: bool,
}

/// `GET …/ci/variables`.
pub const OF_LIST: VariablesOf = VariablesOf { one: false };
/// `PUT|DELETE …/ci/variables/{name}`.
pub const OF_ONE: VariablesOf = VariablesOf { one: true };

impl VariablesOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let (id, name) = path_of(parts.uri.path())?;
        if name.is_some() != self.one {
            return Err(param_invalid("name", "invalid_name"));
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

impl Loader<AppState> for VariablesOf {
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

impl BodyLoader<AppState, PutCiVariable> for VariablesOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PutCiVariable,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

fn request_of<B>(w: &WriteParts<ProjectCtx, B>) -> ChangeRequest {
    ChangeRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn name_of(raw_path: &str) -> Result<String, ForgeError> {
    path_of(raw_path)?
        .1
        .ok_or_else(|| param_invalid("name", "invalid_name"))
}

/// `GET …/ci/variables` → `200`.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let rows = bounded(
            &state,
            "ci_variable_list",
            pipelines(&state)?.variables(&w.proof),
        )
        .await?;
        json(CiVariableList {
            variables: rows.into_iter().map(variable_body).collect(),
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT …/ci/variables/{name}` → `201` / `200`.
pub async fn put(state: AppState, request: Dispatched<ProjectCtx, PutCiVariable>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let input = VariableInput {
            name: name_of(&path)?,
            value: VariableValue::new(w.input.value.clone()), // clone: the value is wiped on drop
            protected_only: w.input.protected_only.unwrap_or(true),
            masked: w.input.masked.unwrap_or(true),
        };
        let (stored, created) = pipelines(&state)?
            .set_variable(&request_of(&w), &w.proof, &input)
            .await?;
        let status = if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok::<_, ForgeError>((status, json(variable_body(stored))?))
    }
    .await;
    match built {
        Ok((status, value)) => no_store(status, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE …/ci/variables/{name}` → `204`.
pub async fn delete(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let done = async {
        let name = name_of(&path)?;
        let deleted = pipelines(&state)?
            .delete_variable(&request_of(&w), &w.proof, &name)
            .await?;
        if deleted {
            Ok(())
        } else {
            Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "ci_variable")
                .with_context("name", name))
        }
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_path_names_the_project_and_the_variable() {
        assert_eq!(
            path_of("/api/v1/projects/7/ci/variables").expect("list"),
            (ProjectId::new(7), None)
        );
        assert_eq!(
            path_of("/api/v1/projects/7/ci/variables/DEPLOY_TOKEN").expect("one"),
            (ProjectId::new(7), Some("DEPLOY_TOKEN".to_owned()))
        );
        for bad in [
            "/api/v1/projects/0/ci/variables",
            "/api/v1/projects/x/ci/variables",
            "/api/v1/projects/7/ci/variablesX",
            "/api/v1/projects/7/ci/variables/",
            "/api/v1/projects/7/ci/variables/A/B",
        ] {
            assert!(path_of(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_put_body_never_prints_its_value() {
        let body: PutCiVariable =
            serde_json::from_str(r#"{"value": "s3cr3t-value", "masked": true}"#).expect("body");
        assert!(!format!("{body:?}").contains("s3cr3t"));
    }
}
