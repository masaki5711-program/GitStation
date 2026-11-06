//! The context API (M3 X5, plan 03 §6, ADR-0023 Amendment X5): what an AI
//! agent (or a person) needs for one work item, one requirement or one
//! merge request, gathered in one bounded answer, every item with where it
//! came from (`source`: the project, the version / commit, the path and
//! the line).
//!
//! | Route | Action | Answer |
//! |---|---|---|
//! | `GET /api/v1/projects/{id}/context/work-item?ref=PBI-<n>` | `ContextRead` | the work item of the project's binding, the project's merge requests linked to it, and the `@req PBI-<n>/AC-<m>` tests on the default branch |
//! | `GET /api/v1/projects/{id}/context/requirement?id=…[&ref=]` | `ContextRead` | the requirement's text and document, the `@req` tests naming it, and its state in the newest version tags |
//! | `GET /api/v1/projects/{id}/merge-requests/{iid}/context` | `MergeRequestRead` | the merge request, its changed files, the requirements it adds or changes, its work items and the check runs on its head |
//!
//! The registry authenticates, the loader reads the project (and the merge
//! request) and `authorize()` decides: the caller's own permissions, on
//! this one project — nothing of another project is read, so nothing tells
//! whether one exists. The spec API's budget (30 s), limit (per caller,
//! weighted by the indexes a request runs) and in-flight cap apply.
//!
//! **Plan 06 D-5** (what an AI agent is never given): when the caller is
//! an agent audience ([`audience`]: a token issued with `is_agent`, a
//! service account, a bot, or a request with `Forge-Context-Audience:
//! agent` — `gstn mcp` always sends it), a work item kanflow marked
//! confidential and a requirement of a document with `confidential: true`
//! are refused when asked for (`FG-AUTH-0021`) and left out of lists,
//! counted in `withheld`. A person sees them. A path counts as marked when
//! it is marked at the revision read **or at the default branch's tip**
//! (review X5: an agent cannot pick an unmarked revision; history marked
//! nowhere stays readable). The repository read routes (blob, raw, blame:
//! refused; compare, a commit, a merge request's diffs: the file left out)
//! apply the same rule through [`marked_paths`]. This is defense in depth
//! on the HTTP read APIs: git clone / fetch and archives do not look at the
//! mark, so a truly confidential specification belongs in a repository the
//! agent's account cannot read (ADR-0023 Amendment X5).

use std::collections::BTreeSet;

use axum::response::Response;
use forge_auth::{AccountKind, MergeRequestCtx, ProjectCtx, ResourceCtx};
use forge_checks::spec_index::{index_commit, IndexLimits, Indexed, Tags};
use forge_core::req_tags::TagTarget;
use forge_core::spec_doc::RequirementId;
use forge_core::spec_index::{diff, SpecIndex};
use forge_core::spec_version::SpecVersion;
use forge_core::RepoId;
use forge_core::{ObjectId, WorkItemRef};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, Diff, RefKind, ResolvedRev};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, QueryParam};
use crate::routes::browse::{query, required, rev_body, take, RepositoryOf, RevBody, Target};
use crate::routes::merge_requests::MergeRequestRead;
use crate::routes::specs::{
    caller_of, charge, incomplete, index, respond, revision, SpecIncompleteBody,
};
use crate::routes::write::bounded;

pub const WORK_ITEM: &str = "/api/v1/projects/{id}/context/work-item";
pub const REQUIREMENT: &str = "/api/v1/projects/{id}/context/requirement";
pub const MERGE_REQUEST: &str = "/api/v1/projects/{id}/merge-requests/{iid}/context";

pub const WORK_ITEM_OF: RepositoryOf = RepositoryOf("/context/work-item");
pub const REQUIREMENT_OF: RepositoryOf = RepositoryOf("/context/requirement");
/// The path after `{iid}` of [`MERGE_REQUEST`].
pub const MERGE_REQUEST_SUFFIX: &str = "/context";
pub const MERGE_REQUEST_OF: MergeRequestRead = MergeRequestRead(MERGE_REQUEST_SUFFIX);

/// The request header by which a caller asks to be answered as an AI agent
/// (only `agent`; it can only narrow an answer, never widen it).
pub const AUDIENCE_HEADER: &str = "forge-context-audience";

/// Merge requests one work item's answer lists at most.
pub const MAX_MERGE_REQUESTS: usize = 50;
/// Test places one answer lists at most (the count is whole).
pub const MAX_TESTS: usize = 50;
/// Version tags a requirement's history looks at (each one index).
pub const MAX_HISTORY: usize = 5;
/// Changed files one merge request's answer lists at most.
pub const MAX_FILES: usize = 300;
/// Requirement changes one merge request's answer lists at most.
pub const MAX_CHANGES: usize = 100;
/// The longest description or check summary carried (bytes, cut at a
/// character boundary; `*_truncated` says so).
pub const MAX_TEXT_BYTES: usize = 4 * 1024;

