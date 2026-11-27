//! A person's changes to the local LLM (plan 19 §8.2, ADR-0039 Amendment
//! `feat/core-llm-settings-and-rerun`), each one audit row and no event:
//!
//! - a project's `llm.review` (`LlmSettingsManage`, on the project);
//! - the gateway's pause and resume (`LlmPause` / `LlmResume`, on the
//!   instance; the worker follows the stored state within seconds);
//! - asking a merge request's first pass again (`AiReviewRerun`, on the
//!   merge request; at most [`RERUNS_PER_HOUR`] an hour per merge request).

use super::ReviewMode;
use crate::ids::{MergeRequestId, ProjectId};

/// The most reruns of one merge request's first pass in [`RERUN_WINDOW_SECS`].
pub const RERUNS_PER_HOUR: i64 = 3;
/// The window of [`RERUNS_PER_HOUR`] (a rolling hour).
pub const RERUN_WINDOW_SECS: i64 = 60 * 60;

/// One audited change (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmChange {
    /// `llm.review_changed` (resource the project).
    ReviewMode {
        project: ProjectId,
        from: ReviewMode,
        to: ReviewMode,
    },
    /// `llm.paused` / `llm.resumed` (resource the instance).
    Gateway { paused: bool },
    /// `llm.review_rerun_requested` (resource the merge request).
    ReviewRerun {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        head_sha: String,
    },
}

impl LlmChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::ReviewMode { .. } => "llm.review_changed",
            Self::Gateway { paused: true } => "llm.paused",
            Self::Gateway { paused: false } => "llm.resumed",
            Self::ReviewRerun { .. } => "llm.review_rerun_requested",
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::ReviewMode { project, from, to } => serde_json::json!({
                "project_id": project.get(),
                "from": from.as_str(),
                "to": to.as_str(),
            }),
            Self::Gateway { paused } => serde_json::json!({ "paused": paused }),
            Self::ReviewRerun {
                project,
                merge_request,
                iid,
                head_sha,
            } => serde_json::json!({
                "project_id": project.get(),
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "head_sha": head_sha,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_change_names_its_audit_action_and_shape() {
        let mode = LlmChange::ReviewMode {
            project: ProjectId::new(7),
            from: ReviewMode::Off,
            to: ReviewMode::Advisory,
        };
        assert_eq!(mode.action(), "llm.review_changed");
        assert_eq!(
            mode.to_value(),
            serde_json::json!({"project_id": 7, "from": "off", "to": "advisory"})
        );
        assert_eq!(LlmChange::Gateway { paused: true }.action(), "llm.paused");
        assert_eq!(LlmChange::Gateway { paused: false }.action(), "llm.resumed");
        let rerun = LlmChange::ReviewRerun {
            project: ProjectId::new(7),
            merge_request: MergeRequestId::new(9),
            iid: 3,
            head_sha: "a".repeat(40),
        };
        assert_eq!(rerun.action(), "llm.review_rerun_requested");
        assert_eq!(rerun.to_value()["merge_request_id"], 9);
        assert_eq!(rerun.to_value()["iid"], 3);
    }
}
