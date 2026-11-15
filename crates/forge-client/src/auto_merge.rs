//! `gstn auto-merge …` (M4 C9, ADR-0017 Amendment 2026-11-13): a
//! project's auto-merge (its mode and emergency stops), one merge request's
//! auto-merge line, and the emergency stop of a project (Maintainer) or of
//! the instance (an administrator). One REST call each; the ids are
//! validated by clap and only formatted into the path. Changing the mode is
//! a browser session's (the Owner's), so gstn has no command for it.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::{AutoMergeCommand, Cli};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn project_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/auto-merge")
}

fn stop_line(label: &str, stop: Option<&Value>) -> Option<String> {
    stop.filter(|s| !s.is_null())
        .map(|s| format!("{label} emergency stop since {}", text(s, "at")))
}

/// A project's auto-merge as text.
fn project_text(a: &Value) -> String {
    let mut lines = vec![format!(
        "auto-merge: {}{}",
        text(a, "mode"),
        if a.get("acts").and_then(Value::as_bool) == Some(true) {
            ""
        } else {
            " (nothing merges by itself)"
        }
    )];
    lines.extend(stop_line("instance", a.get("instance_stop")));
    lines.extend(stop_line("project", a.get("project_stop")));
    lines.join("\n")
}

/// A merge request's auto-merge line.
fn mr_text(m: &Value) -> String {
    let Some(s) = m.get("auto_merge").filter(|s| !s.is_null()) else {
        return format!(
            "!{}: no auto-merge state (mode {})",
            text(m, "iid"),
            text(m, "mode")
        );
    };
    let mut line = format!(
        "!{} {} {}",
        text(m, "iid"),
        text(s, "state"),
        text(s, "reason")
    );
    if s.get("class").is_some_and(|v| !v.is_null()) {
        line.push_str(&format!(" class:{}", text(s, "class")));
    }
    if s.get("detail").is_some_and(|v| !v.is_null()) {
        line.push_str(&format!(" ({})", text(s, "detail")));
    }
    if s.get("error_code").is_some_and(|v| !v.is_null()) {
        line.push_str(&format!(" {}", text(s, "error_code")));
    }
    line
}

pub fn auto_merge(
    cli: &Cli,
    cmd: &AutoMergeCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    match cmd {
        AutoMergeCommand::Status { project } => {
            let out = call(&c, Method::Get, &project_path(*project), &[], None)?;
            let human = project_text(&out);
            emit(io, cli, out, &human)
        }
        AutoMergeCommand::Mr { project, iid } => {
            let path = format!("/api/v1/projects/{project}/merge-requests/{iid}/auto-merge");
            let out = call(&c, Method::Get, &path, &[], None)?;
            let human = mr_text(&out);
            emit(io, cli, out, &human)
        }
        AutoMergeCommand::Stop { project } | AutoMergeCommand::Resume { project } => {
            let verb = if matches!(cmd, AutoMergeCommand::Stop { .. }) {
                "stop"
            } else {
                "resume"
            };
            let path = format!("{}/{verb}", project_path(*project));
            let out = call(&c, Method::Post, &path, &[], None)?;
            let human = project_text(&out);
            emit(io, cli, out, &human)
        }
        AutoMergeCommand::InstanceStop | AutoMergeCommand::InstanceResume => {
            let verb = if matches!(cmd, AutoMergeCommand::InstanceStop) {
                "stop"
            } else {
                "resume"
            };
            let path = format!("/api/v1/admin/auto-merge/{verb}");
            let out = call(&c, Method::Post, &path, &[], None)?;
            let human = match out.get("stop").filter(|s| !s.is_null()) {
                Some(s) => format!("instance auto-merge stopped since {}", text(s, "at")),
                None => "instance auto-merge running".to_owned(),
            };
            emit(io, cli, out, &human)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_stopped_merge_request_names_its_class_and_rule() {
        let m = json!({"iid": 4, "mode": "full", "acts": true, "stop": null, "auto_merge": {
            "state": "stopped", "class": "data_loss", "reason": "risk_stop",
            "detail": "sql_drop", "error_code": "FG-REVIEW-0026"}});
        assert_eq!(
            mr_text(&m),
            "!4 stopped risk_stop class:data_loss (sql_drop) FG-REVIEW-0026"
        );
        let none = json!({"iid": 5, "mode": "off", "auto_merge": null});
        assert_eq!(mr_text(&none), "!5: no auto-merge state (mode off)");
    }

    #[test]
    fn a_project_names_its_mode_and_stops() {
        let a = json!({"mode": "full", "acts": true, "project_stop": {"by": 1, "at": "T"},
            "instance_stop": null});
        assert_eq!(
            project_text(&a),
            "auto-merge: full\nproject emergency stop since T"
        );
    }
}
