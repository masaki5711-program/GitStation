//! `forge admin repo reprovision <repo_id>` (step D4 MR3b): enqueue the
//! provisioning job of a repository still in `provisioning` again, after
//! its job died (`forge doctor`: `repositories.provisioning`,
//! FG-REPO-0012). A mutating operator command: the host operator is the
//! trust root (outside `authorize()`), `forge_db::repositories::reprovision`
//! enforces the rules itself (a ready repository is refused with
//! `FG-REPO-0014 reason=repository_ready`), and in production running it is
//! an approved runbook step (ops/runbooks/repository-provisioning.md).
//! The log event carries ids only.
//!
//! `forge admin repo record-push --from-log <file>` (M1 phase 9 MR2):
//! record pushes whose `repository.pushed` audit row and event could not be
//! written (`FG-GIT-0012`, runbook push-record-failed) from their
//! `forge.git.push_applied` log lines (`forge_core::PushApplied`, the line
//! forge-web logs before recording). Every line is read and checked before
//! anything is written; lines of other events are skipped. Then every
//! push is checked against the database (`repositories::check_replay`: the
//! repository exists, belongs to the logged project, the logged pusher
//! exists) before the first write. Each push is then recorded in its own
//! transaction by `forge_db::repositories::replay_push` under the
//! **logged** request id, so the dedup key is the one the original attempt
//! used: a push already recorded is reported as such and nothing changes,
//! and the command can be run again after a partial failure. The actor is
//! the operator (`system`, `actor_label` naming the CLI and the OS
//! account); the logged pusher is recorded as `replayed_from`, never as the
//! actor (the file is not trusted to say who acted). A mutating command:
//! in production an approved runbook step.

use std::path::Path;

use forge_core::config::DatabaseConfig;
use forge_core::ids::{ProjectId, RepoId};
use forge_core::{PushApplied, PushRecord, PushRecorded, PushReplay, Pusher, RequestId};
use forge_db::repositories::{self, Reprovisioned};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::admin_token::{context, with_db};

pub fn reprovision(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    repo_id: i64,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| {
        reprovision_with(db, request_id, repo_id).await
    })
}

pub(crate) async fn reprovision_with(
    db: &Db,
    request_id: &RequestId,
    repo_id: i64,
) -> Result<String, ForgeError> {
    let repo = RepoId::new(repo_id);
    let done = repositories::reprovision(db, &context(request_id), repo).await?;
    let (project_id, job, after, outcome, job_state) = match done {
        Reprovisioned::Queued {
            project_id,
            job,
            after,
        } => (project_id, job, after, "queued", "queued"),
        Reprovisioned::AlreadyLive {
            project_id,
            job,
            state,
        } => (project_id, job, None, "already_live", state.as_str()),
    };
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.repo.reprovisioned",
        request_id = %request_id,
        repo_id,
        project_id = project_id.get(),
        job_id = job.get(),
        after_job_id = after.map(|a| a.get()),
        outcome,
        job_state,
        "repository reprovision"
    );
    Ok(match (done, after) {
        (Reprovisioned::Queued { .. }, None) => format!(
            "repository {repo_id}: provisioning job {} queued; a running `forge worker` \
             picks it up (check with `forge doctor`, repositories.provisioning)\n",
            job.get()
        ),
        (Reprovisioned::Queued { .. }, Some(after)) => format!(
            "repository {repo_id}: provisioning job {} queued after job {} (which succeeded \
             without the repository becoming ready: a database restore or a manual change); \
             a running `forge worker` picks it up\n",
            job.get(),
            after.get()
        ),
        (Reprovisioned::AlreadyLive { .. }, _) => format!(
            "repository {repo_id}: provisioning job {} is already {job_state} \
             (nothing changed); make sure a `forge worker` runs\n",
            job.get()
        ),
    })
}

/// The largest log file `record-push` reads (an excerpt, not a whole log).
pub const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

fn log_error(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PushRecordFailed).with_context("reason", reason)
}

