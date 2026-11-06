//! `gstn spec` (M3 X4, plan 03 §7.2): a project's specification through
//! the spec API — `requirements`, `versions`, `diff`, `trace`. Each is one
//! `GET` with `read_api` on `/api/v1/projects/{id}/specs/…`; the project id
//! is formatted into the path as an integer (validated by clap), revisions
//! and the search are query values (percent-encoded by the URL builder).
//! Server text reaches a terminal sanitized.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, SpecCommand};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn specs(project: i64, what: &str) -> String {
    format!("/api/v1/projects/{project}/specs/{what}")
}

fn items<'a>(body: &'a Value, key: &str) -> &'a [Value] {
    body.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// `<type> <name> <commit>` of a resolved revision.
fn revision(body: &Value, key: &str) -> String {
    let rev = body.get(key).cloned().unwrap_or(Value::Null);
    format!(
        "{} {} {}",
        text(&rev, "type"),
        text(&rev, "name"),
        text(&rev, "commit")
    )
}

/// The bound an index hit, when it hit one.
fn incomplete(body: &Value) -> Option<String> {
    let i = body.get("incomplete").filter(|i| !i.is_null())?;
    Some(format!(
        "incomplete: {} ({})",
        text(i, "reason"),
        text(i, "error_code")
    ))
}

fn requirements_human(body: &Value) -> String {
    let mut rows = vec![
        revision(body, "ref"),
        format!(
            ".forge/repo.yml: {}; directories: {}",
            text(body, "config"),
            items(body, "dirs")
                .iter()
                .filter_map(Value::as_str)
                .map(crate::error::sanitize)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    for r in items(body, "requirements") {
        let mut row = format!("{}  {}:{}", text(r, "id"), text(r, "path"), text(r, "line"));
        if let Some(p) = r.get("priority").and_then(Value::as_str) {
            row.push_str(&format!("  {}", crate::error::sanitize(p)));
        }
        if r.get("deprecated") == Some(&Value::Bool(true)) {
            row.push_str("  deprecated");
        }
        rows.push(row);
    }
    if body.get("truncated") == Some(&Value::Bool(true)) {
        rows.push("(more requirements match: raise --limit or narrow --search)".to_owned());
    }
    for p in items(body, "problems") {
        let mut row = format!(
            "{} {}:{} {}",
            text(p, "error_code"),
            text(p, "path"),
            text(p, "line"),
            text(p, "rule")
        );
        if let Some(d) = p.get("detail").and_then(Value::as_str) {
            row.push_str(&format!(" — {}", crate::error::sanitize(d)));
        }
        rows.push(row);
    }
    rows.extend(incomplete(body));
    rows.join("\n")
}

fn versions_human(body: &Value) -> String {
    let mut rows: Vec<String> = items(body, "versions")
        .iter()
        .map(|v| format!("{}  {}", text(v, "name"), text(v, "commit")))
        .collect();
    if rows.is_empty() {
        rows.push("no version tags".to_owned());
    }
    if body.get("truncated") == Some(&Value::Bool(true)) {
        rows.push("(more versions exist)".to_owned());
    }
    rows.join("\n")
}

fn diff_human(body: &Value) -> String {
    let mut rows = vec![format!(
        "from {}\nto   {}",
        revision(body, "from"),
        revision(body, "to")
    )];
    let changes = items(body, "changes");
    if changes.is_empty() {
        rows.push("no requirement changed".to_owned());
    }
    for c in changes {
        rows.push(format!(
            "{}  {}  {}:{}",
            text(c, "kind"),
            text(c, "id"),
            text(c, "path"),
            text(c, "line")
        ));
    }
    rows.extend(incomplete(body));
    rows.join("\n")
}

fn trace_human(body: &Value) -> String {
    let mut rows = vec![revision(body, "ref")];
    for r in items(body, "requirements") {
        rows.push(format!(
            "{}  {}  {} {}:{}  {} test(s)",
            text(r, "status"),
            text(r, "id"),
            text(r, "source"),
            text(r, "path"),
            text(r, "line"),
            text(r, "test_count")
        ));
    }
    for e in items(body, "references") {
        let mut row = format!(
            "reference line {}  {}  {}",
            text(e, "line"),
            text(e, "repo"),
            text(e, "status")
        );
        if let Some(code) = e.get("error_code").and_then(Value::as_str) {
            row.push_str(&format!(" ({})", crate::error::sanitize(code)));
        }
        rows.push(row);
    }
    rows.extend(incomplete(body));
    rows.join("\n")
}

/// One call: its path, its query and how its answer is shown.
type Request = (String, Vec<(String, String)>, fn(&Value) -> String);

/// `gstn spec …`.
pub fn spec(cli: &Cli, cmd: &SpecCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let (path, query, human): Request = match cmd {
        SpecCommand::Requirements {
            project,
            rev,
            search,
            limit,
        } => {
            let mut q = Vec::new();
            if let Some(r) = rev {
                q.push(("ref".to_owned(), r.clone())); // clone: the query owns its values
            }
            if let Some(s) = search {
                q.push(("q".to_owned(), s.clone())); // clone: as above
            }
            if let Some(n) = limit {
                q.push(("limit".to_owned(), n.to_string()));
            }
            (specs(*project, "requirements"), q, requirements_human)
        }
        SpecCommand::Versions { project } => {
            (specs(*project, "versions"), Vec::new(), versions_human)
        }
        SpecCommand::Diff { project, from, to } => (
            specs(*project, "diff"),
            vec![
                ("from".to_owned(), from.clone()), // clone: as above
                ("to".to_owned(), to.clone()),     // clone: as above
            ],
            diff_human,
        ),
        SpecCommand::Trace { project, rev } => (
            specs(*project, "trace"),
            rev.iter().map(|r| ("ref".to_owned(), r.clone())).collect(), // clone: as above
            trace_human,
        ),
    };
    let body = call(&c, Method::Get, &path, &query, None)?;
    let h = human(&body);
    emit(io, cli, body, &h)
}
