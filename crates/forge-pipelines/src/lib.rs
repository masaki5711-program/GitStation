//! CI pipelines (M4 C2, plan 14 §5 C2): a push to a branch, a merge
//! request version or a person's manual run becomes one pipeline with its
//! jobs — planned from `.forge-ci.yml` at the commit (the pure `forge-ci`)
//! and written by `forge_db::pipelines` in one transaction.
//!
//! [`Pipelines::plan_and_record`] (the `ci.pipeline_create` job and the
//! manual run):
//! 1. the project (gone: nothing) and its ready repository
//!    (`FG-REPO-0016` while provisioning: the job retries);
//! 2. `.forge-ci.yml` at the commit through the browse API (bounded bytes,
//!    the browse timeout). No entry: **no pipeline** ([`Outcome::NoDefinition`]);
//! 3. the project's protected branch patterns
//!    (`forge_db::protected_refs::rules_for`, [`branch_patterns`]) into
//!    `LintContext::with_protected_branches` — never the local check;
//! 4. `forge_ci::check`, then `forge_ci::plan` with
//!    `Trigger { protected }`: a push to a branch some rule protects is
//!    protected; a merge request's pipeline and a manual run never are, so
//!    a selected job with secrets makes it `invalid`
//!    (`secret_on_unprotected_ref`, never dropped silently);
//! 5. the confidential marks at the commit ([`MarkMeter`], the caller's:
//!    forge-cli measures with `forge_checks::confidential`, the rule of
//!    FG-AUTH-0022 — ADR-0029 decision 5);
//! 6. one write: `pending` with its jobs, `skipped` (no job selected) or
//!    `invalid` (`FG-CI-0001` with the problems). A second write of the
//!    same push or version is a no-op (`existing`).
//!
//! A failure that is not the definition's (git, database) is returned; the
//! job retries it and, on its last attempt, [`Pipelines::fail`] records the
//! pipeline as `error` (`FG-CI-0002`) so a push never stays unanswered.
//!
//! The reads and changes on a route's proof: [`Pipelines::list`] /
//! [`Pipelines::get`] (`PipelineRead`), [`Pipelines::cancel`]
//! (`PipelineCancel`), [`Pipelines::retry`] (`PipelineRetry`),
//! [`Pipelines::run_manual`] (`PipelineCreate`). The tables are derived
//! state (no audit row): who did what is the structured log line
//! (`forge.ci.pipeline_cancelled`, `…_retried`, `…_created`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use forge_auth::{AccountFacts, Action, Authorized, LoadedProject, Principal, ProjectCtx};
use forge_ci::{Event, LintContext, Problem, Trigger};
use forge_core::{
    ObjectId, ProjectAccessLoader, ProjectId, ProtectionRules, RefName, RepoId, RepositoryState,
    RequestId, UserId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::pipelines as db;
use forge_db::projects::ProjectAccessFacts;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RefLookup, RepoPath, SmallFile};

pub use forge_db::pipelines::{
    create_payload, CopyMark, Created, JobStatus, PipelineRequest, PipelineStatus, Source,
    StoredJob, StoredPipeline, StoredProblem, MAX_PAGE, PIPELINE_CREATE_JOB,
};

/// The pipeline definition's path at the root of a commit.
pub const CI_FILE: &str = ".forge-ci.yml";
/// The project lookup's bound.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
/// The protected ref prefix of a branch.
const HEADS: &str = "refs/heads/";

/// What [`MarkMeter::measure`] returns.
pub type MarkFuture<'a> = Pin<Box<dyn Future<Output = CopyMark> + Send + 'a>>;

/// Measures the confidential marks at a commit (ADR-0029 decision 5). The
/// implementation logs its own failure and answers
/// [`CopyMark::Undecided`] then (fail closed: C5 refuses an agent the
/// outputs of an undecided pipeline).
pub trait MarkMeter: Send + Sync {
    fn measure<'a>(&'a self, repo: RepoId, commit: &'a ObjectId) -> MarkFuture<'a>;
}

