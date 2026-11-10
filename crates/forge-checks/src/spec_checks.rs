//! `forge/spec-lint` and `forge/spec-trace` (M3 X4, plan 03 §5.4, plan 06
//! D-7, ADR-0023 Amendment X4), and `forge/doc-lint` (plan 13 stage 2,
//! [`crate::doc_lint`]). Deterministic: the same commits, links and files
//! give the same runs.
//!
//! [`SpecChecks::run`] evaluates one `check.spec_index` job (a merge
//! request version's head, or its head again after a work item link
//! changed):
//!
//! 1. the project (gone: nothing recorded) and its ready repository;
//! 2. the merge request (gone: nothing recorded) and its open work item
//!    links;
//! 3. the head, read once for the index and the documents
//!    ([`crate::spec_index::index_head`]), and the merge base with the
//!    target branch's tip ([`crate::spec_index::index_commit`]; no target
//!    branch or no common history: nothing to compare with, every
//!    requirement and tag at the head is new);
//! 4. [`assess_lint`] on the head and [`assess_trace`] on
//!    `forge_core::spec_index::trace_merge_request`, and
//!    [`crate::doc_lint::assess_doc_lint`] on the head's documents, all
//!    written with `forge_db::checks::record_forge_check_run` (no reporter).
//!
//! Conclusions — `forge/spec-lint`: `success` (no problem), `failure` (a
//! problem, `FG-REPO-0028`), `neutral` (no problem found, but the index hit
//! a bound: `FG-REPO-0031`). `forge/spec-trace` is **advisory** (plan 03
//! §9-4): `success` when every touched requirement and work item has a
//! `@req` test, `neutral` otherwise (`FG-REPO-0029` untested, `FG-REPO-0030`
//! unknown or malformed tags, `FG-REPO-0031`) — never `failure`.
//!
//! What a run shows is this repository's own text only (its own spec ids
//! and paths, its tags, the merge request's own work item links): every
//! reader of the project may see it. Referenced specifications are not
//! read here; a tag of an id their `scope:` covers counts as known.
//!
//! The evaluation has its own deadline ([`EVALUATION_BUDGET`]) and each
//! write its own ([`WRITE_TIMEOUT`]), well inside the job's 120 s, so the
//! last attempt answers `error` itself ([`SpecChecks::fail`]); a job the
//! reaper ends is answered by `forge worker`'s sweep ([`record_errors`]).

use std::sync::Arc;
use std::time::Duration;

use forge_core::checks::{
    CheckAnnotationLevel, CheckConclusion, CheckStatus, DOC_LINT_CHECK, MAX_CHECK_ANNOTATIONS,
    MAX_CHECK_SUMMARY_BYTES, SPEC_LINT_CHECK, SPEC_TRACE_CHECK,
};
use forge_core::namespace_resolver::ProjectAccessLoader;
use forge_core::repo_config::REPO_CONFIG_PATH;
use forge_core::spec_index::{
    lint, trace_merge_request, LintFinding, MrTrace, SpecIndex, TraceState,
};
use forge_core::{CheckName, ObjectId, RepositoryState, RequestId, WorkItemRef};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::spec_index::SpecIndexRequest;
use forge_db::checks::{record_forge_check_run, CheckAnnotation, NewCheckRun, Reported};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RefLookup};

use crate::doc_lint::{assess_doc_lint, DocLimits, DocTree};
use crate::spec_index::{index_commit, index_head, IndexLimits, Indexed, Tags};
use crate::spec_refs::RepoConfigRead;

/// One database lookup (the project, the merge request, its links).
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// The whole evaluation before the writes (two indexes; each git call is
/// bounded by the browse limits and `IndexLimits::grep_timeout`).
pub const EVALUATION_BUDGET: Duration = Duration::from_secs(60);
/// One write of a run.
/// Three runs and, on the last attempt, three `error` writes: 60 s + 6 × 8 s
/// = 108 s, at least 10 s inside the job's 120 s (plan 13 stage 2 added
/// `forge/doc-lint`; review MR3: keep a margin).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(8);

/// The longest annotation message (the table's CHECK, 4 KiB).
const MAX_MESSAGE_BYTES: usize = 4096;

/// `text` cut to at most `max` bytes on a character boundary, ending with
/// `…` when cut (paths are the repository's own text and may be long).
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

