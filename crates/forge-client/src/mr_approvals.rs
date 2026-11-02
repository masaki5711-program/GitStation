//! Approvals and approval rules (`gstn mr approve`, `mr unapprove`,
//! `mr approvals`, `gstn approval-rule list|set|remove`; M3 R5, ADR-0024
//! §8). One REST call each; ids and iids are validated by clap and only
//! formatted into paths as integers, a rule name only after
//! [`parse_rule_name`]. Tests: `tests/merge_requests.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{ApprovalRuleCommand, Cli};
use crate::error::Result;
use crate::hosting::{call, client, confirm, list_human, text};
use crate::http::Method;
use serde_json::{json, Value};

fn one(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}")
}

fn rules(project: i64) -> String {
    format!("/api/v1/projects/{project}/approval-rules")
}

/// A rule name as the server accepts it (`[a-z0-9][a-z0-9_.-]{0,63}`); it
/// goes into the request path, so nothing else may pass.
pub fn parse_rule_name(raw: &str) -> std::result::Result<String, String> {
    let mut bytes = raw.bytes();
    let ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && raw.len() <= 64
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        });
    if ok {
        Ok(raw.to_owned())
    } else {
        Err("a rule name is 1..=64 of a-z 0-9 _ . - (starting with a letter or digit)".to_owned())
    }
}

/// What an approve / unapprove did, in one line.
fn answer_human(body: &Value) -> String {
    let outcome = text(body, "outcome");
    let mut out = format!("!{}: {outcome}", text(body, "iid"));
    if outcome == "approved" {
        if body.get("counted") == Some(&Value::Bool(true)) {
            out.push_str(" (counts)");
        } else {
            out.push_str(&format!(
                " (does not count: {})",
                crate::error::sanitize(body["uncounted_reason"].as_str().unwrap_or("unknown"))
            ));
        }
    }
    out
}

/// The approval state: each approval, the rules, CODEOWNERS, the blocks.
fn state_human(body: &Value) -> String {
    let mut out = format!("!{} head {}", text(body, "iid"), text(body, "head_sha"));
    let list = |key: &str| {
        body.get(key)
            .and_then(Value::as_array)
            .cloned() // clone: the printer owns its rows
            .unwrap_or_default()
    };
    for a in list("approvals") {
        let how = if a["counted"] == Value::Bool(true) {
            "counts".to_owned()
        } else {
            format!(
                "does not count: {}",
                crate::error::sanitize(a["uncounted_reason"].as_str().unwrap_or("unknown"))
            )
        };
        out.push_str(&format!(
            "\napproved by {} ({how})",
            crate::error::sanitize(a["username"].as_str().unwrap_or(""))
        ));
    }
    for r in list("rules") {
        out.push_str(&format!(
            "\nrule {}: {}/{}{}",
            crate::error::sanitize(r["name"].as_str().unwrap_or("")),
            r["counted"],
            r["required"],
            if r["applies"] == Value::Bool(true) {
                ""
            } else {
                " (not for this target)"
            }
        ));
    }
    for c in list("codeowners") {
        out.push_str(&format!(
            "\nCODEOWNERS line {} {}: {}",
            c["line"],
            crate::error::sanitize(c["pattern"].as_str().unwrap_or("")),
            if c["satisfied"] == Value::Bool(true) {
                "approved"
            } else {
                "needs an owner"
            }
        ));
    }
    let blocks = list("blocked_by");
    if blocks.is_empty() {
        out.push_str("\nnothing blocks on approvals");
    }
    for b in blocks {
        out.push_str(&format!(
            "\nblocked: {}{}",
            crate::error::sanitize(b["reason"].as_str().unwrap_or("")),
            b["detail"]
                .as_str()
                .map(|d| format!(" ({})", crate::error::sanitize(d)))
                .unwrap_or_default()
        ));
    }
    out
}

/// `gstn mr approve`.
pub fn approve(
    cli: &Cli,
    (project, iid): (i64, i64),
    sha: &str,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(
        &c,
        Method::Post,
        &format!("{}/approve", one(project, iid)),
        &[],
        Some(&json!({ "sha": sha })),
    )?;
    let h = answer_human(&body);
    emit(io, cli, body, &h)
}

/// `gstn mr unapprove`.
pub fn unapprove(
    cli: &Cli,
    (project, iid): (i64, i64),
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(
        &c,
        Method::Post,
        &format!("{}/unapprove", one(project, iid)),
        &[],
        None,
    )?;
    let h = answer_human(&body);
    emit(io, cli, body, &h)
}

/// `gstn mr approvals`.
pub fn approvals(
    cli: &Cli,
    (project, iid): (i64, i64),
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(
        &c,
        Method::Get,
        &format!("{}/approvals", one(project, iid)),
        &[],
        None,
    )?;
    let h = state_human(&body);
    emit(io, cli, body, &h)
}

/// `gstn approval-rule …`.
pub fn approval_rule(
    cli: &Cli,
    cmd: &ApprovalRuleCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        ApprovalRuleCommand::List { project } => {
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Get, &rules(*project), &[], None)?;
            let human = list_human(
                &body,
                "rules",
                "",
                "",
                &["name", "required_approvals", "target_pattern"],
            );
            emit(io, cli, body, &human)
        }
        ApprovalRuleCommand::Set {
            project,
            name,
            required,
            target_pattern,
            user,
            group,
        } => {
            let req = json!({
                "target_pattern": target_pattern,
                "required_approvals": required,
                "users": user,
                "groups": group,
            });
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Put,
                &format!("{}/{name}", rules(*project)),
                &[],
                Some(&req),
            )?;
            let human = format!(
                "Rule {} needs {} approval(s)",
                text(&body, "name"),
                text(&body, "required_approvals")
            );
            emit(io, cli, body, &human)
        }
        ApprovalRuleCommand::Remove { project, name, yes } => {
            confirm(*yes, "removing an approval rule")?;
            let c = client(cli, env, io)?;
            call(
                &c,
                Method::Delete,
                &format!("{}/{name}", rules(*project)),
                &[],
                None,
            )?;
            let out = json!({"removed": true, "project_id": project, "name": name});
            emit(
                io,
                cli,
                out,
                &format!("Removed approval rule {name} from project {project}"),
            )
        }
    }
}
