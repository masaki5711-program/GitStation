//! `GET|HEAD /api/v1/search` (`SearchQuery`, plan 20 K3, ADR-0040): one
//! search box over projects and merge requests, **showing only what the
//! caller may read**.
//!
//! - The search itself is decided on the instance by the registry (any
//!   active signed-in user, `read_api`, an agent too; never acting for a
//!   person, never CI): an anonymous or disabled caller loads nothing.
//! - **Decided before the read** (review of K3, HIGH): every project is
//!   decided with `authorize()` first, on the project list's access facts
//!   (`ProjectPageLoader`): `ProjectRead` for a project's entry, also
//!   `MergeRequestList` (Reporter) for its merge requests. The search
//!   statement then ranks, pages and skips **only** rows of those
//!   projects, so a short page, `has_more` or a `project=` filter can never
//!   reveal how many hidden rows match (a private project's id answers
//!   exactly like a missing one). Past [`MAX_PROJECT_SCAN`] projects the
//!   search is refused (`FG-KNOW-0001 reason=project_scan_limit`).
//! - **And again per hit**: a hit shows only when `prove(ProjectRead)`
//!   (and `MergeRequestList`) allow it on the same facts; nothing of a hit
//!   (title, path, snippet) is built before that.
//! - **No count**: the answer never says how many matched. A page scans
//!   at most [`SCAN_BUDGET`] candidates.
//! - **An AI audience** (`is_agent_audience`, plan 06 D-5) gets no
//!   `confidential` row: they are left out by the read itself, so neither
//!   shown nor counted.
//! - **Opaque cursor**: the position (candidates consumed) sealed for this
//!   caller, this query (its fingerprint, not its words), these filters
//!   and this audience; anything else is `FG-WEB-0006
//!   reason=cursor_invalid`.
//! - **Concurrency**: the list caps (`FG-WEB-0008 reason=list_concurrency`).
//! - **Snippets** are cut from the stored body; matches come back as
//!   character ranges (`[start, end)` in Unicode code points), never as
//!   HTML (the UI builds `<mark>` itself).
//! - Parameters: `q` (required, 2 ..= 200 characters after normalization,
//!   percent-encoded, `+` = space), `scope` (comma-separated `project`,
//!   `merge_request`), `project` (a positive id), `limit` (1 ..= 50,
//!   default 20), `cursor`. Anything else, or a repeated one, is
//!   `FG-WEB-0006 reason=param_invalid` (`param=q rule=empty|too_short|
//!   too_long|missing|malformed`, …), decided before any lookup.
//! - Failures of the reads are `FG-KNOW-0001` (the cause chained; the
//!   route's own bound with `timeout_ms`). The query's words are never
//!   logged: one `forge.search.query` line carries its length, term count,
//!   the rows scanned and shown.

use std::collections::HashMap;

use axum::http::header::{CACHE_CONTROL, RETRY_AFTER, VARY};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{prove, Action, InstanceCtx, Principal, ProjectCtx};
use forge_core::search::{CandidateFilter, SearchCandidate, SourceKind};
use forge_core::search_text::{snippet, SearchQuery};
use forge_core::{parse_positive_id, ProjectId, ProjectRecord, UserId};
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::dispatch::loaded_project;
use crate::registry::{Dispatched, QueryParam};
use crate::routes::lists::Lists;
use crate::routes::namespaces::percent_decode;

/// `GET /api/v1/search`.
pub const PATH: &str = "/api/v1/search";
/// The most candidates one page decides.
pub const SCAN_BUDGET: u32 = 200;
/// `limit` without the parameter.
const DEFAULT_LIMIT: usize = 20;
/// The largest `limit`.
const MAX_LIMIT: usize = 50;
/// The longest snippet, in characters.
const SNIPPET_CHARS: usize = 160;
/// The deepest position a cursor may hold (forge-db's offset bound).
const MAX_POSITION: u32 = 10_000;

