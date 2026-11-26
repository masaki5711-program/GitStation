//! `gstn deploy …` (M4 C11, plan 14 §4.2, ADR-0031): a project's
//! deployment environments, targets and deployments. One REST call each.
//! A deployment is approved only in the browser (a token is refused with
//! FG-AUTH-0002 session_required, every tier).

use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, DelegationCommand, DeployCommand};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn base(project: i64) -> String {
    format!("/api/v1/projects/{project}")
}

/// One deployment as a line.
fn deployment_line(d: &Value) -> String {
    format!(
        "#{} {} {} {} (requested by {}, approved by {}{})",
        text(d, "id"),
        text(d, "environment"),
        text(d, "sha"),
        text(d, "status"),
        text(d, "requested_by"),
        text(d, "approved_by"),
        match d.get("error_code").and_then(Value::as_str) {
            Some(code) => format!(", {code}"),
            None => String::new(),
        }
    )
}

/// The approval facts and the summary as text (M5 L5-8): every fact list
/// with its count, then the summary apart, labelled as advice.
fn summary_text(out: &Value) -> String {
    let empty = Value::Null;
    let d = out.get("deployment").unwrap_or(&empty);
    let f = out.get("facts").unwrap_or(&empty);
    let s = out.get("summary").unwrap_or(&empty);
    let list = |key: &str, field: Option<&str>| -> Vec<String> {
        f.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|v| match field {
                Some(name) => text(v, name),
                None => crate::error::sanitize(v.as_str().unwrap_or("")),
            })
            .collect()
    };
    let mut lines = vec![deployment_line(d)];
    lines.push(format!(
        "range: {} (base {}, {} commits)",
        text(f, "range"),
        text(f, "base_sha"),
        text(f, "commits")
    ));
    let mrs: Vec<String> = f
        .get("merge_requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|m| {
            format!(
                "!{} {} (forge/risk {})",
                text(m, "iid"),
                text(m, "title"),
                text(m, "risk")
            )
        })
        .collect();
    for (name, items) in [
        ("merge requests", mrs),
        ("migrations", list("migrations", None)),
        ("protected paths", list("protected_paths", Some("path"))),
        ("ci/deploy config", list("ci_config", None)),
        ("permission code", list("permissions", None)),
        ("failed checks", list("failed_checks", Some("name"))),
        ("freeze windows", list("freeze_windows", None)),
    ] {
        lines.push(format!("{name}: {}", items.len()));
        lines.extend(items.into_iter().map(|i| format!("  - {i}")));
    }
    if f.get("lists_truncated") == Some(&Value::Bool(true)) {
        lines.push("lists: truncated".to_owned());
    }
    let stale = if s.get("stale") == Some(&Value::Bool(true)) {
        format!(" (stale: {})", text(s, "stale_reason"))
    } else {
        String::new()
    };
    let words = match s.get("state").and_then(Value::as_str) {
        Some("shown") => text(s, "text"),
        _ => text(s, "note"),
    };
    lines.push(format!("{}{stale}: {words}", text(s, "label")));
    lines.join("\n")
}

/// One environment's approvers as a line.
fn approvers_line(e: &Value) -> String {
    if e.get("unrestricted") == Some(&Value::Bool(true)) {
        return format!("{}: any Maintainer", text(e, "tier"));
    }
    let names = |key: &str, field: &str| -> Vec<String> {
        e.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|v| crate::error::sanitize(v[field].as_str().unwrap_or("")))
            .collect()
    };
    let selectors = e
        .get("selectors")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    format!(
        "{}: {} of users [{}] groups [{}] selectors {}{}",
        text(e, "tier"),
        text(e, "approvals_required"),
        names("users", "username").join(", "),
        names("groups", "full_path").join(", "),
        selectors,
        if e.get("allow_delegation") == Some(&Value::Bool(true)) {
            " (delegation allowed)"
        } else {
            ""
        }
    )
}

/// The body of `env-set`.
pub(crate) fn environment_body(
    requires_approval: bool,
    auto_rollback: bool,
    ttl_minutes: Option<i32>,
    auto_deploy_ref: Option<&str>,
    freeze: &[String],
) -> Value {
    let windows: Vec<Value> = freeze
        .iter()
        .map(|w| {
            let (start, end) = w.split_once('/').unwrap_or((w.as_str(), ""));
            json!({"starts_at": start, "ends_at": end})
        })
        .collect();
    json!({
        "requires_approval": requires_approval,
        "auto_rollback": auto_rollback,
        "approval_ttl_minutes": ttl_minutes,
        "auto_deploy_ref": auto_deploy_ref,
        "freeze_windows": windows,
    })
}

