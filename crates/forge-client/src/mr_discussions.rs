//! The discussion commands (`gstn mr discussions`, `mr comment`,
//! `mr resolve`; M3 R3, ADR-0024 §6, §12): one REST API v1 call each. Ids
//! are validated by clap and formatted into the path as integers; the body
//! and the path of a position go in the JSON body. Note bodies are server
//! text: printed line by line, each line sanitized (no terminal escapes),
//! at most [`MAX_NOTE_LINES`] lines per note. Tests:
//! `tests/merge_requests.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, DiffSide};
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, text};
use crate::http::Method;
use serde_json::{json, Value};

/// The most lines of one note printed (the JSON output has all of it).
pub const MAX_NOTE_LINES: usize = 20;

fn discussions_path(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}/discussions")
}

/// `#<id> <kind> [<path>:<line> (<side>, v<n>)] [resolved by …] [outdated]`
/// then each note, indented.
fn discussion_human(d: &Value) -> String {
    let mut head = format!("#{} {}", text(d, "id"), text(d, "kind"));
    if let Some(p) = d.get("position").filter(|p| !p.is_null()) {
        head.push_str(&format!(
            " {}:{} ({}, v{})",
            text(p, "path"),
            text(p, "line"),
            text(p, "side"),
            text(p, "version")
        ));
    }
    if d.get("resolved") == Some(&Value::Bool(true)) {
        head.push_str(&format!(" [resolved by {}]", text(d, "resolved_by")));
    }
    if d.get("outdated") == Some(&Value::Bool(true)) {
        head.push_str(" [outdated]");
    }
    let mut out = vec![head];
    for note in d
        .get("notes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        out.push(format!(
            "  {} at {}:",
            text(note, "author_id"),
            text(note, "created_at")
        ));
        let body = note.get("body").and_then(Value::as_str).unwrap_or("");
        let lines: Vec<&str> = body.lines().collect();
        for line in lines.iter().take(MAX_NOTE_LINES) {
            out.push(format!("    {}", sanitize(line)));
        }
        if lines.len() > MAX_NOTE_LINES {
            out.push(format!(
                "    (… {} more lines; --json shows them)",
                lines.len() - MAX_NOTE_LINES
            ));
        }
    }
    out.join("\n")
}

/// `gstn mr discussions`.
pub fn list(cli: &Cli, (project, iid): (i64, i64), env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(&c, Method::Get, &discussions_path(project, iid), &[], None)?;
    let items: Vec<String> = body
        .get("discussions")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(discussion_human).collect())
        .unwrap_or_default();
    let mut h = if items.is_empty() {
        "(no discussions)".to_string()
    } else {
        items.join("\n\n")
    };
    if body.get("truncated") == Some(&Value::Bool(true)) {
        h.push_str("\n(truncated: the server lists at most 500 discussions and 5000 notes)");
    }
    emit(io, cli, body, &h)
}

/// What `gstn mr comment` posts.
pub struct Comment<'a> {
    pub body: &'a str,
    pub discussion: Option<i64>,
    /// `(version, path, line, side)` of an inline discussion.
    pub position: Option<(i64, &'a str, i64, Option<DiffSide>)>,
}

/// `gstn mr comment`: a new discussion, or a reply with `--discussion`.
pub fn comment(
    cli: &Cli,
    (project, iid): (i64, i64),
    what: &Comment<'_>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let base = discussions_path(project, iid);
    let (path, req) = match what.discussion {
        Some(d) => (format!("{base}/{d}/notes"), json!({"body": what.body})),
        None => {
            let mut req = json!({"body": what.body});
            if let (Some((version, file, line, side)), Some(m)) =
                (what.position, req.as_object_mut())
            {
                m.insert(
                    "position".into(),
                    json!({
                        "version": version,
                        "path": file,
                        "line": line,
                        "side": side.unwrap_or(DiffSide::New).as_str(),
                    }),
                );
            }
            (base, req)
        }
    };
    let c = client(cli, env, io)?;
    let body = call(&c, Method::Post, &path, &[], Some(&req))?;
    let h = discussion_human(&body);
    emit(io, cli, body, &h)
}

/// `gstn mr resolve` (`--unresolve` opens it again).
pub fn resolve(
    cli: &Cli,
    (project, iid): (i64, i64),
    discussion: i64,
    unresolve: bool,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let verb = if unresolve { "unresolve" } else { "resolve" };
    let path = format!("{}/{discussion}/{verb}", discussions_path(project, iid));
    let c = client(cli, env, io)?;
    let body = call(&c, Method::Post, &path, &[], None)?;
    let h = discussion_human(&body);
    emit(io, cli, body, &h)
}
