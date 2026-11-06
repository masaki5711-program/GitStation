//! `GET /api/v1/projects/{id}/repository/spec-refs[?ref=]` (`SpecRefRead`,
//! `read_api`; M3 X1, plan 03 §4.4, ADR-0023 Amendment X1): a revision's
//! `.forge/repo.yml` and the specifications it pins, each resolved for the
//! reader.
//!
//! 1. The registry authenticates, [`SPEC_REFS_OF`] loads the project by id
//!    and `prove(SpecRefRead)` decides on it (RepoBrowse's rule).
//! 2. The revision: `ref` (a full commit id, a full or short branch / tag
//!    name — the browse API's rule), else the default branch.
//! 3. `.forge/repo.yml` at that commit is read whole (at most 64 KiB, a
//!    regular file; [`forge_git::browse::Browser::small_file`]) and parsed
//!    by `forge_core::repo_config`. Absent: `config: "absent"`. Refused:
//!    `config: "invalid"` with `problem {error_code: FG-REPO-0024, line,
//!    reason}` and **no** entry (fail closed: never a partial reading).
//! 4. Every `repo:` entry: the path is looked up **for the reader** (one
//!    resolver call, the same statements whatever is there), the project
//!    is decided again with `prove_spec_ref` on the reader's own principal,
//!    and only then is its pinned ref resolved (`refs/tags/<version>` /
//!    `refs/heads/<track>`, through the browse API: resolved ids,
//!    `--end-of-options`, one deadline, the per-repository cap).
//!    - `resolved` — `project_id` and `commit`;
//!    - `version_not_found` (`FG-REPO-0025`) — the reader may read the
//!      project, the ref is not there (or names no commit);
//!    - `unavailable` (`FG-REPO-0026`) — nothing at the path, not a
//!      project, a moved or deleted tombstone (a pin is not redirected),
//!      `authorize()` refused, or the repository is not ready. All the same
//!      shape, without `project_id`: no existence oracle.
//!
//!    Each `path:` entry is `local` at the read commit.
//!
//! A git or lookup failure fails the request with its own code (never an
//! entry quietly dropped). Problems are logged once, at info, with
//! `request_id` and `error_code` (`forge.spec_refs.repo_yml_invalid` here,
//! `forge.spec_refs.unresolved` by forge-checks); never the file's text, a
//! path or a tag.
//!
//! Steps 3 and 4 are `forge_checks::spec_refs` (M3 X2 moved them there:
//! the `forge/spec-drift` job reads and follows the same way, for every
//! signed-in person instead of the reader).

use std::time::Instant;

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{Authorized, ProjectCtx};
use forge_checks::spec_refs::{
    follow as follow_ref, read_repo_config, FollowCtx, Followed, Reader, RepoConfigRead,
};
use forge_core::repo_config::{RepoConfig, SpecEntry, SpecSource, REPO_CONFIG_PATH};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::ResolvedRev;
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::DispatchState;
use crate::registry::{Dispatched, QueryParam};
use crate::routes::browse::{query, rev_body, take, RepositoryOf, RevBody, Target};

/// `GET` a revision's spec references.
pub const SPEC_REFS: &str = "/api/v1/projects/{id}/repository/spec-refs";

/// The loader: the project by id, its row carrying the repository.
pub const SPEC_REFS_OF: RepositoryOf = RepositoryOf("/repository/spec-refs");

/// One budget for a whole request (every lookup and git call of every
/// entry): past it, `FG-WEB-0002 operation=spec_refs`.
pub const BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
/// References followed per caller per [`LIMIT_WINDOW`] (`FG-WEB-0010`).
pub const LIMIT: u32 = 600;
pub const LIMIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// The per-caller limit of followed references.
pub fn limiter() -> crate::routes::namespaces::CallerLimiter {
    crate::routes::namespaces::CallerLimiter::new("spec_refs", LIMIT, LIMIT_WINDOW)
}

/// The query parameters.
pub const SPEC_REFS_QUERY: &[QueryParam] = &[QueryParam {
    name: "ref",
    description: "A full commit id, refs/heads/… or refs/tags/…, or a short name (branch first, then tag); default: the default branch",
    integer_range: None,
}];

// ── the answer ─────────────────────────────────────────────────────────

