//! The spec API (`SpecRead`, `read_api`; M3 X4, plan 03 §7.2, ADR-0023
//! Amendment X4): a project's requirement index at a revision, read from
//! its repository with the reader's permissions.
//!
//! | Route | Answer |
//! |---|---|
//! | `GET …/specs/requirements[?ref=&q=&limit=]` | the own specification's documents, requirements (a case-insensitive search of id and body) and document problems |
//! | `GET …/specs/versions` | the version tags (`forge_core::spec_version`), newest first per series |
//! | `GET …/specs/diff?from=&to=` | requirement by requirement between two revisions |
//! | `GET …/specs/trace[?ref=]` | each requirement with the `@req` tests naming it: the own specification, and every `repo:` entry's (within its `scope:`) followed **for the reader** |
//!
//! (`…` is `/api/v1/projects/{id}`.) The registry authenticates, the
//! loader reads the project by id and `prove(SpecRead)` decides on it
//! (RepoBrowse's rule). A revision is the browse API's (`ref`: a full
//! commit id, a full or short branch / tag name; default: the default
//! branch). The index is forge-checks' `spec_index` (the same bounds as
//! `forge/spec-lint`; a bound hit is `incomplete {error_code:
//! FG-REPO-0031, reason}` in the answer, never a silent cut). A trace's
//! reference is followed with `forge_checks::spec_refs::follow`
//! (`Reader::Proof`: looked up as the reader, decided again with
//! `prove_spec_ref`); one it cannot follow for the reader is `unavailable`
//! (`FG-REPO-0026`) with no requirement — never what the reader may not
//! read. Each request indexes at most two revisions plus the references
//! and counts against the caller's limit (`FG-WEB-0010`); one budget
//! bounds it (`FG-WEB-0002 operation=spec_index`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{Authorized, ProjectCtx};
use forge_checks::spec_index::{index_commit, IndexLimits, Indexed, Tags};
use forge_checks::spec_refs::{follow, FollowCtx, Followed, Reader, RepoConfigRead};
use forge_core::repo_config::SpecSource;
use forge_core::spec_index::{diff, lint, trace_table, ExternalScope, TraceTableRow};
use forge_core::spec_version::SpecVersion;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{RefKind, ResolvedRev};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{DispatchState, Dispatched, QueryParam};
use crate::routes::browse::{query, required, rev_body, take, RepositoryOf, RevBody, Target};

pub const REQUIREMENTS: &str = "/api/v1/projects/{id}/specs/requirements";
pub const VERSIONS: &str = "/api/v1/projects/{id}/specs/versions";
pub const DIFF: &str = "/api/v1/projects/{id}/specs/diff";
pub const TRACE: &str = "/api/v1/projects/{id}/specs/trace";

pub const REQUIREMENTS_OF: RepositoryOf = RepositoryOf("/specs/requirements");
pub const VERSIONS_OF: RepositoryOf = RepositoryOf("/specs/versions");
pub const DIFF_OF: RepositoryOf = RepositoryOf("/specs/diff");
pub const TRACE_OF: RepositoryOf = RepositoryOf("/specs/trace");

/// One budget for a whole request (every git call and lookup).
pub const BUDGET: Duration = Duration::from_secs(30);
/// Requests per caller per [`LIMIT_WINDOW`] (`FG-WEB-0010`): each indexes
/// up to two revisions.
pub const LIMIT: u32 = 120;
pub const LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// Requirements one answer lists by default, and at most.
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
/// Document problems one answer lists at most.
pub const MAX_PROBLEMS: usize = 200;
/// Test places listed per requirement of a trace (the count is whole).
pub const MAX_TESTS_PER_ROW: usize = 20;
/// Versions one answer lists at most.
pub const MAX_VERSIONS: usize = 200;
/// The longest search.
pub const MAX_QUERY_BYTES: usize = 256;

/// The per-caller limit of spec API requests.
pub fn limiter() -> crate::routes::namespaces::CallerLimiter {
    crate::routes::namespaces::CallerLimiter::new("spec_index", LIMIT, LIMIT_WINDOW)
}

