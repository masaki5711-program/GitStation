//! The worker's side (ADR-0031 decisions 5, 7, 8): check the deploy key,
//! end unapproved requests, make the automatic requests, take one
//! deployment and run it — `deploy` on every target, then `health` on
//! every target; on a failure roll the changed targets back to the
//! previous version when the environment says so — and record the end.
//!
//! Every failure is logged here, once, with `error_code`, `request_id`,
//! `deployment_id` and `target` (never the script's output: it may carry
//! secrets).

use std::path::Path;
use std::time::Duration;

use forge_core::deploy::{DeploymentStatus, HostKey, RemoteVerb};
use forge_core::RequestId;
use forge_db::deploy::{self as db, Claim, Claimed, Finish, RunTarget};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::transport::{DeployTransport, Outcome, RemoteCall};

/// The timeouts of the remote calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub script: Duration,
    pub health: Duration,
}

/// The most automatic requests made in one pass.
pub const AUTO_BATCH: i64 = 20;

/// Check the deploy key file before anything runs (ADR-0013 style secret
/// file: a regular file, Unix mode without group or other bits). The
/// content is never read.
pub fn check_key_file(path: Option<&Path>) -> Result<(), ForgeError> {
    let unavailable = |reason: &'static str| {
        ForgeError::new(ErrorCode::DeployKeyUnavailable).with_context("reason", reason)
    };
    let Some(path) = path else {
        return Err(unavailable("unconfigured"));
    };
    let meta = std::fs::metadata(path).map_err(|e| unavailable("unreadable").with_source(e))?;
    if !meta.is_file() {
        return Err(unavailable("not_a_file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(
                unavailable("insecure_permissions").with_context("mode", format!("{mode:o}"))
            );
        }
    }
    Ok(())
}

/// What one worker pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pass {
    /// Nothing waiting.
    Idle,
    /// A deployment ended `blocked` at execution time.
    Blocked(i64),
    /// A deployment ran and ended with this status.
    Ran(i64, DeploymentStatus),
}

/// One pass of the `deploy_runner` task. Errors (returned for the
/// supervisor to log): `FG-DEPLOY-0008` (the key; nothing taken), the
/// database's.
pub async fn run_once(
    db_handle: &Db,
    transport: &dyn DeployTransport,
    key_file: Option<&Path>,
    timeouts: Timeouts,
) -> Result<Pass, ForgeError> {
    let request_id = RequestId::generate();
    // Deployments whose worker stopped (this one before a restart
    // included): ended `failed`, never rolled back.
    for (id, project, tier) in db::abandon_stale(db_handle, &request_id, STALE_SECS).await? {
        tracing::error!(
            target: "forge_deploy",
            event = "forge.deploy.abandoned",
            error_code = ErrorCode::DeploymentAbandoned.id(),
            request_id = %request_id,
            deployment_id = id,
            project_id = project,
            environment = tier.as_str(),
            "deployment abandoned: its worker stopped"
        );
    }
    for (id, project, tier) in db::block_unapproved(db_handle, &request_id).await? {
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.blocked",
            error_code = ErrorCode::DeployApprovalRequired.id(),
            request_id = %request_id,
            deployment_id = id,
            project_id = project,
            environment = tier.as_str(),
            reason = "approval_not_given",
            "deployment blocked: no approval within the window"
        );
    }
    for d in db::auto_request(db_handle, &request_id, AUTO_BATCH).await? {
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.requested",
            request_id = %request_id,
            deployment_id = d.id,
            project_id = d.project_id,
            environment = d.tier.as_str(),
            sha = %d.sha,
            status = d.status.as_str(),
            requested_by = "system",
            "automatic deployment requested"
        );
    }
    // Logged once per failure streak by the worker's supervisor
    // (`task=deploy_runner`, with `error_code` and `reason`).
    check_key_file(key_file)?;
    match db::claim_next(db_handle, &request_id).await? {
        Claim::Idle => Ok(Pass::Idle),
        Claim::Blocked {
            id,
            project_id,
            tier,
            code,
            reason,
        } => {
            tracing::info!(
                target: "forge_deploy",
                event = "forge.deploy.blocked",
                error_code = code.id(),
                request_id = %request_id,
                deployment_id = id,
                project_id,
                environment = tier.as_str(),
                reason,
                "deployment blocked at execution time"
            );
            Ok(Pass::Blocked(id))
        }
        Claim::Run(claimed) => {
            // The heartbeat while it runs: a stopped worker leaves a stale
            // one, and the next pass of any worker ends the deployment.
            let beat = {
                let db_handle = db_handle.clone(); // clone: the pool handle
                let id = claimed.id;
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(HEARTBEAT);
                    loop {
                        tick.tick().await;
                        if let Err(e) = db::heartbeat(&db_handle, id).await {
                            tracing::warn!(
                                target: "forge_deploy",
                                event = "forge.deploy.heartbeat_failed",
                                error_code = e.code().id(),
                                deployment_id = id,
                                error = %e,
                                "deployment heartbeat not recorded"
                            );
                        }
                    }
                })
            };
            let end = execute(transport, &claimed, timeouts, &request_id).await;
            let recorded = record_end(db_handle, &request_id, &claimed, &end).await;
            beat.abort();
            recorded?;
            tracing::info!(
                target: "forge_deploy",
                event = "forge.deploy.finished",
                error_code = end.error_code.map(|c| c.id()),
                request_id = %request_id,
                deployment_id = claimed.id,
                project_id = claimed.project_id,
                environment = claimed.tier.as_str(),
                sha = %claimed.sha,
                status = end.status.as_str(),
                "deployment finished"
            );
            Ok(Pass::Ran(claimed.id, end.status))
        }
    }
}