/// What [`Pipelines::plan_and_record`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No `.forge-ci.yml` at the commit: no pipeline.
    NoDefinition,
    /// The project is gone (purged): nothing to record.
    ProjectGone,
    /// The pipeline was written (or already existed: a redelivery).
    Recorded {
        created: Created,
        status: PipelineStatus,
        jobs: usize,
    },
}

/// The protected branch patterns forge-ci can compare, from the project's
/// rules: `refs/heads/<pattern>` without the prefix, as written and
/// case-folded (a rule matches ignoring case). Tag rules protect no branch.
/// A `*` anywhere but at the end is left out: forge-ci compares only a
/// trailing `*`, so those branches count as unprotected (a false alarm,
/// never a secret let through). The same mapping as `forge/ci-lint`
/// (forge-checks `ci_lint::branch_patterns`; feature crates do not depend
/// on each other, so it is repeated here).
pub fn branch_patterns(rules: &ProtectionRules) -> Vec<String> {
    branch_patterns_of(rules)
}

/// Is `request` a push to a branch that is protected now? The one answer
/// both the plan (`Trigger::protected`: may a job name secrets) and the
/// stored row (`ci_pipelines.protected`: may they be released, ADR-0029
/// obligation of C3/C6) are made from. A merge request's pipeline and a
/// manual or scheduled run never, whatever the branch (C12).
fn protected_push(request: &PipelineRequest, rules: &ProtectionRules) -> bool {
    request.source == Source::Push && !rules.levels(&request.git_ref).is_empty()
}

fn branch_patterns_of(rules: &ProtectionRules) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rule in rules.rules() {
        for raw in [rule.pattern.as_str(), rule.pattern.key()] {
            let Some(branch) = raw.strip_prefix(HEADS) else {
                continue;
            };
            let star_inside = branch.find('*').is_some_and(|at| at + 1 != branch.len());
            if branch.is_empty() || star_inside || out.iter().any(|p| p == branch) {
                continue;
            }
            out.push(branch.to_owned());
        }
    }
    out
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

fn caller(principal: &Principal, action: Action) -> Result<UserId, ForgeError> {
    principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", action.name())
    })
}

fn stored(problems: &[Problem]) -> Vec<StoredProblem> {
    problems
        .iter()
        .take(db::MAX_STORED_PROBLEMS)
        .map(|p| StoredProblem {
            line: i64::try_from(p.line).unwrap_or(i64::MAX),
            reason: p.reason.to_owned(),
            subject: p.subject.clone(), // clone: the row owns it
        })
        .collect()
}

/// The jobs of a plan as the table stores them.
fn jobs_of(plan: &forge_ci::Plan) -> Result<Vec<db::NewCiJob>, ForgeError> {
    plan.jobs
        .iter()
        .map(|j| {
            Ok(db::NewCiJob {
                name: j.name.clone(),   // clone: the row owns it
                stage: j.stage.clone(), // clone: as name
                wave: i32::try_from(j.wave).map_err(|_| internal("wave_out_of_range"))?,
                needs: j.after.clone(), // clone: as name
                image: j.image.clone(), // clone: as name
                timeout_s: i32::try_from(j.timeout_secs)
                    .map_err(|_| internal("timeout_out_of_range"))?,
                tags: j.tags.clone(),       // clone: as name
                secrets: j.secrets.clone(), // clone: as name
            })
        })
        .collect()
}

/// One pipeline with its jobs (by wave, stage, name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineDetail {
    pub pipeline: StoredPipeline,
    pub jobs: Vec<StoredJob>,
}

/// A manual run's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualRun {
    /// No `.forge-ci.yml` at the branch's tip.
    NoDefinition,
    /// The pipeline (any status: `pending`, `skipped`, `invalid`).
    Recorded(Box<PipelineDetail>),
}

