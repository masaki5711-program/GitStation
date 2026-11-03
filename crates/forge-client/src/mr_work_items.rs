//! A merge request's work item links (`gstn mr work-items`,
//! `mr link-work-item`, `mr unlink-work-item`; M3 I1b, plan 02 §2). One
//! REST call each; ids and iids are validated by clap and formatted into
//! paths as integers, a reference only after [`parse_work_item_ref`].
//! Tests: `tests/merge_requests.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;
use serde_json::{json, Value};

fn links(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}/work-items")
}

/// A work item reference as the server accepts it, `PBI-<n>` (1..=7
/// digits, no leading zero but `PBI-0`); it goes into the request path, so
/// nothing else may pass.
pub fn parse_work_item_ref(raw: &str) -> std::result::Result<String, String> {
    let ok = raw.strip_prefix("PBI-").is_some_and(|d| {
        !d.is_empty()
            && d.len() <= 7
            && d.bytes().all(|b| b.is_ascii_digit())
            && (d == "0" || !d.starts_with('0'))
    });
    if ok {
        Ok(raw.to_owned())
    } else {
        Err("a work item reference is PBI-<n> (1..=7 digits, no leading zero)".to_owned())
    }
}

/// `gstn mr work-items`.
pub fn list(cli: &Cli, (project, iid): (i64, i64), env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(&c, Method::Get, &links(project, iid), &[], None)?;
    let mut human = format!("!{} work items", text(&body, "iid"));
    for l in body["links"].as_array().into_iter().flatten() {
        let state = if l["unlinked_at"].is_null() {
            "linked".to_owned()
        } else {
            format!("unlinked ({})", text(l, "unlink_source"))
        };
        human.push_str(&format!(
            "\n  {} {} {} {}",
            text(&l["work_item"], "ref"),
            text(l, "source"),
            state,
            text(l, "linked_at"),
        ));
    }
    if body["truncated"] == Value::Bool(true) {
        human.push_str("\n  (more links exist)");
    }
    emit(io, cli, body, &human)
}

/// `gstn mr link-work-item`.
pub fn link(
    cli: &Cli,
    (project, iid): (i64, i64),
    reference: &str,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(
        &c,
        Method::Put,
        &format!("{}/{reference}", links(project, iid)),
        &[],
        None,
    )?;
    let human = format!(
        "!{}: {} {}",
        text(&body, "iid"),
        text(&body["work_item"], "ref"),
        text(&body, "outcome")
    );
    emit(io, cli, body, &human)
}

/// `gstn mr unlink-work-item`.
pub fn unlink(
    cli: &Cli,
    (project, iid): (i64, i64),
    reference: &str,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    call(
        &c,
        Method::Delete,
        &format!("{}/{reference}", links(project, iid)),
        &[],
        None,
    )?;
    let out = json!({"unlinked": true, "project_id": project, "iid": iid, "ref": reference});
    emit(io, cli, out, &format!("!{iid}: {reference} unlinked"))
}
