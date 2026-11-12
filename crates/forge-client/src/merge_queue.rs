//! `gstn queue …` (M4 C8, ADR-0030): the merge queue — add a merge request
//! ("merge when ready"), take it out, its place, a target's queue, and the
//! emergency stop (pause / resume). One REST call each; the ids are
//! validated by clap and only formatted into the path; the branch goes in
//! the query or the JSON body. Adding needs `merge_execute` (as a merge),
//! pausing and resuming `api` (Developer / Maintainer). Tests:
//! `tests/merge_queue.rs`.

use serde_json::{json, Map, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, QueueCommand};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn entry_path(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}/merge-queue")
}

fn queue_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/merge-queue")
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// One entry as a line: place, !iid, state, method, why alone / why out.
fn entry_line(e: &Value) -> String {
    let mut line = match e.get("position").and_then(Value::as_i64) {
        Some(p) => format!("{p}. !{} {}", text(e, "iid"), text(e, "state")),
        None => format!("!{} {}", text(e, "iid"), text(e, "state")),
    };
    line.push_str(&format!(
        " {} {}",
        text(e, "method"),
        short(&text(e, "head_sha"))
    ));
    if e.get("isolate").is_some_and(|v| !v.is_null()) {
        line.push_str(&format!(" alone:{}", text(e, "isolate")));
    }
    if e.get("reason").is_some_and(|v| !v.is_null()) {
        line.push_str(&format!(" reason:{}", text(e, "reason")));
        if e.get("error_code").is_some_and(|v| !v.is_null()) {
            line.push_str(&format!(" {}", text(e, "error_code")));
        }
    }
    line
}

/// A queue as text: its state, the batch being tested, the entries.
fn queue_text(q: &Value) -> String {
    let mut lines = vec![format!(
        "queue {}: {} (next batch up to {})",
        text(q, "target_branch"),
        if q.get("paused").and_then(Value::as_bool) == Some(true) {
            "paused"
        } else {
            "running"
        },
        text(q, "batch_limit")
    )];
    if let Some(b) = q.get("batch").filter(|b| !b.is_null()) {
        lines.push(format!(
            "batch #{} {}: {} entries, train {} on {} (deadline {})",
            text(b, "batch_id"),
            text(b, "state"),
            text(b, "size"),
            short(&text(b, "train_sha")),
            short(&text(b, "base_sha")),
            text(b, "deadline_at")
        ));
    }
    let entries: Vec<String> = q
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(entry_line)
        .collect();
    if entries.is_empty() {
        lines.push("No merge request is queued".to_owned());
    } else {
        lines.extend(entries);
    }
    let recent: Vec<String> = q
        .get("recent")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(entry_line)
        .collect();
    if !recent.is_empty() {
        lines.push("recent:".to_owned());
        lines.extend(recent.into_iter().map(|l| format!("  {l}")));
    }
    lines.join("\n")
}

pub fn queue(cli: &Cli, cmd: &QueueCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        QueueCommand::Add {
            project,
            iid,
            sha,
            method,
        } => {
            let mut body = Map::new();
            body.insert("sha".into(), json!(sha));
            if let Some(m) = method {
                body.insert("method".into(), json!(m.as_str()));
            }
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &entry_path(*project, *iid),
                &[],
                Some(&Value::Object(body)),
            )?;
            let human = format!("Queued\n{}", entry_line(&out));
            emit(io, cli, out, &human)
        }
        QueueCommand::Remove { project, iid } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Delete, &entry_path(*project, *iid), &[], None)?;
            let human = format!("Removed from the queue\n{}", entry_line(&out));
            emit(io, cli, out, &human)
        }
        QueueCommand::Status { project, iid } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &entry_path(*project, *iid), &[], None)?;
            let human = match out.get("entry").filter(|e| !e.is_null()) {
                Some(e) => entry_line(e),
                None => format!("!{iid} was never queued"),
            };
            emit(io, cli, out, &human)
        }
        QueueCommand::List { project, target } => {
            let c = client(cli, env, io)?;
            let query = [("target".to_owned(), target.clone())]; // clone: the query owns it
            let out = call(&c, Method::Get, &queue_path(*project), &query, None)?;
            let human = queue_text(&out);
            emit(io, cli, out, &human)
        }
        QueueCommand::Pause { project, target } | QueueCommand::Resume { project, target } => {
            let verb = if matches!(cmd, QueueCommand::Pause { .. }) {
                "pause"
            } else {
                "resume"
            };
            let c = client(cli, env, io)?;
            let path = format!("{}/{verb}", queue_path(*project));
            let out = call(
                &c,
                Method::Post,
                &path,
                &[],
                Some(&json!({ "target": target })),
            )?;
            let human = queue_text(&out);
            emit(io, cli, out, &human)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_line_names_its_place_and_why() {
        let e = json!({
            "position": 2, "iid": 7, "state": "queued", "method": "squash",
            "head_sha": "0123456789abcdef0123", "isolate": "migration",
            "reason": null, "error_code": null,
        });
        assert_eq!(
            entry_line(&e),
            "2. !7 queued squash 0123456789ab alone:migration"
        );
        let removed = json!({
            "position": null, "iid": 3, "state": "removed", "method": "merge_commit",
            "head_sha": "ab", "isolate": null, "reason": "checks_failed",
            "error_code": "FG-CI-0023",
        });
        assert_eq!(
            entry_line(&removed),
            "!3 removed merge_commit ab reason:checks_failed FG-CI-0023"
        );
    }

    #[test]
    fn a_queue_names_its_state_and_batch() {
        let q = json!({
            "target_branch": "main", "paused": true, "batch_limit": 2,
            "batch": {"batch_id": 5, "state": "testing", "size": 2,
                "train_sha": "t".repeat(40), "base_sha": "b".repeat(40),
                "deadline_at": "2026-11-12T12:00:00Z"},
            "entries": [], "recent": [],
        });
        let text = queue_text(&q);
        assert!(
            text.starts_with("queue main: paused (next batch up to 2)"),
            "{text}"
        );
        assert!(text.contains("batch #5 testing: 2 entries"), "{text}");
        assert!(text.contains("No merge request is queued"), "{text}");
    }
}
