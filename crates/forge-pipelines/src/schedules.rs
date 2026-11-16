//! Pipeline schedules (M4 C12, plan 14 §5 C12; ADR-0029 Amendment
//! 2026-11-13): a project's pipelines on a timetable.
//!
//! - Managed on a route's proof (`PipelineScheduleManage`: Maintainer and
//!   above, people only): [`Pipelines::create_schedule`],
//!   [`Pipelines::update_schedule`], [`Pipelines::delete_schedule`]; read
//!   with `PipelineRead` ([`Pipelines::schedules`]). Its creator is its
//!   **owner**; an edit of the branch, the rule or the offset (or a save
//!   after the owner was refused) makes the editor the owner, a description
//!   or pause/resume edit does not (M4 C12 review).
//! - Run by `forge worker`: the `ci_schedules` task claims the due ones
//!   (`forge_db::ci_schedules::claim_due`, one `ci.schedule_fire` job per
//!   due time) and the job calls [`Pipelines::fire_schedule`]:
//!   1. the schedule (gone, inactive, or this due time already handled:
//!      nothing);
//!   2. the branch's tip (none: `ref_missing`) and the plan of
//!      `.forge-ci.yml` there, exactly as a push's (`source = schedule`;
//!      never protected, so a job naming secrets makes it `invalid` and no
//!      secret is ever released to it — ADR-0029 obligations 5 and 7);
//!   3. in one transaction: `PipelineCreate` decided for the **owner** as
//!      a signed-in person on the locked project and account
//!      ([`owner_decision`]); refused: nothing is written and the schedule
//!      is stopped (`active = false`, `pipeline_schedule.deactivated` by
//!      `system`, `FG-CI-0026`, warn); still busy (its previous pipeline
//!      pending or running) or past the hourly cap (12 per project, 120 on
//!      the instance: `FG-CI-0028`): nothing is written, `busy` / `capped`
//!      recorded; allowed: the pipeline, the schedule's last run and one
//!      `pipeline.scheduled` audit row.
//!
//! A schedule therefore never runs with more rights than its owner has at
//! that moment, and never with secrets.

use forge_auth::{prove, AccountKind, Action, Authorized, Credential, Principal, ProjectCtx};
use forge_core::ci_schedule::{check_offset, Rule};
use forge_core::{ObjectId, RefName, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::ci_schedules as db;
use forge_db::projects::ProjectUpdateFacts;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::RefLookup;

pub use forge_db::ci_schedules::{
    fire_payload, FireRequest, LastOutcome, StoredSchedule, MAX_PER_PROJECT, SCHEDULE_FIRE_JOB,
};

use crate::{caller, change_context, internal, loaded, require};
use crate::{ChangeRequest, Outcome, PipelineRequest, Pipelines, Source};

/// The longest description, in bytes.
pub const MAX_DESCRIPTION_BYTES: usize = 255;

/// What a person sends to create or change a schedule (checked by
/// [`ScheduleDraft::check`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDraft {
    pub description: String,
    /// The branch name (`main`) or its full ref (`refs/heads/main`).
    pub branch: String,
    pub cron: String,
    pub utc_offset_minutes: i32,
    pub active: bool,
}

fn param(param: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("param", param)
        .with_context("reason", reason)
}