pub const WORK_ITEM_QUERY: &[QueryParam] = &[QueryParam {
    name: "ref",
    description: "The kanflow work item, canonical PBI-<n> (of the project's binding)",
    integer_range: None,
}];

pub const REQUIREMENT_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "id",
        description: "The requirement id (ZEVA-CFG-012) of the project's own specification",
        integer_range: None,
    },
    QueryParam {
        name: "ref",
        description: "A full commit id, refs/heads/… or refs/tags/…, or a short name (branch first, then tag); default: the default branch",
        integer_range: None,
    },
];

// ── the audience (plan 06 D-5) ─────────────────────────────────────────

/// Who an answer is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Person,
    /// An AI agent, and why it counts as one (`agent_token`,
    /// `service_account`, `bot`, `header`).
    Agent(&'static str),
}

impl Audience {
    pub fn is_agent(self) -> bool {
        matches!(self, Self::Agent(_))
    }
}

/// The audience of a request: the principal's
/// (`Principal::is_agent_audience`), else `Forge-Context-Audience: agent`.
/// Any other value of the header, or the header twice, is `FG-WEB-0006`.
pub(crate) fn audience<R: ResourceCtx>(request: &Dispatched<R>) -> Result<Audience, ForgeError> {
    let mut values = request.headers().get_all(AUDIENCE_HEADER).iter();
    let header = match (values.next(), values.next()) {
        (None, _) => false,
        (Some(_), Some(_)) => return Err(param_invalid("Forge-Context-Audience", "repeated")),
        (Some(v), None) if v.as_bytes() == b"agent" => true,
        (Some(_), None) => {
            return Err(param_invalid("Forge-Context-Audience", "unknown_value"));
        }
    };
    let principal = request.principal();
    Ok(if principal.is_agent() {
        Audience::Agent("agent_token")
    } else if principal.account_kind() == AccountKind::ServiceAccount {
        Audience::Agent("service_account")
    } else if principal.account_kind() == AccountKind::Bot {
        Audience::Agent("bot")
    } else if principal.is_agent_audience() {
        // A kind this build does not name: still an agent (fail closed).
        Audience::Agent("not_a_person")
    } else if header {
        Audience::Agent("header")
    } else {
        Audience::Person
    })
}

fn reason_of(audience: Audience) -> &'static str {
    match audience {
        Audience::Agent(reason) => reason,
        Audience::Person => "person",
    }
}

/// What an agent's list left out (plan 06 D-5).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextWithheldBody {
    /// `FG-AUTH-0021`.
    pub error_code: String,
    /// Specification documents marked `confidential: true` (or with a
    /// front matter that cannot be read).
    pub documents: usize,
    /// Their requirements.
    pub requirements: usize,
    /// Work items kanflow marked confidential.
    pub work_items: usize,
    /// Changed files of such documents (a diff's or a merge request's).
    pub files: usize,
}

/// `None` when nothing was left out; logged once when something was.
pub(crate) fn withheld<R: ResourceCtx>(
    request: &Dispatched<R>,
    route: &'static str,
    audience: Audience,
    documents: usize,
    requirements: usize,
    work_items: usize,
) -> Option<ContextWithheldBody> {
    withheld_with_files(
        request,
        route,
        audience,
        (documents, requirements, work_items, 0),
    )
}

/// [`withheld`] with the files a diff left out.
pub(crate) fn withheld_with_files<R: ResourceCtx>(
    request: &Dispatched<R>,
    route: &'static str,
    audience: Audience,
    (documents, requirements, work_items, files): (usize, usize, usize, usize),
) -> Option<ContextWithheldBody> {
    if documents + requirements + work_items + files == 0 {
        return None;
    }
    tracing::info!(
        target: "forge_web::context",
        request_id = %request.ctx().request_id,
        route,
        audience = reason_of(audience),
        documents,
        requirements,
        work_items,
        files,
        error_code = ErrorCode::ContextWithheld.id(),
        "forge.context.withheld"
    );
    Some(ContextWithheldBody {
        error_code: ErrorCode::ContextWithheld.id().to_owned(),
        documents,
        requirements,
        work_items,
        files,
    })
}