/// The pipeline as read back after a change of this request.
async fn read_back(
    db_handle: &Db,
    project: ProjectId,
    pipeline: i64,
) -> Result<PipelineDetail, ForgeError> {
    match db::get(db_handle, project, pipeline).await? {
        Some((pipeline, jobs)) => Ok(PipelineDetail { pipeline, jobs }),
        // Only the project purge deletes a pipeline.
        None => Err(internal("written_pipeline_missing").with_context("pipeline_id", pipeline)),
    }
}

/// The pipelines' service (`forge serve` and `forge worker` build one).
#[derive(Clone)]
pub struct Pipelines {
    db: Db,
    browse: Browser,
    marks: Arc<dyn MarkMeter>,
    /// `ci.secret_keys_file` (M4 C6); `None`: not configured (no variable
    /// can be set, a job that asks for secrets fails with FG-CI-0016).
    secret_keys: Option<Arc<forge_core::ci_secret::CiSecretKeys>>,
    /// Where the CI artifacts' files live (M4 C6, `forge serve`).
    artifacts: Option<artifacts::ArtifactStore>,
}

impl std::fmt::Debug for Pipelines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipelines").finish_non_exhaustive()
    }
}

impl Pipelines {
    pub fn new(db: Db, browse: Browser, marks: Arc<dyn MarkMeter>) -> Self {
        Self {
            db,
            browse,
            marks,
            secret_keys: None,
            artifacts: None,
        }
    }

    /// The CI artifacts' store under the storage root (M4 C6).
    #[must_use]
    pub fn with_artifacts(mut self, store: artifacts::ArtifactStore) -> Self {
        self.artifacts = Some(store);
        self
    }

    /// The keys of `ci.secret_keys_file` (M4 C6, `forge serve`).
    #[must_use]
    pub fn with_secret_keys(mut self, keys: Arc<forge_core::ci_secret::CiSecretKeys>) -> Self {
        self.secret_keys = Some(keys);
        self
    }

    /// Plan `request` and write the pipeline (module docs). Errors (nothing
    /// written): git's (`FG-GIT-…`, with `commit`), the database's
    /// (`FG-DB-…`), `FG-REPO-0016` (the repository is not ready),
    /// `FG-WEB-0003`-like `RequestTimeout` for the project lookup.
    pub async fn plan_and_record(
        &self,
        request: &PipelineRequest,
        request_id: &RequestId,
    ) -> Result<Outcome, ForgeError> {
        let new = match self.prepare(request, request_id).await? {
            Ok(new) => new,
            Err(outcome) => return Ok(outcome),
        };
        let created = db::create(&self.db, &new).await?;
        let (status, jobs) = status_of(&new.planned);
        Self::log_recorded(&new, created, status, jobs);
        Ok(Outcome::Recorded {
            created,
            status,
            jobs,
        })
    }

