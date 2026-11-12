//! `gstn ci variable …` (M4 C6, plan 14 §4.2, ADR-0029 decision 8): a
//! project's CI variables — the secrets its jobs name in `secrets:` — for
//! its Maintainers (people only: an agent's token is refused). List, set,
//! delete; one REST call each. The value of `set` is read from standard
//! input (never an argument: argv is visible to every user of the host and
//! kept in shell history), one line, a trailing newline dropped; a
//! terminal on standard input is refused. No command ever prints a value:
//! the server never answers one. Tests: `tests/ci_variables.rs`.

use std::io::Read;

use forge_errors::ErrorCode;
use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{CiVariableCommand, Cli};
use crate::error::{sanitize, ClientError, Result};
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;

/// The most bytes read from standard input (the server takes 4096).
const MAX_VALUE: u64 = 4097;

fn variables_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/ci/variables")
}

fn variable_path(project: i64, name: &str) -> String {
    format!("{}/{name}", variables_path(project))
}

/// A variable's name as the server takes it, `[A-Z][A-Z0-9_]*` (at most 63
/// bytes): it is formatted into the path, so nothing else gets there.
fn checked(name: &str) -> Result<&str> {
    let mut chars = name.chars();
    let ok = name.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(name)
    } else {
        Err(ClientError::local(
            ErrorCode::CliUsageInvalid,
            "reason=name_invalid (a CI variable's name is [A-Z][A-Z0-9_]*)",
        ))
    }
}

/// One variable as a line: `DEPLOY_TOKEN masked protected 24 bytes (user 3, …)`.
fn variable_line(v: &Value) -> String {
    let flag = |key: &str, yes: &str, no: &str| {
        if v.get(key).and_then(Value::as_bool) == Some(true) {
            yes.to_owned()
        } else {
            no.to_owned()
        }
    };
    format!(
        "{} {} {} {} bytes (set by user {} at {})",
        sanitize(&text(v, "name")),
        flag("masked", "masked", "unmasked"),
        flag("protected_only", "protected", "any-branch"),
        text(v, "value_bytes"),
        text(v, "updated_by"),
        text(v, "updated_at")
    )
}

/// The value from standard input: one line, the trailing newline dropped.
fn read_value(io: &mut Io<'_>) -> Result<String> {
    if io.stdin_tty {
        return Err(ClientError::local(
            ErrorCode::CliUsageInvalid,
            "reason=stdin_is_a_terminal (pipe the value: gstn ci variable set --project <id> NAME < value.txt)",
        ));
    }
    let mut raw = String::new();
    io.stdin
        .take(MAX_VALUE)
        .read_to_string(&mut raw)
        .map_err(|_| ClientError::local(ErrorCode::CliUsageInvalid, "reason=stdin_unreadable"))?;
    let value = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(&raw)
        .to_owned();
    if value.is_empty() {
        return Err(ClientError::local(
            ErrorCode::CliUsageInvalid,
            "reason=value_empty (pipe the value on standard input)",
        ));
    }
    Ok(value)
}

pub fn variable(cli: &Cli, cmd: &CiVariableCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        CiVariableCommand::List { project } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &variables_path(*project), &[], None)?;
            let lines: Vec<String> = out
                .get("variables")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(variable_line)
                .collect();
            let human = if lines.is_empty() {
                format!("No CI variable in project {project}.")
            } else {
                lines.join("\n")
            };
            emit(io, cli, out, &human)
        }
        CiVariableCommand::Set {
            project,
            name,
            unmasked,
        } => {
            let path = variable_path(*project, checked(name)?);
            let value = zeroize::Zeroizing::new(read_value(io)?);
            let body = json!({ "value": value.as_str(), "masked": !unmasked });
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Put, &path, &[], Some(&body))?;
            let human = format!("Set {}", variable_line(&out));
            emit(io, cli, out, &human)
        }
        CiVariableCommand::Delete { project, name, yes } => {
            let path = variable_path(*project, checked(name)?);
            confirm(*yes, "deleting a CI variable")?;
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Delete, &path, &[], None)?;
            emit(
                io,
                cli,
                out,
                &format!(
                    "Deleted the CI variable {} of project {project}",
                    sanitize(name)
                ),
            )
        }
    }
}