/// The query parameters (`openapi.json`).
pub const QUERY: &[QueryParam] = &[
    QueryParam {
        name: "q",
        description: "The search words (2..=200 characters; Japanese is matched by 2-grams, other words by word, the last word as a prefix)",
        integer_range: None,
    },
    QueryParam {
        name: "scope",
        description: "Comma-separated kinds to search: project, merge_request (default: every kind)",
        integer_range: None,
    },
    QueryParam {
        name: "project",
        description: "Only hits of this project (its id)",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "limit",
        description: "Hits per page, 1..=50 (default 20); a page can be shorter when its scan budget ends first",
        integer_range: Some((1, 50)),
    },
    QueryParam {
        name: "cursor",
        description: "The previous page's next_cursor (opaque; bound to this caller, query and filters)",
        integer_range: None,
    },
];

/// A matched range of a snippet.
#[derive(Debug, Serialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchHighlight {
    /// First character (Unicode code point) of the match in `text`.
    pub start: u32,
    /// One past the last character.
    pub end: u32,
}

/// The text around the first match.
#[derive(Debug, Serialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchSnippet {
    pub text: String,
    /// Sorted, not overlapping.
    pub highlights: Vec<SearchHighlight>,
    /// Text was left out before / after `text`.
    pub cut_before: bool,
    pub cut_after: bool,
}

/// One hit the caller may read.
#[derive(Debug, Serialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    /// `project` or `merge_request`.
    pub kind: String,
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The project's current full path.
    pub project_path: String,
    pub title: String,
    /// The merge request's number in its project (`null` for a project).
    #[schema(required = true, minimum = 1)]
    pub iid: Option<i64>,
    pub snippet: SearchSnippet,
    /// The source's last change, UTC, RFC 3339.
    pub updated_at: String,
    /// The indexed text was cut (only its first part is searchable).
    pub truncated: bool,
}

/// A page of hits. There is no total: it would count what the caller
/// cannot read.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchPage {
    pub items: Vec<SearchHit>,
    /// Pass as `cursor` for the next page; `null` on the last one.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// More hits may follow (a page can be short, even empty, when its
    /// scan budget ended first).
    pub has_more: bool,
}

/// The parsed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) query: SearchQuery,
    pub(crate) query_chars: usize,
    pub(crate) kinds: Vec<SourceKind>,
    pub(crate) project: Option<ProjectId>,
    pub(crate) limit: usize,
    pub(crate) cursor: Option<String>,
}

/// Strict parameters (see the module docs). Pure.
pub(crate) fn parse(raw: Option<&str>) -> Result<Request, ForgeError> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut q: Option<&str> = None;
    let mut scope: Option<&str> = None;
    let mut project: Option<&str> = None;
    let mut limit: Option<&str> = None;
    let mut cursor: Option<&str> = None;
    for pair in raw.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (name, slot): (&'static str, &mut Option<&str>) = match key {
            "q" => ("q", &mut q),
            "scope" => ("scope", &mut scope),
            "project" => ("project", &mut project),
            "limit" => ("limit", &mut limit),
            "cursor" => ("cursor", &mut cursor),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if seen.contains(&name) {
            return Err(param_invalid(name, "repeated"));
        }
        seen.push(name);
        *slot = Some(value);
    }
    let raw_q = q.ok_or_else(|| param_invalid("q", "missing"))?;
    // A form-encoded `+` is a space.
    let text =
        percent_decode(&raw_q.replace('+', " ")).ok_or_else(|| param_invalid("q", "malformed"))?;
    if text.chars().any(char::is_control) {
        return Err(param_invalid("q", "control_character"));
    }
    let query = SearchQuery::parse(&text).map_err(|r| param_invalid("q", r.as_str()))?;
    let mut kinds: Vec<SourceKind> = Vec::new();
    if let Some(scope) = scope {
        for name in scope.split(',') {
            let kind =
                SourceKind::parse(name).ok_or_else(|| param_invalid("scope", "invalid_value"))?;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        kinds.sort_unstable();
    }
    let project = project
        .map(|raw| {
            parse_positive_id(raw)
                .map(ProjectId::new)
                .ok_or_else(|| param_invalid("project", "not_positive_decimal"))
        })
        .transpose()?;
    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_LIMIT)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    let cursor = match cursor {
        None => None,
        Some("") => return Err(param_invalid("cursor", "empty")),
        Some(raw) => Some(raw.to_owned()),
    };
    Ok(Request {
        query_chars: text.chars().count(),
        query,
        kinds,
        project,
        limit,
        cursor,
    })
}

