//! `gstn browse`: read a project's repository through the browse API (M2
//! MR7): `refs`, `resolve`, `tree`, `file`. Each is one `GET` with
//! `read_api`. The project id is formatted into the path as an integer
//! (validated by clap); refs, paths and specs are query values,
//! percent-encoded by the URL builder — never part of the path. History
//! (commits, compare, blame) follows with MR8.

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
    let rev = body.get("ref").cloned().unwrap_or(Value::Null);
    format!(
        "{} {} {}",
        text(&rev, "type"),
        text(&rev, "name"),
        text(&rev, "commit")
    )
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