pub(crate) fn annotation(
    path: &str,
    line: usize,
    level: CheckAnnotationLevel,
    message: String,
) -> CheckAnnotation {
    let line = u32::try_from(line.max(1)).unwrap_or(u32::MAX);
    CheckAnnotation {
        path: path.to_owned(),
        start_line: line,
        end_line: line,
        level,
        message: clip(message, MAX_MESSAGE_BYTES),
        title: None,
    }
}

fn name(raw: &str) -> Result<CheckName, ForgeError> {
    CheckName::parse(raw).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "spec_check_name_invalid")
            .with_context("name", raw.to_owned())
    })
}

pub(crate) fn completed(
    check: &str,
    conclusion: CheckConclusion,
    lines: &[String],
    annotations: Vec<CheckAnnotation>,
) -> Result<NewCheckRun, ForgeError> {
    Ok(NewCheckRun {
        name: name(check)?,
        status: CheckStatus::Completed,
        conclusion: Some(conclusion),
        summary: clip(lines.join("\n"), MAX_CHECK_SUMMARY_BYTES),
        details_url: None,
        // A path the table refuses (a file named `C:x`, with a backslash or
        // a control character: git allows them) is named in the summary
        // only, never a refused write.
        annotations: annotations
            .into_iter()
            .filter(|a| crate::validate::relative_path(&a.path))
            .take(MAX_CHECK_ANNOTATIONS)
            .collect(),
    })
}

/// The line naming a bound that was hit.
fn incomplete_line(reason: &str) -> String {
    format!(
        "The index is incomplete ({reason}): a bound was hit, the results are about what was \
         read [{}].",
        ErrorCode::SpecIndexIncomplete.id()
    )
}

fn sources_line(head: &Indexed) -> String {
    let dirs = head.dirs.join(", ");
    match &head.config {
        RepoConfigRead::Invalid(p) => format!(
            "{REPO_CONFIG_PATH} cannot be used (line {}, {}) [{}]: the default {dirs} was read.",
            p.line,
            p.reason,
            ErrorCode::RepoConfigInvalid.id()
        ),
        RepoConfigRead::Absent | RepoConfigRead::Valid(_) => {
            format!("Specification directories: {dirs}.")
        }
    }
}

fn finding_line(f: &LintFinding) -> String {
    match &f.detail {
        Some(d) => format!("{}:{} {} — {d}", f.path, f.line, f.rule),
        None => format!("{}:{} {}", f.path, f.line, f.rule),
    }
}

/// The `forge/spec-lint` run of the indexed head (module docs).
pub fn assess_lint(head: &Indexed) -> Result<NewCheckRun, ForgeError> {
    let index = &head.index;
    let found = lint(index, &head.external);
    let requirements = index.requirements().len();
    let mut lines = Vec::new();
    let conclusion = if found.is_empty() {
        lines.push(if index.documents.is_empty() {
            "No specification document at this commit: nothing to check.".to_owned()
        } else {
            format!(
                "{} specification document(s), {requirements} requirement(s): every rule is kept.",
                index.documents.len()
            )
        });
        if index.incomplete.is_some() {
            CheckConclusion::Neutral
        } else {
            CheckConclusion::Success
        }
    } else {
        lines.push(format!(
            "{} problem(s) in {} specification document(s) and their references [{}]:",
            found.len(),
            index.documents.len(),
            ErrorCode::SpecLintProblem.id()
        ));
        lines.extend(found.iter().map(finding_line));
        // Review X4 (LOW): an incomplete index may have left out the
        // definition a tag names; broken references alone are no failure then.
        let only_references = found.iter().all(|f| f.rule == "broken_reference");
        if index.incomplete.is_some() && only_references {
            CheckConclusion::Neutral
        } else {
            CheckConclusion::Failure
        }
    };
    lines.push(sources_line(head));
    if let Some(reason) = index.incomplete {
        lines.push(incomplete_line(reason));
    }
    let annotations = found
        .iter()
        .map(|f| {
            annotation(
                &f.path,
                f.line,
                CheckAnnotationLevel::Failure,
                format!("{} [{}]", finding_line(f), ErrorCode::SpecLintProblem.id()),
            )
        })
        .collect();
    completed(SPEC_LINT_CHECK, conclusion, &lines, annotations)
}

/// What the trace was compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compared {
    /// The merge base with the target branch's tip.
    MergeBase(ObjectId),
    /// No target branch, or no common history: every requirement and tag
    /// at the head counts as new.
    Nothing,
}