/// The cursor's purpose: this caller, this query, these filters, this
/// audience.
fn purpose(viewer: UserId, request: &Request, agent: bool) -> String {
    let kinds: Vec<&str> = request.kinds.iter().map(|k| k.as_str()).collect();
    format!(
        "search:user={}:q={}:scope={}:project={}:audience={}",
        viewer.get(),
        request.query.fingerprint(),
        kinds.join(","),
        request.project.map_or(0, ProjectId::get),
        if agent { "agent" } else { "person" }
    )
}

fn open_cursor(lists: &Lists, purpose: &str, cursor: Option<&str>) -> Result<u32, ForgeError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let payload = lists.keys.decode(purpose, cursor, lists.cursor_max_age)?;
    <[u8; 4]>::try_from(payload.as_slice())
        .ok()
        .map(u32::from_be_bytes)
        .filter(|p| *p > 0 && *p <= MAX_POSITION)
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "cursor_invalid")
                .with_context("rule", "payload")
        })
}

/// A read failed: `FG-KNOW-0001`, the cause chained.
fn unavailable(operation: &'static str, cause: ForgeError) -> ForgeError {
    ForgeError::new(ErrorCode::SearchUnavailable)
        .with_context("operation", operation)
        .with_context("cause_code", cause.code().id())
        .with_source(cause)
}

/// One read, bounded on the client side (forge-db bounds its statement).
async fn bounded<T>(
    state: &AppState,
    operation: &'static str,
    call: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    let limit = state.lookup_timeout;
    match tokio::time::timeout(limit, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(unavailable(operation, e)),
        Err(_) => Err(ForgeError::new(ErrorCode::SearchUnavailable)
            .with_context("operation", operation)
            .with_context("timeout_ms", limit.as_millis())),
    }
}

/// What a project allows the caller, decided once per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Allowed {
    project: bool,
    merge_requests: bool,
}

/// A denial hides the hit; anything that is not a denial fails the page.
fn allowed(result: Result<(), ForgeError>) -> Result<bool, ForgeError> {
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.code() == ErrorCode::Internal => Err(e),
        Err(_) => Ok(false),
    }
}

fn decide(principal: &Principal, record: &ProjectRecord) -> Result<Allowed, ForgeError> {
    let ctx = || ProjectCtx::new(loaded_project(record.id, record.access));
    let project = allowed(prove(principal, Action::ProjectRead, ctx()).map(|_| ()))?;
    // A merge request needs the project too (never a hit of a project the
    // caller cannot read).
    let merge_requests =
        project && allowed(prove(principal, Action::MergeRequestList, ctx()).map(|_| ()))?;
    Ok(Allowed {
        project,
        merge_requests,
    })
}

/// The hit of `candidate`, built only after its project allowed it.
fn hit(candidate: &SearchCandidate, record: &ProjectRecord, query: &SearchQuery) -> SearchHit {
    let s = snippet(&candidate.body, query, SNIPPET_CHARS);
    let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    SearchHit {
        kind: candidate.kind.as_str().to_owned(),
        project_id: record.id.get(),
        project_path: record.path.full_path(),
        title: candidate.title.clone(), // clone: the candidate is borrowed
        iid: match candidate.kind {
            SourceKind::MergeRequest => candidate.key.parse::<i64>().ok().filter(|n| *n > 0),
            SourceKind::Project => None,
        },
        snippet: SearchSnippet {
            text: s.text,
            highlights: s
                .highlights
                .into_iter()
                .map(|(start, end)| SearchHighlight {
                    start: to_u32(start),
                    end: to_u32(end),
                })
                .collect(),
            cut_before: s.cut_before,
            cut_after: s.cut_after,
        },
        updated_at: candidate.updated_at.clone(), // clone: the candidate is borrowed
        truncated: candidate.truncated,
    }
}

