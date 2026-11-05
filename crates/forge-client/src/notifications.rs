//! `gstn notifications get|set` (M3 N1): the caller's own notification
//! settings, one REST call each (`set` reads the current settings first and
//! replaces them as a whole, so a flag changes only what it names).

use serde_json::{json, Value};

use crate::app::{emit, usage, EnvFn, Io};
use crate::cli::{Cli, MailSwitch, NotificationsCommand};
use crate::error::Result;
use crate::hosting::{call, client};
use crate::http::Method;

const ROUTE: &str = "/api/v1/user/notification-preferences";

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
        } => {
            if email.is_none() && language.is_none() && mute.is_empty() && unmute.is_empty() {
                return Err(usage(
                    "nothing_to_change (pass --email, --language, --mute or --unmute)",
                ));
            }
            let c = client(cli, env, io)?;
            let current = call(&c, Method::Get, ROUTE, &[], None)?;
            let request = merged(&current, *email, language.as_deref(), mute, unmute);
            let body = call(&c, Method::Put, ROUTE, &[], Some(&request))?;
            let human = summary(&body);
            emit(io, cli, body, &human)
        }
    }
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
    format!(
        "Mail: {}\nLanguage: {language}\nMuted kinds: {muted}",
        if on { "on" } else { "off" }
    )
}