impl ScheduleDraft {
    /// The stored input, or `FG-WEB-0006` with `param` and `reason`:
    /// `description` (`empty`, `too_long`, `control_character`), `branch`
    /// (`invalid_ref`), `cron` (the rule's reason: `cron_field_count`,
    /// `cron_field_invalid`, `cron_value_out_of_range`, `cron_too_long`,
    /// `too_frequent`, `never_runs`), `utc_offset_minutes`
    /// (`utc_offset_invalid`).
    pub fn check(&self) -> Result<db::ScheduleInput, ForgeError> {
        let description = self.description.trim();
        if description.is_empty() {
            return Err(param("description", "empty"));
        }
        if description.len() > MAX_DESCRIPTION_BYTES {
            return Err(param("description", "too_long"));
        }
        if description.chars().any(char::is_control) {
            return Err(param("description", "control_character"));
        }
        let full = if self.branch.starts_with("refs/heads/") {
            self.branch.clone() // clone: the ref owns its text
        } else {
            format!("refs/heads/{}", self.branch)
        };
        let git_ref = RefName::parse(&full)
            .ok()
            .filter(|r| r.as_str().len() > "refs/heads/".len())
            .ok_or_else(|| param("branch", "invalid_ref"))?;
        let cron = self.cron.split_whitespace().collect::<Vec<_>>().join(" ");
        let rule = Rule::parse(&cron).map_err(|reason| param("cron", reason))?;
        check_offset(self.utc_offset_minutes)
            .map_err(|reason| param("utc_offset_minutes", reason))?;
        // A rule that matches no day (`0 0 31 2 *`) would never run.
        if rule.next_after(0, self.utc_offset_minutes).is_none() {
            return Err(param("cron", "never_runs"));
        }
        Ok(db::ScheduleInput {
            description: description.to_owned(),
            git_ref,
            cron,
            utc_offset_minutes: self.utc_offset_minutes,
            active: self.active,
        })
    }
}

/// `PipelineCreate` for the schedule's owner as a signed-in person, on the
/// project and account as the transaction locked them (the same decision a
/// manual run by that person gets). A refusal is `FG-CI-0026` with the
/// refusal's code (`cause_code`) and context.
pub fn owner_decision(owner: UserId, facts: &ProjectUpdateFacts) -> Result<(), ForgeError> {
    let principal = Principal::authenticated(
        owner,
        facts.account.is_admin,
        facts.account.active,
        Credential::Session,
        AccountKind::Human,
    );
    let ctx = ProjectCtx::new(loaded(&facts.project));
    match prove(&principal, Action::PipelineCreate, ctx) {
        Ok(_) => Ok(()),
        Err(deny) => {
            let mut err = ForgeError::new(ErrorCode::ScheduleOwnerNotAllowed)
                .with_context("cause_code", deny.code().id())
                .with_context("owner_id", owner.get());
            for (k, v) in deny.context() {
                err = err.with_context(k, v.clone()); // clone: the new error owns it
            }
            Err(err)
        }
    }
}

/// What [`Pipelines::fire_schedule`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireResult {
    /// A pipeline was started.
    Started(i64),
    /// Nothing to do (`reason`: `schedule_gone`, `inactive`,
    /// `already_ran`, `owner_changed`, `project_gone`).
    Skipped(&'static str),
    /// The due run started nothing; recorded on the schedule.
    Recorded(LastOutcome),
}

impl Pipelines {
    /// `GET …/pipeline-schedules` (`PipelineRead`): by id.
    pub async fn schedules(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<Vec<StoredSchedule>, ForgeError> {
        require(proof.action(), Action::PipelineRead)?;
        db::list(&self.db, *proof.id()).await
    }

    /// `POST …/pipeline-schedules` (`PipelineScheduleManage`): the caller
    /// owns it. Errors: `FG-WEB-0006` ([`ScheduleDraft::check`]),
    /// `FG-CI-0027`.
    pub async fn create_schedule(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        draft: &ScheduleDraft,
    ) -> Result<StoredSchedule, ForgeError> {
        require(proof.action(), Action::PipelineScheduleManage)?;
        let user = caller(proof.principal(), Action::PipelineScheduleManage)?;
        let input = draft.check()?;
        let ctx = change_context(request, proof.principal(), user);
        let (id, ()) =
            db::create_schedule_decided(&self.db, &ctx, *proof.id(), user, &input, |facts| {
                crate::reconfirm(proof, facts)
            })
            .await?;
        self.log_saved(request, proof, id, user, "created");
        self.schedule_back(proof, id).await
    }

    /// `PUT …/pipeline-schedules/{schedule_id}`: replace its settings; the
    /// caller becomes its owner when the branch, rule or offset changes or
    /// the owner was refused. Errors: as create, `FG-REPO-0005
    /// resource=pipeline_schedule`.
    pub async fn update_schedule(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        schedule: i64,
        draft: &ScheduleDraft,
    ) -> Result<StoredSchedule, ForgeError> {
        require(proof.action(), Action::PipelineScheduleManage)?;
        let user = caller(proof.principal(), Action::PipelineScheduleManage)?;
        let input = draft.check()?;
        let ctx = change_context(request, proof.principal(), user);
        db::update_schedule_decided(
            &self.db,
            &ctx,
            (*proof.id(), schedule),
            user,
            &input,
            |facts| crate::reconfirm(proof, facts),
        )
        .await?;
        self.log_saved(request, proof, schedule, user, "updated");
        self.schedule_back(proof, schedule).await
    }

    /// `DELETE …/pipeline-schedules/{schedule_id}`. Errors: `FG-REPO-0005
    /// resource=pipeline_schedule`.
    pub async fn delete_schedule(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        schedule: i64,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::PipelineScheduleManage)?;
        let user = caller(proof.principal(), Action::PipelineScheduleManage)?;
        let ctx = change_context(request, proof.principal(), user);
        db::delete_schedule_decided(&self.db, &ctx, (*proof.id(), schedule), user, |facts| {
            crate::reconfirm(proof, facts)
        })
        .await?;
        self.log_saved(request, proof, schedule, user, "deleted");
        Ok(())
    }

