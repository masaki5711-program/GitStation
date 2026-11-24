//! `gstn doc status|versions|claim|release` (plan 13 §7.5, stage 4 part
//! A, ADR-0027): a document's open revisions and edit claim, its versions
//! on the default branch, and the caller's edit claim. One REST call each;
//! the project id is validated by clap and the document id by
//! [`crate::doc_view::parse_doc_id`] before either is formatted into the
//! path. Server text (titles) is sanitized before it reaches a terminal.
//! Tests: `tests/doc_revision.rs`.

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::Result;
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;
use serde_json::{json, Value};

fn doc_path(project: i64, id: &str, suffix: &str) -> String {
    format!("/api/v1/projects/{project}/documents/{id}/{suffix}")
}

fn claim_line(c: &Value) -> String {
    let mr = match c.get("merge_request_iid").and_then(Value::as_i64) {
        Some(iid) => format!(" for !{iid}"),
        None => String::new(),
    };
    format!(
        "editing: user {}{mr} since {} (lapses {})",
        text(c, "user_id"),
        text(c, "claimed_at"),
        text(c, "expires_at")
    )
}

/// The status as lines: the claim, then each open revision.
fn status_text(s: &Value) -> String {
    let mut lines = vec![match s.get("edit_claim") {
        Some(c) if c.is_object() => claim_line(c),
        _ => "editing: nobody".to_owned(),
    }];
    let revisions = s.get("revisions").and_then(Value::as_array);
    match revisions {
        Some(list) if !list.is_empty() => {
            for r in list {
                let lang = match r.get("lang").and_then(Value::as_str) {
                    Some(_) => format!(" [{}]", text(r, "lang")),
                    None => String::new(),
                };
                let held = if r.get("holds").and_then(Value::as_bool) == Some(true) {
                    "revising"
                } else {
                    "second revision (FG-DOC-0006)"
                };
                lines.push(format!(
                    "!{}{lang}  {held}  by user {} since {}  {}",
                    text(r, "merge_request_iid"),
                    text(r, "author_id"),
                    text(r, "since"),
                    text(r, "title")
                ));
            }
        }
        _ => lines.push("revisions: none open".to_owned()),
    }
    lines.join("\n")
}

fn versions_text(v: &Value) -> String {
    let mut lines = vec![format!(
        "{} at {}: version {}",
        text(v, "doc_id"),
        text(v, "path"),
        text(v, "current_revision")
    )];
    for x in v
        .get("versions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "r{}  !{}  {}  {}",
            text(x, "revision"),
            text(x, "merge_request_iid"),
            text(x, "merged_at"),
            text(x, "title")
        ));
    }
    if let Some(i) = v.get("incomplete").filter(|i| i.is_object()) {
        lines.push(format!(
            "incomplete: {} ({})",
            text(i, "reason"),
            text(i, "error_code")
        ));
    }
    lines.join("\n")
}

/// `gstn doc status <ID> --project <P>`.
pub fn status(cli: &Cli, project: i64, id: &str, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let out = call(&c, Method::Get, &doc_path(project, id, "status"), &[], None)?;
    let human = status_text(&out);
    emit(io, cli, out, &human)
}

/// `gstn doc versions <ID> --project <P>`.
pub fn versions(cli: &Cli, project: i64, id: &str, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let out = call(
        &c,
        Method::Get,
        &doc_path(project, id, "versions"),
        &[],
        None,
    )?;
    let human = versions_text(&out);
    emit(io, cli, out, &human)
}

/// `gstn doc claim <ID> --project <P> [--merge-request <IID>]`.
pub fn claim(
    cli: &Cli,
    project: i64,
    id: &str,
    merge_request: Option<i64>,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let body = match merge_request {
        Some(iid) => json!({ "merge_request_iid": iid }),
        None => json!({}),
    };
    let c = client(cli, env, io)?;
    let out = call(
        &c,
        Method::Put,
        &doc_path(project, id, "edit-claim"),
        &[],
        Some(&body),
    )?;
    let verb = if out.get("extended").and_then(Value::as_bool) == Some(true) {
        "Extended"
    } else {
        "Took"
    };
    let lapses = out
        .get("claim")
        .map(|c| text(c, "expires_at"))
        .unwrap_or_else(|| "-".to_owned());
    let human = format!("{verb} the edit claim on {id} (lapses {lapses})");
    emit(io, cli, out, &human)
}

/// `gstn doc release <ID> --project <P> [--revoke --yes]`.
pub fn release(
    cli: &Cli,
    project: i64,
    id: &str,
    (revoke, yes): (bool, bool),
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    if revoke {
        confirm(yes, "revoking another person's edit claim")?;
    }
    let c = client(cli, env, io)?;
    let (method, suffix, done) = if revoke {
        (
            Method::Post,
            "edit-claim/revoke",
            "Revoked the edit claim on",
        )
    } else {
        (Method::Delete, "edit-claim", "Released your edit claim on")
    };
    let out = call(&c, method, &doc_path(project, id, suffix), &[], None)?;
    emit(io, cli, out, &format!("{done} {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_names_the_claim_and_marks_a_second_revision() {
        let s = json!({
            "doc_id": "QP-07-001",
            "edit_claim": {"doc_id": "QP-07-001", "user_id": 3, "merge_request_iid": null,
                "claimed_at": "t0", "expires_at": "t1"},
            "revisions": [
                {"lang": null, "merge_request_iid": 4, "title": "A\u{1b}[31m", "author_id": 3,
                 "since": "s", "head_sha": "h", "holds": true},
                {"lang": null, "merge_request_iid": 5, "title": "B", "author_id": 2,
                 "since": "s2", "head_sha": "h", "holds": false}
            ],
            "viewer": {"can_claim": true, "can_revoke": false}
        });
        let t = status_text(&s);
        assert!(t.starts_with("editing: user 3 since t0 (lapses t1)"), "{t}");
        assert!(t.contains("!4  revising"), "{t}");
        assert!(t.contains("!5  second revision (FG-DOC-0006)"), "{t}");
        assert!(!t.contains('\u{1b}'), "{t}");
        let none = json!({"edit_claim": null, "revisions": []});
        assert_eq!(status_text(&none), "editing: nobody\nrevisions: none open");
    }

    #[test]
    fn the_versions_are_listed_oldest_first() {
        let v = json!({
            "doc_id": "QP-07-001", "path": "docs/QP-07-001.md", "current_revision": 2,
            "versions": [
                {"revision": 1, "merge_request_iid": 4, "merged_at": "m1", "title": "first"},
                {"revision": 2, "merge_request_iid": 9, "merged_at": "m2", "title": "second"}
            ],
            "incomplete": null
        });
        assert_eq!(
            versions_text(&v),
            "QP-07-001 at docs/QP-07-001.md: version 2\nr1  !4  m1  first\nr2  !9  m2  second"
        );
    }
}