    /// Everything before the write: the pipeline to write, or the outcome
    /// that writes nothing (`NoDefinition`, `ProjectGone`).
    async fn prepare(
        &self,
        request: &PipelineRequest,
        request_id: &RequestId,
    ) -> Result<Result<db::NewPipeline, Outcome>, ForgeError> {
        let project = request.project;
        let loaded =
            match tokio::time::timeout(LOOKUP_TIMEOUT, self.db.load_project(project, None)).await {
                Ok(loaded) => loaded?,
                Err(_) => {
                    return Err(ForgeError::new(ErrorCode::RequestTimeout)
                        .with_context("operation", "project_load")
                        .with_context("timeout_ms", LOOKUP_TIMEOUT.as_millis()))
                }
            };
        let Some(record) = loaded else {
            tracing::info!(
                target: "forge_pipelines",
                event = "forge.ci.pipeline_project_gone",
                request_id = %request_id,
                project_id = project.get(),
                "the project is gone; no pipeline"
            );
            return Ok(Err(Outcome::ProjectGone));
        };
        let repo = record.repository;
        if repo.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", repo.id.get())
                .with_context("project_id", project.get())
                .with_context("state", repo.state.as_str()));
        }
        let path = RepoPath::parse(CI_FILE)
            .map_err(|rule| internal("ci_file_path_invalid").with_context("rule", rule))?;
        let max = u64::try_from(forge_ci::MAX_BYTES).unwrap_or(u64::MAX);
        let file = self
            .browse
            .small_file(repo.id, request.sha.as_str(), &path, max)
            .await
            .map_err(|e| e.with_context("commit", request.sha.as_str().to_owned()))?;
        let (bytes, unreadable) = match file {
            SmallFile::Absent => {
                tracing::info!(
                    target: "forge_pipelines",
                    event = "forge.ci.no_definition",
                    request_id = %request_id,
                    project_id = project.get(),
                    git_ref = request.git_ref.as_str(),
                    sha = request.sha.as_str(),
                    source = request.source.as_str(),
                    "no .forge-ci.yml at the commit; no pipeline"
                );
                return Ok(Err(Outcome::NoDefinition));
            }
            SmallFile::NotAFile => (Vec::new(), Some("not_a_file")),
            SmallFile::TooLarge => (Vec::new(), Some("too_large")),
            SmallFile::Found(bytes) => (bytes, None),
        };
        let rules = forge_db::protected_refs::rules_for(&self.db, project).await?;
        let planned = match unreadable {
            Some(reason) => db::Planned::Invalid(vec![StoredProblem {
                line: 0,
                reason: reason.to_owned(),
                subject: None,
            }]),
            None => Self::plan(request, &bytes, &rules)?,
        };
        let copy_mark = self.marks.measure(repo.id, &request.sha).await;
        Ok(Ok(db::NewPipeline {
            request: request.clone(), // clone: the row owns it
            request_id: request_id.to_string(),
            definition_hash: unreadable.is_none().then(|| db::definition_hash(&bytes)),
            protected: protected_push(request, &rules),
            copy_mark,
            planned,
        }))
    }

    /// Lint and plan `bytes` for `request` (pure).
    fn plan(
        request: &PipelineRequest,
        bytes: &[u8],
        rules: &ProtectionRules,
    ) -> Result<db::Planned, ForgeError> {
        let ctx = LintContext::with_protected_branches(branch_patterns(rules));
        let def = match forge_ci::check(bytes, &ctx) {
            Ok(def) => def,
            Err(problems) => return Ok(db::Planned::Invalid(stored(&problems))),
        };
        let branch = request
            .git_ref
            .as_str()
            .strip_prefix(HEADS)
            .ok_or_else(|| internal("not_a_branch"))?;
        let trigger = Trigger {
            event: match request.source {
                Source::MergeRequest => Event::MergeRequest,
                Source::Push | Source::Manual | Source::Schedule => Event::Push,
            },
            branch: branch.to_owned(),
            protected: protected_push(request, rules),
        };
        match forge_ci::plan(&def, &trigger) {
            Ok(plan) => Ok(db::Planned::Jobs(jobs_of(&plan)?)),
            Err(problems) => Ok(db::Planned::Invalid(stored(&problems))),
        }
    }

    fn log_recorded(new: &db::NewPipeline, created: Created, status: PipelineStatus, jobs: usize) {
        let r = &new.request;
        if let db::Planned::Invalid(problems) = &new.planned {
            let first = problems.first();
            tracing::info!(
                target: "forge_pipelines",
                event = "forge.ci.pipeline_created",
                error_code = ErrorCode::CiConfigInvalid.id(),
                request_id = %new.request_id,
                project_id = r.project.get(),
                pipeline_id = created.pipeline_id,
                existing = created.existing,
                status = status.as_str(),
                git_ref = r.git_ref.as_str(),
                sha = r.sha.as_str(),
                source = r.source.as_str(),
                reason = first.map_or("", |p| p.reason.as_str()),
                line = first.map_or(0, |p| p.line),
                problems = problems.len(),
                "the pipeline definition is invalid; recorded as invalid"
            );
            return;
        }
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.pipeline_created",
            request_id = %new.request_id,
            project_id = r.project.get(),
            pipeline_id = created.pipeline_id,
            existing = created.existing,
            status = status.as_str(),
            jobs,
            copy_mark = new.copy_mark.as_str(),
            git_ref = r.git_ref.as_str(),
            sha = r.sha.as_str(),
            source = r.source.as_str(),
            "pipeline recorded"
        );
    }

    /// The last attempt failed for good: record the pipeline as `error`
    /// (`FG-CI-0002`, logged once at warn with `cause_code`) so the push
    /// is answered. A write that fails is returned (and logged with the
    /// cause): the job must not end as if the push had been answered — it
    /// dies with that code, which doctor's `ci.pipelines` and `jobs.dead`
    /// show.
    pub async fn fail(
        &self,
        request: &PipelineRequest,
        request_id: &RequestId,
        cause: &ForgeError,
    ) -> Result<(), ForgeError> {
        let new = db::NewPipeline {
            request: request.clone(), // clone: the row owns it
            request_id: request_id.to_string(),
            definition_hash: None,
            // Nothing was read, the rules included: never protected.
            protected: false,
            copy_mark: CopyMark::Undecided,
            planned: db::Planned::Error,
        };
        let written = db::create(&self.db, &new).await;
        let err = ForgeError::new(ErrorCode::PipelineCreateFailed)
            .with_context("cause_code", cause.code().id());
        match written {
            Ok(created) => {
                tracing::warn!(
                target: "forge_pipelines",
                event = "forge.ci.pipeline_create_failed",
                error_code = err.code().id(),
                cause_code = cause.code().id(),
                cause = ?cause.context(),
                request_id = %request_id,
                project_id = request.project.get(),
                pipeline_id = created.pipeline_id,
                existing = created.existing,
                git_ref = request.git_ref.as_str(),
                sha = request.sha.as_str(),
                source = request.source.as_str(),
                "the pipeline could not be planned; recorded as error"
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                target: "forge_pipelines",
                event = "forge.ci.pipeline_create_failed",
                error_code = e.code().id(),
                cause_code = cause.code().id(),
                request_id = %request_id,
                project_id = request.project.get(),
                git_ref = request.git_ref.as_str(),
                sha = request.sha.as_str(),
                source = request.source.as_str(),
                "the pipeline could not be planned, and its error row could not be written"
                );
                Err(e.with_context("cause_code", cause.code().id()))
            }
        }
    }

    /// `GET …/pipelines` (`PipelineRead`): newest first, at most `limit`
    /// (1..=[`MAX_PAGE`]) below `before`, optionally of one branch.
    pub async fn list(
        &self,
        proof: &Authorized<ProjectCtx>,
        git_ref: Option<&RefName>,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<StoredPipeline>, ForgeError> {
        require(proof.action(), Action::PipelineRead)?;
        db::list(
            &self.db,
            *proof.id(),
            git_ref.map(RefName::as_str),
            before,
            limit,
        )
        .await
    }

    /// `GET …/pipelines/{pipeline_id}` (`PipelineRead`): `None` when the
    /// project has no such pipeline.
    pub async fn get(
        &self,
        proof: &Authorized<ProjectCtx>,
        pipeline: i64,
    ) -> Result<Option<PipelineDetail>, ForgeError> {
        require(proof.action(), Action::PipelineRead)?;
        Ok(db::get(&self.db, *proof.id(), pipeline)
            .await?
            .map(|(pipeline, jobs)| PipelineDetail { pipeline, jobs }))
    }

    /// `POST …/cancel` (`PipelineCancel`): the pipeline after the change,
    /// with one `pipeline.canceled` audit row (decided again in the
    /// transaction). `FG-REPO-0005 resource=pipeline` when there is no
    /// such pipeline; `FG-CI-0003` when it is finished.
    pub async fn cancel(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        pipeline: i64,
    ) -> Result<PipelineDetail, ForgeError> {
        require(proof.action(), Action::PipelineCancel)?;
        let user = caller(proof.principal(), Action::PipelineCancel)?;
        let ctx = change_context(request, proof.principal(), user);
        db::cancel_pipeline_decided(&self.db, &ctx, (*proof.id(), pipeline), user, |facts| {
            reconfirm(proof, facts)
        })
        .await?;
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.pipeline_cancelled",
            request_id = %request.request_id,
            project_id = proof.id().get(),
            pipeline_id = pipeline,
            user_id = user.get(),
            "pipeline cancelled"
        );
        read_back(&self.db, *proof.id(), pipeline).await
    }

    /// `POST …/retry` (`PipelineRetry`): the pipeline after the change,
    /// with one `pipeline.retried` audit row. `FG-REPO-0005
    /// resource=pipeline` when there is no such pipeline; `FG-CI-0003`
    /// unless it is `failed` or `canceled`.
    pub async fn retry(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        pipeline: i64,
    ) -> Result<PipelineDetail, ForgeError> {
        require(proof.action(), Action::PipelineRetry)?;
        let user = caller(proof.principal(), Action::PipelineRetry)?;
        let ctx = change_context(request, proof.principal(), user);
        db::retry_pipeline_decided(&self.db, &ctx, (*proof.id(), pipeline), user, |facts| {
            reconfirm(proof, facts)
        })
        .await?;
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.pipeline_retried",
            request_id = %request.request_id,
            project_id = proof.id().get(),
            pipeline_id = pipeline,
            user_id = user.get(),
            "pipeline retried"
        );
        read_back(&self.db, *proof.id(), pipeline).await
    }

    /// `POST …/pipelines` (`PipelineCreate`): plan the tip of `branch`
    /// now. Errors: `FG-REPO-0023 reason=ref_not_found` (no such branch),
    /// `FG-REPO-0016` (not ready), `FG-CI-0002` (503, with `cause_code`:
    /// the definition or the protected branches could not be read; no row).
    pub async fn run_manual(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        branch: &RefName,
    ) -> Result<ManualRun, ForgeError> {
        let request_id = &request.request_id;
        require(proof.action(), Action::PipelineCreate)?;
        let user = caller(proof.principal(), Action::PipelineCreate)?;
        let project: ProjectId = *proof.id();
        if !branch.as_str().starts_with(HEADS) {
            return Err(not_found_ref());
        }
        let repo = self.ready_repo(project).await?;
        let tip = match self.browse.find_ref(repo, branch).await {
            Ok(RefLookup::Commit(rev)) => rev.commit,
            Ok(RefLookup::Missing | RefLookup::NotACommit) => return Err(not_found_ref()),
            Err(e) => return Err(failed(e)),
        };
        let sha = ObjectId::parse(&tip).map_err(|_| internal("invalid_commit_id"))?;
        let planned = PipelineRequest {
            project,
            git_ref: branch.clone(), // clone: the request owns it
            sha,
            source: Source::Manual,
            merge_request: None,
            triggered_by: Some(user),
        };
        let new = match self.prepare(&planned, request_id).await {
            Ok(Ok(new)) => new,
            Ok(Err(Outcome::NoDefinition)) => return Ok(ManualRun::NoDefinition),
            // The proof was decided on a live project: it went meanwhile.
            Ok(Err(Outcome::ProjectGone | Outcome::Recorded { .. })) => {
                return Err(ForgeError::new(ErrorCode::NotFound)
                    .with_context("reason", "project_gone")
                    .with_context("project_id", project.get()))
            }
            Err(e) if e.code() == ErrorCode::RepositoryNotReady => return Err(e),
            Err(e) => return Err(failed(e)),
        };
        let ctx = change_context(request, proof.principal(), user);
        let (pipeline, ()) =
            db::run_manual_decided(&self.db, &ctx, &new, user, |facts| reconfirm(proof, facts))
                .await?;
        let (status, jobs) = status_of(&new.planned);
        Self::log_recorded(
            &new,
            Created {
                pipeline_id: pipeline,
                existing: false,
            },
            status,
            jobs,
        );
        Ok(ManualRun::Recorded(Box::new(
            read_back(&self.db, project, pipeline).await?,
        )))
    }

    async fn ready_repo(&self, project: ProjectId) -> Result<RepoId, ForgeError> {
        let loaded =
            match tokio::time::timeout(LOOKUP_TIMEOUT, self.db.load_project(project, None)).await {
                Ok(loaded) => loaded.map_err(failed)?,
                Err(_) => {
                    return Err(failed(
                        ForgeError::new(ErrorCode::RequestTimeout)
                            .with_context("operation", "project_load"),
                    ))
                }
            };
        let Some(record) = loaded else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("reason", "project_gone")
                .with_context("project_id", project.get()));
        };
        if record.repository.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", record.repository.id.get())
                .with_context("project_id", project.get())
                .with_context("state", record.repository.state.as_str()));
        }
        Ok(record.repository.id)
    }
}

