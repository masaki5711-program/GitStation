//! `gstn agent status|recommend|preview|apply` (plan 16 §6 A7, ADR-0032
//! Amendment A7): the agent builder of a project.
//!
//! - `status --project ID`: the newest builds.
//! - `recommend --project ID`: the rules-only recommendation; `--json`
//!   prints it, and its `slug@version` pairs feed `--select`.
//! - `preview --project ID --select slug@version …`: the Dry-Run (files
//!   added / modified / removed, capabilities, the lock). Nothing is
//!   written.
//! - `apply --project ID --select slug@version … [--title T]`: the build as
//!   a merge request (a person's token only; an agent's is refused).
//! - `update --project ID [--adapter SLUG] [--dry-run]` (plan 16 A9): the
//!   update merge request, queued as a job (`--adapter`: another base
//!   model); `--dry-run` shows each locked part's version and the next one
//!   with the Dry-Run and queues nothing.

use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::{ClientError, Result};
use crate::hosting::{call, client};
use crate::http::Method;

/// `slug@version` → the selection item (the server checks again).
fn selection(select: &[String]) -> Result<Value> {
    let mut items = Vec::with_capacity(select.len());
    for s in select {
        let ok_word = |w: &str| {
            (1..=64).contains(&w.len())
                && w.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        };
        match s.split_once('@') {
            Some((slug, version)) if ok_word(slug) && ok_word(version) => {
                items.push(json!({"slug": slug, "version": version}));
            }
            _ => {
                return Err(ClientError::local(
                    forge_errors::ErrorCode::CliInputFailed,
                    "reason=select_not_slug_at_version",
                ))
            }
        }
    }
    Ok(Value::Array(items))
}

fn base(project: i64) -> String {
    format!("/api/v1/projects/{project}/agent-builder")
}

fn changes_text(out: &Value) -> String {
    let preview = out.get("preview").unwrap_or(out);
    let mut lines = Vec::new();
    if let Some(d) = preview.get("lock_digest").and_then(Value::as_str) {
        lines.push(format!("lock {d}"));
    }
    for c in preview
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("-");
        lines.push(format!("{} {}", s("change"), s("path")));
    }
    for p in preview
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("-");
        let caps = p
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        lines.push(format!(
            "part {}@{} [{}] {caps}",
            s("slug"),
            s("version"),
            s("channel")
        ));
    }
    lines.join("\n")
}

/// `gstn agent status`.
pub fn status(cli: &Cli, project: i64, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let out = call(&c, Method::Get, &base(project), &[], None)?;
    let human = out
        .get("builds")
        .and_then(Value::as_array)
        .map(|b| {
            b.iter()
                .map(|r| {
                    let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("-");
                    let mr = r
                        .get("merge_request_iid")
                        .and_then(Value::as_i64)
                        .map_or_else(String::new, |i| format!(" !{i}"));
                    format!(
                        "{} {} {}{mr}",
                        s("created_at"),
                        s("state"),
                        s("lock_digest")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    emit(io, cli, out, &human)
}

/// `gstn agent recommend`.
pub fn recommend(cli: &Cli, project: i64, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let out = call(
        &c,
        Method::Post,
        &format!("{}/recommend", base(project)),
        &[],
        Some(&json!({})),
    )?;
    let human = out
        .get("parts")
        .and_then(Value::as_array)
        .map(|p| {
            p.iter()
                .map(|r| {
                    let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("-");
                    format!("{}@{} ({})", s("slug"), s("version"), s("reason"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    emit(io, cli, out, &human)
}

/// `gstn agent update` (plan 16 A9).
pub fn update(
    cli: &Cli,
    project: i64,
    adapter: Option<&str>,
    dry_run: bool,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    if let Some(a) = adapter {
        let fine = (1..=64).contains(&a.len())
            && a.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !fine || dry_run {
            let reason = if dry_run {
                "reason=adapter_with_dry_run"
            } else {
                "reason=adapter_not_a_slug"
            };
            return Err(ClientError::local(
                forge_errors::ErrorCode::CliInputFailed,
                reason,
            ));
        }
    }
    let c = client(cli, env, io)?;
    let out = if dry_run {
        call(
            &c,
            Method::Get,
            &format!("{}/updates", base(project)),
            &[],
            None,
        )?
    } else {
        let mut body = json!({});
        if let Some(a) = adapter {
            body["adapter"] = Value::from(a);
        }
        call(
            &c,
            Method::Post,
            &format!("{}/update", base(project)),
            &[],
            Some(&body),
        )?
    };
    let mut lines = Vec::new();
    if let Some(reason) = out.get("reason").and_then(Value::as_str) {
        lines.push(format!("no update ({reason})"));
    }
    for p in out
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("-");
        let revoked = if p.get("locked_revoked").and_then(Value::as_bool) == Some(true) {
            " (revoked)"
        } else {
            ""
        };
        lines.push(format!(
            "{} {}{revoked} -> {}",
            s("slug"),
            s("locked_version"),
            p.get("next_version")
                .and_then(Value::as_str)
                .unwrap_or("removed")
        ));
    }
    if out.get("preview").is_some_and(|v| !v.is_null()) {
        lines.push(changes_text(&out));
    }
    match out.get("queued").and_then(Value::as_bool) {
        Some(true) => lines.push("queued: the merge request opens shortly".to_owned()),
        Some(false) => lines.push("already asked: the same update is queued or open".to_owned()),
        None => {}
    }
    emit(io, cli, out, &lines.join("\n"))
}

/// `gstn agent preview` / `gstn agent apply`.
pub fn build(
    cli: &Cli,
    project: i64,
    select: &[String],
    apply: Option<Option<&str>>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let mut body = json!({ "selection": selection(select)? });
    let path = match apply {
        None => format!("{}/preview", base(project)),
        Some(title) => {
            if let Some(t) = title {
                body["title"] = Value::from(t);
            }
            format!("{}/compile", base(project))
        }
    };
    let c = client(cli, env, io)?;
    let out = call(&c, Method::Post, &path, &[], Some(&body))?;
    let mut human = changes_text(&out);
    if let Some(iid) = out
        .get("merge_request")
        .and_then(|m| m.get("iid"))
        .and_then(Value::as_i64)
    {
        human.push_str(&format!("\nmerge request !{iid}"));
    }
    emit(io, cli, out, &human)
}