/// How often a running deployment's heartbeat is written.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// A running deployment without a heartbeat for this long is abandoned.
pub const STALE_SECS: u32 = 300;

/// Record the end, retried a few times: a database that cannot take it
/// leaves the deployment `running` without a heartbeat, and the sweep
/// ends it (`FG-DEPLOY-0012`). Every failure is logged here.
async fn record_end(
    db_handle: &Db,
    request_id: &RequestId,
    claimed: &Claimed,
    end: &Finish,
) -> Result<(), ForgeError> {
    let mut last = None;
    for attempt in 1..=3u32 {
        match db::finish(db_handle, request_id, claimed.id, end).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::error!(
                    target: "forge_deploy",
                    event = "forge.deploy.finish_failed",
                    error_code = e.code().id(),
                    request_id = %request_id,
                    deployment_id = claimed.id,
                    status = end.status.as_str(),
                    attempt,
                    error = %e,
                    "deployment end not recorded"
                );
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last.unwrap_or_else(|| ForgeError::new(ErrorCode::Internal)))
}

/// A target and its parsed pin, or why it cannot be called.
fn call_of(
    t: &RunTarget,
    verb: RemoteVerb,
    sha: &str,
    timeout: Duration,
) -> Result<RemoteCall, &'static str> {
    let host_key = HostKey::parse(&t.host_key_type, &t.host_key)?;
    let port = u16::try_from(t.port).map_err(|_| "invalid_port")?;
    // The table's CHECKs hold these; checked again before they reach a
    // command line.
    forge_core::deploy::check_host(&t.host)?;
    forge_core::deploy::check_user(&t.login_user)?;
    forge_core::deploy::check_script_name(&t.script)?;
    if !sha
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("invalid_sha");
    }
    Ok(RemoteCall {
        target: t.name.clone(), // clone: the call owns its strings
        host: t.host.clone(),   // clone: as target
        port,
        user: t.login_user.clone(), // clone: as target
        host_key,
        script: t.script.clone(), // clone: as target
        verb,
        sha: sha.to_owned(),
        timeout,
    })
}

