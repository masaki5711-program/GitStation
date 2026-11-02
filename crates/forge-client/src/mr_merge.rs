//! `gstn mr merge` and `gstn mr merge-status` (M3 R4, ADR-0024 §4): queue
//! a merge (`POST …/merge-requests/{iid}/merge`, `202` with the attempt)
//! and read the latest attempt (`GET …/merge`). With `--wait`, `mr merge`
//! (and `mr merge-status --wait`) polls the attempt until it is final
//! (`applied`, `superseded`, `failed`) or `--wait-timeout` passes
//! (`FG-CLI-0008`, retryable). `merge: null` or another attempt is not
//! final, and a 5xx / unreachable server is polled through with a capped
//! backoff (review round 1). A superseded or failed attempt exits with
//! its own error code (e.g. `FG-REVIEW-0008`) and the merge's request id.
//! The token needs `merge_execute` (`api` does not merge).

use std::time::{Duration, Instant};

use forge_errors::{ErrorCode, Lang};
use serde_json::{json, Map, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, MergeMethodArg};
use crate::error::{ClientError, Result};
use crate::hosting::{call, client, text};
use crate::http::{ApiClient, Method};

/// How often `--wait` polls.
const POLL: Duration = Duration::from_secs(1);

fn merge_path(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}/merge")
}

/// What `mr merge` asks for.
pub(crate) struct MergeArgs<'a> {
    pub sha: &'a str,
    pub method: Option<MergeMethodArg>,
    pub delete_source_branch: bool,
    pub message: Option<&'a String>,
    /// `Some(timeout)` with `--wait`.
    pub wait: Option<Duration>,
}

/// The POST body: only what was given.
pub(crate) fn merge_body(args: &MergeArgs<'_>) -> Value {
    let mut body = Map::new();
    body.insert("sha".into(), json!(args.sha));
    if let Some(m) = args.method {
        body.insert("method".into(), json!(m.as_str()));
    }
    if args.delete_source_branch {
        body.insert("delete_source_branch".into(), json!(true));
    }
    if let Some(m) = args.message {
        body.insert("message".into(), json!(m));
    }
    Value::Object(body)
}

/// One attempt as a short block of text.
fn attempt_human(a: &Value) -> String {
    let mut out = format!(
        "merge #{} of !{}: {} ({}) into {}\nhead: {}",
        text(a, "merge_id"),
        text(a, "iid"),
        text(a, "state"),
        text(a, "method"),
        text(a, "target_branch"),
        text(a, "head_sha"),
    );
    if a.get("new_sha").is_some_and(|v| !v.is_null()) {
        out.push_str(&format!("\nmerged as: {}", text(a, "new_sha")));
    }
    if a.get("reason").is_some_and(|v| !v.is_null()) {
        out.push_str(&format!(
            "\nreason: {} ({})",
            text(a, "reason"),
            text(a, "error_code")
        ));
    }
    if a.get("source_outcome").is_some_and(|v| !v.is_null()) {
        out.push_str(&format!("\nsource branch: {}", text(a, "source_outcome")));
    }
    out
}

/// A final attempt that did not apply, as the error it ended with.
fn attempt_error(a: &Value) -> ClientError {
    let code_id = a.get("error_code").and_then(Value::as_str).unwrap_or("");
    let code = ErrorCode::parse(code_id);
    let message = code.map_or("", |c| c.user_message(Lang::Ja));
    let status = code.map_or(409, ErrorCode::http_status);
    ClientError::from_server(
        status,
        if code_id.is_empty() {
            "FG-REVIEW-0009"
        } else {
            code_id
        },
        message,
        a.get("request_id").and_then(Value::as_str),
    )
}

fn is_final(state: &str) -> bool {
    matches!(state, "applied" | "superseded" | "failed")
}

/// A failure worth polling through: the server unreachable, or a 5xx /
/// retryable answer (review round 1: a restart of `forge serve` during a
/// merge is no reason to stop waiting).
fn transient(e: &ClientError) -> bool {
    e.retryable() || ErrorCode::parse(e.code_id()).is_some_and(|c| c.http_status() >= 500)
}

/// The backoff after the `n`-th transient failure in a row: 1 s doubling,
/// capped at 8 s.
fn backoff(n: u32) -> Duration {
    POLL.saturating_mul(1 << n.min(3))
}

/// Poll `GET …/merge` until the attempt `merge_id` (or, `None`, the latest)
/// is final or `timeout` passes (`FG-CLI-0008`). `merge: null` and another
/// attempt's answer are not final (review round 1); transient failures are
/// retried with a capped exponential backoff until the deadline.
fn wait_for(c: &ApiClient, path: &str, merge_id: Option<i64>, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let mut failures = 0u32;
    loop {
        let (wait, last_state) = match call(c, Method::Get, path, &[], None) {
            Ok(status) => {
                failures = 0;
                let attempt = status.get("merge").cloned().unwrap_or(Value::Null);
                let same = !attempt.is_null()
                    && merge_id.is_none_or(|id| {
                        attempt.get("merge_id").and_then(Value::as_i64) == Some(id)
                    });
                let state = attempt.get("state").and_then(Value::as_str).unwrap_or("");
                if same && is_final(state) {
                    return Ok(attempt);
                }
                (
                    POLL,
                    if same {
                        state.to_owned()
                    } else {
                        "none".to_owned()
                    },
                )
            }
            Err(e) if transient(&e) && Instant::now() < deadline => {
                failures = failures.saturating_add(1);
                (backoff(failures - 1), "unreachable".to_owned())
            }
            Err(e) => return Err(e),
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ClientError::local(
                ErrorCode::CliWaitTimedOut,
                format!("reason=merge_not_final state={last_state}"),
            ));
        }
        std::thread::sleep(wait.min(left));
    }
}

/// Print a final attempt; one that did not apply is its own error.
fn finish(io: &mut Io<'_>, cli: &Cli, attempt: Value) -> Result<()> {
    let state = attempt.get("state").and_then(Value::as_str).unwrap_or("");
    if attempt.is_null() || state == "applied" || !is_final(state) {
        let h = if attempt.is_null() {
            "(no merge requested)".to_string()
        } else {
            attempt_human(&attempt)
        };
        return emit(io, cli, attempt, &h);
    }
    Err(attempt_error(&attempt))
}

/// `gstn mr merge`.
pub(crate) fn merge(
    cli: &Cli,
    (project, iid): (i64, i64),
    args: &MergeArgs<'_>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let path = merge_path(project, iid);
    let queued = call(&c, Method::Post, &path, &[], Some(&merge_body(args)))?;
    let Some(timeout) = args.wait else {
        let h = attempt_human(&queued);
        return emit(io, cli, queued, &h);
    };
    let id = queued.get("merge_id").and_then(Value::as_i64);
    let done = wait_for(&c, &path, id, timeout)?;
    finish(io, cli, done)
}

/// `gstn mr merge-status`.
pub(crate) fn status(
    cli: &Cli,
    (project, iid): (i64, i64),
    wait: Option<Duration>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let path = merge_path(project, iid);
    let Some(timeout) = wait else {
        let body = call(&c, Method::Get, &path, &[], None)?;
        let attempt = body.get("merge").cloned().unwrap_or(Value::Null);
        let h = if attempt.is_null() {
            "(no merge requested)".to_string()
        } else {
            attempt_human(&attempt)
        };
        return emit(io, cli, body, &h);
    };
    let done = wait_for(&c, &path, None, timeout)?;
    finish(io, cli, done)
}
