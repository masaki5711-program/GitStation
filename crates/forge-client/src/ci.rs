//! `gstn ci lint` / `gstn ci plan` (plan 14 C1, plan 04 P-3): the
//! `.forge-ci.yml` lint and execution plan on the working tree. **Local
//! only**: nothing is sent to the server, and no configuration or token is
//! read. The lint and the plan are forge-ci's, the same functions on the
//! same bytes as the server's `forge/ci-lint` (forge-checks), so the
//! answers agree when the same protected branches are given.
//!
//! The file is the given path, or `.forge-ci.yml` at the root of the
//! repository found upward from the current directory. It is read up to one
//! byte past forge-ci's limit (the lint then says `too_large`).
//!
//! Protected branches: `--protected main,release/*` gives the repository's
//! protected patterns. Without it the check is a local one
//! ([`forge_ci::LintContext::local_check`]): no branch counts as protected,
//! so a job with secrets is `secret_protection_unknown`.
//!
//! Exit codes: 1 with `FG-CI-0001` when the file has a problem or no plan
//! can be made; `FG-CLI-*` when the file cannot be read.

use std::io::Read;
use std::path::{Path, PathBuf};

use forge_ci::{Event, LintContext, Plan, Problem, Trigger, MAX_BYTES};
use forge_errors::ErrorCode;
use serde_json::{json, Value};

use crate::app::{emit, Io};
use crate::cli::{CiCommand, CiEvent, Cli};
use crate::error::{ClientError, Result};

/// The file read when no path is given.
pub const DEFAULT_FILE: &str = ".forge-ci.yml";

/// `gstn ci …` (module docs).
pub fn ci(cli: &Cli, cmd: &CiCommand, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        CiCommand::Lint { path, protected } => {
            let (shown, bytes) = read_definition(path.as_deref())?;
            let ctx = context(protected.as_deref());
            let problems = forge_ci::lint(&bytes, &ctx);
            report_lint(cli, io, &shown, &problems)
        }
        CiCommand::Plan {
            path,
            branch,
            event,
            protected,
        } => {
            let (shown, bytes) = read_definition(path.as_deref())?;
            let ctx = context(protected.as_deref());
            let def = match forge_ci::check(&bytes, &ctx) {
                Ok(def) => def,
                Err(problems) => return report_lint(cli, io, &shown, &problems),
            };
            let trigger = Trigger {
                event: match event {
                    CiEvent::Push => Event::Push,
                    CiEvent::Mr => Event::MergeRequest,
                },
                // clone: the trigger owns its branch name
                branch: branch.clone(),
                protected: ctx.is_protected(branch),
            };
            match forge_ci::plan(&def, &trigger) {
                Ok(plan) => report_plan(cli, io, &shown, &plan),
                Err(problems) => report_lint(cli, io, &shown, &problems),
            }
        }
    }
}

fn context(protected: Option<&[String]>) -> LintContext {
    match protected {
        // clone: the context owns its patterns
        Some(list) => LintContext::with_protected_branches(list.to_vec()),
        None => LintContext::local_check(),
    }
}

fn cli_input(reason: &str) -> ClientError {
    ClientError::local(ErrorCode::CliInputFailed, format!("reason={reason}"))
}

/// The file to read and how to show it: the given path, or
/// `.forge-ci.yml` at the repository root found upward.
fn read_definition(path: Option<&Path>) -> Result<(String, Vec<u8>)> {
    let file: PathBuf = match path {
        Some(p) => p.to_path_buf(),
        None => {
            let cwd = std::env::current_dir().map_err(|_| cli_input("cwd_unreadable"))?;
            crate::doc::find_root(&cwd)?.join(DEFAULT_FILE)
        }
    };
    let shown = match path {
        Some(p) => p.display().to_string(),
        None => DEFAULT_FILE.to_owned(),
    };
    let f = std::fs::File::open(&file).map_err(|e| {
        ClientError::local(
            ErrorCode::CliInputFailed,
            format!("reason=ci_file_unreadable kind={:?}", e.kind()),
        )
    })?;
    if f.metadata().is_ok_and(|m| m.is_dir()) {
        return Err(cli_input("ci_file_is_a_directory"));
    }
    let mut bytes = Vec::new();
    // One byte past the limit: forge-ci answers `too_large` as the server.
    let limit = u64::try_from(MAX_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    f.take(limit).read_to_end(&mut bytes).map_err(|e| {
        ClientError::local(
            ErrorCode::CliInputFailed,
            format!("reason=ci_file_unreadable kind={:?}", e.kind()),
        )
    })?;
    Ok((shown, bytes))
}

fn problem_json(p: &Problem) -> Value {
    json!({"line": p.line, "reason": p.reason, "subject": p.subject})
}

fn problem_text(file: &str, p: &Problem) -> String {
    let subject = p
        .subject
        .as_deref()
        .map(|s| format!(" — {}", crate::error::sanitize(s)))
        .unwrap_or_default();
    format!(
        "{file}:{}: {}{subject} [{}]",
        p.line,
        p.reason,
        ErrorCode::CiConfigInvalid.id()
    )
}

fn report_lint(cli: &Cli, io: &mut Io<'_>, file: &str, problems: &[Problem]) -> Result<()> {
    let mut human: Vec<String> = problems.iter().map(|p| problem_text(file, p)).collect();
    human.push(if problems.is_empty() {
        format!("No problem in {file}.")
    } else {
        format!("{} problem(s).", problems.len())
    });
    let value = json!({
        "path": file,
        "problems": problems.iter().map(problem_json).collect::<Vec<_>>(),
    });
    emit(io, cli, value, &human.join("\n"))?;
    if let Some(first) = problems.first() {
        return Err(ClientError::local(
            ErrorCode::CiConfigInvalid,
            format!(
                "reason={} line={} problems={}",
                first.reason,
                first.line,
                problems.len()
            ),
        ));
    }
    Ok(())
}

fn report_plan(cli: &Cli, io: &mut Io<'_>, file: &str, plan: &Plan) -> Result<()> {
    let jobs: Vec<Value> = plan
        .jobs
        .iter()
        .map(|j| {
            json!({
                "name": j.name,
                "stage": j.stage,
                "wave": j.wave,
                "after": j.after,
                "image": j.image,
                "timeout_secs": j.timeout_secs,
                "tags": j.tags,
                "secrets": j.secrets,
                "script_lines": j.script_lines,
            })
        })
        .collect();
    let mut human = vec![format!(
        "{file}: {} on {}{} — {} job(s)",
        plan.trigger.event,
        crate::error::sanitize(&plan.trigger.branch),
        if plan.trigger.protected {
            " (protected)"
        } else {
            ""
        },
        plan.jobs.len()
    )];
    for j in &plan.jobs {
        let after = if j.after.is_empty() {
            String::new()
        } else {
            format!(" after {}", j.after.join(", "))
        };
        let secrets = if j.secrets.is_empty() {
            String::new()
        } else {
            format!(" secrets {}", j.secrets.join(", "))
        };
        human.push(format!(
            "  wave {} {} [{}] timeout {}s{after}{secrets}",
            j.wave, j.name, j.stage, j.timeout_secs
        ));
    }
    let value = json!({
        "path": file,
        "event": plan.trigger.event.as_str(),
        "branch": plan.trigger.branch,
        "protected": plan.trigger.protected,
        "jobs": jobs,
    });
    emit(io, cli, value, &human.join("\n"))
}
