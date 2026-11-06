//! `forge/risk` (M3 X7, plan 04 M-6 layer 1, ADR-0023 Amendment X7): the
//! deterministic risk rules ([`forge_core::risk`]) applied to a merge
//! request head. Deterministic: the same commits give the same run, and
//! nothing a commit says (a message, a file) can lower it.
//!
//! [`Risk::run`] evaluates one `check.risk` job (a head): the target
//! branches of the open merge requests that have it as a version
//! ([`forge_db::checks::risk::risk_targets`]), then for each target the
//! diff from the merge base to the head (`diff-tree -M`, renames paired),
//! the path rules, the text of the few files that matter (new migrations,
//! deploy files, and every added or edited text file for secrets, at most
//! [`Limits::max_content_files`] of at most [`Limits::max_file_bytes`]
//! each) and the mass-deletion rule against the target's tree.
//! [`assess`] turns that into the run; `forge_db::checks::record_forge_check_run`
//! writes it (no reporter: forge's own worker).
//!
//! Conclusions: `success` — nothing found; `neutral` — only `warn` findings,
//! or part of the head could not be assessed (`FG-REVIEW-0024` /
//! `FG-REVIEW-0025`: never a `success`); `failure` — a `stop` rule
//! (`FG-REVIEW-0023`). A run names rule, path and line, never the matched
//! text. [`Risk::fail`] / [`record_error`] record `error` when a job dies
//! for good, so a head is never left without an answer.

use std::sync::Arc;
use std::time::Duration;

use forge_core::checks::{
    CheckAnnotationLevel, CheckConclusion, CheckStatus, MAX_CHECK_ANNOTATIONS,
    MAX_CHECK_SUMMARY_BYTES, RISK_CHECK,
};
use forge_core::namespace_resolver::ProjectAccessLoader;
use forge_core::risk::{
    content_findings, content_kind, level, mass_deletion, path_findings, sort_findings,
    ChangeStatus, ChangedFile, ContentKind, Finding, RiskLevel, MASS_DELETE_FILES,
    MASS_DELETE_LINES, MAX_CHANGED_FILES,
};
use forge_core::{CheckName, ObjectId, RefName, RepoId, RepositoryState, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::risk::{risk_targets, RiskRequest, RiskTargets};
use forge_db::checks::{record_forge_check_run, CheckAnnotation, NewCheckRun, Reported};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RefLookup, RepoPath, SmallFile};
use forge_git::risk_inputs::RiskDiff;
use forge_git::secret_scan::{self, SecretScan};
use forge_git::{Git, StorageRoot};

use crate::validate::relative_path;

/// One lookup of the project.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// One git call (the diff, the merge base, the tree count).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The whole evaluation before the write (the handler ends a slow run
/// itself, well inside the job's 90 s, and answers `error` on its last
/// attempt: 45 + 10 + 10 < 90).
pub const EVALUATION_BUDGET: Duration = Duration::from_secs(45);
/// One write of the run (the evaluation's, or `error`'s).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// The findings one target keeps (worst first).
pub const MAX_FINDINGS_PER_TARGET: usize = 1000;

/// What bounds the work of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// More changed files than this is a `stop` (`too_many_changes`).
    pub max_changed_files: usize,
    /// Files whose text is read at most (the rest is counted).
    pub max_content_files: usize,
    /// A larger file is not read (and counted).
    pub max_file_bytes: u64,
    /// Files counted in the target's tree for the mass-deletion share.
    pub max_tree_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_changed_files: MAX_CHANGED_FILES,
            max_content_files: 60,
            max_file_bytes: 256 * 1024,
            max_tree_entries: 200_000,
        }
    }
}

/// The findings for one target branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFindings {
    /// The branch's short name.
    pub target: String,
    /// Changed files looked at.
    pub files: usize,
    pub findings: Vec<Finding>,
}

/// Everything the rules found for a head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub targets: Vec<TargetFindings>,
    /// Findings about the head as a whole (no target to compare with).
    pub general: Vec<Finding>,
}

/// The rules that say part of the head was not assessed (`warn`,
/// `FG-REVIEW-0025`).
const NOT_ASSESSED: &[&str] = &[
    "no_open_merge_request",
    "target_missing",
    "no_common_history",
    "non_utf8_path",
    "content_not_scanned",
    "targets_not_assessed",
];

/// The longest annotation message (the table's CHECK, 4 KiB).
const MAX_MESSAGE_BYTES: usize = 4096;

