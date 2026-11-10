//! `gstn doc show` / `gstn doc list` (plan 13 stage 3): a project's
//! documents through the document API, read with your permissions. One
//! `GET` each with `read_api`. The project id is formatted into the path
//! as an integer and the document id only after clap checked it is a
//! document id (ASCII letters, digits, `.`, `_`, `-`, starting with a
//! letter or digit, at most 128 bytes); the revision, the language and the
//! list's filters are query values. Server text reaches a terminal:
//! sanitized. A reference more confidential than the document is shown as
//! its id only (the server never sends its title, plan 13 Q14). Tests:
//! `tests/doc_view.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::browse::terminal_text;
use crate::cli::Cli;
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;
use serde_json::Value;

fn documents_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/documents")
}

/// A document id as the server accepts it in the path.
pub fn parse_doc_id(raw: &str) -> std::result::Result<String, String> {
    let ok = !raw.is_empty()
        && raw.len() <= 128
        && raw
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(raw.to_string())
    } else {
        Err("a document id is ASCII letters, digits, '.', '_' or '-' (e.g. RA-2026-014)".into())
    }
}

/// How one reference reads: `ID title`, `ID (more confidential)`,
/// `ID (broken)` or `repo:ID (another repository)`.
fn reference_text(r: &Value) -> String {
    let mut id = text(r, "doc");
    if let Some(b) = r.get("block").and_then(Value::as_str) {
        id = format!("{id}#{}", crate::error::sanitize(b));
    }
    match r.get("target").and_then(Value::as_str) {
        Some("found") => format!("{id} {}", text(r, "title")),
        Some("hidden") => format!("{id} (more confidential: id only)"),
        Some("external") => format!("{}:{id} (another repository)", text(r, "repo")),
        _ => format!("{id} (broken)"),
    }
}

/// The document as text: the summary, each section with its text and
/// blocks, then the references.
pub(crate) fn document_text(d: &Value) -> String {
    let mut out = vec![
        format!("{}  {}", text(d, "id"), text(d, "title")),
        format!(
            "type {}  owner {}  classification {}  {}",
            text(d, "doctype"),
            text(d, "owner"),
            text(d, "classification"),
            text(d, "path")
        ),
    ];
    let empty = Vec::new();
    for s in d
        .get("sections")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
    {
        out.push(String::new());
        if let Some(h) = s.get("heading").and_then(Value::as_str) {
            out.push(format!("## {}", terminal_text(h)));
        }
        for p in s.get("pieces").and_then(Value::as_array).unwrap_or(&empty) {
            if let Some(t) = p.get("display").and_then(Value::as_str) {
                out.push(terminal_text(t));
                continue;
            }
            let Some(b) = p.get("block") else { continue };
            let attrs: Vec<String> = b
                .get("attrs")
                .and_then(Value::as_array)
                .unwrap_or(&empty)
                .iter()
                .map(|a| format!("{}={}", text(a, "key"), text(a, "value")))
                .collect();
            let mut head = format!("[{} {}]", text(b, "kind"), text(b, "id"));
            if !attrs.is_empty() {
                head.push(' ');
                head.push_str(&attrs.join(" "));
            }
            if b.get("deprecated").and_then(Value::as_bool) == Some(true) {
                head.push_str(" (deprecated)");
            }
            out.push(head);
            if let Some(body) = b.get("display").and_then(Value::as_str) {
                out.push(terminal_text(body));
            }
        }
    }
    let refs = d.get("refs").and_then(Value::as_array).unwrap_or(&empty);
    if !refs.is_empty() {
        out.push(String::new());
        out.push("References:".into());
        for r in refs {
            out.push(format!("  line {}  {}", text(r, "line"), reference_text(r)));
        }
    }
    if let Some(inc) = d.get("incomplete").filter(|v| !v.is_null()) {
        out.push(format!(
            "(incomplete: {} {} — references may be missing)",
            text(inc, "error_code"),
            text(inc, "reason")
        ));
    }
    out.join("\n")
}

/// The list as one line per document.
pub(crate) fn list_text(l: &Value) -> String {
    let empty = Vec::new();
    let docs = l
        .get("documents")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut out: Vec<String> = docs
        .iter()
        .map(|d| {
            format!(
                "{}  {}  {}  {}  {}",
                text(d, "id"),
                text(d, "classification"),
                text(d, "doctype"),
                text(d, "path"),
                text(d, "title")
            )
        })
        .collect();
    if docs.is_empty() {
        out.push("No documents.".into());
    }
    if let Some(next) = l.get("next_after").and_then(Value::as_str) {
        out.push(format!("(more: --after {})", crate::error::sanitize(next)));
    }
    if let Some(w) = l.get("withheld").filter(|v| !v.is_null()) {
        out.push(format!(
            "({} confidential documents not given to an AI agent: {})",
            text(w, "documents"),
            text(w, "error_code")
        ));
    }
    if let Some(inc) = l.get("incomplete").filter(|v| !v.is_null()) {
        out.push(format!(
            "(incomplete: {} {})",
            text(inc, "error_code"),
            text(inc, "reason")
        ));
    }
    out.join("\n")
}

/// `gstn doc show`.
pub fn show(
    cli: &Cli,
    project: i64,
    id: &str,
    rev: Option<&str>,
    lang: Option<&str>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let mut q: Vec<(String, String)> = Vec::new();
    if let Some(r) = rev {
        q.push(("ref".into(), r.to_owned()));
    }
    if let Some(l) = lang {
        q.push(("lang".into(), l.to_owned()));
    }
    let c = client(cli, env, io)?;
    let body = call(
        &c,
        Method::Get,
        &format!("{}/{id}", documents_path(project)),
        &q,
        None,
    )?;
    let human = document_text(&body);
    emit(io, cli, body, &human)
}

/// The filters of `gstn doc list`.
pub struct ListArgs<'a> {
    pub rev: Option<&'a str>,
    pub doctype: Option<&'a str>,
    pub limit: Option<u32>,
    pub after: Option<&'a str>,
}

/// `gstn doc list`.
pub fn list(
    cli: &Cli,
    project: i64,
    args: &ListArgs<'_>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let mut q: Vec<(String, String)> = Vec::new();
    if let Some(r) = args.rev {
        q.push(("ref".into(), r.to_owned()));
    }
    if let Some(t) = args.doctype {
        q.push(("doctype".into(), t.to_owned()));
    }
    if let Some(n) = args.limit {
        q.push(("limit".into(), n.to_string()));
    }
    if let Some(a) = args.after {
        q.push(("after".into(), a.to_owned()));
    }
    let c = client(cli, env, io)?;
    let body = call(&c, Method::Get, &documents_path(project), &q, None)?;
    let human = list_text(&body);
    emit(io, cli, body, &human)
}