/// The specification documents an agent audience is not given in `repo`:
/// marked in one of `indexed` (indexes already read, by commit), at one of
/// `more`, or at the default branch's tip (review X5 MEDIUM a). Every
/// commit is indexed at most once.
pub(crate) async fn marked_paths(
    browse: &Browser,
    repo: RepoId,
    principal: &str,
    indexed: &[(&str, &SpecIndex)],
    more: &[&str],
) -> Result<BTreeSet<String>, ForgeError> {
    let mut out: BTreeSet<String> = indexed
        .iter()
        .flat_map(|(_, i)| i.confidential_paths())
        .map(str::to_owned)
        .collect();
    let mut todo: Vec<String> = more.iter().map(|c| (*c).to_owned()).collect();
    let list = browse.refs(repo).await?;
    if let Some(branch) = list.default_branch.as_deref() {
        match browse
            .resolve_ref(repo, &format!("refs/heads/{branch}"), principal)
            .await
        {
            Ok(tip) => todo.push(tip.commit),
            // HEAD names a branch that does not exist yet: no tip.
            Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => {}
            Err(e) => return Err(e),
        }
    }
    todo.sort();
    todo.dedup();
    todo.retain(|c| !indexed.iter().any(|(done, _)| done == c));
    for commit in todo {
        let i = index_commit(browse, repo, &commit, &IndexLimits::default(), Tags::Skip).await?;
        out.extend(i.index.confidential_paths().into_iter().map(str::to_owned));
    }
    Ok(out)
}

/// For an agent audience: `FG-AUTH-0021` when `path` is a marked
/// specification document at `commit` (or the default tip).
pub(crate) async fn refuse_marked_path(
    audience: Audience,
    browse: &Browser,
    repo: RepoId,
    principal: &str,
    commit: &str,
    path: &str,
) -> Result<(), ForgeError> {
    if !audience.is_agent() {
        return Ok(());
    }
    let marked = marked_paths(browse, repo, principal, &[], &[commit]).await?;
    if marked.contains(path) {
        return Err(refused(audience, "spec_confidential").with_context("path", path));
    }
    Ok(())
}

/// `diff` without the files of `marked` paths (either side of a rename),
/// and how many it left out.
pub(crate) fn without_marked(diff: &Diff, marked: &BTreeSet<String>) -> (Diff, usize) {
    let hit = |p: &Option<String>| p.as_ref().is_some_and(|p| marked.contains(p));
    let (hidden, shown): (Vec<_>, Vec<_>) = diff
        .files
        .iter()
        .cloned() // clone: the answer owns its files
        .partition(|f| hit(&f.new_path) || hit(&f.old_path));
    (
        Diff {
            files: shown,
            truncated: diff.truncated,
        },
        hidden.len(),
    )
}

/// The withheld files of a diff answer, logged once (an agent's only).
pub(crate) fn withheld_logged(
    ctx: &crate::Ctx,
    route: &'static str,
    audience: Audience,
    files: usize,
) -> Option<ContextWithheldBody> {
    if files == 0 {
        return None;
    }
    tracing::info!(
        target: "forge_web::context",
        request_id = %ctx.request_id,
        route,
        audience = reason_of(audience),
        files,
        error_code = ErrorCode::ContextWithheld.id(),
        "forge.context.withheld"
    );
    Some(ContextWithheldBody {
        error_code: ErrorCode::ContextWithheld.id().to_owned(),
        documents: 0,
        requirements: 0,
        work_items: 0,
        files,
    })
}

fn refused(audience: Audience, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ContextWithheld)
        .with_context("reason", reason)
        .with_context("audience", reason_of(audience))
}

// ── the answers ────────────────────────────────────────────────────────

/// Where an item came from.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextSourceBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The project's path now.
    pub project: String,
    /// The branch or tag read (`main`, `v1.5`); the commit id when a
    /// commit was asked for; `null` for an item of no revision.
    #[schema(required = true)]
    pub version: Option<String>,
    #[schema(required = true)]
    pub commit: Option<String>,
    #[schema(required = true)]
    pub path: Option<String>,
    /// 1-based.
    #[schema(required = true)]
    pub line: Option<usize>,
}

struct Origin {
    project_id: i64,
    project: String,
}

impl Origin {
    fn of<R: ResourceCtx>(request: &Dispatched<R>, t: &Target) -> Result<Self, ForgeError> {
        let project = request.project().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "context_without_project_row")
        })?;
        Ok(Self {
            project_id: t.project.get(),
            project: project.path.to_string(),
        })
    }

    fn at(&self, rev: Option<(&str, &str)>, place: Option<(&str, usize)>) -> ContextSourceBody {
        ContextSourceBody {
            project_id: self.project_id,
            project: self.project.clone(), // clone: each item owns its source
            version: rev.map(|(v, _)| v.to_owned()),
            commit: rev.map(|(_, c)| c.to_owned()),
            path: place.map(|(p, _)| p.to_owned()),
            line: place.map(|(_, l)| l),
        }
    }

    fn rev(&self, rev: &ResolvedRev, place: Option<(&str, usize)>) -> ContextSourceBody {
        self.at(Some((rev.display_name(), rev.commit.as_str())), place)
    }

    fn commit(&self, commit: &str, place: Option<(&str, usize)>) -> ContextSourceBody {
        self.at(Some((commit, commit)), place)
    }
}

