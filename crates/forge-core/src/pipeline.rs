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
    /// `pipeline.run_merge_request` (plan 18 S5): a person asked for the
    /// merge request pipeline on the current target tip — `outcome`
    /// `queued` (its job was enqueued on the merge result `sha` of
    /// `target_sha`) or `computing` (no merge result for the tip yet: the
    /// check `check_seq` was requested and the pipeline follows it).
    MergeRequestRun {
        project: ProjectId,
        merge_request: i64,
        iid: i64,
        outcome: &'static str,
        sha: Option<String>,
        target_sha: Option<String>,
        check_seq: Option<i64>,
    },
    /// `pipeline.job_played` (M4 C12): a person started a `when: manual`
    /// job.
    JobPlayed {
        project: ProjectId,
        pipeline: i64,
        job: i64,
        name: String,
    },
    /// `pipeline_schedule.created` / `.updated` (M4 C12): a Maintainer
    /// saved a schedule and became its owner (the person whose rights it
    /// runs with).
    ScheduleSaved {
        project: ProjectId,
        schedule: i64,
        created: bool,
        git_ref: String,
        cron: String,
        utc_offset_minutes: i32,
        active: bool,
        owner: i64,
    },
    /// `pipeline_schedule.deleted` (M4 C12).
    ScheduleDeleted { project: ProjectId, schedule: i64 },
    /// `pipeline_schedule.deactivated` (M4 C12 review): the worker (actor
    /// `system`) stopped a schedule whose owner may no longer run
    /// pipelines (`cause_code`: the refusal's code).
    ScheduleDeactivated {
        project: ProjectId,
        schedule: i64,
        owner: i64,
        cause_code: String,
    },
    /// `pipeline.scheduled` (M4 C12): the worker (actor `system`) started a
    /// schedule's pipeline after deciding `PipelineCreate` for its owner.
    Scheduled {
        project: ProjectId,
        pipeline: i64,
        schedule: i64,
        owner: i64,
        git_ref: String,
        sha: String,
        status: String,
    },
    /// `spec.released` (M4 C12, plan 04 §6 `spec-publish`): a version of
    /// the repository's specification was published (actor `system`; the
    /// pusher of the tag is `released_by`). Also the event's payload.
    SpecReleased {
        project: ProjectId,
        repository: i64,
        tag: String,
        version: String,
        sha: String,
        released_by: Option<i64>,
        default_branch: String,
    },
}

