//! The simple editor's commit (E1, user decision of 2026-11-12):
//! `POST /api/v1/projects/{id}/repository/commits/single-file` creates or
//! replaces ONE text file and commits it — on a new branch (and, by
//! default, opens a merge request into the branch it started from), or
//! directly on the branch when the push rules allow it. The commit behaves
//! exactly like a push of that commit:
//!
//! 1. The registry authenticated and proved the project-level `RepoPush`
//!    (Developer; a token needs `write_repository`) before the body was
//!    looked at — the push's own first decision.
//! 2. The body is checked (`FG-WEB-0006` for the fields, `FG-GIT-0028` the
//!    path, `FG-GIT-0029` / `FG-GIT-0030` the content) and the content is
//!    scanned with the push checks' secret rules (`FG-GIT-0022`; a file
//!    carrying `forge:allow-secret` is a reviewed waiver, logged like the
//!    push's). The content is at most [`MAX_CONTENT_BYTES`] (so the scan
//!    reads it whole) and the server's push blob limit.
//! 3. The ref update is decided by `forge_auth::authorize_ref_updates` on
//!    the push's protection snapshot (`ProtectionLoader`): a create of the
//!    new branch, or a fast-forward of the branch — so a protected branch's
//!    minimum role (`FG-GIT-0008`), its direct-push ban (`FG-GIT-0017`) and
//!    the reserved namespaces refuse here as they refuse a push. Nothing is
//!    written to git before that decision.
//! 4. No silent overwrite: the branch's head is read; when it is not
//!    `start_sha`, the file at both commits must be the same (else
//!    `FG-GIT-0027 reason=file_changed`) and the commit is built on the
//!    head. Unchanged content is `FG-GIT-0031`.
//! 5. git writes the blob, a temporary index's tree and the commit
//!    (`forge_git::file_commit`), then ONE compare-and-swap `update-ref`
//!    (`create` for the new branch; `update <branch> <new> <head>`): a
//!    concurrent push makes it fail, the head is read again and the build
//!    retried (`forge_git::file_commit::ATTEMPTS` in all, then `FG-GIT-0027 reason=ref_moved`); an
//!    existing new branch is `FG-GIT-0027 reason=branch_exists`.
//! 6. Then what a push does after git: the browse caches are dropped, the
//!    `forge.git.push_applied` line is logged (the replay input), and the
//!    push is recorded through the same `PushRecorder` with the same
//!    retries (`git_push::record_applied`: the `repository.pushed` audit row
//!    and event — pipelines, merge request versions, webhooks and the feed
//!    follow it — and the commit as introduced). The record runs in its own
//!    task counted in `SmartHttp::push_tasks`, so a client that goes away
//!    does not lose it.
//! 7. A new branch with `open_merge_request` (the default) opens the merge
//!    request through the review service (`Review::open`, decided on
//!    `MergeRequestCreate` with `authorize()` and re-decided inside its
//!    transaction). The commit stands whatever happens there: a refusal is
//!    logged once and answered in `merge_request_error` next to the commit.

use std::sync::Arc;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    authorize_ref_updates, prove, Action, LoadedMergeRequest, MergeRequestCtx, MergeRequestState,
    ProjectCtx, RefUpdate, RefUpdateCtx,
};
use forge_core::{
    telemetry, ObjectId, ProjectId, PushApplied, PushRecord, PushUpdateKind, PushedCommits,
    PushedRef, Pusher, RefName, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::file_commit::{check_content, FilePath, OneFile};
use forge_git::secret_scan::{self, SecretScan};
use forge_review::{NewFields, Review, ReviewRequest};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, WriteParts};
use crate::routes::browse::{load_project_lookup, Target};
use crate::routes::git_push::{record_applied, user_agent, PushLog, PUSH_CHECK_TIMEOUT};
use crate::routes::merge_requests::{body_of, MergeRequestBody};
use crate::routes::projects::{self, PREFIX as PROJECT_PREFIX};
use crate::routes::write::{bounded, id_in, json, no_store};

/// `POST` one file as a commit.
pub const PATH: &str = "/api/v1/projects/{id}/repository/commits/single-file";
/// The path after `{id}`.
const SUFFIX: &str = "/repository/commits/single-file";
/// The body cap: a whole file, JSON-escaped (`body::LARGE_BODY_ROUTES`).
pub const MAX_BODY: usize = crate::body::MAX_FILE_BODY_BYTES;
/// The largest content (bytes): half the body cap, so even a file of short
/// lines (each newline escaped to two bytes) fits.
pub const MAX_CONTENT_BYTES: usize = 512 * 1024;
/// The longest commit message (bytes), the merge's.
pub const MAX_MESSAGE_BYTES: usize = forge_git::commit::MAX_MESSAGE_BYTES;
/// The route name in the log.
const ROUTE: &str = "single_file_commit";