fn reasons(row: &forge_core::spec_index::TraceRow) -> String {
    row.reasons
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn row_line(row: &forge_core::spec_index::TraceRow) -> String {
    let at = row
        .at
        .as_ref()
        .map(|(p, l)| format!(" {p}:{l}"))
        .unwrap_or_default();
    match &row.state {
        TraceState::Tested(n) => {
            format!("tested {} ({}; {n} test(s)){at}", row.subject, reasons(row))
        }
        TraceState::Untested => format!(
            "untested {} ({}){at} [{}]",
            row.subject,
            reasons(row),
            ErrorCode::RequirementUntested.id()
        ),
        TraceState::Unknown => format!(
            "unknown {} ({}){at}: no requirement here or in a referenced scope [{}]",
            row.subject,
            reasons(row),
            ErrorCode::RequirementTagUnknown.id()
        ),
    }
}

/// The `forge/spec-trace` run (module docs). `links_truncated`: the merge
/// request has more links than one read keeps.
pub fn assess_trace(
    trace: &MrTrace,
    compared: &Compared,
    links_truncated: bool,
) -> Result<NewCheckRun, ForgeError> {
    let touched = trace.rows.len();
    let attention = trace
        .rows
        .iter()
        .filter(|r| !matches!(r.state, TraceState::Tested(_)))
        .count();
    let mut lines = vec![if touched == 0 {
        "This merge request touches no requirement and is linked to no work item.".to_owned()
    } else if attention == 0 {
        format!(
            "Every one of the {touched} touched requirement(s) and work item(s) has a @req test."
        )
    } else {
        format!(
            "{attention} of {touched} touched requirement(s) and work item(s) need a look \
             (advisory: forge/spec-trace never fails)."
        )
    }];
    lines.extend(trace.rows.iter().map(row_line));
    for m in &trace.malformed {
        lines.push(format!(
            "malformed @req at {}:{} ({:?}) [{}]",
            m.path,
            m.line,
            m.raw,
            ErrorCode::RequirementTagUnknown.id()
        ));
    }
    match compared {
        Compared::MergeBase(base) => {
            lines.push(format!("Compared with the merge base {}.", base.as_str()))
        }
        Compared::Nothing => lines.push(
            "No merge base with the target branch: every requirement and tag at the head counts \
             as new."
                .to_owned(),
        ),
    }
    if links_truncated {
        lines.push(
            "The merge request has more work item links than one read keeps; the first are traced."
                .to_owned(),
        );
    }
    if let Some(reason) = trace.incomplete {
        lines.push(incomplete_line(reason));
    }
    let mut annotations = Vec::new();
    for row in &trace.rows {
        let Some((path, line)) = &row.at else {
            continue;
        };
        let level = match row.state {
            TraceState::Tested(_) => continue,
            TraceState::Untested => CheckAnnotationLevel::Warning,
            TraceState::Unknown => CheckAnnotationLevel::Notice,
        };
        annotations.push(annotation(path, *line, level, row_line(row)));
    }
    for m in &trace.malformed {
        annotations.push(annotation(
            &m.path,
            m.line,
            CheckAnnotationLevel::Notice,
            format!(
                "@req is followed by no requirement id or acceptance criterion (PBI-<n>/AC-<m>) [{}]",
                ErrorCode::RequirementTagUnknown.id()
            ),
        ));
    }
    // Work items have no line: they are in the summary only.
    let conclusion = if trace.advisory_clear() && !links_truncated {
        CheckConclusion::Success
    } else {
        CheckConclusion::Neutral
    };
    completed(SPEC_TRACE_CHECK, conclusion, &lines, annotations)
}

/// The `check.spec_index` job's work (module docs).
#[derive(Clone)]
pub struct SpecChecks {
    db: Db,
    browse: Browser,
    projects: Arc<dyn ProjectAccessLoader>,
    budget: Duration,
    limits: IndexLimits,
    doc_limits: DocLimits,
}

impl std::fmt::Debug for SpecChecks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpecChecks").finish_non_exhaustive()
    }
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    Recorded {
        lint: Box<Reported>,
        trace: Box<Reported>,
        /// `forge/doc-lint` (plan 13 stage 2).
        doc_lint: Box<Reported>,
    },
    /// The project or the merge request is gone: nothing to record.
    Gone,
}

fn timed_out(operation: &'static str, limit: Duration) -> ForgeError {
    ForgeError::new(ErrorCode::RequestTimeout)
        .with_context("operation", operation)
        .with_context("timeout_ms", limit.as_millis())
}

