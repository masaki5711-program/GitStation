//! `forge/spec-drift` (M3 X2, plan 03 §5.4 and §8.1 (b), ADR-0023
//! Amendment X2): whether the specification versions a commit's
//! `.forge/repo.yml` pins are the latest published ones. Deterministic:
//! the same commit and the same tags give the same run.
//!
//! [`SpecDrift::run`] evaluates one `check.spec_drift` job (a merge request
//! version's head): it reads `.forge/repo.yml` at the head
//! ([`crate::spec_refs::read_repo_config`]), follows each `repo:` entry for
//! **every signed-in person** ([`Reader::EveryPerson`]: the run is shown to
//! every reader of the project, so it never tells what one of them may not
//! read) and compares each `version:` pin with the tags of its repository
//! ([`forge_core::spec_version::drift`]). [`assess`] turns that into the
//! run; `forge_db::checks::record_forge_check_run` writes it (no reporter:
//! forge's own worker).
//!
//! Conclusions: `success` — nothing pinned, or every pin is the latest of
//! its series, follows a branch (`track:`) or is no version (nothing to
//! compare); `neutral` — a pin is behind (`FG-REPO-0027`), its version is
//! missing (`FG-REPO-0025`), its repository cannot be followed for every
//! person (`FG-REPO-0026`), its tags could not all be listed, or the file
//! is refused (`FG-REPO-0024`). Never `failure`: drift is a notice (plan
//! 03 §5.4). [`SpecDrift::fail`] records `error` when a job dies for good,
//! so a head is never left without an answer.

use std::sync::Arc;
use std::time::Duration;

use forge_core::checks::{
    CheckAnnotationLevel, CheckConclusion, CheckStatus, MAX_CHECK_SUMMARY_BYTES, SPEC_DRIFT_CHECK,
};
use forge_core::namespace_resolver::{NamespaceResolver, ProjectAccessLoader};
use forge_core::repo_config::{RepoConfigProblem, SpecPin, SpecSource, REPO_CONFIG_PATH};
use forge_core::spec_version::{drift, Drift};
use forge_core::{CheckName, RepositoryState, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::spec_drift::SpecDriftRequest;
use forge_db::checks::{record_forge_check_run, CheckAnnotation, NewCheckRun, Reported};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RefKind};

use crate::spec_refs::{follow, read_repo_config, FollowCtx, Followed, Reader, RepoConfigRead};

/// One lookup of a referenced path.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How one entry of `.forge/repo.yml` stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
    /// `path:` — this repository's own specification.
    Local,
    /// The pin is the latest version of its series.
    Current,
    /// `newer` later versions are published; `latest` is the greatest.
    Behind { latest: String, newer: usize },
    /// `track:` — follows a branch: never behind.
    Tracking,
    /// The pin is no version (`stable`, `v2.0-rc1`): nothing to compare.
    Unversioned,
    /// The repository has more refs than one listing keeps: the latest
    /// cannot be told.
    Undecided,
    /// `FG-REPO-0025`.
    VersionNotFound,
    /// `FG-REPO-0026` (also: not readable by every signed-in person).
    Unavailable,
}

impl EntryState {
    /// The word shown and logged.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Current => "current",
            Self::Behind { .. } => "behind",
            Self::Tracking => "tracking",
            Self::Unversioned => "unversioned",
            Self::Undecided => "undecided",
            Self::VersionNotFound => "version_not_found",
            Self::Unavailable => "unavailable",
        }
    }

    /// Whether this entry makes the run `neutral`.
    fn notable(&self) -> bool {
        match self {
            Self::Local | Self::Current | Self::Tracking | Self::Unversioned => false,
            Self::Behind { .. } | Self::Undecided | Self::VersionNotFound | Self::Unavailable => {
                true
            }
        }
    }

    fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Behind { .. } => Some(ErrorCode::SpecVersionBehind),
            Self::VersionNotFound => Some(ErrorCode::SpecVersionNotFound),
            Self::Unavailable => Some(ErrorCode::SpecRepositoryUnavailable),
            Self::Local | Self::Current | Self::Tracking | Self::Unversioned | Self::Undecided => {
                None
            }
        }
    }
}

