//! `gstn notifications get|set` (M3 N1): the caller's own notification
//! settings, one REST call each (`set` reads the current settings first and
//! replaces them as a whole, so a flag changes only what it names).
//! `gstn notifications list|read` (M3 U4): the caller's in-app
//! notifications and marking them read.

use serde_json::{json, Value};

use crate::app::{emit, usage, EnvFn, Io};
use crate::cli::{Cli, MailSwitch, NotificationsCommand};
use crate::error::Result;
use crate::hosting::{call, client};
use crate::http::Method;

const ROUTE: &str = "/api/v1/user/notification-preferences";
const LIST: &str = "/api/v1/user/notifications";
const READ_ALL: &str = "/api/v1/user/notifications/read-all";
const COUNT: &str = "/api/v1/user/notifications/unread-count";

pub fn notifications(
    cli: &Cli,
    cmd: &NotificationsCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        NotificationsCommand::Get => {
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Get, ROUTE, &[], None)?;
            let human = summary(&body);
            emit(io, cli, body, &human)
        }
        NotificationsCommand::Set {
            email,
            language,
            mute,
            unmute,
            in_app,
        } => {
            if email.is_none()
                && language.is_none()
                && mute.is_empty()
                && unmute.is_empty()
                && in_app.is_none()
            {
                return Err(usage(
                    "nothing_to_change (pass --email, --language, --mute, --unmute or --in-app)",
                ));
            }
            let c = client(cli, env, io)?;
            let current = call(&c, Method::Get, ROUTE, &[], None)?;
            let mut request = merged(&current, *email, language.as_deref(), mute, unmute);
            if let Some(switch) = in_app {
                request["in_app_enabled"] = json!(*switch == MailSwitch::On);
            }
            let body = call(&c, Method::Put, ROUTE, &[], Some(&request))?;
            let human = summary(&body);
            emit(io, cli, body, &human)
        }
        NotificationsCommand::List {
            unread,
            before,
            limit,
        } => {
            let mut query = Vec::new();
            if *unread {
                query.push(("unread".to_owned(), "true".to_owned()));
            }
            if let Some(b) = before {
                query.push(("before".to_owned(), b.to_string()));
            }
            if let Some(l) = limit {
                query.push(("limit".to_owned(), l.to_string()));
            }
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, LIST, &query, None)?;
            let items = out
                .get("items")
                .and_then(Value::as_array)
                .cloned() // clone: the printer owns its rows
                .unwrap_or_default();
            let mut human = if items.is_empty() {
                "No notification".to_owned()
            } else {
                items.iter().map(item_line).collect::<Vec<_>>().join("\n")
            };
            if let Some(next) = out.get("next_before").and_then(Value::as_i64) {
                human.push_str(&format!("\nOlder: --before {next}"));
            }
            emit(io, cli, out, &human)
        }
        NotificationsCommand::Count => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, COUNT, &[], None)?;
            let count = out.get("count").and_then(Value::as_i64).unwrap_or(0);
            let capped = out.get("capped").and_then(Value::as_bool).unwrap_or(false);
            let human = format!("Unread: {count}{}", if capped { "+" } else { "" });
            emit(io, cli, out, &human)
        }
        NotificationsCommand::Read { id, all } => {
            let c = client(cli, env, io)?;
            match (id, all) {
                (Some(id), false) => {
                    let out = call(&c, Method::Post, &format!("{LIST}/{id}/read"), &[], None)?;
                    let human = format!("Read: {id}");
                    emit(io, cli, out, &human)
                }
                _ => {
                    // 204: no count (it would include items you may no
                    // longer see).
                    call(&c, Method::Post, READ_ALL, &[], Some(&json!({})))?;
                    emit(io, cli, json!({"all_read": true}), "All marked read")
                }
            }
        }
    }
}

/// One line of `notifications list`.
fn item_line(item: &Value) -> String {
    let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("");
    let unread = if item.get("read_at").is_some_and(Value::is_null) {
        "*"
    } else {
        " "
    };
    format!(
        "{unread} {} {} {}!{} {} ({})",
        item.get("id").and_then(Value::as_i64).unwrap_or(0),
        text("kind"),
        text("project_path"),
        item.get("merge_request_iid")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        text("merge_request_title"),
        text("created_at"),
    )
}

/// The settings to send: `current` with the flags applied.
fn merged(
    current: &Value,
    email: Option<MailSwitch>,
    language: Option<&str>,
    mute: &[String],
    unmute: &[String],
) -> Value {
    let email_enabled = email.map_or_else(
        || {
            current
                .get("email_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        },
        |s| s == MailSwitch::On,
    );
    let language = match language {
        Some("default") => Value::Null,
        Some(tag) => json!(tag),
        None => current.get("language").cloned().unwrap_or(Value::Null),
    };
    let mut muted: Vec<String> = current
        .get("muted_kinds")
        .and_then(Value::as_array)
        .map(|kinds| {
            kinds
                .iter()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    for kind in mute {
        if !muted.contains(kind) {
            muted.push(kind.clone()); // clone: the flag list stays borrowed
        }
    }
    muted.retain(|kind| !unmute.contains(kind));
    json!({"email_enabled": email_enabled, "language": language, "muted_kinds": muted})
}

fn summary(body: &Value) -> String {
    let on = body
        .get("email_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let language = body
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let muted: Vec<&str> = body
        .get("muted_kinds")
        .and_then(Value::as_array)
        .map(|kinds| kinds.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let muted = if muted.is_empty() {
        "none".to_owned()
    } else {
        muted.join(", ")
    };
    let in_app = body
        .get("in_app_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    format!(
        "Mail: {}\nLanguage: {language}\nMuted kinds: {muted}\nIn-app: {}",
        if on { "on" } else { "off" },
        if in_app { "on" } else { "off" }
    )
}