/// One `@req` test.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextTestBody {
    /// What the tag names: a requirement id or `PBI-<n>/AC-<m>`.
    pub target: String,
    pub source: ContextSourceBody,
}

/// The work item.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextWorkItemRefBody {
    /// `PBI-<n>`.
    #[serde(rename = "ref")]
    pub reference: String,
    #[schema(minimum = 1)]
    pub integration_id: i64,
    /// The bound kanflow project.
    pub kanflow_project_id: String,
    /// forge saw the reference once (a merge request named or linked it).
    pub known: bool,
    /// UTC, RFC 3339: when forge first saw it.
    #[schema(required = true)]
    pub first_seen_at: Option<String>,
}

/// A merge request linked to the work item.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextLinkedMergeRequestBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    pub draft: bool,
    pub source_branch: String,
    pub target_branch: String,
    /// `auto` (named in the title or branch) or `manual`.
    pub link_source: String,
    /// UTC, RFC 3339.
    pub linked_at: String,
    /// The merge request's head (`commit`; `null` before its first version).
    pub source: ContextSourceBody,
}

/// `GET …/context/work-item`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextWorkItemBody {
    pub work_item: ContextWorkItemRefBody,
    /// Newest first, at most 50.
    pub merge_requests: Vec<ContextLinkedMergeRequestBody>,
    pub merge_requests_truncated: bool,
    /// The default branch the tests were read at (`null`: the repository
    /// has none yet).
    #[serde(rename = "ref")]
    #[schema(required = true)]
    pub rev: Option<RevBody>,
    /// The `@req PBI-<n>/AC-<m>` tags, at most 50.
    pub tests: Vec<ContextTestBody>,
    pub test_count: usize,
    #[schema(required = true)]
    pub incomplete: Option<SpecIncompleteBody>,
}

/// The requirement.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextRequirementItemBody {
    pub id: String,
    pub body: String,
    /// `must`, `should`, `could`, `wont`.
    #[schema(required = true)]
    pub priority: Option<String>,
    pub deprecated: bool,
    pub source: ContextSourceBody,
}

/// The document a requirement is defined in.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextDocumentBody {
    pub path: String,
    #[schema(required = true)]
    pub spec: Option<String>,
    #[schema(required = true)]
    pub title: Option<String>,
    #[schema(required = true)]
    pub status: Option<String>,
    /// Never given to an AI agent (plan 06 D-5).
    pub confidential: bool,
}

/// The requirement in one version tag.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextVersionBody {
    /// The tag (`v1.5`).
    pub name: String,
    pub commit: String,
    /// Against the revision asked for: `same`, `changed` (text, priority or
    /// deprecation) or `absent`.
    pub status: String,
    /// Where it was then (`null` when absent).
    #[schema(required = true)]
    pub source: Option<ContextSourceBody>,
}

/// `GET …/context/requirement`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextRequirementBody {
    #[serde(rename = "ref")]
    pub rev: RevBody,
    pub requirement: ContextRequirementItemBody,
    pub document: ContextDocumentBody,
    /// The `@req` tags naming it, at most 50.
    pub tests: Vec<ContextTestBody>,
    pub test_count: usize,
    /// The newest version tags (at most 5), newest first within a series.
    pub versions: Vec<ContextVersionBody>,
    pub versions_truncated: bool,
    #[schema(required = true)]
    pub incomplete: Option<SpecIncompleteBody>,
}

/// The merge request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextMergeRequestItemBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    /// At most 4 KiB.
    pub description: String,
    pub description_truncated: bool,
    /// `open`, `merged` or `closed`.
    pub state: String,
    pub draft: bool,
    pub source_branch: String,
    pub target_branch: String,
    /// The merge base of the newest version (its diff is
    /// `merge_base_sha..head`); `null` before the first version or without
    /// a common history.
    #[schema(required = true)]
    pub merge_base_sha: Option<String>,
    /// The head (`commit`).
    pub source: ContextSourceBody,
}

/// One changed file.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextFileBody {
    /// `added`, `modified`, `deleted`, `renamed`, `copied`, `type_changed`.
    pub status: String,
    /// The path before a rename or copy.
    #[schema(required = true)]
    pub old_path: Option<String>,
    /// At the head (`path`; the old path for a deleted file).
    pub source: ContextSourceBody,
}

/// One requirement the merge request adds, changes, removes or deprecates.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextRequirementChangeBody {
    pub id: String,
    /// `added`, `changed`, `removed` or `deprecated`.
    pub kind: String,
    /// The text at the head (`null` when removed).
    #[schema(required = true)]
    pub body: Option<String>,
    /// At the head (at the merge base when removed).
    pub source: ContextSourceBody,
}

