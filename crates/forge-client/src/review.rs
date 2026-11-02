//! The merge request commands (`gstn mr …`, M3 R1a, ADR-0024): one REST API
//! v1 call each (a list: one page, or every page with `--all`, bounded by
//! `PageLimits`). Ids and iids are validated by clap and only ever
//! formatted into the path as integers; branch names, titles and
//! descriptions go in the JSON body. Tests: `tests/merge_requests.rs`.

use crate::app::{emit, usage, EnvFn, Io};
use crate::cli::{Cli, MrCommand};
use crate::error::Result;
use crate::hosting::{call, client, list_human, text};
use crate::http::{Method, PageLimits, MR_PAGES};
use serde_json::{json, Map, Value};

fn collection(project: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests")
}

fn one(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}")
}

/// `--all` without `--limit` asks for the largest page.
const ALL_PAGE_SIZE: u32 = 100;

const ROW_KEYS: [&str; 6] = [
    "iid",
    "state",
    "draft",
    "source_branch",
    "target_branch",
    "title",
];

/// A merge request as a short block of text.
fn human(body: &Value, verb: &str) -> String {
    format!(
        "{verb} !{} ({}{}): {} -> {}\ntitle: {}",
        text(body, "iid"),
        text(body, "state"),
        if body.get("draft") == Some(&Value::Bool(true)) {
            ", draft"
        } else {
            ""
        },
        text(body, "source_branch"),
        text(body, "target_branch"),
        text(body, "title"),
    )
}

/// The PATCH body of `mr edit`: only what was given; at least one field.
fn edit_body(
    title: Option<&String>,
    description: Option<&String>,
    target: Option<&String>,
    draft: Option<bool>,
) -> Result<Value> {
    let mut m = Map::new();
    if let Some(t) = title {
        m.insert("title".into(), json!(t));
    }
    if let Some(d) = description {
        m.insert("description".into(), json!(d));
    }
    if let Some(t) = target {
        m.insert("target_branch".into(), json!(t));
    }
    if let Some(d) = draft {
        m.insert("draft".into(), json!(d));
    }
    if m.is_empty() {
        return Err(usage(
            "nothing_to_change (pass --title, --description, --target, --draft or --ready)",
        ));
    }
    Ok(Value::Object(m))
}

pub fn mr(cli: &Cli, cmd: &MrCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        MrCommand::Create {
            project,
            source,
            target,
            title,
            description,
            draft,
        } => {
            let mut req = json!({
                "title": title,
                "source_branch": source,
                "target_branch": target,
                "draft": draft,
            });
            if let (Some(d), Some(m)) = (description, req.as_object_mut()) {
                m.insert("description".into(), json!(d));
            }
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Post, &collection(*project), &[], Some(&req))?;
            let h = human(&body, "Opened");
            emit(io, cli, body, &h)
        }
        MrCommand::List {
            project,
            state,
            limit,
            before,
            all,
        } => {
            let c = client(cli, env, io)?;
            let path = collection(*project);
            let mut query: Vec<(String, String)> = Vec::new();
            // The state is a fixed word from clap, never free text.
            if let Some(s) = state {
                query.push(("state".to_string(), s.as_str().to_string()));
            }
            match (limit, all) {
                (Some(n), _) => query.push(("limit".to_string(), n.to_string())),
                (None, true) => query.push(("limit".to_string(), ALL_PAGE_SIZE.to_string())),
                (None, false) => {}
            }
            if let Some(n) = before {
                query.push(("before".to_string(), n.to_string()));
            }
            let body = if *all {
                c.get_all_keyset(&path, &query, PageLimits::default(), &MR_PAGES)?
                    .body
            } else {
                call(&c, Method::Get, &path, &query, None)?
            };
            let h = list_human(
                &body,
                "merge_requests",
                "next_before",
                "--before",
                &ROW_KEYS,
            );
            emit(io, cli, body, &h)
        }
        MrCommand::View { project, iid } => {
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Get, &one(*project, *iid), &[], None)?;
            let h = format!(
                "{}\nauthor_id: {}\ncreated_at: {}\n\n{}",
                human(&body, "Merge request"),
                text(&body, "author_id"),
                text(&body, "created_at"),
                text(&body, "description"),
            );
            emit(io, cli, body, &h)
        }
        MrCommand::Edit {
            project,
            iid,
            title,
            description,
            target,
            draft,
            ready,
        } => {
            let flag = match (*draft, *ready) {
                (true, _) => Some(true),
                (false, true) => Some(false),
                (false, false) => None,
            };
            let req = edit_body(title.as_ref(), description.as_ref(), target.as_ref(), flag)?;
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Patch, &one(*project, *iid), &[], Some(&req))?;
            let h = human(&body, "Updated");
            emit(io, cli, body, &h)
        }
        MrCommand::Close { project, iid } => {
            let c = client(cli, env, io)?;
            let path = format!("{}/close", one(*project, *iid));
            let body = call(&c, Method::Post, &path, &[], None)?;
            let h = human(&body, "Closed");
            emit(io, cli, body, &h)
        }
        MrCommand::Reopen { project, iid } => {
            let c = client(cli, env, io)?;
            let path = format!("{}/reopen", one(*project, *iid));
            let body = call(&c, Method::Post, &path, &[], None)?;
            let h = human(&body, "Reopened");
            emit(io, cli, body, &h)
        }
    }
}