/// A failed step: the code, the target, and whether the target's state
/// may have changed (the script ran).
struct Failure<'a> {
    code: ErrorCode,
    target: &'a RunTarget,
    host_key_mismatch: bool,
    /// The remote script may have run (it failed, timed out, or the
    /// session was lost after it opened): the target counts as changed.
    may_have_run: bool,
}

/// Run one step on one target and log a failure.
async fn step<'a>(
    transport: &dyn DeployTransport,
    claimed: &Claimed,
    t: &'a RunTarget,
    (verb, sha, timeout): (RemoteVerb, &str, Duration),
    request_id: &RequestId,
) -> Result<(), Failure<'a>> {
    let call = match call_of(t, verb, sha, timeout) {
        Ok(call) => call,
        Err(reason) => {
            log_failure(
                claimed,
                t,
                verb,
                ErrorCode::DeployTargetUnreachable,
                reason,
                None,
                request_id,
            );
            return Err(Failure {
                code: ErrorCode::DeployTargetUnreachable,
                target: t,
                host_key_mismatch: false,
                may_have_run: false,
            });
        }
    };
    let script_code = match verb {
        RemoteVerb::Health => ErrorCode::HealthCheckFailed,
        RemoteVerb::Deploy | RemoteVerb::Rollback => ErrorCode::DeployScriptFailed,
    };
    let (code, reason, exit, may_have_run) = match transport.call(&call).await {
        Outcome::Ok => return Ok(()),
        Outcome::Exit(code) => (script_code, "exit", code, true),
        Outcome::Timeout => (script_code, "timeout", None, true),
        Outcome::Lost => (
            ErrorCode::DeployTargetUnreachable,
            "connection_lost",
            None,
            true,
        ),
        Outcome::Unreachable(reason) => (ErrorCode::DeployTargetUnreachable, reason, None, false),
        Outcome::NotAllowed(reason) => (ErrorCode::DeployTargetNotAllowed, reason, None, false),
        Outcome::HostKeyMismatch => (ErrorCode::HostKeyMismatch, "host_key_mismatch", None, false),
    };
    log_failure(claimed, t, verb, code, reason, exit, request_id);
    Err(Failure {
        code,
        target: t,
        host_key_mismatch: code == ErrorCode::HostKeyMismatch,
        may_have_run,
    })
}

fn log_failure(
    claimed: &Claimed,
    t: &RunTarget,
    verb: RemoteVerb,
    code: ErrorCode,
    reason: &str,
    exit: Option<i32>,
    request_id: &RequestId,
) {
    let alerting = matches!(
        code,
        ErrorCode::DeployTargetUnreachable
            | ErrorCode::HostKeyMismatch
            | ErrorCode::DeployTargetNotAllowed
    );
    if alerting {
        tracing::error!(
            target: "forge_deploy",
            event = "forge.deploy.target_failed",
            error_code = code.id(),
            request_id = %request_id,
            deployment_id = claimed.id,
            project_id = claimed.project_id,
            environment = claimed.tier.as_str(),
            target = %t.name,
            host = %t.host,
            port = t.port,
            step = verb.as_str(),
            reason,
            pinned_fingerprint = %t.host_key_fingerprint,
            "deployment target failed"
        );
    } else {
        tracing::warn!(
            target: "forge_deploy",
            event = "forge.deploy.target_failed",
            error_code = code.id(),
            request_id = %request_id,
            deployment_id = claimed.id,
            project_id = claimed.project_id,
            environment = claimed.tier.as_str(),
            target = %t.name,
            host = %t.host,
            port = t.port,
            step = verb.as_str(),
            reason,
            exit_code = exit,
            "deployment target failed"
        );
    }
}