/// One page as decided.
#[derive(Debug, PartialEq)]
pub(crate) struct Decided {
    pub(crate) items: Vec<SearchHit>,
    /// Candidates consumed (shown or hidden).
    pub(crate) consumed: usize,
    pub(crate) hidden: usize,
}

/// Walk `candidates` (best first, at most `budget + 1`), keeping what the
/// caller may read, until `limit` are kept or `budget` are consumed. Pure
/// apart from `authorize()`.
pub(crate) fn walk(
    principal: &Principal,
    candidates: &[SearchCandidate],
    records: &[ProjectRecord],
    query: &SearchQuery,
    limit: usize,
    budget: usize,
) -> Result<Decided, ForgeError> {
    let by_id: HashMap<ProjectId, &ProjectRecord> = records.iter().map(|r| (r.id, r)).collect();
    let mut decided: HashMap<ProjectId, Allowed> = HashMap::new();
    let agent = principal.is_agent_audience();
    let mut out = Decided {
        items: Vec::with_capacity(limit),
        consumed: 0,
        hidden: 0,
    };
    for candidate in candidates.iter().take(budget) {
        out.consumed += 1;
        // A project that no longer exists has no record: hidden.
        let Some(record) = by_id.get(&candidate.project).copied() else {
            out.hidden += 1;
            continue;
        };
        let allow = match decided.get(&candidate.project) {
            Some(a) => *a,
            None => {
                let a = decide(principal, record)?;
                decided.insert(candidate.project, a);
                a
            }
        };
        let shown = match candidate.kind {
            SourceKind::Project => allow.project,
            SourceKind::MergeRequest => allow.merge_requests,
        } && !(agent && candidate.classification == "confidential");
        if !shown {
            out.hidden += 1;
            continue;
        }
        out.items.push(hit(candidate, record, query));
        if out.items.len() == limit {
            break;
        }
    }
    Ok(out)
}

/// Projects scanned per page for the readable sets (the list's scan size).
const PROJECT_PAGE: u32 = 1_000;
/// The most projects one search scans for its readable sets; past it the
/// search is refused (`FG-KNOW-0001 reason=project_scan_limit`) rather than
/// answered from an incomplete set (ADR-0040: an SQL superset then).
const MAX_PROJECT_SCAN: usize = 20_000;

/// What the caller may read, decided before any search row is read.
#[derive(Debug, Default)]
pub(crate) struct Readable {
    /// Projects whose own entry may show (`ProjectRead`).
    pub(crate) projects: Vec<ProjectId>,
    /// Projects whose merge requests may show (also `MergeRequestList`).
    pub(crate) merge_requests: Vec<ProjectId>,
    /// The allowed projects' records (paths for the hits).
    pub(crate) records: Vec<ProjectRecord>,
}

/// The readable sets of `viewer`: every project in id order through the
/// project list's loader (the same access facts), each decided with
/// `authorize()`; only `only` when the request names a project (a project
/// the caller cannot read then gives the same empty sets as one that does
/// not exist).
async fn readable(
    state: &AppState,
    lists: &Lists,
    principal: &Principal,
    viewer: UserId,
    only: Option<ProjectId>,
) -> Result<Readable, ForgeError> {
    let mut out = Readable::default();
    let mut after: Option<ProjectId> = None;
    let mut scanned = 0usize;
    let page_rows = usize::try_from(PROJECT_PAGE).unwrap_or(usize::MAX);
    loop {
        let rows = bounded(
            state,
            "search_projects",
            lists
                .projects
                .project_page(after, viewer, None, PROJECT_PAGE),
        )
        .await?;
        scanned += rows.len();
        let full = rows.len() >= page_rows;
        after = rows.last().map(|r| r.id);
        for record in rows {
            if only.is_some_and(|p| p != record.id) {
                continue;
            }
            let allow = decide(principal, &record)?;
            if !allow.project {
                continue;
            }
            out.projects.push(record.id);
            if allow.merge_requests {
                out.merge_requests.push(record.id);
            }
            out.records.push(record);
        }
        if !full {
            return Ok(out);
        }
        if scanned >= MAX_PROJECT_SCAN {
            return Err(ForgeError::new(ErrorCode::SearchUnavailable)
                .with_context("operation", "search_projects")
                .with_context("reason", "project_scan_limit")
                .with_context("limit", MAX_PROJECT_SCAN));
        }
    }
}