fn code_of(finding: &Finding) -> ErrorCode {
    match finding.level {
        RiskLevel::Stop => ErrorCode::RiskStop,
        RiskLevel::Warn | RiskLevel::Ok => {
            if NOT_ASSESSED.contains(&finding.rule) {
                ErrorCode::RiskNotAssessed
            } else {
                ErrorCode::RiskWarn
            }
        }
    }
}

/// The conclusion of a level: `stop` is a `failure` (a required
/// `forge/risk` is not satisfied and M4's auto-merge does not merge it),
/// `warn` a `neutral`.
pub fn conclusion_of(level: RiskLevel) -> CheckConclusion {
    match level {
        RiskLevel::Ok => CheckConclusion::Success,
        RiskLevel::Warn => CheckConclusion::Neutral,
        RiskLevel::Stop => CheckConclusion::Failure,
    }
}

/// `text` cut to at most `max` bytes on a character boundary, ending with
/// `…` when cut (the run is shortened, never refused).
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

/// A path as shown in the summary: control characters replaced.
fn shown(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn describe(f: &Finding) -> String {
    let place = match (&f.path, f.line) {
        (Some(p), Some(l)) => format!(" {}:{l}", shown(p)),
        (Some(p), None) => format!(" {}", shown(p)),
        (None, _) => String::new(),
    };
    format!(
        "- {} {}{place} — {} [{}]",
        f.level.as_str(),
        f.rule,
        f.reason,
        code_of(f).id()
    )
}

/// The run for what was found (module docs: the conclusions).
pub fn assess(evaluation: &Evaluation) -> Result<NewCheckRun, ForgeError> {
    let name = CheckName::parse(RISK_CHECK).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "risk_name_invalid")
    })?;
    let all: Vec<&Finding> = evaluation
        .targets
        .iter()
        .flat_map(|t| t.findings.iter())
        .chain(evaluation.general.iter())
        .collect();
    let worst = all.iter().map(|f| f.level).max().unwrap_or(RiskLevel::Ok);
    let stops = all.iter().filter(|f| f.level == RiskLevel::Stop).count();
    let warns = all.iter().filter(|f| f.level == RiskLevel::Warn).count();
    let names: Vec<&str> = evaluation
        .targets
        .iter()
        .map(|t| t.target.as_str())
        .collect();
    let head = if worst == RiskLevel::Ok {
        let files: usize = evaluation.targets.iter().map(|t| t.files).sum();
        format!(
            "No risk found against {} ({files} changed file(s) checked). Deterministic rules only (plan 04 M-6, layer 1).",
            names.join(", ")
        )
    } else {
        let mut codes: Vec<&str> = all.iter().map(|f| code_of(f).id()).collect();
        codes.sort_unstable();
        codes.dedup();
        format!(
            "{}: {stops} stop, {warns} warn [{}]. Deterministic rules only (plan 04 M-6, layer 1); rule, path and line are named, never the matched text.",
            worst.as_str().to_ascii_uppercase(),
            codes.join(", ")
        )
    };
    let mut lines = vec![head];
    for t in &evaluation.targets {
        lines.push(format!(
            "target {} ({} changed file(s)):",
            t.target, t.files
        ));
        if t.findings.is_empty() {
            lines.push("- no finding".to_owned());
        }
        lines.extend(t.findings.iter().map(describe));
    }
    lines.extend(evaluation.general.iter().map(describe));

    // The annotations: worst first, paths a run can hold, once each.
    let mut noted: Vec<&Finding> = all.clone();
    noted.sort_by(|a, b| {
        (std::cmp::Reverse(a.level), &a.path, a.line, a.rule).cmp(&(
            std::cmp::Reverse(b.level),
            &b.path,
            b.line,
            b.rule,
        ))
    });
    noted.dedup_by(|a, b| a.rule == b.rule && a.path == b.path && a.line == b.line);
    let annotations: Vec<CheckAnnotation> = noted
        .into_iter()
        .filter_map(|f| {
            let path = f.path.as_deref().filter(|p| relative_path(p))?;
            let line = f.line.unwrap_or(1).max(1);
            Some(CheckAnnotation {
                path: path.to_owned(),
                start_line: line,
                end_line: line,
                level: match f.level {
                    RiskLevel::Stop => CheckAnnotationLevel::Failure,
                    RiskLevel::Warn => CheckAnnotationLevel::Warning,
                    RiskLevel::Ok => CheckAnnotationLevel::Notice,
                },
                message: clip(
                    format!("{}: {} [{}]", f.rule, f.reason, code_of(f).id()),
                    MAX_MESSAGE_BYTES,
                ),
                title: None,
            })
        })
        .take(MAX_CHECK_ANNOTATIONS)
        .collect();
    Ok(NewCheckRun {
        name,
        status: CheckStatus::Completed,
        conclusion: Some(conclusion_of(worst)),
        summary: clip(lines.join("\n"), MAX_CHECK_SUMMARY_BYTES),
        details_url: None,
        annotations,
    })
}

