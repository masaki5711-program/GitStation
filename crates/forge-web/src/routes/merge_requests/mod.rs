//! Merge requests of a project (M3 R1a, ADR-0024):
//! `GET|POST /api/v1/projects/{id}/merge-requests`,
//! `GET|PATCH /api/v1/projects/{id}/merge-requests/{iid}` and
//! `POST …/merge-requests/{iid}/close`, `…/reopen`, and (M3 R1b) the
//! administrator's `POST …/merge-requests/{iid}/version-refs/purge`
//! (`MergeRequestVersionRefsPurge`: a web session only, ADR-0024 §2;
//! runbook `secret-removal`), and (M3 R2, `versions.rs`) the reads
//! `GET …/{iid}/versions`, `…/versions/{version}/diff`, `…/compare` and
//! `…/mergeability` (`MergeRequestRead`), and (M3 R3, `discussions.rs`)
//! the discussions: `GET|POST …/{iid}/discussions`, `POST
//! …/discussions/{discussion_id}/notes` (`MergeRequestComment`) and
//! `…/resolve`, `…/unresolve` (`DiscussionResolve`), and (M3 R4,
//! `merges.rs`) `POST|GET …/{iid}/merge` (`MergeRequestMerge` /
//! `MergeRequestRead`: queue a merge, read the latest attempt), and (M3 R5,
//! `approvals.rs`) `POST …/{iid}/approve`, `…/unapprove`, `GET …/approvals`
//! and the project's `…/approval-rules[/{name}]`, and (M3 I1b,
//! `work_items.rs`) `GET …/{iid}/work-items` (`MergeRequestRead`) and
//! `PUT|DELETE …/{iid}/work-items/{ref}` (`WorkItemLink`). Reading and listing are
//! decided like the repository browse API (`MergeRequestRead`,
//! `MergeRequestList`); opening is Developer and above (`MergeRequestCreate`);
//! an update, a close and a reopen are the author's or a Developer's (another's
//! draft flag only a Maintainer's) — ADR-0024 §12.
//!
//! The registry loads the project (the one `ProjectAccessLoader` call of
//! `GET /api/v1/projects/{id}`) and, for a route on a stored merge request,
//! the row by its `iid`; an invisible project and a missing `iid` are the
//! same `FG-REPO-0005` (`authorize()` on the project, or `Unresolved`).
//! Branch names are checked before anything is looked up (`FG-WEB-0006`
//! `param_invalid`: `not_a_branch`, `source_equals_target`,
//! `invalid_ref_name`; `authorize()` refuses them again). forge-review
//! decides again inside the transaction and checks that the branches exist
//! (`FG-REPO-0023`). Every answer is re-read after the commit and never
//! cached by a shared cache.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{MergeRequestCtx, Principal, ProjectCtx};
use forge_core::{
    parse_positive_id, MergeRequestState, ProjectId, ProjectRepository, RefName, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{
    EditFields, NewFields, Review, ReviewRequest, StoredMergeRequest, UserName, MAX_PAGE,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, QueryParam, WriteParts};
use crate::routes::write::{bounded, json, no_store};

mod approvals;
mod conflicts;
mod discussion_loaders;
mod discussions;
mod loaders;
mod mergeability;
mod merges;
mod queue;
mod settings;
mod status;
mod versions;
mod work_items;

pub use approvals::{
    approvals, approve, delete_rule as delete_approval_rule, put_rule as put_approval_rule,
    rules as approval_rules, unapprove, ApprovalAnswerBody, ApprovalRuleBody, ApprovalRulesBody,
    ApprovalStateBody, ApproveMergeRequest, PutApprovalRule, RulesProject, APPROVALS, APPROVALS_OF,
    APPROVE, MAX_BODY as APPROVAL_MAX_BODY, RULE as APPROVAL_RULE, RULES as APPROVAL_RULES,
    TO_APPROVE, TO_UNAPPROVE, UNAPPROVE,
};
pub use conflicts::{
    conflicts, ConflictPredictionBody, ConflictScanBody, ConflictsBody, CONFLICTS, CONFLICTS_SUFFIX,
};
pub use discussion_loaders::{
    DiscussionToReply, DiscussionToResolve, DiscussionToStart, TO_RESOLVE, TO_UNRESOLVE,
};
pub use discussions::{
    list as discussions, reply as reply_to_discussion, resolve as resolve_discussion,
    start as start_discussion, unresolve as unresolve_discussion, AddNote, DiscussionBody,
    DiscussionsBody, NoteBody, PositionBody, StartDiscussion, DISCUSSIONS, DISCUSSIONS_SUFFIX,
    MAX_BODY as DISCUSSION_MAX_BODY, NOTES, RESOLVE, UNRESOLVE,
};
pub use loaders::{
    MergeRequestOf, MergeRequestRead, MergeRequestToClose, MergeRequestToPurge,
    MergeRequestToReopen, MergeRequestToUpdate, MergeRequestsOf, NewMergeRequest, VersionDiffOf,
    COMPARE_OF, CONFLICTS_OF, DISCUSSIONS_OF, MERGEABILITY_OF, VERSIONS_OF,
};
pub use mergeability::{mergeability, MergeabilityBody};
pub use merges::{
    merge, merge_status, MergeAttemptBody, MergeMergeRequest, MergeRequestToMerge, MergeStatusBody,
    MergeStatusOf, MAX_BODY as MERGE_MAX_BODY, MERGE,
};
pub use queue::{
    dequeue as merge_queue_dequeue, enqueue as merge_queue_enqueue, entry as merge_queue_entry,
    entry_body, EnqueueMergeRequest, MergeRequestToDequeue, MergeRequestToQueue, QueueEntryBody,
    QueueEntryOf, QueueEntryStatusBody, QUEUE as MERGE_QUEUE,
    QUEUE_MAX_BODY as MERGE_QUEUE_MAX_BODY,
};
pub use settings::{
    get_settings as merge_settings, put_codeowners as put_codeowners_enforcement,
    put_settings as put_merge_settings, CodeownersEnforcementBody, CodeownersOf, MergeSettingsBody,
    MergeSettingsViewerBody, PutCodeownersEnforcement, PutMergeSettings, SettingsChangeOf,
    SettingsProject, CODEOWNERS as CODEOWNERS_ENFORCEMENT, MAX_BODY as SETTINGS_MAX_BODY,
    SETTINGS as MERGE_SETTINGS,
};
pub use status::{
    status, MergeRequestStatusBody, StatusApprovalBody, StatusApprovalsBody, StatusCheckBody,
    StatusCodeownerBody, StatusCodeownersBody, StatusDiscussionsBody, StatusReasonBody,
    StatusReasonName, StatusRuleBody, StatusViewerBody, StatusWorkItemLinkBody, STATUS, STATUS_OF,
};
pub use versions::{
    compare, version_diff, versions, VersionBody, VersionCompareBody, VersionDiffBody,
    VersionsBody, COMPARE, COMPARE_QUERY, MERGEABILITY, VERSIONS, VERSION_DIFF,
};
pub use work_items::{
    link as link_work_item, links as work_item_links, unlink as unlink_work_item, LinkOf, LinksOf,
    WorkItemBody, WorkItemLinkAnswerBody, WorkItemLinkBody, WorkItemLinksBody,
    LINK as WORK_ITEM_LINK, LINKS as WORK_ITEM_LINKS,
};

/// `GET` / `POST` a project's merge requests.
pub const COLLECTION: &str = "/api/v1/projects/{id}/merge-requests";
/// `GET` / `PATCH` one merge request.
pub const ONE: &str = "/api/v1/projects/{id}/merge-requests/{iid}";
/// `POST` close one.
pub const CLOSE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/close";
/// `POST` reopen one.
pub const REOPEN: &str = "/api/v1/projects/{id}/merge-requests/{iid}/reopen";
/// The path after `{iid}` of [`PURGE`].
pub const PURGE_SUFFIX: &str = "/version-refs/purge";
/// `POST` remove the hidden version refs (M3 R1b, secret removal).
pub const PURGE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/version-refs/purge";
/// The body cap of the purge (a list of version numbers).
pub const PURGE_MAX_BODY: usize = 16 * 1024;

/// The body cap of the merge request routes (a 64 KiB description,
/// JSON-escaped, and the rest).
pub const MAX_BODY: usize = 64 * 1024;

/// Merge requests per page when `limit` is absent.
pub const DEFAULT_LIMIT: usize = 20;

/// The query parameters of the list (`openapi.json`).
pub const QUERY: &[QueryParam] = &[
    QueryParam {
        name: "state",
        description: "open (default), closed, merged or all",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "Merge requests per page, 1..=100 (default 20)",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "before",
        description: "The previous page's next_before (an iid): merge requests with a smaller iid",
        integer_range: Some((1, i64::MAX)),
    },
];

/// `POST …/merge-requests`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateMergeRequest {
    /// 1..=255 characters.
    pub title: String,
    /// At most 64 KiB; empty when absent.
    #[serde(default)]
    pub description: String,
    /// A branch: `feature/x` or `refs/heads/feature/x`; it must exist.
    pub source_branch: String,
    /// A branch other than the source; it must exist.
    pub target_branch: String,
    #[serde(default)]
    pub draft: bool,
}