async fn page(
    state: &AppState,
    raw_query: Option<&str>,
    principal: &Principal,
    request_id: &str,
) -> Result<SearchPage, ForgeError> {
    let lists = state.lists.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "search_unconfigured")
    })?;
    let reader = lists.search.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "search_unconfigured")
    })?;
    let request = parse(raw_query)?;
    let viewer = principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "search_viewer_missing")
    })?;
    let agent = principal.is_agent_audience();
    let purpose = purpose(viewer, &request, agent);
    let offset = open_cursor(lists, &purpose, request.cursor.as_deref())?;
    let _slot = lists.acquire_slot(viewer)?;
    let readable = readable(state, lists, principal, viewer, request.project)
        .await
        .map_err(|e| with_query(e, &request))?;
    let filter = CandidateFilter {
        kinds: request.kinds.clone(), // clone: the filter owns its kinds
        project: request.project,
        exclude_confidential: agent,
        readable_projects: readable.projects,
        readable_merge_requests: readable.merge_requests,
    };
    let candidates = if filter.readable_projects.is_empty() {
        // Nothing the caller may read: no read at all.
        Vec::new()
    } else {
        let tsquery = request.query.tsquery_literal();
        bounded(
            state,
            "search_candidates",
            reader.candidates(&tsquery, &filter, offset, SCAN_BUDGET + 1),
        )
        .await
        .map_err(|e| with_query(e, &request))?
    };
    let budget = usize::try_from(SCAN_BUDGET).unwrap_or(usize::MAX);
    // Snippets cost CPU (normalizing stored text): off the async workers.
    let (who, query, limit) = (principal.clone(), request.query.clone(), request.limit);
    let records = readable.records;
    let (decided, candidates) = tokio::task::spawn_blocking(move || {
        walk(&who, &candidates, &records, &query, limit, budget).map(|d| (d, candidates))
    })
    .await
    .map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "search_walk_failed")
            .with_context("panicked", e.is_panic())
    })??;
    let consumed = u32::try_from(decided.consumed).unwrap_or(u32::MAX);
    let next = offset.saturating_add(consumed);
    let has_more = candidates.len() > decided.consumed && next <= MAX_POSITION;
    let next_cursor = if has_more {
        Some(lists.keys.encode(&purpose, &next.to_be_bytes())?)
    } else {
        None
    };
    tracing::info!(
        target: "forge_web",
        event = "forge.search.query",
        request_id,
        query_chars = request.query_chars,
        query_terms = request.query.terms().len(),
        scanned = decided.consumed,
        shown = decided.items.len(),
        hidden = decided.hidden,
        "search answered"
    );
    Ok(SearchPage {
        items: decided.items,
        next_cursor,
        has_more,
    })
}

/// The query's shape (never its words) on a failed read.
fn with_query(e: ForgeError, request: &Request) -> ForgeError {
    e.with_context("query_chars", request.query_chars)
        .with_context("query_terms", request.query.terms().len())
}

/// `GET|HEAD /api/v1/search`.
pub async fn search(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    // Owned copies: the request (its body is not Sync) stays out of the await.
    let (query, principal, request_id) = (
        request.raw_query().map(str::to_owned),
        request.principal().clone(),
        request.ctx().request_id.to_string(),
    );
    let page = page(&state, query.as_deref(), &principal, &request_id).await;
    let page = match page {
        Ok(page) => page,
        Err(e) => {
            let busy = e.code() == ErrorCode::ServerBusy;
            let mut res = request.ctx().fail(e).into_response();
            if busy {
                res.headers_mut()
                    .insert(RETRY_AFTER, HeaderValue::from_static("1"));
            }
            return res;
        }
    };
    let mut res = Json(page).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
