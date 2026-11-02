//! The history half of the repository browse API (`RepoBrowse`,
//! `read_api`; M2 MR8, plan 10 §5):
//!
//! - `GET /api/v1/projects/{id}/repository/commits?ref=&path=&limit=&cursor=`
//!   — a page of the history of a revision (limited to a path), newest
//!   first. `next_cursor` continues the walk *from the commit the first page
//!   was read at* (a push in between does not shift pages); a cursor that
//!   does not belong to this path is `FG-WEB-0006 reason=cursor_invalid`;
//! - `GET …/repository/commits/{oid}` — one commit (a full id): metadata,
//!   message and the diff against its first parent (a root commit: against
//!   nothing), with per-file patches under caps (`truncated`, `omitted`);
//! - `GET …/repository/compare?from=&to=` — the commits `to` has and `from`
//!   does not (capped) and the tree diff from `from` to `to`;
//! - `GET …/repository/blame?ref=&path=` — who last changed each line of a
//!   file, in hunks (capped; `binary` files have none).
//!
//! The access path is the one of [`super::browse`]: the registry
//! authenticates, the loader fetches the project (one call), `prove(RepoBrowse)`
//! decides, then forge-git's `Browser` runs git with resolved ids after
//! `--end-of-options` and paths after `--`. Query parameters are strict
//! (`FG-WEB-0006`); the `{oid}` of the commit route is checked after the
//! decision.

use axum::http::request::Parts;
use axum::response::Response;
use forge_auth::ProjectCtx;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Blame, CommitInfo, Diff, DiffFile, LogCursor, RepoPath, ResolvedRev};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::browse::{
    answer, load_project_lookup, path_param, query, required, rev_body, take, RevBody, Target,
};
use crate::routes::projects::PREFIX as PROJECT_PREFIX;

/// `GET` a page of the history.
pub const COMMITS: &str = "/api/v1/projects/{id}/repository/commits";
/// `GET` one commit with its diff.
pub const COMMIT: &str = "/api/v1/projects/{id}/repository/commits/{oid}";
/// `GET` the comparison of two revisions.
pub const COMPARE: &str = "/api/v1/projects/{id}/repository/compare";
/// `GET` a blamed file.
pub const BLAME: &str = "/api/v1/projects/{id}/repository/blame";

/// The loader of `commits/{oid}`: the project id before `/repository/commits/`.
pub struct CommitOf;

/// The loaders of the other three routes.
pub const COMMITS_OF: super::browse::RepositoryOf =
    super::browse::RepositoryOf("/repository/commits");
pub const COMPARE_OF: super::browse::RepositoryOf =
    super::browse::RepositoryOf("/repository/compare");
pub const BLAME_OF: super::browse::RepositoryOf = super::browse::RepositoryOf("/repository/blame");

const COMMIT_INFIX: &str = "/repository/commits/";

impl Loader<AppState> for CommitOf {
    type Ctx = ProjectCtx;

    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let id = parts
                .uri
                .path()
                .strip_prefix(PROJECT_PREFIX)
                .and_then(|rest| rest.split_once(COMMIT_INFIX))
                .and_then(|(id, _)| (!id.contains('/')).then_some(id))
                .and_then(parse_positive_id)
                .map(ProjectId::new);
            let Some(id) = id else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            let found: Lookup<ProjectCtx> = load_project_lookup(state, id, viewer).await?;
            Ok(found)
        })
    }
}

/// The query parameters of `commits`.
pub const COMMITS_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "ref",
        description: "Required. A full commit id, refs/heads/… or refs/tags/…, or a short name (branch first, then tag)",
        integer_range: None,
    },
    QueryParam {
        name: "path",
        description: "Only commits that changed this path (default: all); / separated, no empty, . or .. segment",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "Commits per page, 1..=100 (default 30)",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "cursor",
        description: "The previous page's next_cursor (opaque; belongs to the same path; continues from the commit the first page was read at)",
        integer_range: None,
    },
];

/// The query parameters of `compare`.
pub const COMPARE_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "from",
        description:
            "Required. The base: a full commit id, refs/heads/… or refs/tags/…, or a short name",
        integer_range: None,
    },
    QueryParam {
        name: "to",
        description: "Required. The head, named like from",
        integer_range: None,
    },
];