const REF_PARAM: QueryParam = QueryParam {
    name: "ref",
    description: "A full commit id, refs/heads/… or refs/tags/…, or a short name (branch first, then tag); default: the default branch",
    integer_range: None,
};

pub const REQUIREMENTS_QUERY: &[QueryParam] = &[
    REF_PARAM,
    QueryParam {
        name: "q",
        description:
            "Only requirements whose id or text contains this (case-insensitive, at most 256 bytes)",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "At most this many requirements (default 100)",
        integer_range: Some((1, MAX_LIMIT as i64)),
    },
];

pub const DIFF_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "from",
        description: "The earlier revision (a commit id, a branch or a tag)",
        integer_range: None,
    },
    QueryParam {
        name: "to",
        description: "The later revision (a commit id, a branch or a tag)",
        integer_range: None,
    },
];

pub const TRACE_QUERY: &[QueryParam] = &[REF_PARAM];

// ── the answers ────────────────────────────────────────────────────────

/// A bound the index hit.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecIncompleteBody {
    /// `FG-REPO-0031`.
    pub error_code: String,
    /// `spec_files`, `spec_bytes` or `tag_lines`.
    pub reason: String,
}

/// One specification document.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecDocumentBody {
    pub path: String,
    /// `spec:` (`null` when the front matter cannot be used).
    #[schema(required = true)]
    pub spec: Option<String>,
    #[schema(required = true)]
    pub title: Option<String>,
    /// `draft`, `review`, `approved` or `deprecated`.
    #[schema(required = true)]
    pub status: Option<String>,
    /// Requirement blocks read.
    pub requirements: usize,
    /// Rules the document breaks (they are in `problems`).
    pub problems: usize,
}

/// One requirement.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecRequirementBody {
    pub id: String,
    pub path: String,
    /// The opening line (1-based).
    pub line: usize,
    /// `must`, `should`, `could`, `wont`.
    #[schema(required = true)]
    pub priority: Option<String>,
    pub deprecated: bool,
    /// The document's `spec:`.
    #[schema(required = true)]
    pub spec: Option<String>,
    /// The block's text.
    pub body: String,
}

/// One rule broken.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecProblemBody {
    /// `FG-REPO-0028`.
    pub error_code: String,
    pub path: String,
    /// 1-based; 0 for the whole file.
    pub line: usize,
    pub rule: String,
    #[schema(required = true)]
    pub detail: Option<String>,
}

/// `GET …/specs/requirements`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecRequirementsBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// `.forge/repo.yml`: `absent`, `valid` or `invalid`.
    pub config: String,
    /// The own specification directories read.
    pub dirs: Vec<String>,
    pub documents: Vec<SpecDocumentBody>,
    /// By id (the first definition of an id defined twice).
    pub requirements: Vec<SpecRequirementBody>,
    /// More requirements matched than `limit`.
    pub truncated: bool,
    /// The documents' rules (duplicates included; a broken `@req`
    /// reference needs the tags: forge/spec-lint and the trace), at most 200.
    pub problems: Vec<SpecProblemBody>,
    #[schema(required = true)]
    pub incomplete: Option<SpecIncompleteBody>,
}

/// One version tag.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecVersionBody {
    /// The tag (`v1.5`).
    pub name: String,
    /// `refs/tags/…`.
    pub full_name: String,
    /// The commit it peels to.
    pub commit: String,
}

/// `GET …/specs/versions`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecVersionsBody {
    /// Newest first within a series (`v` / `spec-v`), series by name.
    pub versions: Vec<SpecVersionBody>,
    /// More versions than one answer lists, or more refs than one listing
    /// keeps.
    pub truncated: bool,
}

/// One requirement's change.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecChangeBody {
    pub id: String,
    /// `added`, `changed`, `removed` or `deprecated`.
    pub kind: String,
    /// Where it is in `to` (in `from` when removed).
    pub path: String,
    pub line: usize,
}

/// `GET …/specs/diff`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecDiffBody {
    pub from: RevBody,
    pub to: RevBody,
    /// By id.
    pub changes: Vec<SpecChangeBody>,
    /// Either revision's index hit a bound.
    #[schema(required = true)]
    pub incomplete: Option<SpecIncompleteBody>,
}

