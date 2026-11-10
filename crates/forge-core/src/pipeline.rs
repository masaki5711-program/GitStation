//! The audit details of a person's change to a CI pipeline (M4 C2, plan 14):
//! a cancel, a retry, a manual run. The pipelines and their jobs are
//! derived state (forge writes them from pushes without an audit row);
//! what a person (an agent's token or a service account included) asked
//! for is one audit row on the project, written in the same transaction as
//! the change (`forge_db::pipelines`). No event.

use crate::ProjectId;

/// One person's change to a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineChange {
    /// `pipeline.canceled`: the pipeline and `jobs` unfinished jobs.
    Canceled {
        project: ProjectId,
        pipeline: i64,
        jobs: u64,
    },
    /// `pipeline.retried`: `jobs` failed or cancelled jobs got a new attempt.
    Retried {
        project: ProjectId,
        pipeline: i64,
        jobs: u64,
    },
    /// `pipeline.run_manual`: a pipeline planned on a branch's tip.
    RunManual {
        project: ProjectId,
        pipeline: i64,
        git_ref: String,
        sha: String,
        status: String,
    },
}

impl PipelineChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Canceled { .. } => "pipeline.canceled",
            Self::Retried { .. } => "pipeline.retried",
            Self::RunManual { .. } => "pipeline.run_manual",
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Canceled {
                project,
                pipeline,
                jobs,
            }
            | Self::Retried {
                project,
                pipeline,
                jobs,
            } => serde_json::json!({
                "project_id": project.get(),
                "pipeline_id": pipeline,
                "jobs": jobs,
            }),
            Self::RunManual {
                project,
                pipeline,
                git_ref,
                sha,
                status,
            } => serde_json::json!({
                "project_id": project.get(),
                "pipeline_id": pipeline,
                "ref": git_ref,
                "sha": sha,
                "status": status,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_change_names_its_action_and_shape() {
        let p = ProjectId::new(7);
        let c = PipelineChange::Canceled {
            project: p,
            pipeline: 3,
            jobs: 2,
        };
        assert_eq!(c.action(), "pipeline.canceled");
        assert_eq!(
            c.to_value(),
            serde_json::json!({"project_id": 7, "pipeline_id": 3, "jobs": 2})
        );
        let r = PipelineChange::Retried {
            project: p,
            pipeline: 3,
            jobs: 1,
        };
        assert_eq!(r.action(), "pipeline.retried");
        let m = PipelineChange::RunManual {
            project: p,
            pipeline: 4,
            git_ref: "refs/heads/main".to_owned(),
            sha: "b".repeat(40),
            status: "pending".to_owned(),
        };
        assert_eq!(m.action(), "pipeline.run_manual");
        assert_eq!(m.to_value()["ref"], "refs/heads/main");
    }
}
