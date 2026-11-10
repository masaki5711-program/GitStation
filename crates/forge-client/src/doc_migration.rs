//! `gstn doc migration …` (plan 13 Q13, ADR-0026): the repository setting
//! that lowers broken document references (`ref_broken`) to warnings in
//! the server's `forge/doc-lint` until a date — show, set, clear. One REST
//! call each; the project id is validated by clap and only formatted into
//! the path; the date goes in the JSON body (the server checks it against
//! **its database's date**: after today, at most 180 days ahead —
//! `FG-DOC-0005`). The local `gstn doc lint` cannot read this setting; its
//! `--migration-warnings` is the local counterpart. Tests:
//! `tests/doc_migration.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, DocMigrationCommand};
use crate::error::Result;
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;
use serde_json::{json, Value};

fn migration_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/doc-lint/migration")
}

/// The setting as one line, then the server's date.
fn migration_text(m: &Value) -> String {
    let state = match m.get("warn_until").and_then(Value::as_str) {
        None => "Broken references are errors (no migration).".to_owned(),
        Some(until) if m.get("in_force").and_then(Value::as_bool) == Some(true) => format!(
            "migrating: until {until} — broken references are warnings (set by user {} at {})",
            text(m, "set_by"),
            text(m, "set_at")
        ),
        Some(until) => {
            format!("The migration ended on {until}: broken references are errors again.")
        }
    };
    format!(
        "{state}\nServer date {} (UTC); the latest date allowed is {}.",
        text(m, "today"),
        text(m, "max_until")
    )
}

pub fn migration(
    cli: &Cli,
    cmd: &DocMigrationCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        DocMigrationCommand::Show { project } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &migration_path(*project), &[], None)?;
            let human = migration_text(&out);
            emit(io, cli, out, &human)
        }
        DocMigrationCommand::Set { project, until } => {
            let body = json!({ "warn_until": until });
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Put, &migration_path(*project), &[], Some(&body))?;
            let human = migration_text(&out);
            emit(io, cli, out, &human)
        }
        DocMigrationCommand::Clear { project, yes } => {
            confirm(*yes, "ending a document lint migration")?;
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Delete, &migration_path(*project), &[], None)?;
            emit(
                io,
                cli,
                out,
                &format!(
                    "Ended the migration of project {project}: broken references are errors again"
                ),
            )
        }
    }
}