/// A work item linked to the merge request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextLinkedWorkItemBody {
    #[serde(rename = "ref")]
    pub reference: String,
    #[schema(minimum = 1)]
    pub integration_id: i64,
    pub kanflow_project_id: String,
    /// `auto` or `manual`.
    pub link_source: String,
    /// kanflow marked it confidential (shown to people only).
    pub confidential: bool,
    /// The link's project (no revision).
    pub source: ContextSourceBody,
}

/// One check run on the head.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextCheckBody {
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    #[schema(required = true)]
    pub conclusion: Option<String>,
    /// At most 4 KiB.
    pub summary: String,
    pub summary_truncated: bool,
    /// The head it is about (`commit`).
    pub source: ContextSourceBody,
}

/// `GET …/merge-requests/{iid}/context`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextMergeRequestBody {
    pub merge_request: ContextMergeRequestItemBody,
    /// `merge_base_sha..head`, at most 300.
    pub files: Vec<ContextFileBody>,
    pub files_truncated: bool,
    /// The own specification's changes between the merge base and the
    /// head, at most 100.
    pub requirements: Vec<ContextRequirementChangeBody>,
    pub requirements_truncated: bool,
    /// The open links.
    pub work_items: Vec<ContextLinkedWorkItemBody>,
    /// By name.
    pub checks: Vec<ContextCheckBody>,
    /// What an AI agent is not given (plan 06 D-5).
    #[schema(required = true)]
    pub withheld: Option<ContextWithheldBody>,
    #[schema(required = true)]
    pub incomplete: Option<SpecIncompleteBody>,
}

// ── the handlers ───────────────────────────────────────────────────────

fn review(state: &AppState) -> Result<&forge_review::Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

/// `text` cut to [`MAX_TEXT_BYTES`] at a character boundary.
fn bounded_text(text: &str) -> (String, bool) {
    if text.len() <= MAX_TEXT_BYTES {
        return (text.to_owned(), false);
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text.get(..end).unwrap_or_default().to_owned(), true)
}

/// The tags of `index` naming `target`, as tests (at most [`MAX_TESTS`])
/// and their count.
fn tests_of(
    index: &SpecIndex,
    wanted: impl Fn(&TagTarget) -> bool,
    origin: &Origin,
    rev: &ResolvedRev,
) -> (Vec<ContextTestBody>, usize) {
    let naming: Vec<_> = index.tags.iter().filter(|t| wanted(&t.target)).collect();
    let count = naming.len();
    let tests = naming
        .into_iter()
        .take(MAX_TESTS)
        .map(|t| ContextTestBody {
            target: t.target.to_string(),
            source: origin.rev(rev, Some((t.path.as_str(), t.line))),
        })
        .collect();
    (tests, count)
}

/// `GET /api/v1/projects/{id}/context/work-item?ref=`.
pub async fn work_item(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "context_work_item",
        |state, request, t| async move {
            let params = query(request.raw_query(), &["ref"])?;
            let raw = required(&params, "ref")?;
            let reference = WorkItemRef::parse(raw)
                .filter(|r| r.as_str() == raw)
                .ok_or_else(|| param_invalid("ref", "format"))?;
            let audience = audience(&request)?;
            let origin = Origin::of(&request, &t)?;
            let found = bounded(
                &state,
                "context_work_item",
                review(&state)?.work_item_context(t.project, &reference, MAX_MERGE_REQUESTS),
            )
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::WorkItemProjectNotBound)
                    .with_context("reason", "project_not_bound")
            })?;
            if audience.is_agent() && found.work_item.as_ref().is_some_and(|w| w.confidential) {
                return Err(refused(audience, "work_item_confidential")
                    .with_context("work_item", reference.as_str()));
            }
            // The tests on the default branch (none in an empty repository).
            let list = t.smart.browse.refs(t.repository.id).await?;
            let (rev, tests, test_count, reason) = match list.default_branch.as_deref() {
                None => (None, Vec::new(), 0, None),
                Some(_) => {
                    let rev = revision(&t, None).await?;
                    let indexed = index(&t, &rev, Tags::Read).await?;
                    let (tests, count) = tests_of(
                        &indexed.index,
                        |target| {
                            matches!(target, TagTarget::Criterion(c) if *c.work_item() == reference)
                        },
                        &origin,
                        &rev,
                    );
                    (Some(rev), tests, count, indexed.index.incomplete)
                }
            };
            let merge_requests = found
                .merge_requests
                .iter()
                .map(|m| ContextLinkedMergeRequestBody {
                    iid: m.iid,
                    title: m.title.clone(), // clone: the answer owns its strings
                    state: m.state.as_str().to_owned(),
                    draft: m.draft,
                    source_branch: short_branch(&m.source_branch),
                    target_branch: short_branch(&m.target_branch),
                    link_source: m.link_source.as_str().to_owned(),
                    linked_at: m.linked_at.clone(), // clone: as above
                    source: match &m.head_sha {
                        Some(head) => origin.commit(head, None),
                        None => origin.at(None, None),
                    },
                })
                .collect();
            Ok(ContextWorkItemBody {
                work_item: ContextWorkItemRefBody {
                    reference: reference.as_str().to_owned(),
                    integration_id: found.integration.get(),
                    kanflow_project_id: found.external_project_id.clone(), // clone: as above
                    known: found.work_item.is_some(),
                    first_seen_at: found.work_item.as_ref().map(|w| w.created_at.clone()), // clone: as above
                },
                merge_requests,
                merge_requests_truncated: found.truncated,
                rev: rev.as_ref().map(rev_body),
                tests,
                test_count,
                incomplete: incomplete(reason),
            })
        },
    )
    .await
}