    fn log_saved(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        schedule: i64,
        user: UserId,
        change: &'static str,
    ) {
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.schedule_changed",
            request_id = %request.request_id,
            project_id = proof.id().get(),
            schedule_id = schedule,
            user_id = user.get(),
            change,
            "pipeline schedule changed"
        );
    }

    async fn schedule_back(
        &self,
        proof: &Authorized<ProjectCtx>,
        schedule: i64,
    ) -> Result<StoredSchedule, ForgeError> {
        db::get(&self.db, *proof.id(), schedule)
            .await?
            .ok_or_else(|| {
                internal("written_schedule_missing").with_context("schedule_id", schedule)
            })
    }

    /// One due time of one schedule (the `ci.schedule_fire` job; module
    /// docs). Errors are the planning's and the database's (the job
    /// retries them, and on its last attempt records `failed` with
    /// [`Pipelines::record_schedule_failure`]).
    pub async fn fire_schedule(
        &self,
        request: &FireRequest,
        request_id: &RequestId,
    ) -> Result<FireResult, ForgeError> {
        let Some(schedule) = db::by_id(&self.db, request.schedule).await? else {
            return Ok(FireResult::Skipped("schedule_gone"));
        };
        if !schedule.active {
            return Ok(FireResult::Skipped("inactive"));
        }
        if schedule
            .last_run_epoch
            .is_some_and(|last| last >= request.due)
        {
            return Ok(FireResult::Skipped("already_ran"));
        }
        let git_ref = RefName::parse(&schedule.git_ref)
            .map_err(|_| internal("stored_schedule_ref_invalid"))?;
        let repo = match self.ready_repo(schedule.project).await {
            Ok(repo) => repo,
            Err(e) if e.code() == ErrorCode::NotFound => {
                return Ok(FireResult::Skipped("project_gone"))
            }
            Err(e) => return Err(e),
        };
        let tip = match self.browse.find_ref(repo, &git_ref).await? {
            RefLookup::Commit(rev) => rev.commit,
            RefLookup::Missing | RefLookup::NotACommit => {
                return self
                    .record(request, request_id, LastOutcome::RefMissing, None)
                    .await
            }
        };
        let sha = ObjectId::parse(&tip).map_err(|_| internal("invalid_commit_id"))?;
        let planned = PipelineRequest {
            project: schedule.project,
            git_ref,
            sha,
            source: Source::Schedule,
            merge_request: None,
            triggered_by: Some(schedule.owner),
        };
        let new = match self.prepare(&planned, request_id).await? {
            Ok(new) => new,
            Err(Outcome::NoDefinition) => {
                return self
                    .record(request, request_id, LastOutcome::NoDefinition, None)
                    .await
            }
            Err(Outcome::ProjectGone | Outcome::Recorded { .. }) => {
                return Ok(FireResult::Skipped("project_gone"))
            }
        };
        let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
        let owner = schedule.owner;
        let fired = db::fire_schedule_decided(&self.db, &ctx, request, owner, &new, |facts| {
            owner_decision(owner, facts)
        })
        .await;
        match fired {
            Ok((db::Fired::Created(pipeline), _)) => {
                tracing::info!(
                    target: "forge_pipelines",
                    event = "forge.ci.schedule_fired",
                    request_id = %request_id,
                    project_id = request.project.get(),
                    schedule_id = request.schedule,
                    owner_id = owner.get(),
                    pipeline_id = pipeline,
                    sha = new.request.sha.as_str(),
                    "scheduled pipeline started"
                );
                Ok(FireResult::Started(pipeline))
            }
            Ok((db::Fired::Skipped(reason), _)) => Ok(FireResult::Skipped(reason)),
            // M4 C12 review: one scheduled pipeline at a time per schedule.
            Ok((db::Fired::Busy, _)) => {
                self.record(request, request_id, LastOutcome::Busy, None)
                    .await
            }
            // M4 C12 review: the hourly caps.
            Ok((db::Fired::Capped(scope, count), _)) => {
                tracing::warn!(
                    target: "forge_pipelines",
                    event = "forge.ci.schedule_capped",
                    error_code = ErrorCode::ScheduleRateLimited.id(),
                    request_id = %request_id,
                    project_id = request.project.get(),
                    schedule_id = request.schedule,
                    scope,
                    count,
                    "scheduled pipelines reached their hourly cap; nothing started"
                );
                db::record_outcome(
                    &self.db,
                    request,
                    LastOutcome::Capped,
                    Some(ErrorCode::ScheduleRateLimited),
                )
                .await?;
                Ok(FireResult::Recorded(LastOutcome::Capped))
            }
            Err(e) if e.code() == ErrorCode::ScheduleOwnerNotAllowed => {
                tracing::warn!(
                    target: "forge_pipelines",
                    event = "forge.ci.schedule_refused",
                    error_code = e.code().id(),
                    request_id = %request_id,
                    project_id = request.project.get(),
                    schedule_id = request.schedule,
                    owner_id = owner.get(),
                    context = ?e.context(),
                    "the schedule's owner may not run pipelines now; nothing started, schedule stopped"
                );
                // M4 C12 review: stop it (audited) so it does not keep
                // asking; a Maintainer's save takes it over.
                let cause = e
                    .context()
                    .iter()
                    .find(|(k, _)| *k == "cause_code")
                    .map(|(_, v)| v.to_string().trim_matches('"').to_owned())
                    .unwrap_or_default();
                let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
                db::deactivate_refused(&self.db, &ctx, request, owner, &cause).await?;
                Ok(FireResult::Recorded(LastOutcome::Refused))
            }
            Err(e) => Err(e),
        }
    }

    async fn record(
        &self,
        request: &FireRequest,
        request_id: &RequestId,
        outcome: LastOutcome,
        code: Option<ErrorCode>,
    ) -> Result<FireResult, ForgeError> {
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.schedule_not_started",
            request_id = %request_id,
            project_id = request.project.get(),
            schedule_id = request.schedule,
            outcome = outcome.as_str(),
            "the scheduled run started no pipeline"
        );
        db::record_outcome(&self.db, request, outcome, code).await?;
        Ok(FireResult::Recorded(outcome))
    }

    /// The job's last attempt failed for good: record `failed` with the
    /// cause's code on the schedule (so its page says why), logged once at
    /// warn. A write that fails is returned.
    pub async fn record_schedule_failure(
        &self,
        request: &FireRequest,
        request_id: &RequestId,
        cause: &ForgeError,
    ) -> Result<(), ForgeError> {
        tracing::warn!(
            target: "forge_pipelines",
            event = "forge.ci.schedule_failed",
            error_code = ErrorCode::PipelineCreateFailed.id(),
            cause_code = cause.code().id(),
            cause = ?cause.context(),
            request_id = %request_id,
            project_id = request.project.get(),
            schedule_id = request.schedule,
            "the scheduled run could not be planned"
        );
        db::record_outcome(&self.db, request, LastOutcome::Failed, Some(cause.code())).await
    }
}

#[cfg(test)]
#[path = "schedules_tests.rs"]
mod tests;
