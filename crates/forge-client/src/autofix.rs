//! `gstn autofix …` (M4 C14, ADR-0017 Amendment C14): a project's
//! deterministic autofix switch (off by default), turning it on or off
//! (Maintainer, people only), and one merge request's autofix line. One REST
//! call each; the ids are validated by clap and only formatted into the
//! path.

use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{AutofixCommand, Cli};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn project_path(project: i64) -> String {
    format!("/api/v1/projects/{project}/autofix")
}

/// A project's switch as text.
fn project_text(a: &Value) -> String {
    if a.get("enabled").and_then(Value::as_bool) == Some(true) {
        "autofix: on".to_owned()
    } else {
        "autofix: off".to_owned()
    }
}

/// A merge request's autofix line.
fn mr_text(m: &Value) -> String {
    let Some(a) = m.get("attempt").filter(|a| !a.is_null()) else {
        return format!("!{}: no autofix attempt", text(m, "iid"));
    };
    let mut line = format!(
        "!{} {} {}",
        text(m, "iid"),
        text(a, "state"),
        text(a, "trigger")
    );
    for key in ["fixer", "reason", "commit_sha", "error_code"] {
        if a.get(key).is_some_and(|v| !v.is_null()) {
            line.push_str(&format!(" {key}:{}", text(a, key)));
        }
    }
    line
}

pub fn autofix(cli: &Cli, cmd: &AutofixCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    match cmd {
        AutofixCommand::Status { project } => {
            let out = call(&c, Method::Get, &project_path(*project), &[], None)?;
            let human = project_text(&out);
            emit(io, cli, out, &human)
        }
        AutofixCommand::Enable { project } | AutofixCommand::Disable { project } => {
            let enabled = matches!(cmd, AutofixCommand::Enable { .. });
            let body = json!({ "enabled": enabled });
            let out = call(&c, Method::Put, &project_path(*project), &[], Some(&body))?;
            let human = project_text(&out);
            emit(io, cli, out, &human)
        }
        AutofixCommand::Mr { project, iid } => {
            let path = format!("/api/v1/projects/{project}/merge-requests/{iid}/autofix");
            let out = call(&c, Method::Get, &path, &[], None)?;
            let human = mr_text(&out);
            emit(io, cli, out, &human)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merge_request_line_names_its_state_and_reason() {
        let m = json!({"iid": 4, "enabled": true, "attempt": {"state": "refused",
            "trigger": "pipeline_failed", "fixer": "fmt", "reason": "outside_fixer_paths",
            "commit_sha": null, "error_code": "FG-GIT-0037"}});
        assert_eq!(
            mr_text(&m),
            "!4 refused pipeline_failed fixer:fmt reason:outside_fixer_paths error_code:FG-GIT-0037"
        );
        assert_eq!(
            mr_text(&json!({"iid": 5, "attempt": null})),
            "!5: no autofix attempt"
        );
        assert_eq!(project_text(&json!({"enabled": false})), "autofix: off");
    }
}
