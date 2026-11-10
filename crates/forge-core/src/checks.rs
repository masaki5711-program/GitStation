//! Check runs (M3 C1, ADR-0024 §10, §11; plan 02 §5.4): the status and
//! conclusion of a check on one commit, the runs of a commit as one
//! snapshot ([`CheckRunsOnHead`]), and the source the merge conditions read
//! them from ([`CheckResultsSource`]).
//!
//! The trait lives here so forge-review (which decides merges) never
//! depends on forge-checks (which records the runs): both are feature
//! crates and talk only through forge-core (CLAUDE.md §3-9). forge-db
//! implements it on `Db` with the same SQL the merge gate runs inside its
//! transaction; `forge serve` injects it (`forge worker` builds no
//! `Review`: the merge job runs inside `forge serve`).

use std::collections::BTreeMap;

use crate::ids::ProjectId;
use crate::merge_conditions::CheckName;
use crate::namespace_resolver::LookupFuture;
use crate::push::ObjectId;

/// The most check runs (distinct names) one commit of one project holds
/// (`FG-REVIEW-0021` CheckRunLimitReached for a new name beyond it). It
/// bounds every read of a commit's runs, the merge gate's included.
pub const MAX_CHECK_RUNS_PER_COMMIT: usize = 100;
/// The most annotations one check run carries (`FG-REVIEW-0020`
/// CheckPayloadInvalid beyond it; the table's CHECK is the backstop).
pub const MAX_CHECK_ANNOTATIONS: usize = 50;
/// The longest summary of a check run, in bytes (64 KiB).
pub const MAX_CHECK_SUMMARY_BYTES: usize = 64 * 1024;
/// The longest `details_url` of a check run, in bytes (2 KiB).
pub const MAX_CHECK_DETAILS_URL_BYTES: usize = 2 * 1024;

/// `forge/spec-drift` (M3 X2, plan 03 §5.4): whether the specification
/// versions a commit's `.forge/repo.yml` pins are the latest published
/// (`success`) or not (`neutral`). Reported by forge's worker only.
pub const SPEC_DRIFT_CHECK: &str = "forge/spec-drift";

/// `forge/risk` (M3 X7, plan 04 M-6 layer 1): the deterministic risk rules
/// ([`crate::risk`]) applied to a merge request head's diff. `success` — no
/// finding; `neutral` — only warnings (or part of the head could not be
/// assessed); `failure` — a `stop` rule. Reported by forge's worker only.
pub const RISK_CHECK: &str = "forge/risk";

/// The `forge/` names forge's own worker reports (never over HTTP: the
/// `CheckWrite` route refuses `forge/` to everyone, ADR-0024 §10). A
/// required `forge/` name outside this list has no reporter.
pub const FORGE_REPORTED_CHECKS: &[&str] = &[
    SPEC_DRIFT_CHECK,
    RISK_CHECK,
    SPEC_LINT_CHECK,
    SPEC_TRACE_CHECK,
    DOC_LINT_CHECK,
    CI_LINT_CHECK,
];

/// `forge/spec-lint` (M3 X4, plan 03 §5.4): whether the commit's own
/// specification documents and the `@req` tags of its families keep the
/// rules (`success`) or not (`failure`; `neutral` when the index hit a
/// bound). Reported by forge's worker only.
pub const SPEC_LINT_CHECK: &str = "forge/spec-lint";

/// `forge/spec-trace` (M3 X4, plan 03 §5.4, ADR-0017 §4.1.1 started as
/// advice — plan 03 §9-4): whether every requirement a merge request
/// touches has a `@req` test at its head (`success`) or not (`neutral`,
/// never `failure` while advisory). Reported by forge's worker only.
pub const SPEC_TRACE_CHECK: &str = "forge/spec-trace";

/// `forge/doc-lint` (plan 13 stage 2, ADR-0026): whether the commit's
/// documents (`.md` naming a `doctype`, in any folder) keep their types'
/// rules (`success`) or not (`failure`; `neutral` when reading them hit a
/// bound). Written by the `check.spec_index` job beside `forge/spec-lint`.
/// Reported by forge's worker only.
pub const DOC_LINT_CHECK: &str = "forge/doc-lint";

/// `forge/ci-lint` (plan 14 C1): whether the commit's `.forge-ci.yml`
/// keeps forge-ci's rules, checked against the repository's protected
/// branches (`success`; also when there is no such file: nothing to check)
/// or not (`failure`, `FG-CI-0001`). Written by the `check.spec_index` job
/// beside `forge/doc-lint`. Reported by forge's worker only.
pub const CI_LINT_CHECK: &str = "forge/ci-lint";

/// The most runs one commit holds in all: the reporters' cap
/// ([`MAX_CHECK_RUNS_PER_COMMIT`], which counts no `forge/` name) plus one
/// per name forge reports itself (review X2: other reporters can never
/// crowd forge's own run out). Every read of a commit's runs is bounded by
/// it.
pub const MAX_STORED_CHECK_RUNS_PER_COMMIT: usize =
    MAX_CHECK_RUNS_PER_COMMIT + FORGE_REPORTED_CHECKS.len();

/// Whether forge's own worker reports `name` ([`FORGE_REPORTED_CHECKS`]).
pub fn forge_reports(name: &CheckName) -> bool {
    FORGE_REPORTED_CHECKS.contains(&name.as_str())
}

/// A check run's status (ADR-0024 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

