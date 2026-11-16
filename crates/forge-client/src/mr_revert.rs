//! `gstn mr revert` (M4 C10, plan 04 M-4, ADR-0024 Amendment C10): one
//! step opens a merge request that reverts a merged one
//! (`POST …/merge-requests/{iid}/revert`, `201`): the new branch
//! `revert/mr-<iid>` holds one revert commit on the target's tip. Decided as
//! opening a merge request and pushing the branch: the token needs `api`
//! and `write_repository`. A conflicting revert opens nothing
//! (FG-REVIEW-0030); not merged, an existing revert branch or nothing left
//! to revert is FG-REVIEW-0029.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

fn revert_path(project: i64, iid: i64) -> String {
    format!("/api/v1/projects/{project}/merge-requests/{iid}/revert")
}

/// The answer as a short block of text.
fn opened_human(v: &Value) -> String {
    let mr = v.get("merge_request").cloned().unwrap_or(Value::Null);
    format!(
        "!{} reverts !{}: {} -> {}\ncommit: {} (on {})\ntitle: {}",
        text(&mr, "iid"),
        text(v, "reverted_iid"),
        text(&mr, "source_branch"),
        text(&mr, "target_branch"),
        text(v, "commit_sha"),
        text(v, "parent_sha"),
        text(&mr, "title"),
    )
}

/// `gstn mr revert`.
pub(crate) fn revert(
    cli: &Cli,
    (project, iid): (i64, i64),
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let body = call(&c, Method::Post, &revert_path(project, iid), &[], None)?;
    let h = opened_human(&body);
    emit(io, cli, body, &h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_answer_names_the_revert_and_what_it_reverts() {
        let v = json!({
            "merge_request": {"iid": 4, "source_branch": "revert/mr-3", "target_branch": "main", "title": "Revert \"Fix\""},
            "reverted_iid": 3,
            "commit_sha": "c".repeat(40),
            "parent_sha": "p".repeat(40),
        });
        let h = opened_human(&v);
        assert!(h.starts_with("!4 reverts !3: revert/mr-3 -> main"), "{h}");
        assert!(h.contains(&"c".repeat(40)));
        assert_eq!(
            revert_path(7, 3),
            "/api/v1/projects/7/merge-requests/3/revert"
        );
    }
}