/// `PATCH …/merge-requests/{iid}`: every field optional (absent =
/// unchanged).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateMergeRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    /// Only the author or a Maintainer changes another's flag; only while
    /// open.
    pub draft: Option<bool>,
    /// A branch other than the source; it must exist; only while open.
    pub target_branch: Option<String>,
}

/// `POST …/merge-requests/{iid}/version-refs/purge`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PurgeVersionRefs {
    /// The version numbers whose refs to remove; absent = every version.
    /// An open merge request's current head may not be among them (close
    /// it or rewrite its source branch first: FG-REVIEW-0002).
    pub versions: Option<Vec<i64>>,
}

/// What a purge did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionRefsPurged {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// Versions marked and whose refs were removed by this request.
    pub purged: Vec<i64>,
    /// Versions purged before (their refs were removed again if present).
    pub already_purged: Vec<i64>,
}

/// A user as the merge request screens name them (MR B0).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserNameBody {
    pub username: String,
    pub display_name: String,
}

impl From<UserName> for UserNameBody {
    fn from(name: UserName) -> Self {
        Self {
            username: name.username,
            display_name: name.display_name,
        }
    }
}

/// A person a merge request names, in the development status's shape
/// (`docs/api/dev-status.md`): no employee number without the caller's
/// `user_employee_number` scope (a browser session never sees one).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestPersonBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
    pub display_name: String,
    /// Only for a token holding `user_employee_number` (absent otherwise,
    /// and for a person without one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub employee_number: Option<String>,
}