/// One entry as evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryResult {
    /// Its line in `.forge/repo.yml` (1-based).
    pub line: usize,
    /// `repo:` or `path:` as written (the file's own text).
    pub written: String,
    /// `version <tag>` / `track <branch>` (`repo:` entries).
    pub pin: Option<(&'static str, String)>,
    pub state: EntryState,
}

/// What the head's `.forge/repo.yml` came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessed {
    Absent,
    Invalid(RepoConfigProblem),
    Entries(Vec<EntryResult>),
}

/// The longest annotation message (the table's CHECK, 4 KiB).
const MAX_MESSAGE_BYTES: usize = 4096;

/// `text` cut to at most `max` bytes on a character boundary, ending with
/// `…` when cut (the written paths are the file's own text and may be long:
/// the run is shortened, never refused).
fn clip(mut text: String, max: usize) -> String {
    const MARK: char = '…';
    if text.len() <= max {
        return text;
    }
    let mut end = max.saturating_sub(MARK.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push(MARK);
    text
}

fn annotation(line: usize, level: CheckAnnotationLevel, message: String) -> CheckAnnotation {
    let line = u32::try_from(line.max(1)).unwrap_or(u32::MAX);
    CheckAnnotation {
        path: REPO_CONFIG_PATH.to_owned(),
        start_line: line,
        end_line: line,
        level,
        message: clip(message, MAX_MESSAGE_BYTES),
        title: None,
    }
}

fn describe(e: &EntryResult) -> String {
    let what = match &e.pin {
        Some((kind, name)) => format!("{} {kind} {name}", e.written),
        None => e.written.clone(), // clone: the line owns its text
    };
    let state = match &e.state {
        EntryState::Local => "this repository's own specification".to_owned(),
        EntryState::Current => "the latest published version".to_owned(),
        EntryState::Behind { latest, newer } => {
            format!("behind: {latest} is published ({newer} later version(s))")
        }
        EntryState::Tracking => "follows the branch".to_owned(),
        EntryState::Unversioned => "the pin is no version; nothing to compare".to_owned(),
        EntryState::Undecided => {
            "the repository has too many refs to tell its latest version".to_owned()
        }
        EntryState::VersionNotFound => "the pinned version does not exist".to_owned(),
        EntryState::Unavailable => {
            "not found, or not readable by every signed-in person; check it with \
             gstn browse spec-refs"
                .to_owned()
        }
    };
    match e.state.code() {
        Some(code) => format!("line {}: {what} — {state} [{}]", e.line, code.id()),
        None => format!("line {}: {what} — {state}", e.line),
    }
}

/// The run for what was assessed (module docs: the conclusions).
pub fn assess(assessed: &Assessed) -> Result<NewCheckRun, ForgeError> {
    let name = CheckName::parse(SPEC_DRIFT_CHECK).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "spec_drift_name_invalid")
    })?;
    let (conclusion, summary, annotations) = match assessed {
        Assessed::Absent => (
            CheckConclusion::Success,
            format!("No {REPO_CONFIG_PATH} at this commit: no specification is pinned."),
            Vec::new(),
        ),
        Assessed::Invalid(problem) => (
            CheckConclusion::Neutral,
            format!(
                "{REPO_CONFIG_PATH} cannot be used (line {}, {}) [{}]: no reference was checked.",
                problem.line,
                problem.reason,
                ErrorCode::RepoConfigInvalid.id()
            ),
            vec![annotation(
                problem.line,
                CheckAnnotationLevel::Warning,
                format!(
                    "{REPO_CONFIG_PATH} is refused: {} [{}]",
                    problem.reason,
                    ErrorCode::RepoConfigInvalid.id()
                ),
            )],
        ),
        Assessed::Entries(entries) => {
            let pinned = entries.iter().filter(|e| e.pin.is_some()).count();
            let notable: Vec<&EntryResult> = entries.iter().filter(|e| e.state.notable()).collect();
            let head = if notable.is_empty() {
                format!("Every pinned specification is up to date ({pinned} pinned).")
            } else {
                format!(
                    "{} of {pinned} pinned specification(s) need a look.",
                    notable.len()
                )
            };
            let mut lines = vec![head];
            lines.extend(entries.iter().map(describe));
            let annotations = notable
                .iter()
                .map(|e| {
                    let level = match e.state {
                        EntryState::Behind { .. } | EntryState::VersionNotFound => {
                            CheckAnnotationLevel::Warning
                        }
                        EntryState::Local
                        | EntryState::Current
                        | EntryState::Tracking
                        | EntryState::Unversioned
                        | EntryState::Undecided
                        | EntryState::Unavailable => CheckAnnotationLevel::Notice,
                    };
                    annotation(e.line, level, describe(e))
                })
                .collect();
            let conclusion = if notable.is_empty() {
                CheckConclusion::Success
            } else {
                CheckConclusion::Neutral
            };
            (conclusion, lines.join("\n"), annotations)
        }
    };
    Ok(NewCheckRun {
        name,
        status: CheckStatus::Completed,
        conclusion: Some(conclusion),
        summary: clip(summary, MAX_CHECK_SUMMARY_BYTES),
        details_url: None,
        annotations,
    })
}

