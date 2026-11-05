//! `gstn webhook …` (M3 I2, ADR-0016 §8, docs/api/webhooks.md): a project's
//! webhooks for its Maintainers — list, view, create (the secret is shown
//! once), update (`--enable` clears the failure count), delete, rotate the
//! secret, the delivery history, redeliver one. One REST call each; ids are
//! validated by clap and only formatted into the path; the URL and event
//! types go in the JSON body (the server checks the URL against the
//! operator's allowlist: `FG-INT-0010`). Tests: `tests/webhooks.rs`.

use crate::app::{emit, usage, EnvFn, Io};
use crate::cli::{Cli, WebhookCommand};
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;
use serde_json::{json, Map, Value};

fn hooks_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/webhooks")
}

fn hook_path(project: i64, id: i64) -> String {
    format!("{}/{id}", hooks_path(project))
}

/// One webhook as a line: `3 enabled https://… [merge_request.*] failures 0`.
fn hook_line(h: &Value) -> String {
    let state = if h.get("enabled").and_then(Value::as_bool) == Some(true) {
        "enabled".to_owned()
    } else {
        format!("disabled ({})", text(h, "disabled_reason"))
    };
    let types = h
        .get("event_types")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(Value::as_str)
                .map(sanitize)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    format!(
        "{} {} {} [{}] failures {}",
        text(h, "id"),
        state,
        text(h, "url"),
        types,
        text(h, "consecutive_failures")
    )
}

/// One delivery as a line: `12 succeeded check.completed attempts 1 status 204`.
fn delivery_line(d: &Value) -> String {
    format!(
        "{} {} {} attempts {} status {} reason {} ({})",
        text(d, "id"),
        text(d, "state"),
        text(d, "event_type"),
        text(d, "attempts"),
        text(d, "last_status"),
        text(d, "last_reason"),
        text(d, "delivery_id")
    )
}

fn event_types(events: &[String]) -> Option<Value> {
    (!events.is_empty()).then(|| json!(events))
}

pub fn webhook(cli: &Cli, cmd: &WebhookCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        WebhookCommand::List { project } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &hooks_path(*project), &[], None)?;
            let items = out
                .get("items")
                .and_then(Value::as_array)
                .cloned() // clone: the printer owns its rows
                .unwrap_or_default();
            let human = if items.is_empty() {
                format!("No webhook on project {project}")
            } else {
                items.iter().map(hook_line).collect::<Vec<_>>().join("\n")
            };
            emit(io, cli, out, &human)
        }
        WebhookCommand::View { id, project } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &hook_path(*project, *id), &[], None)?;
            let human = hook_line(&out);
            emit(io, cli, out, &human)
        }
        WebhookCommand::Create {
            project,
            url,
            events,
        } => {
            let mut body = Map::new();
            body.insert("url".to_owned(), json!(url));
            if let Some(t) = event_types(events) {
                body.insert("event_types".to_owned(), t);
            }
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &hooks_path(*project),
                &[],
                Some(&Value::Object(body)),
            )?;
            let hook = out.get("webhook").cloned().unwrap_or(Value::Null); // clone: the line reads it
            let human = format!(
                "Created webhook {}\nSecret (shown once; give it to the receiver): {}",
                hook_line(&hook),
                text(&out, "secret")
            );
            emit(io, cli, out, &human)
        }
        WebhookCommand::Update {
            id,
            project,
            url,
            events,
            enable,
            disable,
        } => {
            let mut body = Map::new();
            if let Some(u) = url {
                body.insert("url".to_owned(), json!(u));
            }
            if let Some(t) = event_types(events) {
                body.insert("event_types".to_owned(), t);
            }
            if *enable {
                body.insert("enabled".to_owned(), json!(true));
            }
            if *disable {
                body.insert("enabled".to_owned(), json!(false));
            }
            if body.is_empty() {
                return Err(usage(
                    "nothing_to_change (pass --url, --event, --enable or --disable)",
                ));
            }
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Patch,
                &hook_path(*project, *id),
                &[],
                Some(&Value::Object(body)),
            )?;
            let hook = out.get("webhook").cloned().unwrap_or(Value::Null); // clone: the line reads it
            let human = format!("{}: {}", text(&out, "outcome"), hook_line(&hook));
            emit(io, cli, out, &human)
        }
        WebhookCommand::Delete { id, project, yes } => {
            confirm(*yes, "deleting a webhook and its delivery history")?;
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Delete, &hook_path(*project, *id), &[], None)?;
            emit(io, cli, out, &format!("Deleted webhook {id}"))
        }
        WebhookCommand::RotateSecret { id, project } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &format!("{}/secret", hook_path(*project, *id)),
                &[],
                None,
            )?;
            let human = format!(
                "New secret (version {}, shown once): {}\nThe previous secret still signs until {}",
                text(&out, "secret_version"),
                text(&out, "secret"),
                text(&out, "previous_secret_valid_until")
            );
            emit(io, cli, out, &human)
        }
        WebhookCommand::Deliveries {
            id,
            project,
            before,
            limit,
        } => {
            let mut query = Vec::new();
            if let Some(b) = before {
                query.push(("before".to_owned(), b.to_string()));
            }
            if let Some(l) = limit {
                query.push(("limit".to_owned(), l.to_string()));
            }
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Get,
                &format!("{}/deliveries", hook_path(*project, *id)),
                &query,
                None,
            )?;
            let items = out
                .get("items")
                .and_then(Value::as_array)
                .cloned() // clone: the printer owns its rows
                .unwrap_or_default();
            let mut human = if items.is_empty() {
                "No delivery".to_owned()
            } else {
                items
                    .iter()
                    .map(delivery_line)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            if let Some(next) = out.get("next_before").and_then(Value::as_i64) {
                human.push_str(&format!("\nOlder: --before {next}"));
            }
            emit(io, cli, out, &human)
        }
        WebhookCommand::Redeliver {
            id,
            delivery,
            project,
        } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &format!(
                    "{}/deliveries/{delivery}/redeliver",
                    hook_path(*project, *id)
                ),
                &[],
                None,
            )?;
            emit(io, cli, out, &format!("Queued delivery {delivery} again"))
        }
    }
}