/// `POST …/repository/commits/single-file`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SingleFileCommit {
    /// The branch the editor read the file from: `main` or
    /// `refs/heads/main`. It must exist.
    pub branch: String,
    /// The commit the editor read the file at (40 / 64 hex). When the
    /// branch moved since, the file must be unchanged there (else 409
    /// FG-GIT-0027).
    pub start_sha: String,
    /// The file's path from the repository root (`docs/spec.md`); created
    /// when absent (with its directories).
    pub path: String,
    /// The whole new content: UTF-8 text without NUL, at most 512 KiB.
    pub content: String,
    /// The commit message (not blank, at most 64 KiB).
    pub message: String,
    /// Commit on this NEW branch (created from the branch's head) instead
    /// of the branch itself.
    #[serde(default)]
    pub create_branch: Option<String>,
    /// With `create_branch`: open a merge request into `branch` (default
    /// true). Without `create_branch` only `false` is accepted.
    #[serde(default)]
    pub open_merge_request: Option<bool>,
    /// The merge request's title (default: the message's first line; 1..=255
    /// characters).
    #[serde(default)]
    pub title: Option<String>,
}

/// Why the merge request was not opened (the commit stands).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestNotOpened {
    pub error_code: String,
    /// The user message in the request's language.
    pub message: String,
}

/// The commit as written.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SingleFileCommitBody {
    /// The new commit.
    pub commit_sha: String,
    /// The commit it was built on (the branch's head).
    pub parent_sha: String,
    /// The branch the commit is on (without `refs/heads/`).
    pub branch: String,
    /// Whether `branch` was created by this commit.
    pub branch_created: bool,
    pub path: String,
    /// The file's new blob.
    pub blob_sha: String,
    /// The merge request opened for the new branch (null when none was
    /// asked or it could not be opened).
    pub merge_request: Option<MergeRequestBody>,
    /// Set when the merge request was asked for and refused.
    pub merge_request_error: Option<MergeRequestNotOpened>,
}

/// Loads the project of `…/{id}/repository/commits/single-file` (the
/// browse routes' lookup; the row carries the repository).
pub struct SingleFileOf;

impl BodyLoader<AppState, SingleFileCommit> for SingleFileOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a SingleFileCommit,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(async move {
            let Some(id) = id_in(parts.uri.path(), PROJECT_PREFIX, SUFFIX).map(ProjectId::new)
            else {
                return Err(param_invalid("id", "not_positive_decimal"));
            };
            load_project_lookup(state, id, viewer).await
        })
    }
}

/// A branch as the API takes it: `main` or `refs/heads/main`.
fn branch(raw: &str, param: &'static str) -> Result<RefName, ForgeError> {
    let full = if raw.starts_with("refs/") {
        raw.to_owned()
    } else {
        format!("refs/heads/{raw}")
    };
    if !full.starts_with("refs/heads/") {
        return Err(param_invalid(param, "not_a_branch"));
    }
    let name = RefName::parse(&full).map_err(|_| param_invalid(param, "invalid_ref"))?;
    forge_git::receive_request::check_unambiguous(&name)
        .map_err(|_| param_invalid(param, "hex_like"))?;
    Ok(name)
}

fn short(name: &RefName) -> String {
    name.branch().unwrap_or(name.as_str()).to_owned()
}

/// The request's fields, checked (step 2 without the scan).
#[derive(Debug)]
struct Checked {
    branch: RefName,
    start: ObjectId,
    path: FilePath,
    target: RefName,
    created: bool,
    open_mr: bool,
    title: String,
}

fn check(input: &SingleFileCommit, blob_limit: u64) -> Result<Checked, ForgeError> {
    let from = branch(&input.branch, "branch")?;
    let start = ObjectId::parse(&input.start_sha).map_err(|_| param_invalid("start_sha", "oid"))?;
    if input.message.trim().is_empty() {
        return Err(param_invalid("message", "blank"));
    }
    if input.message.len() > MAX_MESSAGE_BYTES {
        return Err(param_invalid("message", "too_long"));
    }
    let (target, created) = match &input.create_branch {
        Some(raw) => {
            let new = branch(raw, "create_branch")?;
            if new == from {
                return Err(param_invalid("create_branch", "same_as_branch"));
            }
            (new, true)
        }
        None => (from.clone(), false), // clone: the target is the branch itself
    };
    let open_mr = match (input.open_merge_request, created) {
        (Some(true), false) => {
            return Err(param_invalid("open_merge_request", "needs_create_branch"))
        }
        (Some(open), true) => open,
        (None, true) => true,
        (_, false) => false,
    };
    let title = match &input.title {
        Some(t) => t.clone(), // clone: the checked request owns it
        None => input
            .message
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned(),
    };
    if open_mr && (title.trim().is_empty() || title.chars().count() > 255) {
        return Err(param_invalid("title", "length"));
    }
    let path = FilePath::parse(&input.path)?;
    let limit = usize::try_from(blob_limit)
        .unwrap_or(usize::MAX)
        .min(MAX_CONTENT_BYTES);
    check_content(&input.content, limit)?;
    Ok(Checked {
        branch: from,
        start,
        path,
        target,
        created,
        open_mr,
        title,
    })
}