/// The query parameters of `blame`.
pub const BLAME_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "ref",
        description: "Required. A full commit id, refs/heads/… or refs/tags/…, or a short name (branch first, then tag)",
        integer_range: None,
    },
    QueryParam {
        name: "path",
        description: "Required. The file's path; / separated, no empty, . or .. segment",
        integer_range: None,
    },
];

/// `limit` without the parameter.
const DEFAULT_LIMIT: usize = 30;

// ── the answers ────────────────────────────────────────────────────────

/// One commit's metadata.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitBody {
    pub oid: String,
    /// In order; the first is the mainline of a merge.
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    /// ISO 8601 with the author's offset.
    pub authored_at: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committed_at: String,
    /// The first paragraph of the message on one line.
    pub subject: String,
}

/// A page of the history.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitsBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    /// The commit the history is read from: `ref`'s commit on the first
    /// page, the one the cursor keeps on the next pages.
    pub head: String,
    pub path: String,
    pub commits: Vec<CommitBody>,
    /// Pass as `cursor` for the next page; `null` on the last one.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// One changed file of a diff.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffFileBody {
    /// `added`, `modified`, `deleted`, `renamed`, `copied` or `type_changed`.
    pub status: String,
    /// `null` for an added file.
    #[schema(required = true)]
    pub old_path: Option<String>,
    /// `null` for a deleted file.
    #[schema(required = true)]
    pub new_path: Option<String>,
    pub old_mode: String,
    pub new_mode: String,
    pub old_oid: String,
    pub new_oid: String,
    /// Renamed / copied: the similarity in percent.
    #[schema(required = true)]
    pub similarity: Option<u8>,
    /// git reports the change as binary: no patch.
    pub binary: bool,
    /// The hunks (`@@ …`); `""` for a change without any (mode only, a
    /// pure rename, an empty file); `null` when `binary` or `omitted`.
    #[schema(required = true)]
    pub patch: Option<String>,
    /// Why a file with hunks has no `patch`: `too_large` (over 256 KiB),
    /// `total_cap` (the diff's patch bytes ran out; 2 MiB) or `not_utf8`.
    #[schema(required = true)]
    pub omitted: Option<String>,
}

/// The changed files of a diff.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffBody {
    pub files: Vec<DiffFileBody>,
    /// A cap cut the diff: more files than listed (500), or a patch left
    /// out because of its size.
    pub truncated: bool,
}

/// One commit with its message and diff.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitDetailBody {
    pub commit: CommitBody,
    /// The whole message.
    pub message: String,
    /// Against the first parent (a root commit: against nothing).
    pub diff: DiffBody,
}

/// Two revisions compared.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareBody {
    pub from: RevBody,
    pub to: RevBody,
    /// The commits `to` has and `from` does not, newest first.
    pub commits: Vec<CommitBody>,
    /// More commits than the server lists (200).
    pub commits_truncated: bool,
    /// The tree diff from `from` to `to`.
    pub diff: DiffBody,
}

/// A commit named by a blame.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameCommitBody {
    pub oid: String,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the epoch.
    pub author_time: i64,
    /// `+0900`.
    pub author_tz: String,
    pub summary: String,
    /// The line is as old as the history git blamed goes.
    pub boundary: bool,
    /// The file's name in that commit.
    pub path: String,
}

/// Consecutive lines last changed by one commit.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameHunkBody {
    /// The commit's id (see `commits`).
    pub commit: String,
    /// The first line's number in the file (1-based).
    pub start: usize,
    /// The first line's number in that commit's version.
    pub orig_start: usize,
    pub lines: Vec<String>,
}

/// A blamed file.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    pub path: String,
    pub oid: String,
    /// The whole file's size in bytes.
    pub size: u64,
    /// NUL bytes or not UTF-8: no hunks (download it through the raw route).
    pub binary: bool,
    /// Hunks in file order.
    pub hunks: Vec<BlameHunkBody>,
    /// The commits the hunks name, in order of first appearance.
    pub commits: Vec<BlameCommitBody>,
    /// The file has more lines than the server blames (10 000).
    pub truncated: bool,
}