impl PipelineChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Canceled { .. } => "pipeline.canceled",
            Self::Retried { .. } => "pipeline.retried",
            Self::RunManual { .. } => "pipeline.run_manual",
            Self::MergeRequestRun { .. } => "pipeline.run_merge_request",
            Self::JobPlayed { .. } => "pipeline.job_played",
            Self::ScheduleSaved { created: true, .. } => "pipeline_schedule.created",
            Self::ScheduleSaved { created: false, .. } => "pipeline_schedule.updated",
            Self::ScheduleDeleted { .. } => "pipeline_schedule.deleted",
            Self::ScheduleDeactivated { .. } => "pipeline_schedule.deactivated",
            Self::Scheduled { .. } => "pipeline.scheduled",
            Self::SpecReleased { .. } => "spec.released",
        }
    }

    /// The event the change publishes (M4 C12: only a release).
    pub fn event_type(&self) -> Option<crate::EventType> {
        match self {
            Self::SpecReleased { .. } => Some(crate::EventType::SpecReleased),
            Self::Canceled { .. }
            | Self::Retried { .. }
            | Self::RunManual { .. }
            | Self::MergeRequestRun { .. }
            | Self::JobPlayed { .. }
            | Self::ScheduleSaved { .. }
            | Self::ScheduleDeleted { .. }
            | Self::ScheduleDeactivated { .. }
            | Self::Scheduled { .. } => None,
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
            Self::MergeRequestRun {
                project,
                merge_request,
                iid,
                outcome,
                sha,
                target_sha,
                check_seq,
            } => serde_json::json!({
                "project_id": project.get(),
                "merge_request_id": merge_request,
                "iid": iid,
                "outcome": outcome,
                "sha": sha,
                "target_sha": target_sha,
                "check_seq": check_seq,
            }),
            Self::JobPlayed {
                project,
                pipeline,
                job,
                name,
            } => serde_json::json!({
                "project_id": project.get(),
                "pipeline_id": pipeline,
                "job_id": job,
                "name": name,
            }),
            Self::ScheduleSaved {
                project,
                schedule,
                git_ref,
                cron,
                utc_offset_minutes,
                active,
                owner,
                ..
            } => serde_json::json!({
                "project_id": project.get(),
                "schedule_id": schedule,
                "ref": git_ref,
                "cron": cron,
                "utc_offset_minutes": utc_offset_minutes,
                "active": active,
                "owner_id": owner,
            }),
            Self::ScheduleDeleted { project, schedule } => serde_json::json!({
                "project_id": project.get(),
                "schedule_id": schedule,
            }),
            Self::ScheduleDeactivated {
                project,
                schedule,
                owner,
                cause_code,
            } => serde_json::json!({
                "project_id": project.get(),
                "schedule_id": schedule,
                "owner_id": owner,
                "reason": "owner_refused",
                "cause_code": cause_code,
            }),
            Self::Scheduled {
                project,
                pipeline,
                schedule,
                owner,
                git_ref,
                sha,
                status,
            } => serde_json::json!({
                "project_id": project.get(),
                "pipeline_id": pipeline,
                "schedule_id": schedule,
                "owner_id": owner,
                "ref": git_ref,
                "sha": sha,
                "status": status,
            }),
            Self::SpecReleased {
                project,
                repository,
                tag,
                version,
                sha,
                released_by,
                default_branch,
            } => serde_json::json!({
                "project_id": project.get(),
                "repository_id": repository,
                "tag": tag,
                "version": version,
                "sha": sha,
                "released_by": released_by,
                "default_branch": default_branch,
            }),
        }
    }
}

/// An administrator's change to a CI runner (M4 C3): audited on the runner
/// (`resource_type = ci_runner`), no event. Never the token or its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerChange {
    /// `ci_runner.registered`: a runner and its token were created.
    Registered {
        runner: i64,
        name: String,
        tags: Vec<String>,
    },
    /// `ci_runner.updated`: what changed (`None`: unchanged).
    Updated {
        runner: i64,
        enabled: Option<bool>,
        tags: Option<Vec<String>>,
        description: bool,
    },
}

impl RunnerChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Registered { .. } => "ci_runner.registered",
            Self::Updated { .. } => "ci_runner.updated",
        }
    }

    pub fn runner(&self) -> i64 {
        match self {
            Self::Registered { runner, .. } | Self::Updated { runner, .. } => *runner,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Registered { runner, name, tags } => serde_json::json!({
                "runner_id": runner, "name": name, "tags": tags,
            }),
            Self::Updated {
                runner,
                enabled,
                tags,
                description,
            } => serde_json::json!({
                "runner_id": runner,
                "enabled": enabled,
                "tags": tags,
                "description_changed": description,
            }),
        }
    }
}

/// A Maintainer's change to a CI variable (M4 C6): audited on the project
/// (`resource_type = project`), no event. Never the value or its
/// ciphertext: the name, the flags and the key the value was sealed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiVariableChange {
    /// `ci_variable.created` / `ci_variable.updated`: a value was set.
    Set {
        project: ProjectId,
        name: String,
        created: bool,
        protected_only: bool,
        masked: bool,
        key_id: i16,
    },
    /// `ci_variable.deleted`.
    Deleted { project: ProjectId, name: String },
}

