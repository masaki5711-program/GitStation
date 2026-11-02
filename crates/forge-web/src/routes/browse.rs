//! The repository browse API (`RepoBrowse`, `read_api`; M2 MR7, ADR-0015
//! Amendment M2, plan 10 §5):
//!
//! - `GET /api/v1/projects/{id}/repository/refs[?type=branch|tag]` — the
//!   branches and tags (each with the commit it peels to), the default
//!   branch, `truncated` past the ref cap;
//! - `GET …/repository/resolve?spec=<ref>/<path>` — split a URL tail into a
//!   revision and a path (ADR-0015 order: full commit id, longest branch,
//!   longest tag);
//! - `GET …/repository/tree?ref=&path=` — one tree level (`truncated` past
//!   the entry cap);
//! - `GET …/repository/blob?ref=&path=` — a file's head as text
//!   (`content`, at most 1 MiB, `truncated`), or `binary: true` without
//!   content (NUL or not UTF-8; the raw route serves it).
//!
//! The registry authenticates, [`RepositoryOf`] loads the project by id
//! (one loader call; the row carries the repository), `prove(RepoBrowse)`
//! decides. Only then is anything about the repository looked at: a
//! repository still provisioning is `FG-REPO-0016`; a ref or a path that
//! does not resolve is `FG-REPO-0023` (`reason`). git runs only through
//! forge-git's browse API (`forge_git::browse::Browser`): resolved ids
//! after `--end-of-options`, paths after `--`, one 10 s deadline per
//! request, capped output, a per-repository cap.
//!
//! Query parameters are strict: unknown or repeated ones, a missing `ref`
//! / `spec` and a path breaking [`RepoPath`]'s rules are `FG-WEB-0006
//! reason=param_invalid` (the value is never logged). `ref` and `path` are
//! percent-decoded once (`%2F` is a `/`).

use std::sync::Arc;

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::request::Parts;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::ProjectCtx;
use forge_core::{ProjectId, ProjectRepository, RepositoryState, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Blob, RefKind, RefList, RepoPath, ResolvedRev, Tree, TreeEntry};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::git::SmartHttp;
use crate::routes::namespaces::percent_decode;
use crate::routes::projects::{self, PREFIX as PROJECT_PREFIX};
use crate::routes::write::{absent, id_in};

/// `GET` the branches and tags.
pub const REFS: &str = "/api/v1/projects/{id}/repository/refs";
/// `GET` a spec split into a revision and a path.
pub const RESOLVE: &str = "/api/v1/projects/{id}/repository/resolve";
/// `GET` one tree level.
pub const TREE: &str = "/api/v1/projects/{id}/repository/tree";
/// `GET` one file's head.
pub const BLOB: &str = "/api/v1/projects/{id}/repository/blob";

/// The query parameters of `refs`.
pub const REFS_QUERY: &[QueryParam] = &[QueryParam {
    name: "type",
    description: "branch | tag (default: both)",
    integer_range: None,
}];

/// The query parameters of `resolve`.
pub const RESOLVE_QUERY: &[QueryParam] = &[QueryParam {
    name: "spec",
    description: "Required. <ref>/<path> as in a page URL (e.g. feature/x/src/lib.rs): full commit id first, then the longest branch, then the longest tag",
    integer_range: None,
}];

/// The query parameters of `tree` and `blob`.
pub const TREE_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "ref",
        description: "Required. A full commit id, refs/heads/… or refs/tags/…, or a short name (branch first, then tag)",
        integer_range: None,
    },
    QueryParam {
        name: "path",
        description: "The path in the tree (default: the root); / separated, no empty, . or .. segment",
        integer_range: None,
    },
];

/// Loads the project named by the path's `{id}` (followed by `suffix`)
/// for `RepoBrowse`: one `load_project` call, whose row carries the
/// repository (handed to the handler only after `authorize()`).
pub struct RepositoryOf(pub &'static str);

/// The loaders of the four routes.
pub const REFS_OF: RepositoryOf = RepositoryOf("/repository/refs");
pub const RESOLVE_OF: RepositoryOf = RepositoryOf("/repository/resolve");
pub const TREE_OF: RepositoryOf = RepositoryOf("/repository/tree");
pub const BLOB_OF: RepositoryOf = RepositoryOf("/repository/blob");

impl Loader<AppState> for RepositoryOf {
    type Ctx = ProjectCtx;

    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let Some(id) = id_in(parts.uri.path(), PROJECT_PREFIX, self.0).map(ProjectId::new)
            else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            load_project_lookup(state, id, viewer).await
        })
    }
}