fn commit_body(c: &CommitInfo) -> CommitBody {
    CommitBody {
        oid: c.oid.clone(),                         // clone: the answer owns its strings
        parents: c.parents.clone(),                 // clone: as above
        author_name: c.author_name.clone(),         // clone: as above
        author_email: c.author_email.clone(),       // clone: as above
        authored_at: c.authored_at.clone(),         // clone: as above
        committer_name: c.committer_name.clone(),   // clone: as above
        committer_email: c.committer_email.clone(), // clone: as above
        committed_at: c.committed_at.clone(),       // clone: as above
        subject: c.subject.clone(),                 // clone: as above
    }
}

fn diff_file_body(f: &DiffFile) -> DiffFileBody {
    DiffFileBody {
        status: f.status.as_str().to_owned(),
        old_path: f.old_path.clone(), // clone: the answer owns its strings
        new_path: f.new_path.clone(), // clone: as above
        old_mode: f.old_mode.clone(), // clone: as above
        new_mode: f.new_mode.clone(), // clone: as above
        old_oid: f.old_oid.clone(),   // clone: as above
        new_oid: f.new_oid.clone(),   // clone: as above
        similarity: f.similarity,
        binary: f.binary,
        patch: f.patch.clone(), // clone: as above
        omitted: f.omitted.map(|o| o.as_str().to_owned()),
    }
}

fn diff_body(d: &Diff) -> DiffBody {
    DiffBody {
        files: d.files.iter().map(diff_file_body).collect(),
        truncated: d.truncated,
    }
}

fn blame_body(rev: &ResolvedRev, path: &RepoPath, blame: &Blame) -> BlameBody {
    BlameBody {
        rev: rev_body(rev),
        path: path.as_str().to_owned(),
        oid: blame.entry.oid.clone(), // clone: the answer owns its id
        size: blame.size,
        binary: blame.binary,
        hunks: blame
            .hunks
            .iter()
            .filter_map(|h| {
                blame.commits.get(h.commit).map(|c| BlameHunkBody {
                    commit: c.oid.clone(), // clone: the answer owns its id
                    start: h.start,
                    orig_start: h.orig_start,
                    lines: h.lines.clone(), // clone: as above
                })
            })
            .collect(),
        commits: blame
            .commits
            .iter()
            .map(|c| BlameCommitBody {
                oid: c.oid.clone(),                   // clone: as above
                author_name: c.author_name.clone(),   // clone: as above
                author_email: c.author_email.clone(), // clone: as above
                author_time: c.author_time,
                author_tz: c.author_tz.clone(), // clone: as above
                summary: c.summary.clone(),     // clone: as above
                boundary: c.boundary,
                path: c.path.clone(), // clone: as above
            })
            .collect(),
        truncated: blame.truncated,
    }
}

// ── parameters ─────────────────────────────────────────────────────────

/// `limit`: a plain decimal in `1..=max` (`FG-WEB-0006 param_invalid`).
fn limit_param(params: &[(&'static str, String)], max: usize) -> Result<usize, ForgeError> {
    let Some(raw) = take(params, "limit") else {
        return Ok(DEFAULT_LIMIT.min(max.max(1)));
    };
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(param_invalid("limit", "not_positive_decimal"));
    }
    match raw.parse::<usize>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(param_invalid("limit", "out_of_range")),
    }
}