/// Run a claimed deployment to its end (module docs). Never panics; every
/// path ends in a [`Finish`].
pub async fn execute(
    transport: &dyn DeployTransport,
    claimed: &Claimed,
    timeouts: Timeouts,
    request_id: &RequestId,
) -> Finish {
    if claimed.targets.is_empty() {
        tracing::error!(
            target: "forge_deploy",
            event = "forge.deploy.target_failed",
            error_code = ErrorCode::DeployTargetUnreachable.id(),
            request_id = %request_id,
            deployment_id = claimed.id,
            project_id = claimed.project_id,
            environment = claimed.tier.as_str(),
            reason = "no_enabled_target",
            "deployment has no enabled target"
        );
        return Finish {
            status: DeploymentStatus::Failed,
            error_code: Some(ErrorCode::DeployTargetUnreachable),
            failed_target: None,
        };
    }
    let mut changed: Vec<&RunTarget> = Vec::new();
    let mut failure = None;
    for t in &claimed.targets {
        let deployed = step(
            transport,
            claimed,
            t,
            (RemoteVerb::Deploy, &claimed.sha, timeouts.script),
            request_id,
        )
        .await;
        match deployed {
            Ok(()) => changed.push(t),
            Err(f) => {
                // The script may have run (an exit, a timeout, a lost
                // session): the target may be half changed and is rolled
                // back too.
                if f.may_have_run {
                    changed.push(t);
                }
                failure = Some(f);
                break;
            }
        }
    }
    if failure.is_none() {
        for t in &claimed.targets {
            if let Err(f) = step(
                transport,
                claimed,
                t,
                (RemoteVerb::Health, &claimed.sha, timeouts.health),
                request_id,
            )
            .await
            {
                failure = Some(f);
                break;
            }
        }
    }
    let Some(f) = failure else {
        return Finish {
            status: DeploymentStatus::Succeeded,
            error_code: None,
            failed_target: None,
        };
    };
    let failed = Finish {
        status: DeploymentStatus::Failed,
        error_code: Some(f.code),
        failed_target: Some(f.target.name.clone()), // clone: the row owns it
    };
    // A host that is not the pinned one is never talked to again, and no
    // other target is touched: a person decides (runbook deploy).
    if f.host_key_mismatch || !claimed.auto_rollback || changed.is_empty() {
        return failed;
    }
    rollback(transport, claimed, &changed, f, timeouts, request_id).await
}

/// Roll `changed` back to the previous version, then check their health.
async fn rollback(
    transport: &dyn DeployTransport,
    claimed: &Claimed,
    changed: &[&RunTarget],
    cause: Failure<'_>,
    timeouts: Timeouts,
    request_id: &RequestId,
) -> Finish {
    let rollback_failed = |target: &RunTarget, reason: &'static str| {
        tracing::error!(
            target: "forge_deploy",
            event = "forge.deploy.rollback_failed",
            error_code = ErrorCode::RollbackFailed.id(),
            request_id = %request_id,
            deployment_id = claimed.id,
            project_id = claimed.project_id,
            environment = claimed.tier.as_str(),
            target = %target.name,
            previous_sha = claimed.previous_sha.as_deref().unwrap_or("none"),
            reason,
            "deployment rollback failed"
        );
        Finish {
            status: DeploymentStatus::RollbackFailed,
            error_code: Some(ErrorCode::RollbackFailed),
            failed_target: Some(target.name.clone()), // clone: the row owns it
        }
    };
    let Some(previous) = claimed.previous_sha.as_deref() else {
        return rollback_failed(cause.target, "no_previous_version");
    };
    for &t in changed {
        for (verb, timeout) in [
            (RemoteVerb::Rollback, timeouts.script),
            (RemoteVerb::Health, timeouts.health),
        ] {
            if let Err(f) = step(transport, claimed, t, (verb, previous, timeout), request_id).await
            {
                let reason = if f.code == ErrorCode::HostKeyMismatch {
                    "host_key_mismatch"
                } else if f.code == ErrorCode::DeployTargetUnreachable {
                    "unreachable"
                } else if f.code == ErrorCode::HealthCheckFailed {
                    "health_failed"
                } else {
                    "script_failed"
                };
                return rollback_failed(t, reason);
            }
        }
    }
    Finish {
        status: DeploymentStatus::RolledBack,
        error_code: Some(cause.code),
        failed_target: Some(cause.target.name.clone()), // clone: the row owns it
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