/// The pushes of the `forge.git.push_applied` lines of `text`, checked, in
/// file order. A push line that cannot be replayed stops everything (with
/// its 1-based `line`); no push line at all is `push_log_empty`.
pub(crate) fn pushes_in(text: &str) -> Result<Vec<(Pusher, ProjectId, PushRecord)>, ForgeError> {
    let mut pushes = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let parsed = PushApplied::from_log_line(line)
            .and_then(|applied| applied.map(|a| a.to_parts()).transpose())
            .map_err(|e| e.with_context("line", n + 1))?;
        if let Some(push) = parsed {
            pushes.push(push);
        }
    }
    if pushes.is_empty() {
        return Err(log_error("push_log_empty"));
    }
    Ok(pushes)
}

fn read_log(path: &Path) -> Result<String, ForgeError> {
    let size = std::fs::metadata(path)
        .map_err(|e| log_error("push_log_unreadable").with_source(e))?
        .len();
    if size > MAX_LOG_BYTES {
        return Err(log_error("push_log_too_large")
            .with_context("bytes", size)
            .with_context("max_bytes", MAX_LOG_BYTES));
    }
    let bytes = std::fs::read(path).map_err(|e| log_error("push_log_unreadable").with_source(e))?;
    String::from_utf8(bytes)
        .map_err(|_| log_error("push_log_invalid").with_context("field", "utf8"))
}

pub fn record_push(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    from_log: &Path,
) -> Result<String, ForgeError> {
    let text = read_log(from_log)?;
    with_db(cfg, async |db| {
        record_push_with(db, request_id, &text).await
    })
}

pub(crate) async fn record_push_with(
    db: &Db,
    request_id: &RequestId,
    text: &str,
) -> Result<String, ForgeError> {
    let pushes = pushes_in(text)?;
    let replay_of = |pusher: &Pusher| PushReplay {
        pusher: pusher.user,
        credential_id: pusher.credential_id,
    };
    // All or nothing up to the first write: every line must name an
    // existing repository of its logged project and an existing pusher.
    for (n, (pusher, logged_project, push)) in pushes.iter().enumerate() {
        repositories::check_replay(db, *logged_project, replay_of(pusher), push)
            .await
            .map_err(|e| {
                e.with_context("push", n + 1)
                    .with_context("push_request_id", &pusher.request_id)
            })?;
    }
    // The operator acts (system + the CLI's label); the logged pusher goes
    // into the record as `replayed_from`.
    let operator = context(request_id);
    let mut out = String::new();
    for (pusher, logged_project, push) in &pushes {
        let mut ctx = operator.clone(); // clone: one operator context per replayed push
        ctx.request_id = pusher.request_id.clone(); // clone: the logged id keys the record
        let done = repositories::replay_push(db, &ctx, *logged_project, replay_of(pusher), push)
            .await
            .map_err(|e| e.with_context("push_request_id", &pusher.request_id))?;
        let outcome = match done {
            PushRecorded::Recorded => "recorded",
            PushRecorded::Deduped => "already_recorded",
        };
        tracing::info!(
            target: "forge_cli",
            event = "forge.admin.repo.push_recorded",
            request_id = %request_id,
            push_request_id = %pusher.request_id,
            repo_id = push.repository_id.get(),
            project_id = logged_project.get(),
            refs = push.updates.len(),
            outcome,
            "push record replayed"
        );
        let line = match done {
            PushRecorded::Recorded => format!(
                "push {} (repository {}, {} refs): recorded\n",
                pusher.request_id,
                push.repository_id.get(),
                push.updates.len()
            ),
            PushRecorded::Deduped => format!(
                "push {} (repository {}, {} refs): already recorded (nothing changed)\n",
                pusher.request_id,
                push.repository_id.get(),
                push.updates.len()
            ),
        };
        out.push_str(&line);
    }
    Ok(out)
}

#[cfg(test)]
#[path = "admin_repo_tests.rs"]
mod tests;