/// The `check.risk` job's work (module docs).
#[derive(Clone)]
pub struct Risk {
    db: Db,
    git: Git,
    storage: StorageRoot,
    browse: Browser,
    projects: Arc<dyn ProjectAccessLoader>,
    budget: Duration,
    limits: Limits,
}

impl std::fmt::Debug for Risk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Risk").finish_non_exhaustive()
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

/// Write the run within [`WRITE_TIMEOUT`] (`FG-WEB-0002
/// operation=risk_record` past it: nothing is committed).
async fn record(
    db: &Db,
    request: &RiskRequest,
    request_id: &RequestId,
    new: &NewCheckRun,
) -> Result<Reported, ForgeError> {
    let ctx = context(request_id);
    let write = record_forge_check_run(db, &ctx, (request.project, &request.head), new);
    match tokio::time::timeout(WRITE_TIMEOUT, write).await {
        Ok(written) => written,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", "risk_record")
            .with_context("timeout_ms", WRITE_TIMEOUT.as_millis())),
    }
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

fn rule(id: &'static str) -> Result<Finding, ForgeError> {
    Finding::for_rule(id).ok_or_else(|| internal("unknown_risk_rule"))
}

impl Risk {
    /// `projects` is forge-db's (`Db` implements it); tests inject others.
    pub fn new(
        db: Db,
        git: Git,
        storage: StorageRoot,
        browse: Browser,
        projects: Arc<dyn ProjectAccessLoader>,
    ) -> Self {
        Self {
            db,
            git,
            storage,
            browse,
            projects,
            budget: EVALUATION_BUDGET,
            limits: Limits::default(),
        }
    }

    /// The same, with another evaluation budget (tests).
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// The same, with other limits (tests).
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Evaluate `request`'s head against the targets of its open merge
    /// requests and record the run. Errors (nothing written): the
    /// project's lookup, the targets' query (`FG-DB-*`), the repository
    /// not ready (`FG-REPO-0016`), git (`FG-GIT-*`), the write, the
    /// evaluation's deadline (`FG-WEB-0002 operation=risk`).
    pub async fn run(
        &self,
        request: &RiskRequest,
        request_id: &RequestId,
    ) -> Result<Ran, ForgeError> {
        let targets = risk_targets(&self.db, request.project, &request.head).await?;
        self.run_against(request, &targets, request_id).await
    }

    /// The same for the given `targets` (what [`Self::run`] reads).
    pub async fn run_against(
        &self,
        request: &RiskRequest,
        targets: &RiskTargets,
        request_id: &RequestId,
    ) -> Result<Ran, ForgeError> {
        let evaluated = tokio::time::timeout(self.budget, self.evaluate(request, targets)).await;
        let evaluation = match evaluated {
            Ok(evaluation) => evaluation?,
            // Dropping the evaluation stops its git work (killed and
            // reaped by forge-git) and its reads.
            Err(_) => {
                return Err(ForgeError::new(ErrorCode::RequestTimeout)
                    .with_context("operation", "risk")
                    .with_context("timeout_ms", self.budget.as_millis()))
            }
        };
        let Some(evaluation) = evaluation else {
            tracing::info!(
                target: "forge_checks",
                event = "forge.risk.project_gone",
                request_id = %request_id,
                project_id = request.project.get(),
                head_sha = request.head.as_str(),
                "the project is gone; no risk run recorded"
            );
            return Ok(Ran::ProjectGone);
        };
        let new = assess(&evaluation)?;
        let reported = record(&self.db, request, request_id, &new).await?;
        let all: Vec<&Finding> = evaluation
            .targets
            .iter()
            .flat_map(|t| t.findings.iter())
            .chain(evaluation.general.iter())
            .collect();
        tracing::info!(
            target: "forge_checks",
            event = "forge.risk.recorded",
            request_id = %request_id,
            project_id = request.project.get(),
            head_sha = request.head.as_str(),
            level = level_of(&all).as_str(),
            findings = all.len(),
            targets = evaluation.targets.len(),
            conclusion = new.conclusion.map(CheckConclusion::as_str),
            check_run_id = reported.run.id,
            "risk evaluated"
        );
        Ok(Ran::Recorded(Box::new(reported)))
    }