/// The push checks' secret rules on the one new blob (step 2).
fn scan(content: &str, path: &FilePath, log: &PushLog) -> Result<(), ForgeError> {
    match secret_scan::scan(content.as_bytes()) {
        SecretScan::Clean | SecretScan::Binary => Ok(()),
        SecretScan::Found(rule) => Err(ForgeError::new(ErrorCode::PushSecretDetected)
            .with_context("event", "forge.git.push_checked")
            .with_context("verdict", "refused")
            .with_context("findings", 1)
            .with_context("first_rule", rule)
            .with_context("first_path", path.as_str().to_owned())),
        SecretScan::Allowed(rule) => {
            tracing::info!(
                target: "forge_web::git",
                event = "forge.git.push_check_waived",
                request_id = %log.request_id,
                repo_id = log.repo_id,
                route = ROUTE,
                rule,
                path = %path.as_str(),
                "a reviewed false positive (forge:allow-secret)"
            );
            Ok(())
        }
    }
}

fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

/// `POST …/repository/commits/single-file` → `201` the commit (and the
/// merge request when one was opened).
pub async fn commit(
    state: AppState,
    request: Dispatched<ProjectCtx, SingleFileCommit>,
) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match commit_inner(&state, request).await {
        Ok(body) => no_store(StatusCode::CREATED, body),
        Err(err) => ctx.fail(err.with_context("route", ROUTE)).into_response(),
    }
}

