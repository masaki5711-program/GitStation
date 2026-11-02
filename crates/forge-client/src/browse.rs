//! `gstn browse`: read a project's repository through the browse API (M2
//! MR7): `refs`, `resolve`, `tree`, `file`. Each is one `GET` with
//! `read_api`. The project id is formatted into the path as an integer
//! (validated by clap); refs, paths and specs are query values,
//! percent-encoded by the URL builder — never part of the path. History
//! (M2 MR8): `commits`, `commit`, `compare`, `blame`. The one value that is
//! part of a path, a commit id, is validated to be 40 / 64 hex digits by
//! clap before any request.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{BrowseCommand, Cli};
use crate::error::Result;
use crate::hosting::{call, client, list_human, text};
use crate::http::Method;
use serde_json::Value;

/// File text for a terminal: kept whole with its newlines and tabs, every
/// other control character (escape sequences, `\r`) and the bidi
/// overrides replaced by `?` (it is untrusted, like all server text).
pub(crate) fn terminal_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            let bidi =
                ('\u{202a}'..='\u{202e}').contains(&c) || ('\u{2066}'..='\u{2069}').contains(&c);
            if (c.is_control() && c != '\n' && c != '\t') || bidi {
                '?'
            } else {
                c
            }
        })
        .collect()
}

fn repository(project: i64, what: &str) -> String {
    format!("/api/v1/projects/{project}/repository/{what}")
}

/// `<type> <name> <commit>` of a resolved revision.
fn revision(body: &Value) -> String {
    revision_of(body, "ref")
}

fn revision_of(body: &Value, key: &str) -> String {
    let rev = body.get(key).cloned().unwrap_or(Value::Null);
    format!(
        "{} {} {}",
        text(&rev, "type"),
        text(&rev, "name"),
        text(&rev, "commit")
    )
}

/// One changed file as `status  path` (a rename shows both paths), plus
/// its patch when asked. Server text reaches a terminal: sanitized.
fn diff_human(diff: &Value, patch: bool) -> String {
    let files = diff
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for f in &files {
        let new_path = text(f, "new_path");
        let name = match f.get("status").and_then(Value::as_str) {
            Some("renamed" | "copied") => format!("{} -> {new_path}", text(f, "old_path")),
            Some("deleted") => text(f, "old_path"),
            _ => new_path,
        };
        let mut line = format!("{}  {name}", text(f, "status"));
        if f.get("binary").and_then(Value::as_bool) == Some(true) {
            line.push_str("  (binary)");
        }
        if let Some(why) = f.get("omitted").and_then(Value::as_str) {
            line.push_str(&format!(
                "  (patch left out: {})",
                crate::error::sanitize(why)
            ));
        }
        out.push(line);
        if patch {
            if let Some(p) = f.get("patch").and_then(Value::as_str) {
                if !p.is_empty() {
                    out.push(terminal_text(p).trim_end().to_string());
                }
            }
        }
    }
    if out.is_empty() {
        out.push("(no changed files)".to_string());
    }
    if diff.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push("(truncated: the server lists at most 500 files, 2 MiB of patches)".to_string());
    }
    out.join("\n")
}

fn commit_line(c: &Value) -> String {
    format!(
        "{}  {}  {}  {}",
        text(c, "oid"),
        text(c, "author_name"),
        text(c, "authored_at"),
        text(c, "subject")
    )
}

/// `line  commit  author  text`, one row per line of the file.
fn blame_human(body: &Value) -> String {
    if body.get("binary").and_then(Value::as_bool) == Some(true) {
        return format!(
            "(binary file, {} bytes; nothing to blame)",
            text(body, "size")
        );
    }
    let authors: std::collections::HashMap<&str, &str> = body
        .get("commits")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some((
                        c.get("oid")?.as_str()?,
                        c.get("author_name")?.as_str().unwrap_or(""),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut rows: Vec<String> = Vec::new();
    for h in body
        .get("hunks")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let oid = h.get("commit").and_then(Value::as_str).unwrap_or("");
        let short: String = oid.chars().take(8).collect();
        let author = crate::error::sanitize(authors.get(oid).copied().unwrap_or("-"));
        let start = h.get("start").and_then(Value::as_u64).unwrap_or(0);
        let lines = h
            .get("lines")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (number, line) in (start..).zip(lines) {
            rows.push(format!(
                "{number:>5}  {short}  {author}  {}",
                terminal_text(line.as_str().unwrap_or(""))
            ));
        }
    }
    if rows.is_empty() {
        rows.push("(empty file)".to_string());
    }
    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
        rows.push("(truncated: the server blames at most 10000 lines)".to_string());
    }
    rows.join("\n")
}

fn truncated_note(body: &Value, what: &str) -> &'static str {
    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
        match what {
            "refs" => "\n(truncated: the server lists at most 5000 refs)",
            "tree" => "\n(truncated: the server lists at most 5000 entries)",
            _ => "\n(truncated: the server returns at most 1 MiB)",
        }
    } else {
        ""
    }
}