/// The index of a commit id read from forge's own records (a tag's
/// commit, a merge request version's head and merge base).
async fn index_at(t: &Target, commit: &str, tags: Tags) -> Result<Indexed, ForgeError> {
    index_commit(
        &t.smart.browse,
        t.repository.id,
        commit,
        &IndexLimits::default(),
        tags,
    )
    .await
}

fn short_branch(full: &str) -> String {
    full.strip_prefix("refs/heads/").unwrap_or(full).to_owned()
}

/// `GET /api/v1/projects/{id}/context/requirement?id=&ref=`.
pub async fn requirement(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    respond(
        state,
        request,
        "context_requirement",
        |state, request, t| async move {
            let params = query(request.raw_query(), &["id", "ref"])?;
            let id = RequirementId::parse(required(&params, "id")?)
                .ok_or_else(|| param_invalid("id", "format"))?;
            let audience = audience(&request)?;
            let origin = Origin::of(&request, &t)?;
            let rev = revision(&t, take(&params, "ref")).await?;
            let indexed = index(&t, &rev, Tags::Read).await?;
            let mut reason = indexed.index.incomplete;
            let located = indexed
                .index
                .requirements()
                .into_iter()
                .find(|l| l.requirement.id == id)
                .ok_or_else(|| {
                    ForgeError::new(ErrorCode::RequirementNotFound)
                        .with_context("requirement", id.as_str())
                        .with_context("commit", &rev.commit)
                })?;
            let doc = indexed
                .index
                .documents
                .iter()
                .find(|d| d.path == located.path)
                .ok_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "requirement_without_document")
                })?;
            let marked_at_tip = if audience.is_agent() {
                charge(&state, caller_of(&request)?, 1)?;
                marked_paths(
                    &t.smart.browse,
                    t.repository.id,
                    &t.principal,
                    &[(rev.commit.as_str(), &indexed.index)],
                    &[],
                )
                .await?
                .contains(located.path)
            } else {
                false
            };
            if audience.is_agent() && (doc.doc.confidential() || marked_at_tip) {
                return Err(refused(audience, "spec_confidential")
                    .with_context("requirement", id.as_str())
                    .with_context("path", located.path));
            }
            let target = TagTarget::Requirement(id.clone()); // clone: the probe owns its id
            let (tests, test_count) = tests_of(&indexed.index, |t| *t == target, &origin, &rev);
            // The history: the newest version tags, each indexed once.
            let list = t.smart.browse.refs(t.repository.id).await?;
            let mut tags: Vec<(SpecVersion, String, String)> = list
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::Tag)
                .filter_map(|r| {
                    let commit = r.commit.clone()?; // clone: the history owns its ids
                    let v = SpecVersion::parse(r.short())?;
                    Some((v, r.short().to_owned(), commit))
                })
                .collect();
            tags.sort_by(|(a, an, _), (b, bn, _)| {
                a.series()
                    .cmp(b.series())
                    .then_with(|| b.cmp(a))
                    .then_with(|| bn.cmp(an))
            });
            let versions_truncated = list.truncated || tags.len() > MAX_HISTORY;
            let to_index = tags
                .iter()
                .take(MAX_HISTORY)
                .filter(|(_, _, c)| *c != rev.commit)
                .count();
            charge(&state, caller_of(&request)?, to_index)?;
            let mut versions = Vec::new();
            for (_, name, commit) in tags.into_iter().take(MAX_HISTORY) {
                if commit == rev.commit {
                    versions.push(ContextVersionBody {
                        source: Some(origin.at(
                            Some((name.as_str(), commit.as_str())),
                            Some((located.path, located.requirement.line)),
                        )),
                        name,
                        commit,
                        status: "same".to_owned(),
                    });
                    continue;
                }
                let then = index_at(&t, &commit, Tags::Skip).await?;
                reason = reason.or(then.index.incomplete);
                let found = then
                    .index
                    .requirements()
                    .into_iter()
                    .find(|l| l.requirement.id == id);
                // Review X5 (LOW): a version whose document was marked is
                // no history an agent is given.
                if audience.is_agent()
                    && found.is_some_and(|l| then.index.confidential_paths().contains(l.path))
                {
                    continue;
                }
                let (status, source) = match found {
                    None => ("absent", None),
                    Some(l) => {
                        let same = l.requirement.body == located.requirement.body
                            && l.requirement.priority == located.requirement.priority
                            && l.requirement.deprecated == located.requirement.deprecated;
                        (
                            if same { "same" } else { "changed" },
                            Some(origin.at(
                                Some((name.as_str(), commit.as_str())),
                                Some((l.path, l.requirement.line)),
                            )),
                        )
                    }
                };
                versions.push(ContextVersionBody {
                    name,
                    commit,
                    status: status.to_owned(),
                    source,
                });
            }
            let meta = doc.doc.meta.as_ref();
            Ok(ContextRequirementBody {
                rev: rev_body(&rev),
                requirement: ContextRequirementItemBody {
                    id: id.as_str().to_owned(),
                    body: located.requirement.body.clone(), // clone: the answer owns its text
                    priority: located.requirement.priority.map(|p| p.as_str().to_owned()),
                    deprecated: located.requirement.deprecated,
                    source: origin.rev(&rev, Some((located.path, located.requirement.line))),
                },
                document: ContextDocumentBody {
                    path: located.path.to_owned(),
                    spec: meta.map(|m| m.spec.clone()), // clone: as above
                    title: meta.map(|m| m.title.clone()), // clone: as above
                    status: meta.map(|m| m.status.as_str().to_owned()),
                    confidential: doc.doc.confidential(),
                },
                tests,
                test_count,
                versions,
                versions_truncated,
                incomplete: incomplete(reason),
            })
        },
    )
    .await
}