/// `cursor`: what a page handed out (`FG-WEB-0006 reason=cursor_invalid`).
pub(crate) fn cursor_param(
    params: &[(&'static str, String)],
    max_skip: usize,
) -> Result<Option<LogCursor>, ForgeError> {
    match take(params, "cursor") {
        None => Ok(None),
        Some("") => Err(param_invalid("cursor", "empty")),
        Some(raw) => LogCursor::parse(raw, max_skip).map(Some).ok_or_else(|| {
            ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "cursor_invalid")
                .with_context("rule", "payload")
        }),
    }
}

/// The `{oid}` of `…/repository/commits/{oid}`: a full commit id, in any
/// case (lower-cased). Checked after `authorize()` allowed.
fn oid_param(raw_path: &str) -> Result<String, ForgeError> {
    let oid = raw_path
        .split_once(COMMIT_INFIX)
        .map(|(_, oid)| oid)
        .ok_or_else(|| param_invalid("oid", "missing"))?;
    let hex = matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex {
        return Err(param_invalid("oid", "not_object_id"));
    }
    Ok(oid.to_ascii_lowercase())
}

// ── the handlers ───────────────────────────────────────────────────────

/// `GET …/repository/commits?ref=&path=&limit=&cursor=`.
pub async fn commits(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    answer(
        state,
        request,
        "repository_commits",
        |t: Target, q| async move {
            let params = query(q.as_deref(), &["ref", "path", "limit", "cursor"])?;
            let rev_name = required(&params, "ref")?;
            let path = path_param(&params)?;
            let browse = &t.smart.browse;
            let limit = limit_param(&params, browse.limits().max_log_page)?;
            let cursor = cursor_param(&params, browse.limits().max_log_skip)?;
            let repo = t.repository.id;
            let rev = browse.resolve_ref(repo, rev_name).await?;
            let tip = cursor
                .as_ref()
                .map_or(rev.commit.as_str(), |c| c.tip.as_str());
            let page = browse
                .commits(repo, tip, &path, limit, cursor.as_ref())
                .await
                .map_err(|e| e.with_context("commit", &rev.commit))?;
            Ok(CommitsBody {
                rev: rev_body(&rev),
                head: page.tip.clone(), // clone: the page's commits are borrowed below
                path: path.as_str().to_owned(),
                commits: page.commits.iter().map(commit_body).collect(),
                has_more: page.next.is_some(),
                next_cursor: page.next.as_ref().map(LogCursor::encode),
            })
        },
    )
    .await
}

/// `GET …/repository/commits/{oid}`.
pub async fn commit(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let raw_path = request.raw_path().to_owned();
    answer(
        state,
        request,
        "repository_commit",
        |t: Target, q| async move {
            query(q.as_deref(), &[])?;
            let oid = oid_param(&raw_path)?;
            let detail = t
                .smart
                .browse
                .commit(t.repository.id, &oid)
                .await
                .map_err(|e| e.with_context("commit", &oid))?;
            Ok(CommitDetailBody {
                commit: commit_body(&detail.info),
                message: detail.message,
                diff: diff_body(&detail.diff),
            })
        },
    )
    .await
}

/// `GET …/repository/compare?from=&to=`.
pub async fn compare(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    answer(
        state,
        request,
        "repository_compare",
        |t: Target, q| async move {
            let params = query(q.as_deref(), &["from", "to"])?;
            let from_name = required(&params, "from")?;
            let to_name = required(&params, "to")?;
            let repo = t.repository.id;
            let from = t.smart.browse.resolve_ref(repo, from_name).await?;
            let to = t.smart.browse.resolve_ref(repo, to_name).await?;
            let cmp = t
                .smart
                .browse
                .compare(repo, &from.commit, &to.commit)
                .await
                .map_err(|e| e.with_context("commit", &to.commit))?;
            Ok(CompareBody {
                from: rev_body(&from),
                to: rev_body(&to),
                commits: cmp.commits.iter().map(commit_body).collect(),
                commits_truncated: cmp.commits_truncated,
                diff: diff_body(&cmp.diff),
            })
        },
    )
    .await
}

/// `GET …/repository/blame?ref=&path=`.
pub async fn blame(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    answer(
        state,
        request,
        "repository_blame",
        |t: Target, q| async move {
            let params = query(q.as_deref(), &["ref", "path"])?;
            let rev_name = required(&params, "ref")?;
            required(&params, "path")?;
            let path = path_param(&params)?;
            let repo = t.repository.id;
            let rev = t.smart.browse.resolve_ref(repo, rev_name).await?;
            let blamed = t
                .smart
                .browse
                .blame(repo, &rev.commit, &path)
                .await
                .map_err(|e| e.with_context("commit", &rev.commit))?;
            Ok(blame_body(&rev, &path, &blamed))
        },
    )
    .await
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