/// The project `id` as the browse routes' lookup: one `load_project` call,
/// whose row carries the repository.
pub(crate) async fn load_project_lookup(
    state: &AppState,
    id: ProjectId,
    viewer: Option<UserId>,
) -> Result<Lookup<ProjectCtx>, ForgeError> {
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
        canonical: Some(record.path.clone()), // clone: the record moves on too
        tombstone: None,
        project: Some(record),
        group: None,
    })
}

// ── the answers ────────────────────────────────────────────────────────

/// One branch or tag.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RefBody {
    /// The short name (`main`, `v1.0`).
    pub name: String,
    /// `refs/heads/…` or `refs/tags/…`.
    pub full_name: String,
    /// `branch` or `tag`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The commit it peels to; `null` for a tag of a tree or a blob.
    #[schema(required = true)]
    pub commit: Option<String>,
    /// The object the ref names (an annotated tag: the tag object).
    pub target: String,
}

/// A repository's branches and tags.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RefsBody {
    /// The branch HEAD names; `null` for an empty repository.
    #[schema(required = true)]
    pub default_branch: Option<String>,
    /// In ref name order.
    pub refs: Vec<RefBody>,
    /// More refs than the server lists (5 000): the list stops there.
    pub truncated: bool,
}

/// A resolved revision.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RevBody {
    /// `branch`, `tag` or `commit` (a full commit id was given).
    #[serde(rename = "type")]
    pub kind: String,
    /// The short ref name, or the commit id.
    pub name: String,
    /// `refs/heads/…` / `refs/tags/…`; `null` for a commit id.
    #[schema(required = true)]
    pub full_name: Option<String>,
    /// The commit id.
    pub commit: String,
}

/// A spec split into a revision and a path.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// The rest of the spec: a path in the tree (`""` for the root).
    pub path: String,
}

/// One tree entry.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryBody {
    pub name: String,
    /// The entry's full path in the tree.
    pub path: String,
    /// `tree`, `blob`, `symlink` or `submodule`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The octal mode as git prints it (`100644`, `040000`, …).
    pub mode: String,
    pub oid: String,
    /// Bytes; `null` for a tree or a submodule.
    #[schema(required = true)]
    pub size: Option<u64>,
}

/// One tree level.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TreeBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// The tree's path (`""` for the root).
    pub path: String,
    /// The tree's id.
    pub oid: String,
    /// In git's order (byte order of the names).
    pub entries: Vec<EntryBody>,
    /// More entries than the server lists (5 000).
    pub truncated: bool,
}

/// One file's head.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BlobBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    pub path: String,
    pub name: String,
    /// `blob` or `symlink` (the content is the link's target).
    #[serde(rename = "type")]
    pub kind: String,
    pub mode: String,
    pub oid: String,
    /// The whole file's size in bytes.
    pub size: u64,
    /// NUL bytes or not UTF-8: no `content` (download it through the raw
    /// route).
    pub binary: bool,
    /// The text, at most 1 MiB; `null` when `binary`.
    #[schema(required = true)]
    pub content: Option<String>,
    /// The file is longer than `content`.
    pub truncated: bool,
}

pub(crate) fn rev_body(rev: &ResolvedRev) -> RevBody {
    RevBody {
        kind: rev.kind().to_owned(),
        name: rev.display_name().to_owned(),
        full_name: rev.ref_name().map(|r| r.as_str().to_owned()),
        commit: rev.commit.clone(), // clone: the answer owns its id
    }
}

fn entry_body(dir: &RepoPath, e: &TreeEntry) -> EntryBody {
    EntryBody {
        name: e.name.clone(), // clone: the answer owns its strings
        path: dir.child(&e.name),
        kind: e.kind.as_str().to_owned(),
        mode: e.mode.clone(), // clone: as above
        oid: e.oid.clone(),   // clone: as above
        size: e.size,
    }
}

