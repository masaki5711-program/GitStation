//! `gstn pipeline …` (M4 C2, plan 14): a project's CI pipelines — list,
//! show (with the jobs), cancel, retry, run (by hand on a branch). One REST
//! call each; the ids are validated by clap and only formatted into the
//! path; the branch goes in the query or the JSON body. An `invalid`
//! pipeline shows its problems (`line reason [subject]`, FG-CI-0001), an
//! `error` one its code (FG-CI-0002, runbook ci-pipelines). Tests:
//! `tests/pipelines.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, PipelineCommand};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;
use serde_json::{json, Value};

fn pipelines_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/pipelines")
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// One pipeline as a line: id, status, source, ref, commit.
fn pipeline_line(p: &Value) -> String {
    let git_ref = text(p, "ref");
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(&git_ref);
    let sha = text(p, "sha");
    let mut line = format!(
        "#{} {} {} {} {}",
        text(p, "id"),
        text(p, "status"),
        text(p, "source"),
        branch,
        short(&sha)
    );
    if p.get("error_code").and_then(Value::as_str).is_some() {
        line.push_str(&format!(" {}", text(p, "error_code")));
    }
    line
}

/// A pipeline with its problems and jobs.
fn pipeline_text(p: &Value) -> String {
    let mut lines = vec![pipeline_line(p)];
    for problem in p
        .get("problems")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let subject = problem
            .get("subject")
            .and_then(Value::as_str)
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        lines.push(format!(
            "  line {} {}{subject}",
            text(problem, "line"),
            text(problem, "reason")
        ));
    }
    for job in p
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "  [{}] {} {} (stage {}, attempt {})",
            text(job, "wave"),
            text(job, "name"),
            text(job, "status"),
            text(job, "stage"),
            text(job, "attempt")
        ));
    }
    lines.join("\n")
}

pub fn pipeline(cli: &Cli, cmd: &PipelineCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        PipelineCommand::List {
            project,
            git_ref,
            before,
            limit,
        } => {
            let mut query = Vec::new();
            if let Some(r) = git_ref {
                query.push(("ref".to_owned(), r.clone())); // clone: the query owns it
            }
            if let Some(b) = before {
                query.push(("before".to_owned(), b.to_string()));
            }
            if let Some(l) = limit {
                query.push(("limit".to_owned(), l.to_string()));
            }
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &pipelines_path(*project), &query, None)?;
            let rows: Vec<String> = out
                .get("pipelines")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(pipeline_line)
                .collect();
            let mut human = if rows.is_empty() {
                "No pipelines".to_owned()
            } else {
                rows.join("\n")
            };
            if let Some(next) = out.get("next_before").and_then(Value::as_i64) {
                human.push_str(&format!("\nmore: --before {next}"));
            }
            emit(io, cli, out, &human)
        }
        PipelineCommand::Show { project, pipeline } => {
            let c = client(cli, env, io)?;
            let path = format!("{}/{pipeline}", pipelines_path(*project));
            let out = call(&c, Method::Get, &path, &[], None)?;
            let human = pipeline_text(&out);
            emit(io, cli, out, &human)
        }
        PipelineCommand::Cancel { project, pipeline } => {
            let c = client(cli, env, io)?;
            let path = format!("{}/{pipeline}/cancel", pipelines_path(*project));
            let out = call(&c, Method::Post, &path, &[], None)?;
            let human = format!("Cancelled\n{}", pipeline_text(&out));
            emit(io, cli, out, &human)
        }
        PipelineCommand::Retry { project, pipeline } => {
            let c = client(cli, env, io)?;
            let path = format!("{}/{pipeline}/retry", pipelines_path(*project));
            let out = call(&c, Method::Post, &path, &[], None)?;
            let human = format!("Retried\n{}", pipeline_text(&out));
            emit(io, cli, out, &human)
        }
        PipelineCommand::Run { project, git_ref } => {
            let c = client(cli, env, io)?;
            let body = json!({ "ref": git_ref });
            let out = call(
                &c,
                Method::Post,
                &pipelines_path(*project),
                &[],
                Some(&body),
            )?;
            let human = format!("Started\n{}", pipeline_text(&out));
            emit(io, cli, out, &human)
        }
    }
}
