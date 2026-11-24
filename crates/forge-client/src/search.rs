//! `gstn search` (plan 20 K3, ADR-0040): one page of the knowledge search
//! over projects and merge requests — one REST call (`GET /api/v1/search`).
//! The server decides what is shown (only what the caller may read, never a
//! total); a page may be short, even empty, while `has_more` is true, and
//! `next_cursor` continues it. Words are sent percent-encoded as `q`; the
//! server checks their length and the other parameters.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

const ROUTE: &str = "/api/v1/search";

/// What one `gstn search` asks for.
pub struct SearchArgs<'a> {
    pub words: &'a [String],
    pub scope: &'a [String],
    pub project: Option<i64>,
    pub limit: Option<u32>,
    pub cursor: Option<&'a str>,
}

pub fn search(cli: &Cli, args: &SearchArgs<'_>, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let mut query = vec![("q".to_owned(), args.words.join(" "))];
    if !args.scope.is_empty() {
        query.push(("scope".to_owned(), args.scope.join(",")));
    }
    if let Some(p) = args.project {
        query.push(("project".to_owned(), p.to_string()));
    }
    if let Some(l) = args.limit {
        query.push(("limit".to_owned(), l.to_string()));
    }
    if let Some(c) = args.cursor {
        query.push(("cursor".to_owned(), c.to_owned()));
    }
    let c = client(cli, env, io)?;
    let out = call(&c, Method::Get, ROUTE, &query, None)?;
    let human = human(&out);
    emit(io, cli, out, &human)
}

/// One block per hit, then the paging note.
fn human(out: &Value) -> String {
    let mut lines: Vec<String> = out
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(hit_lines).collect())
        .unwrap_or_default();
    if lines.is_empty() {
        lines.push("(no hits on this page)".to_owned());
    }
    match out.get("next_cursor").and_then(Value::as_str) {
        Some(next) => lines.push(format!("more: --cursor {}", crate::error::sanitize(next))),
        None if out.get("has_more").and_then(Value::as_bool) == Some(true) => {
            lines.push("more hits may follow".to_owned());
        }
        None => {}
    }
    lines.join("\n")
}

/// `<kind>  <project>[!iid]  <title>` and an indented snippet line.
fn hit_lines(h: &Value) -> String {
    let subject = match h.get("iid") {
        Some(Value::Number(iid)) => format!("{}!{iid}", text(h, "project_path")),
        _ => text(h, "project_path"),
    };
    let snippet = h
        .get("snippet")
        .map_or_else(|| "-".to_owned(), |s| text(s, "text"));
    format!(
        "{}  {subject}  {}\n    {snippet}",
        text(h, "kind"),
        text(h, "title"),
    )
}