pub fn browse(cli: &Cli, cmd: &BrowseCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        BrowseCommand::Refs { project, kind } => {
            let c = client(cli, env, io)?;
            let q: Vec<(String, String)> = kind
                .map(|k| vec![("type".to_string(), k.as_str().to_string())])
                .unwrap_or_default();
            let body = call(&c, Method::Get, &repository(*project, "refs"), &q, None)?;
            let mut human = format!(
                "default branch: {}\n{}",
                text(&body, "default_branch"),
                list_human(&body, "refs", "", "", &["type", "name", "commit"])
            );
            human.push_str(truncated_note(&body, "refs"));
            emit(io, cli, body, &human)
        }
        BrowseCommand::Resolve { project, spec } => {
            let c = client(cli, env, io)?;
            let q = [("spec".to_string(), spec.clone())];
            let body = call(&c, Method::Get, &repository(*project, "resolve"), &q, None)?;
            let human = format!("{}\npath: {}", revision(&body), text(&body, "path"));
            emit(io, cli, body, &human)
        }
        BrowseCommand::Tree { project, rev, path } => {
            let c = client(cli, env, io)?;
            let mut q = vec![("ref".to_string(), rev.clone())];
            if let Some(path) = path {
                q.push(("path".to_string(), path.clone()));
            }
            let body = call(&c, Method::Get, &repository(*project, "tree"), &q, None)?;
            let mut human = format!(
                "{}\n{}",
                revision(&body),
                list_human(&body, "entries", "", "", &["type", "size", "path"])
            );
            human.push_str(truncated_note(&body, "tree"));
            emit(io, cli, body, &human)
        }
        BrowseCommand::Commits {
            project,
            rev,
            path,
            limit,
            cursor,
        } => {
            let c = client(cli, env, io)?;
            let mut q = vec![("ref".to_string(), rev.clone())];
            if let Some(path) = path {
                q.push(("path".to_string(), path.clone()));
            }
            if let Some(limit) = limit {
                q.push(("limit".to_string(), limit.to_string()));
            }
            if let Some(cursor) = cursor {
                q.push(("cursor".to_string(), cursor.clone()));
            }
            let body = call(&c, Method::Get, &repository(*project, "commits"), &q, None)?;
            let lines: Vec<String> = body
                .get("commits")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(commit_line).collect())
                .unwrap_or_default();
            let mut human = if lines.is_empty() {
                "(no commits)".to_string()
            } else {
                lines.join("\n")
            };
            if let Some(next) = body.get("next_cursor").and_then(Value::as_str) {
                human.push_str(&format!(
                    "\nmore: --cursor {}",
                    crate::error::sanitize(next)
                ));
            }
            emit(io, cli, body, &human)
        }
        BrowseCommand::Commit {
            project,
            oid,
            patch,
        } => {
            let c = client(cli, env, io)?;
            let path = format!("{}/{oid}", repository(*project, "commits"));
            let body = call(&c, Method::Get, &path, &[], None)?;
            let commit = body.get("commit").cloned().unwrap_or(Value::Null);
            let message = body.get("message").and_then(Value::as_str).unwrap_or("");
            let human = format!(
                "commit {}\nauthor: {} <{}> {}\nparents: {}\n\n{}\n\n{}",
                text(&commit, "oid"),
                text(&commit, "author_name"),
                text(&commit, "author_email"),
                text(&commit, "authored_at"),
                commit
                    .get("parents")
                    .and_then(Value::as_array)
                    .map(|p| p
                        .iter()
                        .filter_map(Value::as_str)
                        .map(crate::error::sanitize)
                        .collect::<Vec<_>>()
                        .join(" "))
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "-".to_string()),
                terminal_text(message.trim_end()),
                diff_human(body.get("diff").unwrap_or(&Value::Null), *patch)
            );
            emit(io, cli, body, &human)
        }
        BrowseCommand::Compare {
            project,
            from,
            to,
            patch,
        } => {
            let c = client(cli, env, io)?;
            let q = [
                ("from".to_string(), from.clone()),
                ("to".to_string(), to.clone()),
            ];
            let body = call(&c, Method::Get, &repository(*project, "compare"), &q, None)?;
            let lines: Vec<String> = body
                .get("commits")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(commit_line).collect())
                .unwrap_or_default();
            let mut human = format!(
                "{}\n{}\ncommits ({}):\n{}",
                revision_of(&body, "from"),
                revision_of(&body, "to"),
                lines.len(),
                if lines.is_empty() {
                    "(none)".to_string()
                } else {
                    lines.join("\n")
                }
            );
            if body.get("commits_truncated").and_then(Value::as_bool) == Some(true) {
                human.push_str("\n(truncated: the server lists at most 200 commits)");
            }
            human.push_str("\n\n");
            human.push_str(&diff_human(
                body.get("diff").unwrap_or(&Value::Null),
                *patch,
            ));
            emit(io, cli, body, &human)
        }
        BrowseCommand::Blame { project, rev, path } => {
            let c = client(cli, env, io)?;
            let q = [
                ("ref".to_string(), rev.clone()),
                ("path".to_string(), path.clone()),
            ];
            let body = call(&c, Method::Get, &repository(*project, "blame"), &q, None)?;
            let human = blame_human(&body);
            emit(io, cli, body, &human)
        }
        BrowseCommand::File { project, rev, path } => {
            let c = client(cli, env, io)?;
            let q = [
                ("ref".to_string(), rev.clone()),
                ("path".to_string(), path.clone()),
            ];
            let body = call(&c, Method::Get, &repository(*project, "blob"), &q, None)?;
            let mut human = match body.get("content").and_then(Value::as_str) {
                Some(content) => terminal_text(content),
                None => format!(
                    "(binary file, {} bytes; download it with the raw URL)",
                    text(&body, "size")
                ),
            };
            human.push_str(truncated_note(&body, "file"));
            emit(io, cli, body, &human)
        }
    }
}