fn refs_body(list: &RefList, only: Option<RefKind>) -> RefsBody {
    RefsBody {
        default_branch: list.default_branch.clone(), // clone: the list is shared
        refs: list
            .refs
            .iter()
            .filter(|r| only.is_none_or(|k| r.kind == k))
            .map(|r| RefBody {
                name: r.short().to_owned(),
                full_name: r.name.as_str().to_owned(),
                kind: r.kind.as_str().to_owned(),
                commit: r.commit.clone(), // clone: the list is shared
                target: r.target.clone(), // clone: as above
            })
            .collect(),
        truncated: list.truncated,
    }
}

fn tree_body(rev: &ResolvedRev, path: &RepoPath, tree: &Tree) -> TreeBody {
    TreeBody {
        rev: rev_body(rev),
        path: path.as_str().to_owned(),
        oid: tree.oid.clone(), // clone: the answer owns its id
        entries: tree.entries.iter().map(|e| entry_body(path, e)).collect(),
        truncated: tree.truncated,
    }
}

/// The text of a blob's head, or `None` (binary: a NUL byte, or not
/// UTF-8). A head cut by the cap may end inside a character: up to three
/// trailing bytes of an incomplete sequence are dropped.
pub(crate) fn text_of(content: &[u8], truncated: bool) -> Option<String> {
    if content.contains(&0) {
        return None;
    }
    match std::str::from_utf8(content) {
        Ok(text) => Some(text.to_owned()),
        Err(e) if truncated && e.error_len().is_none() && content.len() - e.valid_up_to() <= 3 => {
            content
                .get(..e.valid_up_to())
                .and_then(|valid| std::str::from_utf8(valid).ok())
                .map(str::to_owned)
        }
        Err(_) => None,
    }
}

fn blob_body(rev: &ResolvedRev, path: &RepoPath, blob: &Blob) -> BlobBody {
    let content = text_of(&blob.content, blob.truncated);
    BlobBody {
        rev: rev_body(rev),
        path: path.as_str().to_owned(),
        name: blob.entry.name.clone(), // clone: the answer owns its strings
        kind: blob.entry.kind.as_str().to_owned(),
        mode: blob.entry.mode.clone(), // clone: as above
        oid: blob.entry.oid.clone(),   // clone: as above
        size: blob.size,
        binary: content.is_none(),
        content,
        truncated: blob.truncated,
    }
}

// ── query parameters ───────────────────────────────────────────────────

/// The decoded values of the allowed parameters (each at most once).
/// `FG-WEB-0006 param_invalid`: `query unknown_parameter`, `<name>
/// repeated`, `<name> malformed` (bad percent-encoding or not UTF-8).
pub(crate) fn query(
    raw: Option<&str>,
    allowed: &[&'static str],
) -> Result<Vec<(&'static str, String)>, ForgeError> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for pair in raw.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(name) = allowed.iter().copied().find(|a| *a == key) else {
            return Err(param_invalid("query", "unknown_parameter"));
        };
        if out.iter().any(|(k, _)| *k == name) {
            return Err(param_invalid(name, "repeated"));
        }
        let value = percent_decode(value).ok_or_else(|| param_invalid(name, "malformed"))?;
        out.push((name, value));
    }
    Ok(out)
}

pub(crate) fn take<'a>(params: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.as_str())
}

/// A required, non-empty parameter.
pub(crate) fn required<'a>(
    params: &'a [(&'static str, String)],
    name: &'static str,
) -> Result<&'a str, ForgeError> {
    match take(params, name) {
        None => Err(param_invalid(name, "missing")),
        Some("") => Err(param_invalid(name, "empty")),
        Some(v) => Ok(v),
    }
}