impl MergeRequestPersonBody {
    fn of(id: UserId, name: &UserName) -> Self {
        Self {
            id: id.get(),
            username: name.username.clone(), // clone: the row is borrowed
            display_name: name.display_name.clone(),
            employee_number: None,
        }
    }
}

// Built only by `body_of`; its doc comment is the schema description.
/// One merge request as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The project's own number of it.
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    pub description: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    pub draft: bool,
    /// The branch name without `refs/heads/`.
    pub source_branch: String,
    pub target_branch: String,
    #[schema(minimum = 1)]
    pub author_id: i64,
    /// The author (read with the merge request); `employee_number` only on
    /// the list and the detail, for a token holding `user_employee_number`.
    pub author: MergeRequestPersonBody,
    /// Who merged it (the applied merge's requester); `null` unless merged.
    #[schema(required = true)]
    pub merged_by: Option<MergeRequestPersonBody>,
    /// The head commit of the latest version (`null` until versions are
    /// recorded).
    pub head_sha: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub closed_by: Option<i64>,
}

/// One page of a project's merge requests, newest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestPage {
    pub merge_requests: Vec<MergeRequestBody>,
    /// Pass as `before` for the next page; `null` on the last page.
    pub next_before: Option<i64>,
}

fn short(name: &RefName) -> String {
    name.branch().unwrap_or(name.as_str()).to_owned()
}

