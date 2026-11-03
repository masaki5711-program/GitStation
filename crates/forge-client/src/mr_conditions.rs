//! The merge conditions (M3 R6, ADR-0024 §7, §8, §9, §12): `gstn mr
//! status` (the stable merge status contract: mergeable and every blocking
//! reason), `gstn merge-settings show|set`, `gstn codeowners-enforcement
//! suspend|enforce`, `gstn merge-rule set|remove`. One REST call each
//! (`merge-settings set` reads the settings first and replaces only what
//! was asked); ids are validated by clap and only formatted into paths as
//! integers. Tests: `tests/merge_requests.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, CodeownersCommand, MergeRuleCommand, MergeSettingsCommand};
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;
use serde_json::{json, Value};

fn settings(project: i64) -> String {
    format!("/api/v1/projects/{project}/merge-settings")
}

fn merge_rule(project: i64, rule: i64) -> String {
    format!("/api/v1/projects/{project}/protected-refs/{rule}/merge-rule")
}

/// A merge method as the server names it.
pub fn parse_method(raw: &str) -> std::result::Result<String, String> {
    match raw {
        "merge_commit" | "squash" | "fast_forward" => Ok(raw.to_owned()),
        _ => Err("a method is merge_commit, squash or fast_forward".to_owned()),
    }
}

/// The merge status in a few lines: mergeable, or every reason.
fn status_human(body: &Value) -> String {
    let iid = text(body, "iid");
    let reasons = body
        .get("reasons")
        .and_then(Value::as_array)
        .cloned() // clone: the printer owns its rows
        .unwrap_or_default();
    if body.get("mergeable") == Some(&Value::Bool(true)) {
        return format!("!{iid}: mergeable ({})", text(body, "method"));
    }
    let mut out = format!("!{iid}: blocked");
    for r in reasons {
        out.push_str(&format!(
            "\n  {}{}",
            sanitize(r["reason"].as_str().unwrap_or("")),
            r["detail"]
                .as_str()
                .map(|d| format!(" ({})", sanitize(d)))
                .unwrap_or_default()
        ));
    }
    out
}

fn settings_human(body: &Value) -> String {
    let methods: Vec<String> = body["allowed_methods"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(sanitize).collect())
        .unwrap_or_default();
    let codeowners = if body["codeowners"]["enforced"] == Value::Bool(true) {
        "enforced".to_owned()
    } else {
        format!(
            "suspended until {}",
            sanitize(
                body["codeowners"]["suspended_until"]
                    .as_str()
                    .unwrap_or("-")
            )
        )
    };
    format!(
        "methods {} (default {}); delete source by default: {}; new commits clear approvals: {}; committers' approvals excluded: {}; CODEOWNERS {codeowners}",
        methods.join(","),
        text(body, "default_method"),
        text(body, "delete_source_default"),
        text(body, "reset_approvals_on_push"),
        text(body, "committer_approvals_excluded"),
    )
}

/// `gstn mr status`.
pub fn status(
    cli: &Cli,
    (project, iid): (i64, i64),
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(
        &c,
        Method::Get,
        &format!("/api/v1/projects/{project}/merge-requests/{iid}/status"),
        &[],
        None,
    )?;
    let h = status_human(&body);
    emit(io, cli, body, &h)
}

/// `gstn merge-settings …`.
pub fn merge_settings(
    cli: &Cli,
    cmd: &MergeSettingsCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        MergeSettingsCommand::Show { project } => {
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Get, &settings(*project), &[], None)?;
            let h = settings_human(&body);
            emit(io, cli, body, &h)
        }
        MergeSettingsCommand::Set {
            project,
            allow,
            default,
            delete_source_branch,
            reset_approvals_on_push,
            exclude_committer_approvals,
        } => {
            let c = client(cli, env, io)?;
            let current = call(&c, Method::Get, &settings(*project), &[], None)?;
            let allowed = if allow.is_empty() {
                current["allowed_methods"].clone()
            } else {
                json!(allow)
            };
            let req = json!({
                "allowed_methods": allowed,
                "default_method": default.clone().map_or_else(|| current["default_method"].clone(), Value::String),
                "delete_source_default": delete_source_branch.map_or_else(|| current["delete_source_default"].clone(), Value::Bool),
                "reset_approvals_on_push": reset_approvals_on_push.map_or_else(|| current["reset_approvals_on_push"].clone(), Value::Bool),
                "committer_approvals_excluded": exclude_committer_approvals.map_or_else(|| current["committer_approvals_excluded"].clone(), Value::Bool),
                // U2: the settings this replaces; changed in between is
                // FG-REVIEW-0017 (read again and retry), never overwritten.
                "expected_updated_at": current["updated_at"].clone(),
            });
            let body = call(&c, Method::Put, &settings(*project), &[], Some(&req))?;
            let h = settings_human(&body);
            emit(io, cli, body, &h)
        }
    }
}