/// Where a test is.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecTestPlaceBody {
    pub path: String,
    pub line: usize,
}

/// One requirement with the tests naming it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecTraceRowBody {
    pub id: String,
    /// The own directory (`spec/`) or the `repo:` path as written.
    pub source: String,
    /// In its source repository.
    pub path: String,
    pub line: usize,
    pub deprecated: bool,
    /// `tested` or `untested`.
    pub status: String,
    /// Every `@req` tag of this repository naming it.
    pub test_count: usize,
    /// The first 20 of them.
    pub tests: Vec<SpecTestPlaceBody>,
}

/// One `repo:` entry of the trace.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecTraceReferenceBody {
    /// Its line in `.forge/repo.yml`.
    pub line: usize,
    /// As written.
    pub repo: String,
    /// `resolved`, `version_not_found` or `unavailable`.
    pub status: String,
    /// `FG-REPO-0025` / `FG-REPO-0026`; `null` when resolved.
    #[schema(required = true)]
    pub error_code: Option<String>,
    /// Its requirements within the entry's scope.
    pub requirements: usize,
}

/// `GET …/specs/trace`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SpecTraceBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// The own specification's requirements by id, then each followed
    /// reference's in file order.
    pub requirements: Vec<SpecTraceRowBody>,
    /// `repo:` entries in file order (none when the file cannot be used).
    pub references: Vec<SpecTraceReferenceBody>,
    /// An index (this revision's or a reference's) hit a bound.
    #[schema(required = true)]
    pub incomplete: Option<SpecIncompleteBody>,
}

// ── the handlers ───────────────────────────────────────────────────────

fn incomplete(reason: Option<&'static str>) -> Option<SpecIncompleteBody> {
    reason.map(|r| SpecIncompleteBody {
        error_code: ErrorCode::SpecIndexIncomplete.id().to_owned(),
        reason: r.to_owned(),
    })
}

fn config_word(c: &RepoConfigRead) -> &'static str {
    match c {
        RepoConfigRead::Absent => "absent",
        RepoConfigRead::Valid(_) => "valid",
        RepoConfigRead::Invalid(_) => "invalid",
    }
}

/// Run `work` for a signed-in caller under the limit and the budget, and
/// answer it (or its error with the route and the target's ids).
async fn respond<T, F, Fut>(
    state: AppState,
    request: Dispatched<ProjectCtx>,
    route: &'static str,
    work: F,
) -> Response
where
    T: Serialize,
    F: FnOnce(AppState, Dispatched<ProjectCtx>, Target) -> Fut,
    Fut: std::future::Future<Output = Result<T, ForgeError>>,
{
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let target = match Target::of(&state, &request) {
        Ok(t) => t,
        Err(err) => return ctx.fail(err.with_context("route", route)).into_response(),
    };
    let ids = (target.project.get(), target.repository.id.get());
    let caller = match request.proof().principal().user() {
        Some(user) => user,
        None => {
            return ctx
                .fail(
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "spec_index_without_user")
                        .with_context("route", route),
                )
                .into_response()
        }
    };
    // Review X4 (MEDIUM f): at most `MAX_IN_FLIGHT` of a caller's spec
    // requests at once, and the limit counts every index a request runs
    // (one here; `charge` adds the rest).
    let Some(_running) = state.spec_index_in_flight.enter(caller) else {
        return ctx
            .fail(
                ForgeError::new(ErrorCode::RateLimited)
                    .with_context("route", route)
                    .with_context("reason", "in_flight")
                    .with_context("limit", MAX_IN_FLIGHT)
                    .with_context(crate::error::RETRY_AFTER_CONTEXT, 1u64),
            )
            .into_response();
    };
    if let Err(err) = charge(&state, caller, 1) {
        return ctx.fail(err.with_context("route", route)).into_response();
    }
    let ran = match tokio::time::timeout(BUDGET, work(state.clone(), request, target)).await {
        Ok(ran) => ran,
        // Dropping the future stops its git work (killed and reaped by
        // forge-git) and its lookups.
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", "spec_index")
            .with_context("timeout_ms", BUDGET.as_millis())),
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
                err.with_context("route", route)
                    .with_context("project_id", ids.0)
                    .with_context("repo_id", ids.1),
            )
            .into_response(),
    }
}