pub(crate) fn body_of(mr: &StoredMergeRequest) -> MergeRequestBody {
    MergeRequestBody {
        project_id: mr.project_id.get(),
        iid: mr.iid,
        title: mr.title.clone(), // clone: the row is borrowed
        description: mr.description.clone(),
        state: mr.state.as_str().to_owned(),
        draft: mr.draft,
        source_branch: short(&mr.source_branch),
        target_branch: short(&mr.target_branch),
        author_id: mr.author.get(),
        author: MergeRequestPersonBody::of(mr.author, &mr.author_name),
        merged_by: mr
            .merged_by
            .zip(mr.merged_by_name.as_ref())
            .map(|(id, name)| MergeRequestPersonBody::of(id, name)),
        head_sha: mr.head_sha.clone(),
        created_at: mr.created_at.clone(),
        updated_at: mr.updated_at.clone(),
        closed_at: mr.closed_at.clone(),
        closed_by: mr.closed_by.map(UserId::get),
    }
}

/// The people of `rows` (authors and mergers), for
/// [`Review::employee_numbers`].
fn people(rows: &[StoredMergeRequest]) -> Vec<UserId> {
    rows.iter()
        .flat_map(|mr| [Some(mr.author), mr.merged_by])
        .flatten()
        .collect()
}

/// `body` with the employee numbers the caller may see (an empty map: none).
fn with_numbers(
    mut body: MergeRequestBody,
    numbers: &std::collections::BTreeMap<UserId, String>,
) -> MergeRequestBody {
    for person in [Some(&mut body.author), body.merged_by.as_mut()]
        .into_iter()
        .flatten()
    {
        person.employee_number = numbers.get(&UserId::new(person.id)).cloned();
    }
    body
}

/// The review commands; `FG-SYS-0001 reason=review_unconfigured` when the
/// server was built without them (`forge serve` always sets them).
fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

/// A read behind the viewer flags (U2 review): the flags are a hint, so a
/// failed read closes them (`None`: every flag `false`) instead of failing
/// the answer — logged once here, at warn, with the code and the request.
fn viewer_read<T>(
    ctx: &crate::Ctx,
    read: &'static str,
    result: Result<T, ForgeError>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(
                target: "forge_web",
                event = "forge.web.viewer_flags_closed",
                error_code = e.code().id(),
                request_id = %ctx.request_id,
                read,
                error = %e,
                "a read behind the viewer flags failed; every flag is false (the answer is unchanged)"
            );
            None
        }
    }
}

// ── handlers ──

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

fn review_request<B>(w: &WriteParts<MergeRequestCtx, B>) -> ReviewRequest {
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// The repository of the loaded project row (read with the access facts).
fn repository<B>(
    request: &Dispatched<MergeRequestCtx, B>,
) -> Result<ProjectRepository, ForgeError> {
    request.project().map(|r| r.repository).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "merge_request_without_project_row")
            .with_context("project_id", request.proof().id().0.get())
    })
}

/// The merge request read for `GET …/{iid}` (the loader read it before
/// the decision; the answer is read again, after it).
async fn written(
    state: &AppState,
    caller: &Principal,
    project: ProjectId,
    iid: i64,
) -> Result<Value, ForgeError> {
    let stored = bounded(
        state,
        "merge_request_load",
        review(state)?.merge_request(project, iid),
    )
    .await?
    .ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "written_merge_request_missing")
            .with_context("iid", iid)
    })?;
    let numbers = bounded(
        state,
        "merge_request_people",
        review(state)?.employee_numbers(caller, &people(std::slice::from_ref(&stored))),
    )
    .await?;
    json(with_numbers(body_of(&stored), &numbers))
}