async fn within<T>(
    operation: &'static str,
    limit: Duration,
    work: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    tokio::time::timeout(limit, work)
        .await
        .unwrap_or_else(|_| Err(timed_out(operation, limit)))
}

/// Write `new` on `request`'s head within [`WRITE_TIMEOUT`].
async fn record(
    db: &Db,
    request: &SpecIndexRequest,
    request_id: &RequestId,
    new: &NewCheckRun,
) -> Result<Reported, ForgeError> {
    let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the change owns its id
    within(
        "spec_index_record",
        WRITE_TIMEOUT,
        record_forge_check_run(db, &ctx, (request.project, &request.head), new),
    )
    .await
}

/// Everything read before the runs are assessed.
struct Evaluated {
    head: Indexed,
    docs: DocTree,
    trace: MrTrace,
    compared: Compared,
    links_truncated: bool,
}

impl SpecChecks {
    /// `projects` is forge-db's (`Db` implements it); tests inject others.
    pub fn new(db: Db, browse: Browser, projects: Arc<dyn ProjectAccessLoader>) -> Self {
        Self {
            db,
            browse,
            projects,
            budget: EVALUATION_BUDGET,
            limits: IndexLimits::default(),
            doc_limits: DocLimits::default(),
        }
    }

    /// The same, with other bounds for the documents (tests).
    #[must_use]
    pub fn with_doc_limits(mut self, doc_limits: DocLimits) -> Self {
        self.doc_limits = doc_limits;
        self
    }

    /// The same, with another evaluation budget and index bounds (tests).
    #[must_use]
    pub fn with_bounds(mut self, budget: Duration, limits: IndexLimits) -> Self {
        self.budget = budget;
        self.limits = limits;
        self
    }

    /// Evaluate `request` and record the three runs (`forge/spec-lint`,
    /// `forge/spec-trace`, `forge/doc-lint`). Errors (nothing written, or
    /// only the first runs when a later write fails — the retry writes all
    /// again): a lookup (`FG-DB-*`, `FG-WEB-0002`), the
    /// repository not ready (`FG-REPO-0016`), git (`FG-GIT-*`, the browse
    /// limits), the budget (`FG-WEB-0002 operation=spec_index`), a write.
    pub async fn run(
        &self,
        request: &SpecIndexRequest,
        request_id: &RequestId,
    ) -> Result<Ran, ForgeError> {
        let evaluated = within(
            "spec_index",
            self.budget,
            self.evaluate(request, request_id),
        )
        .await?;
        let Some(e) = evaluated else {
            return Ok(Ran::Gone);
        };
        let lint_run = assess_lint(&e.head)?;
        let trace_run = assess_trace(&e.trace, &e.compared, e.links_truncated)?;
        let doc_run = assess_doc_lint(&e.docs)?;
        let lint = record(&self.db, request, request_id, &lint_run).await?;
        let trace = record(&self.db, request, request_id, &trace_run).await?;
        let doc_lint = record(&self.db, request, request_id, &doc_run).await?;
        tracing::info!(
            target: "forge_checks",
            event = "forge.spec_index.recorded",
            request_id = %request_id,
            project_id = request.project.get(),
            iid = request.iid,
            head_sha = request.head.as_str(),
            lint_conclusion = lint_run.conclusion.map(CheckConclusion::as_str),
            trace_conclusion = trace_run.conclusion.map(CheckConclusion::as_str),
            doc_lint_conclusion = doc_run.conclusion.map(CheckConclusion::as_str),
            doc_files = e.docs.markdown.len(),
            doc_incomplete = e.docs.incomplete,
            documents = e.head.index.documents.len(),
            tags = e.head.index.tags.len(),
            incomplete = e.head.index.incomplete,
            "spec lint, trace and doc lint evaluated"
        );
        Ok(Ran::Recorded {
            lint: Box::new(lint),
            trace: Box::new(trace),
            doc_lint: Box::new(doc_lint),
        })
    }

    fn gone(request: &SpecIndexRequest, request_id: &RequestId, what: &'static str) {
        tracing::info!(
            target: "forge_checks",
            event = "forge.spec_index.gone",
            request_id = %request_id,
            project_id = request.project.get(),
            iid = request.iid,
            head_sha = request.head.as_str(),
            gone = what,
            "nothing to evaluate; no spec lint, trace or doc lint run recorded"
        );
    }