/// Count `n` more indexes of `caller`'s request against the limit
/// (`FG-WEB-0010` past it).
fn charge(state: &AppState, caller: forge_core::UserId, n: usize) -> Result<(), ForgeError> {
    for _ in 0..n {
        if let Err(retry) = state.spec_index_limiter.check(caller, Instant::now()) {
            return Err(state.spec_index_limiter.refused(retry));
        }
    }
    Ok(())
}

/// The caller of a request (every spec route needs a signed-in one).
fn caller_of(request: &Dispatched<ProjectCtx>) -> Result<forge_core::UserId, ForgeError> {
    request.proof().principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "spec_index_without_user")
    })
}

/// At most this many spec requests of one caller run at once.
pub const MAX_IN_FLIGHT: usize = 2;

/// The spec requests each caller has running (review X4, MEDIUM f): an
/// index is git work, so one caller cannot hold the repository slots.
#[derive(Debug)]
pub struct InFlight {
    max: usize,
    running: std::sync::Mutex<std::collections::HashMap<i64, usize>>,
}

/// One running request; ends on drop.
#[derive(Debug)]
pub struct Running<'a> {
    gate: &'a InFlight,
    caller: i64,
}

impl InFlight {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            running: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// A place for one more of `caller`'s requests, or `None` at the cap.
    pub fn enter(&self, caller: forge_core::UserId) -> Option<Running<'_>> {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = running.entry(caller.get()).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(Running {
            gate: self,
            caller: caller.get(),
        })
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new(MAX_IN_FLIGHT)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut running = self
            .gate
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = running.get_mut(&self.caller) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                running.remove(&self.caller);
            }
        }
    }
}

/// The revision `name` names, or the default branch.
async fn revision(t: &Target, name: Option<&str>) -> Result<ResolvedRev, ForgeError> {
    let browse = &t.smart.browse;
    let repo = t.repository.id;
    match name {
        Some("") => Err(param_invalid("ref", "empty")),
        Some(name) => browse.resolve_ref(repo, name, &t.principal).await,
        None => {
            let list = browse.refs(repo).await?;
            let Some(branch) = list.default_branch.as_deref() else {
                return Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found"));
            };
            browse
                .resolve_ref(repo, &format!("refs/heads/{branch}"), &t.principal)
                .await
        }
    }
}

async fn index(t: &Target, rev: &ResolvedRev, tags: Tags) -> Result<Indexed, ForgeError> {
    index_commit(
        &t.smart.browse,
        t.repository.id,
        &rev.commit,
        &IndexLimits::default(),
        tags,
    )
    .await
}

