//! The first commit of a project the server creates for a person (plan 20:
//! a wiki, W2; a person's notes, N1): a root commit of a few files on a new
//! branch, recorded exactly like a push of it (the `forge.git.push_applied`
//! line, then `git_push::record_applied` in its own counted task: the
//! `repository.pushed` audit row and event — search indexing, webhooks and
//! the feed follow it).
//!
//! No separate `RepoPush` decision: the commit is part of the creation the
//! route already decided (the caller is the new project's creator and
//! Owner; ADR-0040 Amendment W1/W2).

use std::sync::Arc;
use std::time::Duration;

use forge_auth::ResourceCtx;
use forge_core::{
    ObjectId, ProjectId, PushApplied, PushRecord, PushUpdateKind, PushedCommits, PushedRef, Pusher,
    RefName, RepoId, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::file_commit::{FilePath, RootFiles};

use crate::app::AppState;
use crate::registry::WriteParts;
use crate::routes::git::SmartHttp;
use crate::routes::git_push::{record_applied, PushLog};

/// Each git step of the first commit.
pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// What to commit.
pub(crate) struct FirstCommit<'a> {
    pub(crate) project: ProjectId,
    pub(crate) repo: RepoId,
    /// The full branch name (`refs/heads/main`).
    pub(crate) branch: &'a str,
    /// `(path, content)` in commit order.
    pub(crate) files: &'a [(&'static str, String)],
    pub(crate) message: &'a str,
    /// The route name in logs.
    pub(crate) route: &'static str,
}

/// Write `first` as the root commit of a new branch by `user` and record it
/// like a push (module docs).
pub(crate) async fn commit<R: ResourceCtx, B>(
    state: &AppState,
    smart: &SmartHttp,
    w: &WriteParts<R, B>,
    first: &FirstCommit<'_>,
    user: UserId,
) -> Result<ObjectId, ForgeError> {
    let repo = first.repo;
    let review = state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })?;
    let author = review.commit_identity(user).await?;
    let files = first
        .files
        .iter()
        .map(|(path, content)| Ok((FilePath::parse(path)?, content.as_bytes())))
        .collect::<Result<Vec<_>, ForgeError>>()?;
    let branch = RefName::parse(first.branch)?;
    let commit = smart
        .git
        .commit_root_files(
            &smart.storage,
            repo,
            &RootFiles {
                branch: &branch,
                files: &files,
                message: first.message,
                author: &author,
            },
            GIT_TIMEOUT,
        )
        .await;
    // Whatever the outcome, the ref may have changed.
    smart.browse.invalidate(repo);
    let commit = commit?;
    if let Some(push) = smart.push.clone() {
        let record = PushRecord {
            repository_id: repo,
            updates: vec![PushedRef {
                name: branch,
                old: ObjectId::parse(&"0".repeat(commit.as_str().len()))?,
                new: commit.clone(), // clone: the record owns its ids
                kind: PushUpdateKind::Create,
                commits: vec![commit.clone()], // clone: as above
                total_commits: 1,
                total_commits_partial: false,
            }],
        };
        let pusher = Pusher {
            request_id: w.ctx.request_id.clone(), // clone: the record keeps its own
            user,
            kind: w.proof.principal().account_kind(),
            credential_id: w.credential_id,
            source_ip: None,
            user_agent: None,
        };
        let log = PushLog {
            request_id: w.ctx.request_id.clone(), // clone: as above
            project_id: first.project,
            repo_id: repo.get(),
            lang: w.ctx.lang,
            route: first.route,
        };
        let line = PushApplied::new(&pusher, first.project, &record);
        if let Err(err) = line.log_as("final", first.route) {
            log.failure(err);
        }
        let commits = PushedCommits {
            commits: vec![commit.clone()], // clone: the record task owns it
            complete: true,
        };
        let recorder = Arc::clone(&push.recorder);
        let guard = smart.push_tasks.start();
        let recorded = tokio::spawn(async move {
            let _guard = guard;
            record_applied(&*recorder, &pusher, &record, &commits, &log).await;
        });
        if let Err(e) = recorded.await {
            tracing::warn!(
                target: "forge_web",
                event = "forge.first_commit.record_failed",
                request_id = %w.ctx.request_id,
                route = first.route,
                error_code = ErrorCode::PushRecordFailed.id(),
                detail = %e,
                "the first commit of a created project could not be recorded as a push"
            );
        }
    }
    Ok(commit)
}