    /// Everything before the write (`None`: the project is gone).
    async fn evaluate(
        &self,
        request: &RiskRequest,
        targets: &RiskTargets,
    ) -> Result<Option<Evaluation>, ForgeError> {
        let project = request.project;
        let loaded =
            match tokio::time::timeout(LOOKUP_TIMEOUT, self.projects.load_project(project, None))
                .await
            {
                Ok(loaded) => loaded?,
                Err(_) => {
                    return Err(ForgeError::new(ErrorCode::RequestTimeout)
                        .with_context("operation", "project_load")
                        .with_context("timeout_ms", LOOKUP_TIMEOUT.as_millis()))
                }
            };
        let Some(record) = loaded else {
            return Ok(None);
        };
        let repo = record.repository;
        if repo.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", repo.id.get())
                .with_context("project_id", project.get())
                .with_context("state", repo.state.as_str()));
        }
        let mut evaluation = Evaluation {
            targets: Vec::with_capacity(targets.targets.len()),
            general: Vec::new(),
        };
        if targets.targets.is_empty() {
            evaluation.general.push(rule("no_open_merge_request")?);
        }
        if targets.more {
            evaluation.general.push(rule("targets_not_assessed")?);
        }
        for target in &targets.targets {
            evaluation
                .targets
                .push(self.target(repo.id, target, &request.head).await?);
        }
        Ok(Some(evaluation))
    }

    /// One target branch: the head's diff from the merge base.
    async fn target(
        &self,
        repo: RepoId,
        target: &RefName,
        head: &ObjectId,
    ) -> Result<TargetFindings, ForgeError> {
        let short = target
            .as_str()
            .strip_prefix("refs/heads/")
            .unwrap_or_else(|| target.as_str())
            .to_owned();
        let mut found = TargetFindings {
            target: short,
            files: 0,
            findings: Vec::new(),
        };
        // The cached ref list may be older than the target's tip.
        self.browse.invalidate(repo);
        let tip = match self.browse.find_ref(repo, target).await? {
            RefLookup::Commit(rev) => ObjectId::parse(&rev.commit)
                .map_err(|_| internal("invalid_commit_id").with_context("repo_id", repo.get()))?,
            RefLookup::Missing | RefLookup::NotACommit => {
                found.findings.push(rule("target_missing")?);
                return Ok(found);
            }
        };
        let Some(base) = self
            .git
            .merge_base(&self.storage, repo, &tip, head, GIT_TIMEOUT)
            .await?
        else {
            found.findings.push(rule("no_common_history")?);
            return Ok(found);
        };
        let files = match self
            .git
            .risk_diff(
                &self.storage,
                repo,
                &base,
                head,
                self.limits.max_changed_files,
                GIT_TIMEOUT,
            )
            .await?
        {
            RiskDiff::Complete(files) => files,
            RiskDiff::TooMany => {
                found.findings.push(rule("too_many_changes")?);
                return Ok(found);
            }
            RiskDiff::NonUtf8 => {
                found.findings.push(rule("non_utf8_path")?);
                return Ok(found);
            }
        };
        found.files = files.len();
        for file in &files {
            found.findings.extend(path_findings(file));
        }
        if let Some(f) = self.mass_deletion(repo, &base, &files).await? {
            found.findings.push(f);
        }
        found
            .findings
            .extend(self.contents(repo, head, &files).await?);
        sort_findings(&mut found.findings);
        found.findings.dedup();
        found.findings.truncate(MAX_FINDINGS_PER_TARGET);
        Ok(found)
    }

    /// The mass-deletion rule; the target's tree is counted only when the
    /// absolute thresholds are reached.
    async fn mass_deletion(
        &self,
        repo: RepoId,
        base: &ObjectId,
        files: &[ChangedFile],
    ) -> Result<Option<Finding>, ForgeError> {
        let lines: u64 = files
            .iter()
            .map(|f| f.deleted.unwrap_or(0))
            .fold(0, u64::saturating_add);
        let removed = files
            .iter()
            .filter(|f| f.status == ChangeStatus::Deleted)
            .count();
        if lines < MASS_DELETE_LINES
            && u64::try_from(removed).unwrap_or(u64::MAX) < MASS_DELETE_FILES
        {
            return Ok(None);
        }
        let totals = self
            .git
            .tree_totals(
                &self.storage,
                repo,
                base,
                self.limits.max_tree_entries,
                GIT_TIMEOUT,
            )
            .await?;
        Ok(mass_deletion(files, &totals))
    }

    /// The findings in the text of the files that matter, and the note when
    /// some were not read.
    async fn contents(
        &self,
        repo: RepoId,
        head: &ObjectId,
        files: &[ChangedFile],
    ) -> Result<Vec<Finding>, ForgeError> {
        // Migrations and deploy files first, then every other file that
        // gained lines. A binary file (git prints `-`: one NUL byte is
        // enough) is a candidate too: the scans run on bytes.
        let mut candidates: Vec<(&ChangedFile, Option<ContentKind>)> = files
            .iter()
            .filter(|f| {
                matches!(f.new_mode, 0o100_644 | 0o100_755)
                    && f.status != ChangeStatus::Deleted
                    && f.added != Some(0)
            })
            .map(|f| (f, content_kind(f)))
            .collect();
        candidates.sort_by_key(|(_, kind)| match kind {
            Some(ContentKind::Sql) => 0,
            Some(ContentKind::Deploy) => 1,
            None => 2,
        });
        let mut found = Vec::new();
        let mut unread = 0usize;
        let mut unread_critical = 0usize;
        for (index, (file, kind)) in candidates.into_iter().enumerate() {
            // A file that cannot be shown safe: a new migration or a CI /
            // deploy file is a `stop`, any other is only counted.
            let mut unread_file = |found: &mut Vec<Finding>| -> Result<(), ForgeError> {
                if kind.is_some() {
                    unread_critical += 1;
                    found.push(rule("critical_content_unread")?.at(&file.path));
                } else {
                    unread += 1;
                }
                Ok(())
            };
            if index >= self.limits.max_content_files {
                unread_file(&mut found)?;
                continue;
            }
            let Ok(path) = RepoPath::parse(&file.path) else {
                unread_file(&mut found)?;
                continue;
            };
            let bytes = match self
                .browse
                .small_file(repo, head.as_str(), &path, self.limits.max_file_bytes)
                .await?
            {
                SmallFile::Found(bytes) => bytes,
                SmallFile::TooLarge | SmallFile::Absent | SmallFile::NotAFile => {
                    unread_file(&mut found)?;
                    continue;
                }
            };
            if let SecretScan::Found(_) = secret_scan::scan_bytes(&bytes) {
                found.push(rule("secret_in_content")?.at(&file.path));
            }
            if let Some(kind) = kind {
                let text = String::from_utf8_lossy(&bytes);
                found.extend(content_findings(kind, &file.path, &text));
            }
        }
        if unread > 0 || unread_critical > 0 {
            tracing::info!(
                target: "forge_checks",
                event = "forge.risk.content_not_scanned",
                repo_id = repo.get(),
                head_sha = head.as_str(),
                unread,
                unread_critical,
                "some changed files were not read for the risk rules"
            );
        }
        if unread > 0 {
            found.push(rule("content_not_scanned")?);
        }
        Ok(found)
    }

    /// The job failed for good (its last attempt, or a failure that can
    /// never succeed): record `error` on the head so the check is never
    /// left without an answer. A failure to record it is logged here (the
    /// job's own failure is logged by the job runner).
    pub async fn fail(&self, request: &RiskRequest, request_id: &RequestId, error: &ForgeError) {
        record_error(&self.db, request, request_id, error.code().id()).await;
    }
}

