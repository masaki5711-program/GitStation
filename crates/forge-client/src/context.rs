//! `gstn context` (M3 X5, plan 03 §6): the context API — `work-item`,
//! `requirement`, `merge-request`. Each is one `GET` with `read_api`; ids
//! are formatted into the path as integers (validated by clap), the
//! references are query values (percent-encoded by the URL builder).
//! Server text reaches a terminal sanitized.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, ContextCommand};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn items<'a>(body: &'a Value, key: &str) -> &'a [Value] {
    body.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// `path:line @ commit` of an item's source.
fn place(item: &Value) -> String {
    let s = item.get("source").cloned().unwrap_or(Value::Null);
    let mut out = text(&s, "path");
    if let Some(line) = s.get("line").and_then(Value::as_u64) {
        out.push_str(&format!(":{line}"));
    }
    format!("{out} @ {}", text(&s, "commit"))
}

fn withheld(body: &Value) -> Option<String> {
    let w = body.get("withheld").filter(|w| !w.is_null())?;
    Some(format!(
        "withheld for an AI agent ({}): {} document(s), {} requirement(s), {} work item(s)",
        text(w, "error_code"),
        text(w, "documents"),
        text(w, "requirements"),
        text(w, "work_items")
    ))
}

fn incomplete(body: &Value) -> Option<String> {
    let i = body.get("incomplete").filter(|i| !i.is_null())?;
    Some(format!(
        "incomplete: {} ({})",
        text(i, "reason"),
        text(i, "error_code")
    ))
}

fn work_item_human(body: &Value) -> String {
    let w = body.get("work_item").cloned().unwrap_or(Value::Null);
    let mut rows = vec![format!(
        "{}  kanflow {}  known: {}",
        text(&w, "ref"),
        text(&w, "kanflow_project_id"),
        text(&w, "known")
    )];
    for m in items(body, "merge_requests") {
        rows.push(format!(
            "!{}  {}  {}  {}",
            text(m, "iid"),
            text(m, "state"),
            text(m, "link_source"),
            text(m, "title")
        ));
    }
    if body.get("merge_requests_truncated") == Some(&Value::Bool(true)) {
        rows.push("(more merge requests are linked)".to_owned());
    }
    rows.push(format!("{} test(s)", text(body, "test_count")));
    for t in items(body, "tests") {
        rows.push(format!("{}  {}", text(t, "target"), place(t)));
    }
    rows.extend(incomplete(body));
    rows.join("\n")
}

fn requirement_human(body: &Value) -> String {
    let r = body.get("requirement").cloned().unwrap_or(Value::Null);
    let mut rows = vec![
        format!("{}  {}", text(&r, "id"), place(&r)),
        text(&r, "body"),
        format!("{} test(s)", text(body, "test_count")),
    ];
    for t in items(body, "tests") {
        rows.push(format!("test  {}", place(t)));
    }
    for v in items(body, "versions") {
        rows.push(format!("{}  {}", text(v, "name"), text(v, "status")));
    }
    rows.extend(incomplete(body));
    rows.join("\n")
}

fn merge_request_human(body: &Value) -> String {
    let m = body.get("merge_request").cloned().unwrap_or(Value::Null);
    let mut rows = vec![format!(
        "!{}  {}  {}  {} -> {}",
        text(&m, "iid"),
        text(&m, "state"),
        text(&m, "title"),
        text(&m, "source_branch"),
        text(&m, "target_branch")
    )];
    for f in items(body, "files") {
        rows.push(format!("{}  {}", text(f, "status"), place(f)));
    }
    for c in items(body, "requirements") {
        rows.push(format!(
            "{}  {}  {}",
            text(c, "kind"),
            text(c, "id"),
            place(c)
        ));
    }
    for w in items(body, "work_items") {
        rows.push(format!("{}  {}", text(w, "ref"), text(w, "link_source")));
    }
    for c in items(body, "checks") {
        rows.push(format!(
            "{}  {}  {}",
            text(c, "name"),
            text(c, "status"),
            text(c, "conclusion")
        ));
    }
    rows.extend(withheld(body));
    rows.extend(incomplete(body));
    rows.join("\n")
}

/// One call: its path, its query and how its answer is shown.
type Request = (String, Vec<(String, String)>, fn(&Value) -> String);

/// The request of one command (also the MCP server's).
pub(crate) fn request_of(cmd: &ContextCommand) -> Request {
    match cmd {
        ContextCommand::WorkItem { project, reference } => (
            format!("/api/v1/projects/{project}/context/work-item"),
            vec![("ref".to_owned(), reference.clone())], // clone: the query owns its values
            work_item_human,
        ),
        ContextCommand::Requirement { project, id, rev } => {
            let mut q = vec![("id".to_owned(), id.clone())]; // clone: as above
            if let Some(r) = rev {
                q.push(("ref".to_owned(), r.clone())); // clone: as above
            }
            (
                format!("/api/v1/projects/{project}/context/requirement"),
                q,
                requirement_human,
            )
        }
        ContextCommand::MergeRequest { project, iid } => (
            format!("/api/v1/projects/{project}/merge-requests/{iid}/context"),
            Vec::new(),
            merge_request_human,
        ),
    }
}

/// `gstn context …`.
pub fn context(cli: &Cli, cmd: &ContextCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let (path, query, human) = request_of(cmd);
    let body = call(&c, Method::Get, &path, &query, None)?;
    let h = human(&body);
    emit(io, cli, body, &h)
}