/// `state`, `limit` and `before`, strictly: `FG-WEB-0006
/// reason=param_invalid`.
fn page_query(
    query: Option<&str>,
) -> Result<(Option<MergeRequestState>, usize, Option<i64>), ForgeError> {
    let (mut state, mut limit, mut before): (Option<&str>, Option<&str>, Option<&str>) =
        (None, None, None);
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "state" => (&mut state, "state"),
            "limit" => (&mut limit, "limit"),
            "before" => (&mut before, "before"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let state = match state {
        None => Some(MergeRequestState::Open),
        Some("all") => None,
        Some(raw) => Some(
            MergeRequestState::parse(raw).ok_or_else(|| param_invalid("state", "unknown_value"))?,
        ),
    };
    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_PAGE)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    let before = match before {
        None => None,
        Some(raw) => Some(
            parse_positive_id(raw)
                .ok_or_else(|| param_invalid("before", "not_positive_decimal"))?,
        ),
    };
    Ok((state, limit, before))
}

/// `GET …/merge-requests` → `200` one page.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let query = page_query(request.raw_query());
    let id = *request.proof().id();
    let caller = request.principal().clone(); // clone: owned across the awaits
    let built = async {
        let (wanted, limit, before) = query?;
        let rows = bounded(
            &state,
            "merge_request_page",
            review(&state)?.merge_requests(id, wanted, before, limit),
        )
        .await?;
        let next_before = if rows.len() == limit {
            rows.last().map(|r| r.iid)
        } else {
            None
        };
        let numbers = bounded(
            &state,
            "merge_request_people",
            review(&state)?.employee_numbers(&caller, &people(&rows)),
        )
        .await?;
        json(MergeRequestPage {
            merge_requests: rows
                .iter()
                .map(|mr| with_numbers(body_of(mr), &numbers))
                .collect(),
            next_before,
        })
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `GET …/merge-requests/{iid}` → `200` the merge request as decided on.
pub async fn view(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let (project, iid) = *request.proof().id();
    let caller = request.principal().clone(); // clone: owned across the awaits
    let built = async {
        let iid = iid.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
        })?;
        written(&state, &caller, project, iid).await
    }
    .await;
    respond(request.ctx(), StatusCode::OK, built)
}

/// `POST …/merge-requests` → `201` the merge request as committed (the
/// command's own row: a committed write never answers 5xx for a re-read).
pub async fn create(
    state: AppState,
    request: Dispatched<MergeRequestCtx, CreateMergeRequest>,
) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let fields = NewFields {
            title: w.input.title.clone(), // clone: the input stays borrowed by the parts
            description: w.input.description.clone(), // clone: as the title
        };
        let opened = review(&state)?
            .open(&review_request(&w), &w.proof, repo?, &fields)
            .await?;
        json(body_of(&opened))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `PATCH …/merge-requests/{iid}` → `200` the merge request as it is now
/// (also when nothing changed).
pub async fn update(
    state: AppState,
    request: Dispatched<MergeRequestCtx, UpdateMergeRequest>,
) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let fields = EditFields {
            title: w.input.title.clone(), // clone: the input stays borrowed by the parts
            description: w.input.description.clone(), // clone: as the title
        };
        let (_, stored) = review(&state)?
            .edit(&review_request(&w), &w.proof, repo?, &fields)
            .await?;
        json(body_of(&stored))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/merge-requests/{iid}/close` → `200` the merge request (also
/// when it was already closed).
pub async fn close(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let (_, stored) = review(&state)?.close(&review_request(&w), &w.proof).await?;
        json(body_of(&stored))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/merge-requests/{iid}/reopen` → `200` the merge request (also
/// when it was already open).
pub async fn reopen(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let (_, stored) = review(&state)?
            .reopen(&review_request(&w), &w.proof, repo?)
            .await?;
        json(body_of(&stored))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/merge-requests/{iid}/version-refs/purge` → `200` what was
/// purged (also when every version was purged before).
pub async fn purge_version_refs(
    state: AppState,
    request: Dispatched<MergeRequestCtx, PurgeVersionRefs>,
) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let outcome = review(&state)?
            .purge_version_refs(
                &review_request(&w),
                &w.proof,
                repo?,
                w.input.versions.as_deref(),
            )
            .await?;
        json(VersionRefsPurged {
            iid: outcome.merge_request.iid,
            purged: outcome.purged,
            already_purged: outcome.already_purged,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

#[cfg(test)]
mod tests;