/// Why `.forge/repo.yml` cannot be used.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoConfigProblemBody {
    /// `FG-REPO-0024`.
    pub error_code: String,
    /// 1-based; 0 for the whole file.
    pub line: usize,
    /// The rule broken (`FG-REPO-0024` lists them).
    pub reason: String,
}

/// How a referenced repository's version is fixed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecPinBody {
    /// `version` (a tag) or `track` (a branch).
    #[serde(rename = "type")]
    pub kind: String,
    /// The tag or branch name as written.
    pub name: String,
    /// `refs/tags/…` or `refs/heads/…`.
    pub ref_name: String,
}

/// One `specs:` entry, resolved for the reader.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecRefBody {
    /// The entry's line in `.forge/repo.yml` (1-based).
    pub line: usize,
    /// `repo` (another repository) or `path` (this one's directory).
    #[serde(rename = "type")]
    pub kind: String,
    /// The repository path as written (`repo` entries).
    #[schema(required = true)]
    pub repo: Option<String>,
    /// The directory as written (`path` entries).
    #[schema(required = true)]
    pub path: Option<String>,
    /// The pin (`repo` entries).
    #[schema(required = true)]
    pub pin: Option<SpecPinBody>,
    /// The requirement id patterns this repository covers.
    pub scope: Vec<String>,
    /// `resolved`, `version_not_found`, `unavailable` (`repo` entries) or
    /// `local` (`path` entries).
    pub status: String,
    /// `FG-REPO-0025` (`version_not_found`), `FG-REPO-0026`
    /// (`unavailable`); `null` otherwise.
    #[schema(required = true)]
    pub error_code: Option<String>,
    /// The referenced project, when the reader may read it.
    #[schema(required = true)]
    pub project_id: Option<i64>,
    /// The pinned commit (`resolved`), or the read commit (`local`).
    #[schema(required = true)]
    pub commit: Option<String>,
}

/// A revision's `.forge/repo.yml` and its spec references.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecRefsBody {
    /// The revision read.
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// `.forge/repo.yml`.
    pub path: String,
    /// `absent`, `valid` or `invalid`.
    pub config: String,
    /// Why the file cannot be used (`invalid` only).
    #[schema(required = true)]
    pub problem: Option<RepoConfigProblemBody>,
    /// In file order; empty unless `valid`.
    pub specs: Vec<SpecRefBody>,
}

// ── the handler ────────────────────────────────────────────────────────

/// `GET /api/v1/projects/{id}/repository/spec-refs`.
pub async fn spec_refs(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let target = match Target::of(&state, &request) {
        Ok(t) => t,
        Err(err) => {
            return ctx
                .fail(err.with_context("route", "repository_spec_refs"))
                .into_response()
        }
    };
    let ids = (target.project.get(), target.repository.id.get());
    let budget = state.spec_refs_budget;
    let ran = match tokio::time::timeout(budget, run(&state, request, &target)).await {
        Ok(ran) => ran,
        // Dropping the future stops its git work (killed and reaped by
        // forge-git) and its lookup.
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", "spec_refs")
            .with_context("timeout_ms", budget.as_millis())),
    };
    match ran {
        Ok(body) => {
            let mut res = Json(body).into_response();
            let headers = res.headers_mut();
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
            res
        }
        Err(err) => ctx
            .fail(
                err.with_context("route", "repository_spec_refs")
                    .with_context("project_id", ids.0)
                    .with_context("repo_id", ids.1),
            )
            .into_response(),
    }
}