    async fn evaluate(
        &self,
        request: &SpecIndexRequest,
        request_id: &RequestId,
    ) -> Result<Option<Evaluated>, ForgeError> {
        let project = request.project;
        let loaded = within(
            "project_load",
            LOOKUP_TIMEOUT,
            self.projects.load_project(project, None),
        )
        .await?;
        let Some(record) = loaded else {
            Self::gone(request, request_id, "project");
            return Ok(None);
        };
        let repo = record.repository;
        if repo.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", repo.id.get())
                .with_context("project_id", project.get())
                .with_context("state", repo.state.as_str()));
        }
        let mr = within(
            "merge_request_load",
            LOOKUP_TIMEOUT,
            forge_db::review::get(&self.db, project, request.iid),
        )
        .await?;
        let Some(mr) = mr else {
            Self::gone(request, request_id, "merge_request");
            return Ok(None);
        };
        let links = within(
            "work_item_links",
            LOOKUP_TIMEOUT,
            forge_db::work_items::links(&self.db, mr.id),
        )
        .await?;
        let mut linked: Vec<WorkItemRef> = links
            .links
            .iter()
            .filter(|l| l.unlinked_at.is_none())
            .map(|l| l.work_item.reference.clone()) // clone: the trace owns its refs
            .collect();
        linked.sort();
        linked.dedup();
        // One read of the head for both the index and the documents.
        let (head, docs) = index_head(
            &self.browse,
            repo.id,
            request.head.as_str(),
            &self.limits,
            &self.doc_limits,
        )
        .await?;
        let compared = match self.browse.find_ref(repo.id, &mr.target_branch).await? {
            RefLookup::Commit(tip) => {
                let tip = ObjectId::parse(&tip.commit).map_err(|_| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "resolved_commit_invalid")
                        .with_context("repo_id", repo.id.get())
                })?;
                match self.browse.merge_base(repo.id, &request.head, &tip).await? {
                    Some(base) => Compared::MergeBase(base),
                    None => Compared::Nothing,
                }
            }
            RefLookup::Missing | RefLookup::NotACommit => Compared::Nothing,
        };
        let base = match &compared {
            Compared::MergeBase(b) if b == &request.head => head.index.clone(), // clone: the same commit
            Compared::MergeBase(b) => {
                index_commit(&self.browse, repo.id, b.as_str(), &self.limits, Tags::Read)
                    .await?
                    .index
            }
            Compared::Nothing => SpecIndex::default(),
        };
        let trace = trace_merge_request(&base, &head.index, &linked, &head.external);
        Ok(Some(Evaluated {
            head,
            docs,
            trace,
            compared,
            links_truncated: links.truncated,
        }))
    }

    /// The job failed for good (its last attempt, or a failure that can
    /// never succeed): record `error` on the head for its three checks, so
    /// they are never left without an answer.
    pub async fn fail(
        &self,
        request: &SpecIndexRequest,
        request_id: &RequestId,
        error: &ForgeError,
    ) {
        record_errors(&self.db, request, request_id, error.code().id()).await;
    }
}

/// Record `error` for `forge/spec-lint`, `forge/spec-trace` and
/// `forge/doc-lint` on
/// `request`'s head (the job's last failure: `code`): [`SpecChecks::fail`]
/// and the worker's sweep of a job that died without an answer. Each write
/// is bounded by [`WRITE_TIMEOUT`]; a failure to record is logged here
/// (`error`). Whether both were recorded.
pub async fn record_errors(
    db: &Db,
    request: &SpecIndexRequest,
    request_id: &RequestId,
    code: &str,
) -> bool {
    let mut all = true;
    for check in [SPEC_LINT_CHECK, SPEC_TRACE_CHECK, DOC_LINT_CHECK] {
        let new = name(check).map(|name| NewCheckRun {
            name,
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Error),
            summary: format!(
                "{check} could not be evaluated [{code}] (request {request_id}). An operator can \
                 find the cause in the worker's log by this request id."
            ),
            details_url: None,
            annotations: Vec::new(),
        });
        let recorded = match new {
            Ok(new) => record(db, request, request_id, &new).await.map(drop),
            Err(e) => Err(e),
        };
        if let Err(e) = recorded {
            all = false;
            tracing::error!(
                target: "forge_checks",
                event = "forge.spec_index.error_unrecorded",
                error_code = e.code().id(),
                error = %e,
                job_error_code = code,
                check,
                request_id = %request_id,
                project_id = request.project.get(),
                iid = request.iid,
                head_sha = request.head.as_str(),
                "the failed spec or doc check could not be recorded as error"
            );
        }
    }
    all
}

#[cfg(test)]
#[path = "spec_checks_tests.rs"]
mod tests;