/// The `check.spec_drift` job's work (module docs).
#[derive(Clone)]
pub struct SpecDrift {
    db: Db,
    browse: Browser,
    resolver: Arc<dyn NamespaceResolver>,
    projects: Arc<dyn ProjectAccessLoader>,
}

impl std::fmt::Debug for SpecDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpecDrift").finish_non_exhaustive()
    }
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    Recorded(Box<Reported>),
    /// The project is gone (purged): nothing to record.
    ProjectGone,
}

fn context(request_id: &RequestId) -> ChangeContext {
    ChangeContext::new(Actor::System, request_id.clone()) // clone: the change owns its id
}

impl SpecDrift {
    /// `resolver` and `projects` are forge-db's (`Db` implements both);
    /// tests inject others.
    pub fn new(
        db: Db,
        browse: Browser,
        resolver: Arc<dyn NamespaceResolver>,
        projects: Arc<dyn ProjectAccessLoader>,
    ) -> Self {
        Self {
            db,
            browse,
            resolver,
            projects,
        }
    }

    /// Evaluate `request`'s head and record the run. Errors (nothing
    /// written): the project's lookup (`FG-DB-*`, `FG-WEB-0002
    /// operation=project_load`), the repository not ready
    /// (`FG-REPO-0016`), git reading the head (`FG-GIT-*`, the browse
    /// limits), a referenced path's lookup, the write.
    pub async fn run(
        &self,
        request: &SpecDriftRequest,
        request_id: &RequestId,
    ) -> Result<Ran, ForgeError> {
        let project = request.project;
        let Some(record) = self.load(request).await? else {
            tracing::info!(
                target: "forge_checks",
                event = "forge.spec_drift.project_gone",
                request_id = %request_id,
                project_id = project.get(),
                head_sha = request.head.as_str(),
                "the project is gone; no spec drift run recorded"
            );
            return Ok(Ran::ProjectGone);
        };
        let repo = record.repository;
        if repo.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", repo.id.get())
                .with_context("project_id", project.get())
                .with_context("state", repo.state.as_str()));
        }
        let assessed = match read_repo_config(&self.browse, repo.id, request.head.as_str()).await? {
            RepoConfigRead::Absent => Assessed::Absent,
            RepoConfigRead::Invalid(problem) => Assessed::Invalid(problem),
            RepoConfigRead::Valid(config) => {
                let ctx = FollowCtx {
                    resolver: self.resolver.as_ref(),
                    lookup_timeout: LOOKUP_TIMEOUT,
                    browse: &self.browse,
                    request_id,
                    project,
                };
                let mut entries = Vec::with_capacity(config.specs.len());
                for entry in &config.specs {
                    entries.push(match &entry.source {
                        SpecSource::Path(p) => EntryResult {
                            line: entry.line,
                            written: p.as_str().to_owned(),
                            pin: None,
                            state: EntryState::Local,
                        },
                        SpecSource::Repo { written, repo, pin } => EntryResult {
                            line: entry.line,
                            written: written.clone(), // clone: the result owns its text
                            pin: Some((pin.kind(), pin.name().to_owned())),
                            state: self.entry(&ctx, entry.line, repo, pin).await?,
                        },
                    });
                }
                Assessed::Entries(entries)
            }
        };
        let new = assess(&assessed)?;
        let reported = record_forge_check_run(
            &self.db,
            &context(request_id),
            (project, &request.head),
            &new,
        )
        .await?;
        tracing::info!(
            target: "forge_checks",
            event = "forge.spec_drift.recorded",
            request_id = %request_id,
            project_id = project.get(),
            head_sha = request.head.as_str(),
            conclusion = new.conclusion.map(CheckConclusion::as_str),
            check_run_id = reported.run.id,
            "spec drift evaluated"
        );
        Ok(Ran::Recorded(Box::new(reported)))
    }

    async fn load(
        &self,
        request: &SpecDriftRequest,
    ) -> Result<Option<forge_core::namespace_resolver::ProjectRecord>, ForgeError> {
        match tokio::time::timeout(
            LOOKUP_TIMEOUT,
            self.projects.load_project(request.project, None),
        )
        .await
        {
            Ok(loaded) => loaded,
            Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("operation", "project_load")
                .with_context("timeout_ms", LOOKUP_TIMEOUT.as_millis())),
        }
    }

    /// One `repo:` entry, for every signed-in person.
    async fn entry(
        &self,
        ctx: &FollowCtx<'_>,
        line: usize,
        repo: &forge_core::LookupPath,
        pin: &SpecPin,
    ) -> Result<EntryState, ForgeError> {
        let followed = follow(ctx, Reader::EveryPerson, line, repo, pin).await?;
        let repository = match followed {
            Followed::Unavailable => return Ok(EntryState::Unavailable),
            Followed::VersionNotFound { .. } => return Ok(EntryState::VersionNotFound),
            Followed::Resolved { repository, .. } => repository,
        };
        let SpecPin::Version(_) = pin else {
            return Ok(EntryState::Tracking);
        };
        let list = match self.browse.refs(repository).await {
            Ok(list) => list,
            // As for the pin's own lookup: a referenced repository's git
            // failure is this entry's answer (logged at warn with its code),
            // never this run's.
            Err(e) => {
                tracing::warn!(
                    target: "forge_checks",
                    event = "forge.spec_drift.tags_unread",
                    error_code = e.code().id(),
                    error = %e,
                    request_id = %ctx.request_id,
                    project_id = ctx.project.get(),
                    referenced_repo_id = repository.get(),
                    line,
                    "the referenced repository's tags could not be listed"
                );
                return Ok(EntryState::Undecided);
            }
        };
        if list.truncated {
            return Ok(EntryState::Undecided);
        }
        let tags = list
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Tag && r.commit.is_some())
            .map(|r| r.short());
        Ok(match drift(pin.name(), tags) {
            Drift::Current => EntryState::Current,
            Drift::Behind { latest, newer } => EntryState::Behind { latest, newer },
            Drift::Unversioned => EntryState::Unversioned,
        })
    }

    /// The job failed for good (its last attempt, or a failure that can
    /// never succeed): record `error` on the head so the check is never
    /// left without an answer. A failure to record it is logged here (the
    /// job's own failure is logged by the job runner).
    pub async fn fail(
        &self,
        request: &SpecDriftRequest,
        request_id: &RequestId,
        error: &ForgeError,
    ) {
        let recorded = match CheckName::parse(SPEC_DRIFT_CHECK) {
            Ok(name) => {
                let new = NewCheckRun {
                    name,
                    status: CheckStatus::Completed,
                    conclusion: Some(CheckConclusion::Error),
                    summary: format!(
                        "forge/spec-drift could not be evaluated [{}] (request {request_id}). \
                         An operator can find the cause in the worker's log by this request id.",
                        error.code().id()
                    ),
                    details_url: None,
                    annotations: Vec::new(),
                };
                record_forge_check_run(
                    &self.db,
                    &context(request_id),
                    (request.project, &request.head),
                    &new,
                )
                .await
                .map(drop)
            }
            Err(_) => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "spec_drift_name_invalid")),
        };
        if let Err(e) = recorded {
            tracing::error!(
                target: "forge_checks",
                event = "forge.spec_drift.error_unrecorded",
                error_code = e.code().id(),
                error = %e,
                job_error_code = error.code().id(),
                request_id = %request_id,
                project_id = request.project.get(),
                head_sha = request.head.as_str(),
                "the failed spec drift evaluation could not be recorded as error"
            );
        }
    }
}

#[cfg(test)]
#[path = "spec_drift_tests.rs"]
mod tests;
