//! `gstn mr merge` and `gstn mr merge-status` (M3 R4, ADR-0024 §4): queue
//! a merge (`POST …/merge-requests/{iid}/merge`, `202` with the attempt)
//! and read the latest attempt (`GET …/merge`). With `--wait`, `mr merge`
//! (and `mr merge-status --wait`) polls the attempt until it is final
//! (`applied`, `superseded`, `failed`) or `--wait-timeout` passes
//! (`FG-CLI-0008`, retryable): a superseded or failed attempt exits with
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

/// Poll `GET …/merge` until the attempt `merge_id` (or, `None`, the latest)
/// is final or `timeout` passes.
fn wait_for(c: &ApiClient, path: &str, merge_id: Option<i64>, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = call(c, Method::Get, path, &[], None)?;
        let attempt = status.get("merge").cloned().unwrap_or(Value::Null);
        let same =
            merge_id.is_none_or(|id| attempt.get("merge_id").and_then(Value::as_i64) == Some(id));
        let state = attempt.get("state").and_then(Value::as_str).unwrap_or("");
        if attempt.is_null() || (same && is_final(state)) {
            return Ok(attempt);
        }
        if Instant::now() >= deadline {
            return Err(ClientError::local(
                ErrorCode::CliWaitTimedOut,
                format!("reason=merge_not_final state={state}"),
            ));
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
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