fn limit_param(params: &[(&'static str, String)]) -> Result<usize, ForgeError> {
    match take(params, "limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| param_invalid("limit", "out_of_range")),
    }
}

/// `GET /api/v1/projects/{id}/specs/requirements`.
pub async fn requirements(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "specs_requirements",
        |_, request, t| async move {
            let params = query(request.raw_query(), &["ref", "q", "limit"])?;
            let limit = limit_param(&params)?;
            let needle = match take(&params, "q") {
                Some(q) if q.len() > MAX_QUERY_BYTES => return Err(param_invalid("q", "too_long")),
                Some("") | None => None,
                Some(q) => Some(q.to_lowercase()),
            };
            let rev = revision(&t, take(&params, "ref")).await?;
            let indexed = index(&t, &rev, Tags::Skip).await?;
            let specs: BTreeMap<&str, Option<&str>> = indexed
                .index
                .documents
                .iter()
                .map(|d| {
                    (
                        d.path.as_str(),
                        d.doc.meta.as_ref().map(|m| m.spec.as_str()),
                    )
                })
                .collect();
            let matching: Vec<SpecRequirementBody> = indexed
                .index
                .requirements()
                .into_iter()
                .filter(|l| {
                    needle.as_ref().is_none_or(|n| {
                        l.requirement.id.as_str().to_lowercase().contains(n)
                            || l.requirement.body.to_lowercase().contains(n)
                    })
                })
                .map(|l| SpecRequirementBody {
                    id: l.requirement.id.as_str().to_owned(),
                    path: l.path.to_owned(),
                    line: l.requirement.line,
                    priority: l.requirement.priority.map(|p| p.as_str().to_owned()),
                    deprecated: l.requirement.deprecated,
                    spec: specs.get(l.path).copied().flatten().map(str::to_owned),
                    body: l.requirement.body.clone(), // clone: the answer owns its text
                })
                .collect();
            let truncated = matching.len() > limit;
            let problems = lint(&indexed.index, &ExternalScope::everything())
                .into_iter()
                .take(MAX_PROBLEMS)
                .map(|f| SpecProblemBody {
                    error_code: ErrorCode::SpecLintProblem.id().to_owned(),
                    path: f.path,
                    line: f.line,
                    rule: f.rule.to_owned(),
                    detail: f.detail,
                })
                .collect();
            let documents = indexed
                .index
                .documents
                .iter()
                .map(|d| SpecDocumentBody {
                    path: d.path.clone(), // clone: the answer owns its strings
                    spec: d.doc.meta.as_ref().map(|m| m.spec.clone()), // clone: as above
                    title: d.doc.meta.as_ref().map(|m| m.title.clone()), // clone: as above
                    status: d.doc.meta.as_ref().map(|m| m.status.as_str().to_owned()),
                    requirements: d.doc.requirements.len(),
                    problems: d.doc.problems.len() + d.doc.problems_dropped,
                })
                .collect();
            Ok(SpecRequirementsBody {
                rev: rev_body(&rev),
                config: config_word(&indexed.config).to_owned(),
                dirs: indexed.dirs.clone(), // clone: the answer owns its strings
                documents,
                requirements: matching.into_iter().take(limit).collect(),
                truncated,
                problems,
                incomplete: incomplete(indexed.index.incomplete),
            })
        },
    )
    .await
}

/// `GET /api/v1/projects/{id}/specs/versions`.
pub async fn versions(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "specs_versions",
        |_, request, t| async move {
            let _params = query(request.raw_query(), &[])?;
            let list = t.smart.browse.refs(t.repository.id).await?;
            let mut found: Vec<(SpecVersion, SpecVersionBody)> = list
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::Tag)
                .filter_map(|r| {
                    let commit = r.commit.clone()?; // clone: the answer owns its id
                    let v = SpecVersion::parse(r.short())?;
                    Some((
                        v,
                        SpecVersionBody {
                            name: r.short().to_owned(),
                            full_name: r.name.as_str().to_owned(),
                            commit,
                        },
                    ))
                })
                .collect();
            // Series by name, newest first within one; the same version's two
            // spellings by name (deterministic).
            found.sort_by(|(a, an), (b, bn)| {
                a.series()
                    .cmp(b.series())
                    .then_with(|| b.cmp(a))
                    .then_with(|| bn.name.cmp(&an.name))
            });
            let truncated = list.truncated || found.len() > MAX_VERSIONS;
            Ok(SpecVersionsBody {
                versions: found
                    .into_iter()
                    .take(MAX_VERSIONS)
                    .map(|(_, body)| body)
                    .collect(),
                truncated,
            })
        },
    )
    .await
}

/// `GET /api/v1/projects/{id}/specs/diff`.
pub async fn diff_route(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "specs_diff",
        |state, request, t| async move {
            let params = query(request.raw_query(), &["from", "to"])?;
            // Two revisions: one more index.
            charge(&state, caller_of(&request)?, 1)?;
            let from = revision(&t, Some(required(&params, "from")?)).await?;
            let to = revision(&t, Some(required(&params, "to")?)).await?;
            let before = index(&t, &from, Tags::Skip).await?;
            let after = index(&t, &to, Tags::Skip).await?;
            let changes = diff(&before.index, &after.index)
                .into_iter()
                .map(|c| SpecChangeBody {
                    id: c.id.as_str().to_owned(),
                    kind: c.kind.as_str().to_owned(),
                    path: c.path,
                    line: c.line,
                })
                .collect();
            Ok(SpecDiffBody {
                from: rev_body(&from),
                to: rev_body(&to),
                changes,
                incomplete: incomplete(before.index.incomplete.or(after.index.incomplete)),
            })
        },
    )
    .await
}