impl CiVariableChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Set { created: true, .. } => "ci_variable.created",
            Self::Set { created: false, .. } => "ci_variable.updated",
            Self::Deleted { .. } => "ci_variable.deleted",
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::Set { project, .. } | Self::Deleted { project, .. } => *project,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Set {
                project,
                name,
                protected_only,
                masked,
                key_id,
                ..
            } => serde_json::json!({
                "project_id": project.get(),
                "name": name,
                "protected_only": protected_only,
                "masked": masked,
                "key_id": key_id,
            }),
            Self::Deleted { project, name } => serde_json::json!({
                "project_id": project.get(),
                "name": name,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variable_change_never_carries_a_value() {
        let set = CiVariableChange::Set {
            project: ProjectId::new(7),
            name: "TOKEN".to_owned(),
            created: true,
            protected_only: true,
            masked: true,
            key_id: 2,
        };
        assert_eq!(set.action(), "ci_variable.created");
        assert_eq!(
            set.to_value(),
            serde_json::json!({"project_id": 7, "name": "TOKEN", "protected_only": true,
                "masked": true, "key_id": 2})
        );
        let deleted = CiVariableChange::Deleted {
            project: ProjectId::new(7),
            name: "TOKEN".to_owned(),
        };
        assert_eq!(deleted.action(), "ci_variable.deleted");
        assert_eq!(deleted.project(), ProjectId::new(7));
    }

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
        let asked = PipelineChange::MergeRequestRun {
            project: p,
            merge_request: 12,
            iid: 3,
            outcome: "queued",
            sha: Some("c".repeat(40)),
            target_sha: Some("d".repeat(40)),
            check_seq: None,
        };
        assert_eq!(asked.action(), "pipeline.run_merge_request");
        assert_eq!(asked.event_type(), None);
        assert_eq!(asked.to_value()["outcome"], "queued");
        assert_eq!(asked.to_value()["check_seq"], serde_json::Value::Null);
        let played = PipelineChange::JobPlayed {
            project: p,
            pipeline: 4,
            job: 9,
            name: "deploy".to_owned(),
        };
        assert_eq!(played.action(), "pipeline.job_played");
        assert_eq!(
            played.to_value(),
            serde_json::json!({"project_id": 7, "pipeline_id": 4, "job_id": 9, "name": "deploy"})
        );
        let saved = |created| PipelineChange::ScheduleSaved {
            project: p,
            schedule: 2,
            created,
            git_ref: "refs/heads/main".to_owned(),
            cron: "0 2 * * *".to_owned(),
            utc_offset_minutes: 540,
            active: true,
            owner: 11,
        };
        assert_eq!(saved(true).action(), "pipeline_schedule.created");
        assert_eq!(saved(false).action(), "pipeline_schedule.updated");
        assert_eq!(saved(true).to_value()["owner_id"], 11);
        let deleted = PipelineChange::ScheduleDeleted {
            project: p,
            schedule: 2,
        };
        assert_eq!(deleted.action(), "pipeline_schedule.deleted");
        let fired = PipelineChange::Scheduled {
            project: p,
            pipeline: 5,
            schedule: 2,
            owner: 11,
            git_ref: "refs/heads/main".to_owned(),
            sha: "c".repeat(40),
            status: "pending".to_owned(),
        };
        assert_eq!(fired.action(), "pipeline.scheduled");
        assert_eq!(fired.to_value()["schedule_id"], 2);
    }

    #[test]
    fn a_runner_change_names_its_action_and_never_a_token() {
        let r = RunnerChange::Registered {
            runner: 3,
            name: "linux-1".to_owned(),
            tags: vec!["podman".to_owned()],
        };
        assert_eq!(r.action(), "ci_runner.registered");
        assert_eq!(
            r.to_value(),
            serde_json::json!({"runner_id": 3, "name": "linux-1", "tags": ["podman"]})
        );
        let u = RunnerChange::Updated {
            runner: 3,
            enabled: Some(false),
            tags: None,
            description: false,
        };
        assert_eq!(u.action(), "ci_runner.updated");
        assert_eq!(u.runner(), 3);
        assert_eq!(u.to_value()["enabled"], false);
    }
}
