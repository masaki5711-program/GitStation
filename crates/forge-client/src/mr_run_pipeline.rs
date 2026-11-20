//! `gstn mr run-pipeline` (plan 18 S5, ADR-0037): ask for the merge request
//! pipeline (`ci/pipeline`) on the merge result of the current target
//! (`POST …/merge-requests/{iid}/pipeline`, `202`). Decided as a manual
//! pipeline run (`PipelineCreate`, Developer; the token needs `api`). The
//! answer is `queued` (its job was enqueued), `exists` (asked already:
//! nothing written) or `computing` (the merge result is written first; the
//! pipeline follows). Not open, no version or a conflict with the target is
//! `409 FG-CI-0003`; an unknown merge request `404 FG-REPO-0005`.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn pipeline_path(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}/pipeline")
}

/// The answer as one line.
fn run_human(v: &Value) -> String {
    let iid = text(v, "iid");
    match v.get("outcome").and_then(Value::as_str) {
        Some("queued") => format!(
            "!{iid}: pipeline queued on merge result {} (target {})",
            text(v, "merge_result_sha"),
            text(v, "target_sha")
        ),
        Some("exists") => format!(
            "!{iid}: pipeline already asked for on merge result {} (target {})",
            text(v, "merge_result_sha"),
            text(v, "target_sha")
        ),
        Some("computing") => format!(
            "!{iid}: merge result being computed (check {}); the pipeline follows it",
            text(v, "check_seq")
        ),
        _ => format!("!{iid}: {}", text(v, "outcome")),
    }
}

/// `gstn mr run-pipeline`.
pub(crate) fn run(
    cli: &Cli,
    (project, iid): (i64, i64),
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(&c, Method::Post, &pipeline_path(project, iid), &[], None)?;
    let h = run_human(&body);
    emit(io, cli, body, &h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_answer_names_the_outcome_and_the_merge_result() {
        let queued = json!({
            "iid": 3, "outcome": "queued",
            "merge_result_sha": "m".repeat(40), "target_sha": "t".repeat(40), "check_seq": null,
        });
        let h = run_human(&queued);
        assert!(h.starts_with("!3: pipeline queued on merge result"), "{h}");
        assert!(h.contains(&"m".repeat(40)) && h.contains(&"t".repeat(40)));

        let exists = json!({
            "iid": 3, "outcome": "exists",
            "merge_result_sha": "m".repeat(40), "target_sha": "t".repeat(40), "check_seq": null,
        });
        assert!(run_human(&exists).contains("already asked for"));

        let computing = json!({
            "iid": 3, "outcome": "computing",
            "merge_result_sha": null, "target_sha": null, "check_seq": 12,
        });
        let h = run_human(&computing);
        assert!(h.contains("check 12") && h.contains("follows"), "{h}");

        assert_eq!(
            pipeline_path(7, 3),
            "/api/v1/projects/7/merge-requests/3/pipeline"
        );
    }
}