/// `gstn codeowners-enforcement …`.
pub fn codeowners(
    cli: &Cli,
    cmd: &CodeownersCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let (project, req) = match cmd {
        CodeownersCommand::Suspend {
            project,
            for_minutes,
        } => (
            *project,
            json!({"enforced": false, "for_minutes": for_minutes}),
        ),
        CodeownersCommand::Enforce { project } => (*project, json!({"enforced": true})),
    };
    let c = client(cli, env, io)?;
    let body = call(
        &c,
        Method::Put,
        &format!("{}/codeowners-enforcement", settings(project)),
        &[],
        Some(&req),
    )?;
    let h = settings_human(&body);
    emit(io, cli, body, &h)
}

/// `gstn merge-rule …`.
pub fn merge_rule_cmd(
    cli: &Cli,
    cmd: &MergeRuleCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        MergeRuleCommand::Set {
            project,
            rule_id,
            approvals,
            check,
            clear_checks,
            resolved_discussions,
            direct_push_forbidden,
        } => {
            let c = client(cli, env, io)?;
            // The PUT replaces the merge rule: read it first and keep what
            // was not asked (R6 review).
            let rules = call(
                &c,
                Method::Get,
                &format!("/api/v1/projects/{project}/protected-refs"),
                &[],
                None,
            )?;
            let current = rules["rules"]
                .as_array()
                .and_then(|a| a.iter().find(|r| r["id"].as_i64() == Some(*rule_id)))
                .map(|r| r["merge_rule"].clone()) // clone: merged below
                .unwrap_or(Value::Null);
            let keep = |key: &str, default: Value| match current.get(key) {
                Some(v) if !v.is_null() => v.clone(), // clone: the request owns it
                _ => default,
            };
            let checks = if *clear_checks {
                json!([])
            } else if check.is_empty() {
                keep("required_checks", json!([]))
            } else {
                json!(check)
            };
            let req = json!({
                "required_approvals": approvals
                    .map_or_else(|| keep("required_approvals", json!(0)), |n| json!(n)),
                "required_checks": checks,
                "require_resolved_discussions": resolved_discussions
                    .map_or_else(|| keep("require_resolved_discussions", json!(false)), Value::Bool),
                "direct_push_forbidden": direct_push_forbidden
                    .map_or_else(|| keep("direct_push_forbidden", json!(false)), Value::Bool),
                // U2 follow-up: the merge rule this replaces (`null`: none
                // yet); changed in between is FG-REVIEW-0019 (run it
                // again), never overwritten.
                "expected_updated_at": keep("updated_at", Value::Null),
            });
            let body = call(
                &c,
                Method::Put,
                &merge_rule(*project, *rule_id),
                &[],
                Some(&req),
            )?;
            let m = &body["merge_rule"];
            let warned = body["warnings"]
                .as_array()
                .is_some_and(|w| w.iter().any(|x| x == "required_checks_unreportable"));
            let mut human = format!(
                "Rule {} ({}): {} approval(s), checks [{}], resolved discussions {}, direct pushes {}",
                text(&body, "id"),
                text(&body, "pattern"),
                m["required_approvals"],
                m["required_checks"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(Value::as_str)
                        .map(sanitize)
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default(),
                m["require_resolved_discussions"],
                if m["direct_push_forbidden"] == Value::Bool(true) {
                    "forbidden"
                } else {
                    "allowed"
                }
            );
            if warned {
                human.push_str(
                    "\nwarning: no check can be reported before the Checks API (C1): every merge into this branch is blocked (required_check_missing). Remove the checks with --clear-checks or remove the merge rule (runbook merge-blocked).",
                );
            }
            emit(io, cli, body, &human)
        }
        MergeRuleCommand::Remove {
            project,
            rule_id,
            yes,
        } => {
            confirm(*yes, "removing a merge rule")?;
            let c = client(cli, env, io)?;
            call(
                &c,
                Method::Delete,
                &merge_rule(*project, *rule_id),
                &[],
                None,
            )?;
            let out = json!({"removed": true, "project_id": project, "rule_id": rule_id});
            emit(
                io,
                cli,
                out,
                &format!("Removed the merge rule of rule {rule_id} in project {project}"),
            )
        }
    }
}