/// `GET /api/v1/projects/{id}/merge-requests/{iid}/context`.
pub async fn merge_request(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    respond(
        state,
        request,
        "context_merge_request",
        |state, request, t| async move {
            let _params = query(request.raw_query(), &[])?;
            let audience = audience(&request)?;
            let origin = Origin::of(&request, &t)?;
            let (project, iid) = match *request.proof().id() {
                (project, Some(iid)) => (project, iid),
                (_, None) => {
                    return Err(ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "proof_without_iid"))
                }
            };
            let review = review(&state)?;
            let mr = bounded(
                &state,
                "merge_request_load",
                review.merge_request(project, iid),
            )
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "decided_merge_request_missing")
            })?;
            let versions =
                bounded(&state, "merge_request_versions", review.versions(mr.id)).await?;
            let newest = versions.last();
            let head = mr.head_sha.clone(); // clone: the answer owns its id
            let merge_base = newest.and_then(|v| v.merge_base_sha.as_ref());
            // The files and the requirement changes: merge base → head.
            let mut files = Vec::new();
            let mut files_truncated = false;
            let mut changes = Vec::new();
            let mut changes_truncated = false;
            let mut withheld_docs = BTreeSet::new();
            let mut withheld_requirements = 0;
            let mut reason = None;
            let mut withheld_files = 0;
            if let (Some(v), Some(base)) = (newest, merge_base) {
                let head_sha = v.head_sha.as_str();
                // Two more indexes and the compare (review X5 LOW: weighted).
                charge(&state, caller_of(&request)?, 2)?;
                // The stored version's commits (its version ref keeps the
                // head; no branch reaches it necessarily).
                let before = index_at(&t, base.as_str(), Tags::Skip).await?;
                let after = index_at(&t, head_sha, Tags::Skip).await?;
                reason = before.index.incomplete.or(after.index.incomplete);
                let secret: BTreeSet<String> = if audience.is_agent() {
                    charge(&state, caller_of(&request)?, 1)?;
                    marked_paths(
                        &t.smart.browse,
                        t.repository.id,
                        &t.principal,
                        &[(base.as_str(), &before.index), (head_sha, &after.index)],
                        &[],
                    )
                    .await?
                } else {
                    BTreeSet::new()
                };
                let compared = t
                    .smart
                    .browse
                    .compare(t.repository.id, base.as_str(), head_sha)
                    .await?;
                // Review X5 (MEDIUM b): a marked document is no changed file
                // for an agent (either side of a rename).
                let (shown, hidden) = without_marked(&compared.diff, &secret);
                withheld_files = hidden;
                files_truncated = shown.truncated || shown.files.len() > MAX_FILES;
                for f in shown.files.iter().take(MAX_FILES) {
                    let path = f
                        .new_path
                        .as_deref()
                        .or(f.old_path.as_deref())
                        .unwrap_or("");
                    let renamed = f
                        .old_path
                        .as_deref()
                        .filter(|o| Some(*o) != f.new_path.as_deref());
                    files.push(ContextFileBody {
                        status: f.status.as_str().to_owned(),
                        old_path: renamed.map(str::to_owned),
                        source: origin.commit(head_sha, Some((path, 0))).without_line(),
                    });
                }
                let bodies: std::collections::BTreeMap<&RequirementId, &str> = after
                    .index
                    .requirements()
                    .into_iter()
                    .map(|l| (&l.requirement.id, l.requirement.body.as_str()))
                    .collect();
                let all = diff(&before.index, &after.index);
                let mut kept = Vec::new();
                for c in all {
                    if audience.is_agent() && secret.contains(c.path.as_str()) {
                        withheld_docs.insert(c.path.clone()); // clone: counted by path
                        withheld_requirements += 1;
                        continue;
                    }
                    kept.push(c);
                }
                changes_truncated = kept.len() > MAX_CHANGES;
                for c in kept.into_iter().take(MAX_CHANGES) {
                    let removed = c.kind == forge_core::spec_index::ChangeKind::Removed;
                    let at = if removed { base.as_str() } else { head_sha };
                    changes.push(ContextRequirementChangeBody {
                        id: c.id.as_str().to_owned(),
                        kind: c.kind.as_str().to_owned(),
                        body: if removed {
                            None
                        } else {
                            bodies.get(&c.id).map(|b| (*b).to_owned())
                        },
                        source: origin.commit(at, Some((c.path.as_str(), c.line))),
                    });
                }
            }
            // The work items (open links), the marked ones left out for an agent.
            let links = bounded(&state, "work_item_links", review.work_item_links(mr.id)).await?;
            let open: Vec<_> = links
                .links
                .into_iter()
                .filter(|l| l.unlinked_at.is_none())
                .collect();
            let ids: Vec<i64> = open.iter().map(|l| l.work_item.id).collect();
            let marked = bounded(
                &state,
                "work_item_confidential",
                review.confidential_work_items(&ids),
            )
            .await?;
            let mut withheld_items = 0;
            let mut work_items = Vec::new();
            for l in open {
                let confidential = marked.contains(&l.work_item.id);
                if audience.is_agent() && confidential {
                    withheld_items += 1;
                    continue;
                }
                work_items.push(ContextLinkedWorkItemBody {
                    reference: l.work_item.reference.as_str().to_owned(),
                    integration_id: l.work_item.integration.get(),
                    kanflow_project_id: l.work_item.external_project_id,
                    link_source: l.source.as_str().to_owned(),
                    confidential,
                    source: origin.at(None, None),
                });
            }
            // The check runs on the head.
            let mut checks = Vec::new();
            if let Some(head) = head.as_deref() {
                let sha = ObjectId::parse(head).map_err(|e| e.with_context("field", "head_sha"))?;
                let runs = bounded(
                    &state,
                    "merge_request_checks",
                    crate::routes::checks::checks(&state)?
                        .merge_request_head_runs(request.proof(), &sha),
                )
                .await?;
                for run in runs {
                    let (summary, summary_truncated) = bounded_text(&run.summary);
                    checks.push(ContextCheckBody {
                        name: run.name.as_str().to_owned(),
                        status: run.status.as_str().to_owned(),
                        conclusion: run.conclusion.map(|c| c.as_str().to_owned()),
                        summary,
                        summary_truncated,
                        source: origin.commit(head, None),
                    });
                }
            }
            let (description, description_truncated) = bounded_text(&mr.description);
            let withheld = withheld_with_files(
                &request,
                "context_merge_request",
                audience,
                (
                    withheld_docs.len(),
                    withheld_requirements,
                    withheld_items,
                    withheld_files,
                ),
            );
            Ok(ContextMergeRequestBody {
                merge_request: ContextMergeRequestItemBody {
                    iid: mr.iid,
                    title: mr.title.clone(), // clone: the answer owns its strings
                    description,
                    description_truncated,
                    state: mr.state.as_str().to_owned(),
                    draft: mr.draft,
                    source_branch: short_branch(mr.source_branch.as_str()),
                    target_branch: short_branch(mr.target_branch.as_str()),
                    merge_base_sha: merge_base.map(|b| b.as_str().to_owned()),
                    source: match head.as_deref() {
                        Some(h) => origin.commit(h, None),
                        None => origin.at(None, None),
                    },
                },
                files,
                files_truncated,
                requirements: changes,
                requirements_truncated: changes_truncated,
                work_items,
                checks,
                withheld,
                incomplete: incomplete(reason),
            })
        },
    )
    .await
}

impl ContextSourceBody {
    /// A whole file: no line.
    fn without_line(mut self) -> Self {
        self.line = None;
        self
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
