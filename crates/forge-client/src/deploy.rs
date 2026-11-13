//! `gstn deploy …` (M4 C11, plan 14 §4.2, ADR-0031): a project's
//! deployment environments, targets and deployments. One REST call each.
//! prod is approved only in the browser (a token is refused with
//! FG-AUTH-0002 session_required); `approve` serves dev and stg.

use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, DeployCommand};
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
        DeployCommand::Approve {
            project,
            deployment,
        } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &format!("{}/deployments/{deployment}/approve", base(*project)),
                &[],
                None,
            )?;
            let human = format!("Approved {}", deployment_line(&out));
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