/// Owns the request: only `Sync` parts of it (the proof, the request id)
/// are borrowed across an await.
async fn run(
    state: &AppState,
    request: Dispatched<ProjectCtx>,
    t: &Target,
) -> Result<SpecRefsBody, ForgeError> {
    let params = query(request.raw_query(), &["ref"])?;
    let request_id = request.ctx().request_id.clone(); // clone: logged after awaits
    let proof = request.proof();
    let browse = &t.smart.browse;
    let repo = t.repository.id;
    let rev = match take(&params, "ref") {
        Some("") => return Err(param_invalid("ref", "empty")),
        Some(name) => browse.resolve_ref(repo, name, &t.principal).await?,
        None => {
            let list = browse.refs(repo).await?;
            let Some(branch) = list.default_branch.as_deref() else {
                return Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found"));
            };
            browse
                .resolve_ref(repo, &format!("refs/heads/{branch}"), &t.principal)
                .await?
        }
    };
    let config: RepoConfig = match read_repo_config(browse, repo, &rev.commit).await? {
        RepoConfigRead::Absent => return Ok(answer(&rev, "absent", None, Vec::new())),
        RepoConfigRead::Valid(c) => c,
        RepoConfigRead::Invalid(problem) => {
            tracing::info!(
                target: "forge_web",
                event = "forge.spec_refs.repo_yml_invalid",
                error_code = ErrorCode::RepoConfigInvalid.id(),
                request_id = %request_id,
                project_id = t.project.get(),
                repo_id = repo.get(),
                commit = rev.commit.as_str(),
                line = problem.line,
                reason = problem.reason,
                "the revision's .forge/repo.yml cannot be used; no reference was followed"
            );
            let body = RepoConfigProblemBody {
                error_code: ErrorCode::RepoConfigInvalid.id().to_owned(),
                line: problem.line,
                reason: problem.reason.to_owned(),
            };
            return Ok(answer(&rev, "invalid", Some(body), Vec::new()));
        }
    };
    let mut specs = Vec::with_capacity(config.specs.len());
    for entry in &config.specs {
        specs.push(follow(state, proof, &request_id, t, &rev, entry).await?);
    }
    Ok(answer(&rev, "valid", None, specs))
}

fn answer(
    rev: &ResolvedRev,
    config: &str,
    problem: Option<RepoConfigProblemBody>,
    specs: Vec<SpecRefBody>,
) -> SpecRefsBody {
    SpecRefsBody {
        rev: rev_body(rev),
        path: REPO_CONFIG_PATH.to_owned(),
        config: config.to_owned(),
        problem,
        specs,
    }
}

/// One entry, resolved for the reader (module docs, step 4).
async fn follow(
    state: &AppState,
    proof: &Authorized<ProjectCtx>,
    request_id: &forge_core::RequestId,
    t: &Target,
    rev: &ResolvedRev,
    entry: &SpecEntry,
) -> Result<SpecRefBody, ForgeError> {
    let scope = entry.scope.iter().map(|s| s.as_str().to_owned()).collect();
    let (written, lookup, pin) = match &entry.source {
        SpecSource::Path(p) => {
            return Ok(SpecRefBody {
                line: entry.line,
                kind: "path".to_owned(),
                repo: None,
                path: Some(p.as_str().to_owned()),
                pin: None,
                scope,
                status: "local".to_owned(),
                error_code: None,
                project_id: None,
                commit: Some(rev.commit.clone()), // clone: the answer owns its strings
            });
        }
        SpecSource::Repo { written, repo, pin } => (written, repo, pin),
    };
    // Every followed reference counts against the caller's limit (a
    // lookup and a ref resolution each).
    let Some(reader) = proof.principal().user() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "spec_refs_without_user")
        );
    };
    if let Err(retry) = state.spec_refs_limiter.check(reader, Instant::now()) {
        return Err(state.spec_refs_limiter.refused(retry));
    }
    let ctx = FollowCtx {
        resolver: state.resolver(),
        lookup_timeout: state.lookup_timeout(),
        browse: &t.smart.browse,
        request_id,
        project: t.project,
    };
    let followed = follow_ref(&ctx, Reader::Proof(proof), entry.line, lookup, pin).await?;
    let (status, error_code, project_id, commit) = match followed {
        Followed::Resolved {
            project, commit, ..
        } => ("resolved", None, Some(project.get()), Some(commit)),
        Followed::VersionNotFound { project } => (
            "version_not_found",
            Some(ErrorCode::SpecVersionNotFound),
            Some(project.get()),
            None,
        ),
        Followed::Unavailable => (
            "unavailable",
            Some(ErrorCode::SpecRepositoryUnavailable),
            None,
            None,
        ),
    };
    Ok(SpecRefBody {
        line: entry.line,
        kind: "repo".to_owned(),
        repo: Some(written.clone()), // clone: the answer owns its strings
        path: None,
        pin: Some(SpecPinBody {
            kind: pin.kind().to_owned(),
            name: pin.name().to_owned(),
            ref_name: pin.ref_name().as_str().to_owned(),
        }),
        scope,
        status: status.to_owned(),
        error_code: error_code.map(|c| c.id().to_owned()),
        project_id,
        commit,
    })
}

#[cfg(test)]
#[path = "spec_refs_tests.rs"]
mod tests;
