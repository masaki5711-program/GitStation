//! `gstn checks report|list` (M3 C1, ADR-0024 §10, docs/api/checks.md): a
//! service account reports a check run on a commit (the same name again
//! updates it), anyone who reads the project lists them. One REST call
//! each; ids and the commit id are validated by clap and only formatted
//! into the path (a commit id as 40 / 64 hex digits); free text and the
//! annotations go in the JSON body. Tests: `tests/checks.rs`.

use std::path::Path;

use crate::app::{emit, usage, EnvFn, Io};
use crate::cli::{ChecksCommand, Cli};
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, text};
use crate::http::Method;
use serde_json::{json, Map, Value};

/// The most a file given to `--summary-file` / `--annotations-file` may be
/// (the server's whole body cap).
const MAX_FILE_BYTES: u64 = 64 * 1024;

fn checks_path(project: i64, sha: &str) -> String {
    format!("/api/v1/projects/{project}/commits/{sha}/checks")
}

/// A status as the server names it.
pub fn parse_status(raw: &str) -> std::result::Result<String, String> {
    match raw {
        "queued" | "in_progress" | "completed" => Ok(raw.to_owned()),
        _ => Err("a status is queued, in_progress or completed".to_owned()),
    }
}

/// A conclusion as the server names it.
pub fn parse_conclusion(raw: &str) -> std::result::Result<String, String> {
    match raw {
        "success" | "neutral" | "failure" | "error" | "action_required" => Ok(raw.to_owned()),
        _ => Err("a conclusion is success, neutral, failure, error or action_required".to_owned()),
    }
}

/// A small file read whole (`usage` when it is missing, too large or not
/// UTF-8: nothing is sent).
fn read_file(path: &Path, what: &str) -> Result<String> {
    let meta =
        std::fs::metadata(path).map_err(|_| usage(format!("{what}_unreadable (no such file)")))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(usage(format!("{what}_too_large (at most 64 KiB)")));
    }
    let bytes = std::fs::read(path).map_err(|_| usage(format!("{what}_unreadable")))?;
    String::from_utf8(bytes).map_err(|_| usage(format!("{what}_not_utf8")))
}

/// One run as a line: `ci/gates completed success (by ci-runner)`; a
/// run without a reporter (`null`: forge's own, M3 X2) is `(by forge)`.
fn run_line(run: &Value) -> String {
    let conclusion = run.get("conclusion").and_then(Value::as_str).unwrap_or("-");
    let by = match run.get("reported_by") {
        Some(Value::Null) => "forge",
        reporter => reporter
            .and_then(|r| r.get("username"))
            .and_then(Value::as_str)
            .unwrap_or("-"),
    };
    format!(
        "{} {} {} (by {})",
        sanitize(&text(run, "name")),
        text(run, "status"),
        conclusion,
        sanitize(by)
    )
}

pub fn checks(cli: &Cli, cmd: &ChecksCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        ChecksCommand::Report {
            project,
            sha,
            name,
            status,
            conclusion,
            summary,
            summary_file,
            details_url,
            annotations_file,
        } => {
            let mut body = Map::new();
            body.insert("name".to_owned(), json!(name));
            body.insert("status".to_owned(), json!(status));
            if let Some(c) = conclusion {
                body.insert("conclusion".to_owned(), json!(c));
            }
            let summary = match (summary, summary_file) {
                (Some(s), None) => Some(s.clone()), // clone: the body owns it
                (None, Some(f)) => Some(read_file(f, "summary_file")?),
                (None, None) => None,
                (Some(_), Some(_)) => {
                    return Err(usage("summary_twice (pass --summary or --summary-file)"))
                }
            };
            if let Some(s) = summary {
                body.insert("summary".to_owned(), json!(s));
            }
            if let Some(u) = details_url {
                body.insert("details_url".to_owned(), json!(u));
            }
            if let Some(f) = annotations_file {
                let raw = read_file(f, "annotations_file")?;
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|_| usage("annotations_file_invalid (a JSON array)"))?;
                if !parsed.is_array() {
                    return Err(usage("annotations_file_invalid (a JSON array)"));
                }
                body.insert("annotations".to_owned(), parsed);
            }
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &checks_path(*project, sha),
                &[],
                Some(&Value::Object(body)),
            )?;
            let human = format!(
                "Reported {} on {}",
                run_line(&out),
                &sha[..12.min(sha.len())]
            );
            emit(io, cli, out, &human)
        }
        ChecksCommand::List { project, sha } => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, &checks_path(*project, sha), &[], None)?;
            let runs = out
                .get("check_runs")
                .and_then(Value::as_array)
                .cloned() // clone: the printer owns its rows
                .unwrap_or_default();
            let human = if runs.is_empty() {
                format!("No check reported on {}", &sha[..12.min(sha.len())])
            } else {
                runs.iter().map(run_line).collect::<Vec<_>>().join("\n")
            };
            emit(io, cli, out, &human)
        }
    }
}