/// The request a person's change belongs to: its id and the token it came
/// with (the audit row's `credential_id`).
#[derive(Debug, Clone)]
pub struct ChangeRequest {
    pub request_id: RequestId,
    pub credential_id: Option<i64>,
}

/// The audit context of a person's change: the actor (a service account's
/// own kind), the request, the token and its scopes.
fn change_context(request: &ChangeRequest, principal: &Principal, user: UserId) -> ChangeContext {
    let actor = match principal.account_kind() {
        forge_auth::AccountKind::ServiceAccount => Actor::ServiceAccount(user),
        forge_auth::AccountKind::Human | forge_auth::AccountKind::Bot => Actor::User(user),
    };
    let mut ctx = ChangeContext::new(actor, request.request_id.clone()); // clone: the context owns it
    ctx.credential_id = request.credential_id;
    ctx.effective_scope = principal.scope_names();
    ctx
}

/// The route's decision made again on the facts locked by the change.
fn reconfirm(
    proof: &Authorized<ProjectCtx>,
    facts: &forge_db::projects::ProjectUpdateFacts,
) -> Result<(), ForgeError> {
    let account = AccountFacts {
        active: facts.account.active,
        is_admin: facts.account.is_admin,
    };
    let fresh = ProjectCtx::new(loaded(&facts.project));
    proof.reconfirm(account, fresh).map(drop)
}

