//! `gstn mirror …` (M3 O2, ADR-0024 Amendment O2, runbook gitlab-mirror):
//! a project's push mirror to GitLab for administrators — show, set,
//! remove, sync. One REST call each; the project id is validated by clap
//! and only formatted into the path; the URL and the credential name go in
//! the JSON body (the server checks them against `mirror.*`:
//! `FG-INT-0021`). The credential's content never passes through gstn: it
//! is a file on the server. Tests: `tests/mirror.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, MirrorCommand};
use crate::error::Result;
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;
use serde_json::{json, Value};

fn mirror_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/push-mirror")
}

/// The mirror as lines: destination and state, then its progress.
fn mirror_text(m: &Value) -> String {
    let state = if m.get("enabled").and_then(Value::as_bool) == Some(true) {
        "enabled"
    } else {
        "disabled"
    };
    let status = m.get("status").cloned().unwrap_or(Value::Null); // clone: the lines read it
    let waiting = match status.get("pending_since").and_then(Value::as_str) {
        Some(_) => format!("waiting since {}", text(&status, "pending_since")),
        None => "up to date".to_owned(),
    };
    let mut lines = vec![
        format!(
            "{} {} credential {} ({state})",
            text(m, "repo_id"),
            text(m, "url"),
            text(m, "credential_name")
        ),
        format!(
            "{waiting}; last success {}; failures {}",
            text(&status, "last_success_at"),
            text(&status, "failures")
        ),
    ];
    if status
        .get("last_error_code")
        .and_then(Value::as_str)
        .is_some()
    {
        lines.push(format!(
            "last failure {} reason {}",
            text(&status, "last_error_code"),
            text(&status, "last_reason")
        ));
    }
    lines.join("\n")
}

pub fn mirror(cli: &Cli, cmd: &MirrorCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        MirrorCommand::Show { project } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &mirror_path(*project), &[], None)?;
            let human = mirror_text(&out);
            emit(io, cli, out, &human)
        }
        MirrorCommand::Set {
            project,
            url,
            credential,
            disabled,
        } => {
            let body = json!({
                "url": url,
                "credential_name": credential,
                "enabled": !*disabled,
            });
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Put, &mirror_path(*project), &[], Some(&body))?;
            let human = format!("Mirror set\n{}", mirror_text(&out));
            emit(io, cli, out, &human)
        }
        MirrorCommand::Remove { project, yes } => {
            confirm(*yes, "removing a project's push mirror")?;
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Delete, &mirror_path(*project), &[], None)?;
            emit(
                io,
                cli,
                out,
                &format!("Removed the mirror of project {project}"),
            )
        }
        MirrorCommand::Sync { project } => {
            let c = client(cli, env, io)?;
            let path = format!("{}/sync", mirror_path(*project));
            let out = call(&c, Method::Post, &path, &[], None)?;
            let human = if out.get("queued").and_then(Value::as_bool) == Some(true) {
                "Queued a push to the mirror".to_owned()
            } else {
                "A push to the mirror was already queued".to_owned()
            };
            emit(io, cli, out, &human)
        }
    }
}
