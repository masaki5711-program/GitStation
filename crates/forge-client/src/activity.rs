//! `gstn activity` (plan 15 MR 8): one page of the recent events of the
//! projects the caller is involved in, newest first — one REST call
//! (`GET /api/v1/user/activity`). The server decides what is shown (only
//! events the caller may read, with only the allowed fields); a page may
//! hold fewer than `--limit`, and `next_before` pages back. A person's
//! route (`people_only`): an agent's token is refused.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

const ROUTE: &str = "/api/v1/user/activity";

pub fn activity(
    cli: &Cli,
    before: Option<i64>,
    limit: Option<u32>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let mut query = Vec::new();
    if let Some(b) = before {
        query.push(("before".to_owned(), b.to_string()));
    }
    if let Some(l) = limit {
        query.push(("limit".to_owned(), l.to_string()));
    }
    let c = client(cli, env, io)?;
    let out = call(&c, Method::Get, ROUTE, &query, None)?;
    let human = human(&out);
    emit(io, cli, out, &human)
}

/// One line per event, then the paging and truncation notes.
fn human(out: &Value) -> String {
    let mut lines: Vec<String> = out
        .get("events")
        .and_then(Value::as_array)
        .map(|events| events.iter().map(event_line).collect())
        .unwrap_or_default();
    if lines.is_empty() {
        lines.push("(none)".to_owned());
    }
    if let Some(next) = out.get("next_before").and_then(Value::as_i64) {
        lines.push(format!("more: --before {next}"));
    }
    if out.get("truncated").and_then(Value::as_bool) == Some(true) {
        lines.push("truncated: the events of your oldest projects were not read".to_owned());
    }
    lines.join("\n")
}

/// `<seq>  <occurred_at>  <type>  <project>[!iid]  <actor>`.
fn event_line(e: &Value) -> String {
    let subject = match e.get("merge_request").and_then(|m| m.get("iid")) {
        Some(Value::Number(iid)) => format!("{}!{iid}", text(e, "project_path")),
        _ => text(e, "project_path"),
    };
    let actor = e
        .get("actor")
        .filter(|a| !a.is_null())
        .map_or_else(|| "-".to_owned(), |a| text(a, "username"));
    format!(
        "{}  {}  {}  {subject}  {actor}",
        text(e, "seq"),
        text(e, "occurred_at"),
        text(e, "type"),
    )
}