fn level_of(findings: &[&Finding]) -> RiskLevel {
    let owned: Vec<Finding> = findings.iter().map(|f| (*f).clone()).collect(); // clone: `level` takes a slice of owned findings
    level(&owned)
}

/// Record `error` on `request`'s head (the job's last failure: `code`) so
/// the check is never left without an answer: [`Risk::fail`], and the
/// worker's sweep for a job that died without one (reaped). Bounded by
/// [`WRITE_TIMEOUT`]. A failure to record it is logged here (`error`).
/// Whether it was recorded.
pub async fn record_error(
    db: &Db,
    request: &RiskRequest,
    request_id: &RequestId,
    code: &str,
) -> bool {
    let recorded = match CheckName::parse(RISK_CHECK) {
        Ok(name) => {
            let new = NewCheckRun {
                name,
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Error),
                summary: format!(
                    "forge/risk could not be evaluated [{code}] (request {request_id}). \
                     An operator can find the cause in the worker's log by this request id."
                ),
                details_url: None,
                annotations: Vec::new(),
            };
            record(db, request, request_id, &new).await.map(drop)
        }
        Err(_) => Err(internal("risk_name_invalid")),
    };
    match recorded {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                target: "forge_checks",
                event = "forge.risk.error_unrecorded",
                error_code = e.code().id(),
                error = %e,
                job_error_code = code,
                request_id = %request_id,
                project_id = request.project.get(),
                head_sha = request.head.as_str(),
                "the failed risk evaluation could not be recorded as error"
            );
            false
        }
    }
}

#[cfg(test)]
#[path = "risk_tests.rs"]
mod tests;
