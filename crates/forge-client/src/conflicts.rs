//! `gstn conflicts` (M3 X6, plan 04 C-2): a merge request's predicted
//! conflicts with the other open merge requests on its target branch
//! (drafts included), as the last conflict scan stored them —
//! `GET /api/v1/projects/{id}/merge-requests/{iid}/conflicts`. Never starts
//! a scan (the server scans after every new head, retarget and reopen).

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, ConflictsArgs};
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, text};
use crate::http::Method;

/// `gstn conflicts --project ID --iid N`.
pub fn conflicts(cli: &Cli, args: &ConflictsArgs, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let path = format!(
        "/api/v1/projects/{}/merge-requests/{}/conflicts",
        args.project, args.iid
    );
    let body = call(&c, Method::Get, &path, &[], None)?;
    let h = human(&body);
    emit(io, cli, body, &h)
}

/// The scan's state, then one line per prediction and its paths (server
/// text sanitized).
fn human(body: &Value) -> String {
    let scan = body.get("scan").unwrap_or(&Value::Null);
    let mut out = format!("!{}: scan {}", text(body, "iid"), text(scan, "status"));
    if let Some(reason) = scan.get("reason").and_then(Value::as_str) {
        out.push_str(&format!(" ({})", sanitize(reason)));
    }
    if scan.get("stale") == Some(&Value::Bool(true)) {
        out.push_str(" (the last scan is for another head or target)");
    }
    out.push_str(&format!(
        "\nhead: {}\nscanned: head {} at {}; {} other merge requests, {} trial merges, {} left out",
        text(body, "head_sha"),
        text(scan, "scanned_head_sha"),
        text(scan, "scanned_at"),
        text(scan, "peers"),
        text(scan, "pairs_tried"),
        text(scan, "pairs_skipped"),
    ));
    let predictions = body
        .get("predictions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if predictions.is_empty() {
        out.push_str("\nno predicted conflicts");
    }
    for p in predictions {
        let draft = if p.get("draft") == Some(&Value::Bool(true)) {
            " (draft)"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n!{}{draft}: {}",
            text(p, "iid"),
            text(p, "outcome")
        ));
        if let Some(reason) = p.get("reason").and_then(Value::as_str) {
            out.push_str(&format!(" ({})", sanitize(reason)));
        }
        for path in p
            .get("conflicts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            out.push_str(&format!(
                "\n  conflict: {}",
                sanitize(path.as_str().unwrap_or(""))
            ));
        }
        if p.get("conflicts_truncated").and_then(Value::as_bool) == Some(true) {
            out.push_str("\n  (more conflicted files than listed)");
        }
    }
    out
}
