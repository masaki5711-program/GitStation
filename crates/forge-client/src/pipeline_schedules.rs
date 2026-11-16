//! `gstn pipeline schedule …` (M4 C12, plan 14 §5 C12): a project's
//! pipeline schedules. `list` for readers; `create`, `update`, `delete` for
//! a Maintainer (people only: an agent's token is refused). Whoever saves a
//! schedule becomes its owner, and it runs with that person's rights at each
//! run (ADR-0029 Amendment 2026-11-13). One REST call each.

use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, PipelineScheduleCommand, ScheduleArgs};
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;

fn schedules_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/pipeline-schedules")
}

fn body_of(s: &ScheduleArgs) -> Value {
    json!({
        "description": s.description,
        "ref": s.git_ref,
        "cron": s.cron,
        "utc_offset_minutes": s.utc_offset,
        "active": !s.inactive,
    })
}

/// One schedule as a line: `#3 Nightly main "0 2 * * *" +540 active next … last refused`.
fn schedule_line(v: &Value) -> String {
    let active = v.get("active").and_then(Value::as_bool) == Some(true);
    let last = v
        .get("last_outcome")
        .and_then(Value::as_str)
        .unwrap_or("never");
    format!(
        "#{} {} {} \"{}\" UTC{:+}min {} next {} last {} (owner user {})",
        text(v, "id"),
        sanitize(&text(v, "description")),
        sanitize(&text(v, "ref")),
        sanitize(&text(v, "cron")),
        v.get("utc_offset_minutes")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        if active { "active" } else { "paused" },
        sanitize(&text(v, "next_run_at")),
        last,
        text(v, "owner_id"),
    )
}

pub fn schedule(
    cli: &Cli,
    cmd: &PipelineScheduleCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        PipelineScheduleCommand::List { project } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &schedules_path(*project), &[], None)?;
            let lines: Vec<String> = out
                .get("schedules")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(schedule_line)
                .collect();
            let human = if lines.is_empty() {
                format!("No pipeline schedule in project {project}.")
            } else {
                lines.join("\n")
            };
            emit(io, cli, out, &human)
        }
        PipelineScheduleCommand::Create { project, settings } => {
            let c = client(cli, env, io)?;
            let body = body_of(settings);
            let out = call(
                &c,
                Method::Post,
                &schedules_path(*project),
                &[],
                Some(&body),
            )?;
            let human = format!("Created {}", schedule_line(&out));
            emit(io, cli, out, &human)
        }
        PipelineScheduleCommand::Update {
            project,
            schedule,
            settings,
        } => {
            let c = client(cli, env, io)?;
            let body = body_of(settings);
            let path = format!("{}/{schedule}", schedules_path(*project));
            let out = call(&c, Method::Put, &path, &[], Some(&body))?;
            let human = format!("Updated {}", schedule_line(&out));
            emit(io, cli, out, &human)
        }
        PipelineScheduleCommand::Delete {
            project,
            schedule,
            yes,
        } => {
            confirm(*yes, "deleting a pipeline schedule")?;
            let c = client(cli, env, io)?;
            let path = format!("{}/{schedule}", schedules_path(*project));
            let out = call(&c, Method::Delete, &path, &[], None)?;
            emit(
                io,
                cli,
                out,
                &format!("Deleted pipeline schedule {schedule} of project {project}"),
            )
        }
    }
}