fn loaded(facts: &ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}

/// The status and job count a plan is written with.
fn status_of(planned: &db::Planned) -> (PipelineStatus, usize) {
    match planned {
        db::Planned::Jobs(jobs) if jobs.is_empty() => (PipelineStatus::Skipped, 0),
        db::Planned::Jobs(jobs) => (PipelineStatus::Pending, jobs.len()),
        db::Planned::Invalid(_) => (PipelineStatus::Invalid, 0),
        db::Planned::Error => (PipelineStatus::Error, 0),
    }
}

fn not_found_ref() -> ForgeError {
    ForgeError::new(ErrorCode::RepositoryPathNotResolved).with_context("reason", "ref_not_found")
}

/// `FG-CI-0002` for a manual run, with the cause's code and context.
fn failed(cause: ForgeError) -> ForgeError {
    let mut err = ForgeError::new(ErrorCode::PipelineCreateFailed)
        .with_context("cause_code", cause.code().id());
    for (k, v) in cause.context() {
        err = err.with_context(k, v.clone()); // clone: the new error owns it
    }
    err
}

// M4 C3: the runner API's commands.
pub mod runners;
// M4 C6: CI variables (secrets): manage, release with a claim, mask.
pub mod secrets;
// M4 C6: CI artifacts: upload, read on the marks, needs, expiry.
pub mod artifacts;
// M4 C5: the CI outputs (logs, diagnosis, fingerprints, ci/pipeline).
pub mod fingerprint;
pub mod outputs;

#[cfg(test)]
mod tests;