async fn commit_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx, SingleFileCommit>,
) -> Result<serde_json::Value, ForgeError> {
    let target = Target::of(state, &request)?;
    let smart = Arc::clone(&target.smart);
    let Some(push) = smart.push.clone() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "push_unconfigured")
        );
    };
    let Some(user) = request.principal().user() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "git_route_without_user")
        );
    };
    let Some(project_row) = request.project().cloned() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_project")
        );
    };
    let peer = request.peer();
    let agent = user_agent(&request);
    // Nothing of the request (not `Sync`) is held across an await.
    let w = request.into_write();
    let ctx = w.ctx.clone(); // clone: the log and the answer keep it
    let repo = target.repository.id;
    let fail = |e: ForgeError| {
        e.with_context("project_id", target.project.get())
            .with_context("repo_id", repo.get())
    };
    let log = PushLog {
        request_id: ctx.request_id.clone(), // clone: the record task owns its copy
        project_id: target.project,
        repo_id: repo.get(),
        lang: ctx.lang,
        route: ROUTE,
    };
    let input = &w.input;
    let checked = check(input, smart.limits.push_max_blob_bytes).map_err(fail)?;
    scan(&input.content, &checked.path, &log).map_err(fail)?;

    // Step 3: the ref update decided like a push's, before git writes.
    let rules = bounded(
        state,
        "protection_load",
        push.protection.load_protection(target.project),
    )
    .await
    .map_err(fail)?;
    let kind = if checked.created {
        RefUpdate::Create
    } else {
        RefUpdate::FastForward
    };
    let decided = authorize_ref_updates(
        &w.proof,
        &[RefUpdateCtx::new(checked.target.clone(), kind, &rules)], // clone: the context owns its name
    )?;
    if let Some(Err(denial)) = decided.into_iter().next() {
        return Err(fail(denial));
    }
    let review = review(state)?;
    let author = review.commit_identity(user).await.map_err(fail)?;

    // Steps 4 and 5 (forge-git: the conflict check, the writes, the CAS).
    let browse = smart.browse.clone(); // clone: a shared cache handle
    let written = smart
        .git
        .commit_one_file(
            &smart.storage,
            repo,
            &OneFile {
                branch: &checked.branch,
                target: &checked.target,
                create: checked.created,
                start: &checked.start,
                path: &checked.path,
                content: input.content.as_bytes(),
                message: &input.message,
                author: &author,
            },
            PUSH_CHECK_TIMEOUT,
            // Every update-ref outcome may have changed the ref.
            &move || browse.invalidate(repo),
        )
        .await
        .map_err(fail)?;
    let (blob, attempts) = (written.blob.clone(), written.attempts); // clone: the answer keeps it
    tracing::info!(
        target: "forge_web::git",
        event = "forge.git.single_file_committed",
        request_id = %log.request_id,
        project_id = target.project.get(),
        repo_id = log.repo_id,
        branch = %checked.target.as_str(),
        created = checked.created,
        commit = %written.commit.as_str(),
        parent = %written.parent.as_str(),
        attempts,
    );

    // Step 6: recorded like a push, in a task the request does not own.
    let old = if checked.created {
        ObjectId::parse(&"0".repeat(written.commit.as_str().len()))?
    } else {
        written.parent.clone() // clone: the record owns its ids
    };
    let record = PushRecord {
        repository_id: repo,
        updates: vec![PushedRef {
            name: checked.target.clone(), // clone: the record owns its name
            old,
            new: written.commit.clone(), // clone: as above
            kind: if checked.created {
                PushUpdateKind::Create
            } else {
                PushUpdateKind::FastForward
            },
            commits: vec![written.commit.clone()], // clone: as above
            total_commits: 1,
            total_commits_partial: false,
        }],
    };
    let pusher = Pusher {
        request_id: ctx.request_id.clone(), // clone: the record keeps its own
        user,
        kind: w.proof.principal().account_kind(),
        credential_id: w.credential_id,
        source_ip: peer,
        user_agent: agent,
    };
    let line = PushApplied::new(&pusher, target.project, &record);
    if let Err(err) = line.log_as("final", "single_file_commit") {
        log.failure(err);
    }
    let commits = PushedCommits {
        commits: vec![written.commit.clone()], // clone: the record task owns it
        complete: true,
    };
    let recorder = Arc::clone(&push.recorder);
    let task_log = log.clone(); // clone: the task owns its copy
    let guard = smart.push_tasks.start();
    let recorded = tokio::spawn(async move {
        let _guard = guard;
        record_applied(&*recorder, &pusher, &record, &commits, &task_log).await;
    });
    if let Err(e) = recorded.await {
        // The task panicked or was cancelled: the record may be missing.
        log.failure(
            ForgeError::new(ErrorCode::PushRecordFailed)
                .with_context("reason", "record_task_failed")
                .with_context("detail", e.to_string()),
        );
    }

    // Step 7.
    let (merge_request, merge_request_error) = if checked.open_mr {
        match open_merge_request(review, &w, &project_row, &checked).await {
            Ok(mr) => (Some(mr), None),
            Err(e) => {
                let code = e.code();
                telemetry::log_failure(
                    &fail(e)
                        .with_context("route", ROUTE)
                        .with_context("phase", "open_merge_request")
                        .with_context("commit", written.commit.as_str().to_owned()),
                    &ctx.request_id,
                );
                (
                    None,
                    Some(MergeRequestNotOpened {
                        error_code: code.id().to_owned(),
                        message: code.user_message(ctx.lang).to_owned(),
                    }),
                )
            }
        }
    } else {
        (None, None)
    };
    json(SingleFileCommitBody {
        commit_sha: written.commit.as_str().to_owned(),
        parent_sha: written.parent.as_str().to_owned(),
        branch: short(&checked.target),
        branch_created: checked.created,
        path: checked.path.as_str().to_owned(),
        blob_sha: blob.as_str().to_owned(),
        merge_request,
        merge_request_error,
    })
}

/// Open the merge request of the new branch into the branch (step 7),
/// decided on `MergeRequestCreate` for the same caller.
async fn open_merge_request(
    review: &Review,
    w: &WriteParts<ProjectCtx, SingleFileCommit>,
    record: &forge_core::ProjectRecord,
    checked: &Checked,
) -> Result<MergeRequestBody, ForgeError> {
    let Some(user) = w.proof.principal().user() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_project")
        );
    };
    let proof = prove(
        w.proof.principal(),
        Action::MergeRequestCreate,
        MergeRequestCtx::new(
            projects::loaded(record),
            LoadedMergeRequest {
                iid: None,
                author: user,
                state: MergeRequestState::Open,
                draft: false,
                source_branch: checked.target.clone(), // clone: the context owns it
                target_branch: checked.branch.clone(), // clone: as above
            },
        ),
    )?;
    let opened = review
        .open(
            &ReviewRequest {
                request_id: w.ctx.request_id.clone(), // clone: the request keeps its id
                credential_id: w.credential_id,
            },
            &proof,
            record.repository,
            &NewFields {
                title: checked.title.clone(), // clone: the fields own it
                description: String::new(),
            },
        )
        .await?;
    Ok(body_of(&opened))
}

#[cfg(test)]
#[path = "file_commit_tests.rs"]
mod tests;