fn trace_rows(
    source: &str,
    rows: Vec<TraceTableRow>,
) -> impl Iterator<Item = SpecTraceRowBody> + '_ {
    rows.into_iter().map(move |r| SpecTraceRowBody {
        id: r.id.as_str().to_owned(),
        source: source.to_owned(),
        path: r.path,
        line: r.line,
        deprecated: r.deprecated,
        status: if r.test_count > 0 {
            "tested"
        } else {
            "untested"
        }
        .to_owned(),
        test_count: r.test_count,
        tests: r
            .tests
            .into_iter()
            .map(|(path, line)| SpecTestPlaceBody { path, line })
            .collect(),
    })
}

/// `GET /api/v1/projects/{id}/specs/trace`.
pub async fn trace(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "specs_trace",
        |state, request, t| async move {
            let params = query(request.raw_query(), &["ref"])?;
            let request_id = request.ctx().request_id.clone(); // clone: logged after awaits
            let proof = request.proof();
            let rev = revision(&t, take(&params, "ref")).await?;
            let own = index(&t, &rev, Tags::Read).await?;
            let mut reason = own.index.incomplete;
            let mut requirements: Vec<SpecTraceRowBody> = trace_rows(
                &own.dirs.join(", "),
                trace_table(&own.index, None, &own.index, MAX_TESTS_PER_ROW),
            )
            .collect();
            let mut references = Vec::new();
            if let RepoConfigRead::Valid(config) = &own.config {
                // One more index per reference followed.
                let refs = config
                    .specs
                    .iter()
                    .filter(|e| matches!(e.source, SpecSource::Repo { .. }))
                    .count();
                charge(&state, caller_of(&request)?, refs)?;
                for entry in &config.specs {
                    let SpecSource::Repo { written, repo, pin } = &entry.source else {
                        continue;
                    };
                    let followed =
                        follow_one(&state, proof, &request_id, &t, entry.line, repo, pin).await?;
                    let (status, code, rows) = match followed {
                        Followed::Resolved {
                            repository, commit, ..
                        } => {
                            let theirs = index_commit(
                                &t.smart.browse,
                                repository,
                                &commit,
                                &IndexLimits::default(),
                                Tags::Skip,
                            )
                            .await?;
                            reason = reason.or(theirs.index.incomplete);
                            let scope = (!entry.scope.is_empty()).then_some(entry.scope.as_slice());
                            let rows =
                                trace_table(&theirs.index, scope, &own.index, MAX_TESTS_PER_ROW);
                            ("resolved", None, rows)
                        }
                        Followed::VersionNotFound { .. } => (
                            "version_not_found",
                            Some(ErrorCode::SpecVersionNotFound),
                            Vec::new(),
                        ),
                        Followed::Unavailable => (
                            "unavailable",
                            Some(ErrorCode::SpecRepositoryUnavailable),
                            Vec::new(),
                        ),
                    };
                    references.push(SpecTraceReferenceBody {
                        line: entry.line,
                        repo: written.clone(), // clone: the answer owns its strings
                        status: status.to_owned(),
                        error_code: code.map(|c| c.id().to_owned()),
                        requirements: rows.len(),
                    });
                    requirements.extend(trace_rows(written, rows));
                }
            }
            Ok(SpecTraceBody {
                rev: rev_body(&rev),
                requirements,
                references,
                incomplete: incomplete(reason),
            })
        },
    )
    .await
}

/// One `repo:` entry followed for the reader (every followed reference
/// counts against the spec references' per-caller limit, as in X1).
async fn follow_one(
    state: &AppState,
    proof: &Authorized<ProjectCtx>,
    request_id: &forge_core::RequestId,
    t: &Target,
    line: usize,
    repo: &forge_core::LookupPath,
    pin: &forge_core::repo_config::SpecPin,
) -> Result<Followed, ForgeError> {
    let Some(reader) = proof.principal().user() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "spec_index_without_user")
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
    follow(&ctx, Reader::Proof(proof), line, repo, pin).await
}

#[cfg(test)]
#[path = "specs_tests.rs"]
mod tests;