pub fn deploy(cli: &Cli, cmd: &DeployCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        DeployCommand::Envs { project } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Get,
                &format!("{}/deploy/environments", base(*project)),
                &[],
                None,
            )?;
            let lines: Vec<String> = out
                .get("environments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|e| {
                    let targets: Vec<String> = e
                        .get("targets")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|t| {
                            format!(
                                "  {} {}:{} {}",
                                text(t, "name"),
                                text(t, "host"),
                                text(t, "port"),
                                text(t, "host_key_fingerprint")
                            )
                        })
                        .collect();
                    format!(
                        "{} approval={} rollback={} frozen={}\n{}",
                        text(e, "tier"),
                        text(e, "requires_approval"),
                        text(e, "auto_rollback"),
                        text(e, "frozen_now"),
                        targets.join("\n")
                    )
                })
                .collect();
            let human = if lines.is_empty() {
                format!("No deployment environment in project {project}.")
            } else {
                lines.join("\n")
            };
            emit(io, cli, out, &human)
        }
        DeployCommand::EnvSet {
            project,
            tier,
            requires_approval,
            auto_rollback,
            ttl_minutes,
            auto_deploy_ref,
            freeze,
        } => {
            let body = environment_body(
                *requires_approval,
                *auto_rollback,
                *ttl_minutes,
                auto_deploy_ref.as_deref(),
                freeze,
            );
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Put,
                &format!("{}/deploy/environments/{}", base(*project), tier.as_str()),
                &[],
                Some(&body),
            )?;
            let human = format!("Set the environment {}", text(&out, "tier"));
            emit(io, cli, out, &human)
        }
        DeployCommand::TargetSet {
            project,
            tier,
            name,
            host,
            port,
            user,
            host_key_type,
            host_key,
            script,
            disabled,
        } => {
            let body = json!({
                "host": host, "port": port, "user": user,
                "host_key_type": host_key_type, "host_key": host_key,
                "script": script, "enabled": !disabled,
            });
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Put,
                &format!(
                    "{}/deploy/environments/{}/targets/{name}",
                    base(*project),
                    tier.as_str()
                ),
                &[],
                Some(&body),
            )?;
            let human = format!(
                "Set the target {} pinned to {}",
                text(&out, "name"),
                text(&out, "host_key_fingerprint")
            );
            emit(io, cli, out, &human)
        }
        DeployCommand::List { project } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Get,
                &format!("{}/deployments", base(*project)),
                &[],
                None,
            )?;
            let lines: Vec<String> = out
                .get("deployments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(deployment_line)
                .collect();
            let human = if lines.is_empty() {
                format!("No deployment in project {project}.")
            } else {
                lines.join("\n")
            };
            emit(io, cli, out, &human)
        }
        DeployCommand::Summary {
            project,
            deployment,
        } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Get,
                &format!(
                    "{}/deployments/{deployment}/approval-summary",
                    base(*project)
                ),
                &[],
                None,
            )?;
            let human = summary_text(&out);
            emit(io, cli, out, &human)
        }
        DeployCommand::Approvers { project } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Get,
                &format!("{}/deploy/approvers", base(*project)),
                &[],
                None,
            )?;
            let lines: Vec<String> = out
                .get("environments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(approvers_line)
                .collect();
            let human = if lines.is_empty() {
                format!("No deployment environment in project {project}.")
            } else {
                lines.join("\n")
            };
            emit(io, cli, out, &human)
        }
        DeployCommand::ApproversSet {
            project,
            tier,
            user,
            group,
            selector,
            required,
            allow_delegation,
        } => {
            let selectors = selector
                .iter()
                .map(|s| serde_json::from_str::<Value>(s))
                .collect::<std::result::Result<Vec<Value>, _>>()
                .map_err(|_| {
                    crate::error::ClientError::local(
                        forge_errors::ErrorCode::CliUsageInvalid,
                        "reason=selector_not_json",
                    )
                })?;
            let body = json!({
                "users": user, "groups": group, "selectors": selectors,
                "approvals_required": required, "allow_delegation": allow_delegation,
            });
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Put,
                &format!(
                    "{}/deploy/environments/{}/approvers",
                    base(*project),
                    tier.as_str()
                ),
                &[],
                Some(&body),
            )?;
            let human = approvers_line(&out);
            emit(io, cli, out, &human)
        }
        DeployCommand::Request { project, tier, sha } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &format!("{}/deployments", base(*project)),
                &[],
                Some(&json!({"environment": tier.as_str(), "sha": sha})),
            )?;
            let human = format!("Requested {}", deployment_line(&out));
            emit(io, cli, out, &human)
        }
    }
}

/// `gstn delegation show` (ADR-0038 §3.5): your delegation and the people
/// who delegate to you. Setting one is a browser session's (a token is
/// refused, FG-AUTH-0002).
pub fn delegation(
    cli: &Cli,
    cmd: &DelegationCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        DelegationCommand::Show => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Get,
                "/api/v1/user/approval-delegation",
                &[],
                None,
            )?;
            let line = |d: &Value, who: &str| {
                format!(
                    "{} from {} until {}{}",
                    crate::error::sanitize(d[who].as_str().unwrap_or("")),
                    text(d, "starts_at"),
                    text(d, "ends_at"),
                    if d["active"] == Value::Bool(true) {
                        " (active)"
                    } else {
                        ""
                    }
                )
            };
            let mut lines = vec![match out.get("mine").filter(|m| !m.is_null()) {
                Some(m) => format!("You delegate to {}", line(m, "delegate_username")),
                None => "You delegate to no one.".to_owned(),
            }];
            for d in out
                .get("to_me")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                lines.push(format!("You approve for {}", line(d, "delegator_username")));
            }
            let human = lines.join("\n");
            emit(io, cli, out, &human)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freeze_windows_are_sent_as_start_and_end() {
        let body = environment_body(
            true,
            false,
            Some(30),
            None,
            &["2026-12-28T00:00:00+09:00/2027-01-04T00:00:00+09:00".to_owned()],
        );
        assert_eq!(
            body["freeze_windows"][0]["starts_at"],
            "2026-12-28T00:00:00+09:00"
        );
        assert_eq!(
            body["freeze_windows"][0]["ends_at"],
            "2027-01-04T00:00:00+09:00"
        );
        assert_eq!(body["approval_ttl_minutes"], 30);
        assert!(body["auto_deploy_ref"].is_null());
    }
}