pub(crate) fn path_param(params: &[(&'static str, String)]) -> Result<RepoPath, ForgeError> {
    RepoPath::parse(take(params, "path").unwrap_or_default())
        .map_err(|rule| param_invalid("path", rule))
}

// ── the handlers ───────────────────────────────────────────────────────

/// What a handler works on, once `authorize()` allowed: the browse state,
/// the project and its ready repository.
pub(crate) struct Target {
    pub(crate) smart: Arc<SmartHttp>,
    pub(crate) project: ProjectId,
    pub(crate) repository: ProjectRepository,
}

impl Target {
    pub(crate) fn of(
        state: &AppState,
        request: &Dispatched<ProjectCtx>,
    ) -> Result<Self, ForgeError> {
        let Some(smart) = state.smart_http.as_ref() else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "smart_http_unconfigured"));
        };
        let project = *request.proof().id();
        let Some(record) = request.project() else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "browse_without_project_row")
                .with_context("project_id", project.get()));
        };
        let repository = record.repository;
        if repository.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", repository.id.get())
                .with_context("project_id", project.get())
                .with_context("state", repository.state.as_str()));
        }
        Ok(Self {
            smart: Arc::clone(smart),
            project,
            repository,
        })
    }
}

/// Run one handler body; any failure is rendered with the request id and
/// the route (the target's ids once known).
pub(crate) async fn answer<T, F, Fut>(
    state: AppState,
    request: Dispatched<ProjectCtx>,
    route: &'static str,
    run: F,
) -> Response
where
    T: Serialize,
    F: FnOnce(Target, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<T, ForgeError>>,
{
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let target = match Target::of(&state, &request) {
        Ok(t) => t,
        Err(err) => return ctx.fail(err.with_context("route", route)).into_response(),
    };
    let ids = (target.project, target.repository);
    let raw_query = request.raw_query().map(str::to_owned);
    drop(request);
    match run(target, raw_query).await {
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
                    .with_context("project_id", ids.0.get())
                    .with_context("repo_id", ids.1.id.get()),
            )
            .into_response(),
    }
}

/// `GET /api/v1/projects/{id}/repository/refs`.
pub async fn refs(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    answer(state, request, "repository_refs", |t, q| async move {
        let params = query(q.as_deref(), &["type"])?;
        let only = match take(&params, "type") {
            None => None,
            Some("branch") => Some(RefKind::Branch),
            Some("tag") => Some(RefKind::Tag),
            Some(_) => return Err(param_invalid("type", "unknown_value")),
        };
        let list = t.smart.browse.refs(t.repository.id).await?;
        Ok(refs_body(&list, only))
    })
    .await
}

/// `GET /api/v1/projects/{id}/repository/resolve?spec=`.
pub async fn resolve(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    answer(state, request, "repository_resolve", |t, q| async move {
        let params = query(q.as_deref(), &["spec"])?;
        let spec = required(&params, "spec")?;
        let spec = spec.strip_prefix('/').unwrap_or(spec);
        let segments: Vec<&str> = spec.split('/').collect();
        let resolved = t
            .smart
            .browse
            .resolve_spec(t.repository.id, &segments)
            .await?;
        Ok(ResolveBody {
            rev: rev_body(&resolved.rev),
            path: resolved.path.as_str().to_owned(),
        })
    })
    .await
}

/// `GET /api/v1/projects/{id}/repository/tree?ref=&path=`.
pub async fn tree(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    answer(state, request, "repository_tree", |t, q| async move {
        let params = query(q.as_deref(), &["ref", "path"])?;
        let rev_name = required(&params, "ref")?;
        let path = path_param(&params)?;
        let repo = t.repository.id;
        let rev = t.smart.browse.resolve_ref(repo, rev_name).await?;
        let tree = t
            .smart
            .browse
            .tree(repo, &rev.commit, &path)
            .await
            .map_err(|e| e.with_context("commit", &rev.commit))?;
        Ok(tree_body(&rev, &path, &tree))
    })
    .await
}

/// `GET /api/v1/projects/{id}/repository/blob?ref=&path=`.
pub async fn blob(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    answer(state, request, "repository_blob", |t, q| async move {
        let params = query(q.as_deref(), &["ref", "path"])?;
        let rev_name = required(&params, "ref")?;
        let path = path_param(&params)?;
        let repo = t.repository.id;
        let rev = t.smart.browse.resolve_ref(repo, rev_name).await?;
        let blob = t
            .smart
            .browse
            .blob(repo, &rev.commit, &path)
            .await
            .map_err(|e| e.with_context("commit", &rev.commit))?;
        Ok(blob_body(&rev, &path, &blob))
    })
    .await
}

#[cfg(test)]
#[path = "browse_tests.rs"]
pub(crate) mod tests;