impl CheckStatus {
    pub const ALL: [CheckStatus; 3] = [Self::Queued, Self::InProgress, Self::Completed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    /// The status named `raw` (its `as_str`); `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }
}

/// A completed check run's conclusion (ADR-0024 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckConclusion {
    Success,
    Neutral,
    Failure,
    Error,
    ActionRequired,
}

impl CheckConclusion {
    pub const ALL: [CheckConclusion; 5] = [
        Self::Success,
        Self::Neutral,
        Self::Failure,
        Self::Error,
        Self::ActionRequired,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Neutral => "neutral",
            Self::Failure => "failure",
            Self::Error => "error",
            Self::ActionRequired => "action_required",
        }
    }

    /// The conclusion named `raw` (its `as_str`); `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == raw)
    }
}

/// The run of a check on a commit as the merge conditions see it (one row
/// per name, the latest report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckRun {
    pub status: CheckStatus,
    /// `Some` only when `completed`.
    pub conclusion: Option<CheckConclusion>,
}

impl CheckRun {
    /// A run from its parts: `None` unless the conclusion is present
    /// exactly when the status is `completed` (the table's CHECK).
    pub fn new(status: CheckStatus, conclusion: Option<CheckConclusion>) -> Option<Self> {
        ((status == CheckStatus::Completed) == conclusion.is_some())
            .then_some(Self { status, conclusion })
    }

    /// Whether it satisfies a required check: `completed` and `success`
    /// only (ADR-0024 §10: `neutral` does not, nor a run still going).
    pub fn succeeded(&self) -> bool {
        self.status == CheckStatus::Completed && self.conclusion == Some(CheckConclusion::Success)
    }
}

/// Every check run of one commit of one project, by name (at most
/// [`MAX_CHECK_RUNS_PER_COMMIT`]). The merge conditions look a required
/// name up here: absent means "no run reported", which is never success.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckRunsOnHead {
    head: Option<ObjectId>,
    runs: BTreeMap<CheckName, CheckRun>,
}

impl CheckRunsOnHead {
    /// The runs of `head` (later entries of one name replace earlier ones;
    /// the store holds one row per name anyway).
    pub fn new(head: ObjectId, runs: impl IntoIterator<Item = (CheckName, CheckRun)>) -> Self {
        Self {
            head: Some(head),
            runs: runs.into_iter().collect(),
        }
    }

    /// The run of `name` on `head`; `None` when none was reported — or when
    /// `head` is not the commit these runs belong to (a run on an old head
    /// never counts for a new one, ADR-0024 §10).
    pub fn run(&self, head: &ObjectId, name: &CheckName) -> Option<CheckRun> {
        if self.head.as_ref() != Some(head) {
            return None;
        }
        self.runs.get(name).copied()
    }

    /// The commit the runs belong to (`None` for [`Default`]: no runs).
    pub fn head(&self) -> Option<&ObjectId> {
        self.head.as_ref()
    }

    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

/// Reads the check runs of a commit (forge-db implements it on `Db`; the
/// merge status reads through it, `forge serve` injects
/// it). One bounded statement; a stored row this build cannot read is
/// `FG-DB-0022` CheckRunDataInvalid and a failed or timed-out read its own
/// `FG-DB-*` error — never an empty snapshot (a failure is not "no run").
pub trait CheckResultsSource: Send + Sync {
    /// Every run on `head` of `project`.
    fn runs_on<'a>(
        &'a self,
        project: ProjectId,
        head: &'a ObjectId,
    ) -> LookupFuture<'a, CheckRunsOnHead>;
}

/// An annotation's level (`check_annotations.level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckAnnotationLevel {
    Notice,
    Warning,
    Failure,
}

impl CheckAnnotationLevel {
    pub const ALL: [CheckAnnotationLevel; 3] = [Self::Notice, Self::Warning, Self::Failure];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notice => "notice",
            Self::Warning => "warning",
            Self::Failure => "failure",
        }
    }

    /// The level named `raw` (its `as_str`); `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.as_str() == raw)
    }
}

/// The audit details of one check run report (M3 C1, on the project):
/// `check_run.reported` (audit only) when the conclusion stayed what it
/// was, `check.completed` (an event of the integration class,
/// deduplicated by `check_completed:<check_run_id>:<revision>`) when the
/// report changed it to a new conclusion (ADR-0024 §10: only then).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRunChange {
    pub project: ProjectId,
    pub check_run_id: i64,
    pub head_sha: ObjectId,
    pub name: CheckName,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    /// The conclusion before this report (`None`: none, or a new run).
    pub previous_conclusion: Option<CheckConclusion>,
    /// The report created the run.
    pub created: bool,
    /// The run's revision after this report.
    pub revision: i64,
}

impl CheckRunChange {
    /// Whether the report changed the run to a new conclusion — the only
    /// case that publishes `check.completed`.
    pub fn completed(&self) -> bool {
        self.conclusion.is_some() && self.conclusion != self.previous_conclusion
    }

    pub fn action(&self) -> &'static str {
        if self.completed() {
            "check.completed"
        } else {
            "check_run.reported"
        }
    }

    /// `{"project_id","check_run_id","head_sha","name","status","conclusion",
    /// "previous_conclusion","created","revision"}` (no summary, URL or
    /// annotation text: they are free text).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "project_id": self.project.get(),
            "check_run_id": self.check_run_id,
            "head_sha": self.head_sha.as_str(),
            "name": self.name.as_str(),
            "status": self.status.as_str(),
            "conclusion": self.conclusion.map(CheckConclusion::as_str),
            "previous_conclusion": self.previous_conclusion.map(CheckConclusion::as_str),
            "created": self.created,
            "revision": self.revision,
        })
    }
}
