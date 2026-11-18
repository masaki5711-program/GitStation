//! Auto-merge (M4 C9; plan 04 M-5 / M-6; ADR-0017 Amendments 2026-10-30,
//! 2026-11-03, 2026-11-12 and 2026-11-13). Pure: what the auto-merge
//! principal does with one merge request, from facts read elsewhere.
//!
//! - [`AutoMergeMode`]: a project's `merge.auto` (`off` / `reserve` /
//!   `conditional` / `full`). New projects are `full`; projects that
//!   existed before C9 keep `off` (what they effectively had).
//! - The stop classes ([`StopClass`]): auto-merge stops **only** on
//!   program breakage, database destruction / data loss, a leak, code that
//!   runs on developers' PCs (the four `stop` classes of `forge/risk` and a
//!   red check), on a head whose risk cannot be shown (`not_assessed`) and
//!   on the emergency stop. A `warn` never stops.
//! - The risk layers ([`deterministic_layer`], [`combine`]): the AI layer
//!   (M5) can only raise what the deterministic layer found, never lower it,
//!   and an AI that does not answer is a stop.
//! - [`decide`]: the verdict for one merge request.
//! - The instance's `merge.allow_full_auto` (M4 close): while an
//!   administrator does not allow it, `full` cannot be selected and a project
//!   set to `full` is effectively `reserve` ([`AutoMergeMode::effective`]):
//!   its merge requests show `held reason=full_auto_not_allowed`.
//! - Documents ([`DocLintState`], ADR-0030 Amendment 2026-11-17): a
//!   `forge/doc-lint` that **failed** on the head stops auto-merge like a
//!   red required check (`doc_lint_failed`), whether or not a Maintainer
//!   made it a required check; it fails only on a document (a `.md` naming
//!   a `doctype`) or a type definition with a problem, so a repository
//!   without documents (`success`) is never stopped. No run on the head
//!   yet, or a run that ended `error`, is a wait (`doc_lint_pending`) while
//!   auto-merge asks for the run again; past [`DOC_LINT_WAIT_LIMIT`] it
//!   stops `not_assessed` (`doc_lint_missing` / `doc_lint_error`), never an
//!   endless wait. `neutral` (a bound hit, no problem) does not stop. A
//!   person's merge is unchanged; a branch's required checks decide that.
//! - The merge result ([`MergeResultAssessor`], [`merge_result_decision`]):
//!   the merge queue judges `forge/risk`'s rules again on the combined tree
//!   it will write to the target, so a stop that only the combination shows
//!   stops the auto-merge entry that brings it in.

use std::future::Future;
use std::pin::Pin;

use forge_errors::ForgeError;

use crate::checks::{CheckConclusion, CheckRun, CheckStatus};
use crate::risk::{RiskLevel, RULES};
use crate::{ObjectId, ProjectId, RefName, RepoId};

/// The instance's `merge.allow_full_auto` before an administrator changes
/// it: allowed (the 2026-11-03 user decision, new projects are `full`).
pub const ALLOW_FULL_AUTO_DEFAULT: bool = true;

/// A project's `merge.auto` (plan 04 M-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutoMergeMode {
    /// Nothing automatic: a person merges.
    Off,
    /// A person's "merge when ready" (the merge queue, C8) only.
    Reserve,
    /// As `full`, except a change touching a protected path waits for a
    /// person ([`is_protected_path`]).
    Conditional,
    /// Every mergeable change is queued by the auto-merge principal; only
    /// the stop classes and the emergency stop hold it.
    Full,
}

impl AutoMergeMode {
    pub const ALL: [AutoMergeMode; 4] = [Self::Off, Self::Reserve, Self::Conditional, Self::Full];

    /// The mode of a project created after C9 (2026-11-03 user decision).
    pub const NEW_PROJECT: AutoMergeMode = Self::Full;
    /// The mode C9's migration gives every project that already existed:
    /// what those projects effectively had (nothing merged by itself).
    pub const EXISTING_PROJECT: AutoMergeMode = Self::Off;

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Reserve => "reserve",
            Self::Conditional => "conditional",
            Self::Full => "full",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == raw)
    }

    /// Whether the auto-merge principal acts in this mode.
    pub fn acts(self) -> bool {
        matches!(self, Self::Conditional | Self::Full)
    }

    /// The mode in effect under the instance's `merge.allow_full_auto`:
    /// `full` while it is not allowed is `reserve` (the safest mode that
    /// still lets a person press "merge when ready"; ADR-0017 Amendment
    /// 2026-11-03 B). Every other mode is itself.
    pub fn effective(self, full_allowed: bool) -> Self {
        if self == Self::Full && !full_allowed {
            Self::Reserve
        } else {
            self
        }
    }
}

/// Where an emergency stop was turned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopScope {
    /// An instance administrator: every project.
    Instance,
    /// A project Maintainer: this project.
    Project,
}

impl StopScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::Project => "project",
        }
    }
}

/// Why auto-merge stopped (the closed list; nothing else stops it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StopClass {
    /// A red required check or pipeline, the merge request alone red in
    /// the merge queue, a mass deletion, a change too large to assess.
    ProgramBreakage,
    /// An applied migration changed, a destructive statement, a data
    /// lifecycle job, a rollback step, critical content unread.
    DataLoss,
    /// A secret by name or by content.
    Leak,
    /// `agent_config` / `submodule_change` (ADR-0017 Amendment 2026-11-12).
    DeveloperPc,
    /// The head's risk cannot be shown (`forge/risk` errored, or failed
    /// without a rule forge knows; the AI layer did not answer).
    NotAssessed,
    /// The instance or the project emergency stop.
    EmergencyStop,
}

impl StopClass {
    pub const ALL: [StopClass; 6] = [
        Self::ProgramBreakage,
        Self::DataLoss,
        Self::Leak,
        Self::DeveloperPc,
        Self::NotAssessed,
        Self::EmergencyStop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProgramBreakage => "program_breakage",
            Self::DataLoss => "data_loss",
            Self::Leak => "leak",
            Self::DeveloperPc => "developer_pc",
            Self::NotAssessed => "not_assessed",
            Self::EmergencyStop => "emergency_stop",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == raw)
    }
}

/// The class of a `forge/risk` rule of level `stop` (`None` for a rule
/// that is not a `stop` rule, or unknown).
pub fn rule_class(rule: &str) -> Option<StopClass> {
    let class = match rule {
        "mass_deletion_lines" | "mass_deletion_files" | "too_many_changes" => {
            StopClass::ProgramBreakage
        }
        "applied_migration_changed"
        | "sql_drop"
        | "sql_truncate"
        | "sql_unbounded_write"
        | "sql_column_change"
        | "sql_drop_constraint"
        | "sql_disable_guard"
        | "deploy_rollback"
        | "data_lifecycle_job"
        | "critical_content_unread" => StopClass::DataLoss,
        // Plan 17 §1.7: raw data in an AI evaluation case is a leak (the
        // third class; no new stop class).
        "secret_file" | "secret_in_content" | "ai_case_raw_data" => StopClass::Leak,
        "agent_config" | "submodule_change" => StopClass::DeveloperPc,
        _ => return None,
    };
    RULES
        .iter()
        .any(|r| r.id == rule && r.level == RiskLevel::Stop)
        .then_some(class)
}

/// The `stop` rules a `forge/risk` run's summary names (its `- stop
/// <rule>` lines, written by forge's own worker), known rules only, each
/// once, in summary order.
pub fn stop_rules(summary: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    for line in summary.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- stop ") else {
            continue;
        };
        let word = rest.split_whitespace().next().unwrap_or("");
        if let Some(rule) = RULES
            .iter()
            .find(|r| r.id == word && r.level == RiskLevel::Stop)
        {
            if !found.contains(&rule.id) {
                found.push(rule.id);
            }
        }
    }
    found
}

/// The `warn` rules of `forge/risk` that mean part of the head was not
/// assessed (`FG-REVIEW-0025`: no open merge request, the target missing,
/// no common history, a path that is no UTF-8, files the content scan did
/// not read, targets not assessed). A run naming one of them never shows the
/// head safe, so auto-merge treats it as `not_assessed` (C9 security review).
pub const NOT_ASSESSED_RULES: &[&str] = &[
    "no_open_merge_request",
    "target_missing",
    "no_common_history",
    "non_utf8_path",
    "content_not_scanned",
    "targets_not_assessed",
];

/// The `warn` rules a `forge/risk` run's summary names (its `- warn
/// <rule>` lines), known rules only, each once, in summary order.
pub fn warn_rules(summary: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    for line in summary.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- warn ") else {
            continue;
        };
        let word = rest.split_whitespace().next().unwrap_or("");
        if let Some(rule) = RULES
            .iter()
            .find(|r| r.id == word && r.level == RiskLevel::Warn)
        {
            if !found.contains(&rule.id) {
                found.push(rule.id);
            }
        }
    }
    found
}

/// What a risk layer says about a head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskVerdict {
    /// No answer yet (no run, or still running): wait.
    Pending,
    /// Nothing that stops; `warned` when a `warn` was found.
    Clear { warned: bool },
    /// A stop class; `rule` the `forge/risk` rule that decided it.
    Stop {
        class: StopClass,
        rule: Option<&'static str>,
    },
}

/// The deterministic layer (`forge/risk`, plan 04 M-6 layer 1) from its run
/// on the head and the run's summary: `success` clear, `neutral` clear with
/// a warning only when the summary names known `warn` rules and none of
/// [`NOT_ASSESSED_RULES`] (a partly assessed head, or a `neutral` whose
/// rules cannot be read, is `not_assessed`: C9 security review), `failure` a stop of the worst class its rules name
/// (`not_assessed` when it names none forge knows), `error` /
/// `action_required` `not_assessed`.
pub fn deterministic_layer(run: Option<CheckRun>, summary: Option<&str>) -> RiskVerdict {
    let Some(run) = run else {
        return RiskVerdict::Pending;
    };
    if run.status != CheckStatus::Completed {
        return RiskVerdict::Pending;
    }
    match run.conclusion {
        None => RiskVerdict::Pending,
        Some(CheckConclusion::Success) => RiskVerdict::Clear { warned: false },
        Some(CheckConclusion::Neutral) => {
            let warns = summary.map(warn_rules).unwrap_or_default();
            let partial = warns.iter().find(|r| NOT_ASSESSED_RULES.contains(r));
            match (warns.is_empty(), partial) {
                (false, None) => RiskVerdict::Clear { warned: true },
                (_, Some(rule)) => RiskVerdict::Stop {
                    class: StopClass::NotAssessed,
                    rule: Some(rule),
                },
                (true, None) => RiskVerdict::Stop {
                    class: StopClass::NotAssessed,
                    rule: None,
                },
            }
        }
        Some(CheckConclusion::Error | CheckConclusion::ActionRequired) => RiskVerdict::Stop {
            class: StopClass::NotAssessed,
            rule: None,
        },
        Some(CheckConclusion::Failure) => {
            let rules = summary.map(stop_rules).unwrap_or_default();
            let worst = rules
                .iter()
                .filter_map(|r| rule_class(r).map(|c| (severity(c), c, *r)))
                .max_by_key(|(rank, _, _)| *rank);
            match worst {
                Some((_, class, rule)) => RiskVerdict::Stop {
                    class,
                    rule: Some(rule),
                },
                None => RiskVerdict::Stop {
                    class: StopClass::NotAssessed,
                    rule: None,
                },
            }
        }
    }
}

/// Which class a merge request shows when several rules hit (the harm
/// hardest to undo first).
fn severity(class: StopClass) -> u8 {
    match class {
        StopClass::Leak => 6,
        StopClass::DataLoss => 5,
        StopClass::DeveloperPc => 4,
        StopClass::ProgramBreakage => 3,
        StopClass::NotAssessed => 2,
        StopClass::EmergencyStop => 1,
    }
}

/// The local AI's risk answer (plan 04 M-6 layer 2; the LLM gateway lands
/// in M5 — until then no layer-2 answer exists and `combine` is given
/// `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRisk {
    /// No answer, a timeout or malformed output: a stop (never `ok`).
    Unavailable,
    Ok,
    Warn,
    Stop(StopClass),
}

/// Both layers together: the AI can only raise. A deterministic stop
/// stays whatever the AI says; an AI stop or an AI that did not answer
/// stops a head the rules let through; an AI `ok` changes nothing.
pub fn combine(deterministic: RiskVerdict, ai: Option<AiRisk>) -> RiskVerdict {
    if let RiskVerdict::Stop { .. } = deterministic {
        return deterministic;
    }
    match ai {
        Some(AiRisk::Unavailable) => RiskVerdict::Stop {
            class: StopClass::NotAssessed,
            rule: None,
        },
        Some(AiRisk::Stop(class)) => RiskVerdict::Stop { class, rule: None },
        Some(AiRisk::Warn) => match deterministic {
            RiskVerdict::Clear { .. } => RiskVerdict::Clear { warned: true },
            other @ (RiskVerdict::Pending | RiskVerdict::Stop { .. }) => other,
        },
        Some(AiRisk::Ok) | None => deterministic,
    }
}

/// A path `conditional` keeps for a person (plan 04 M-5, ADR-0017
/// Amendment 2026-10-30 B): a migration, forge's own configuration
/// (`.forge/`, `.forge-ci.yml`), a CODEOWNERS file, or authentication /
/// permission code by name. `full` has no such exception.
pub fn is_protected_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if crate::risk::is_migration_path(path) {
        return true;
    }
    if lower.starts_with(".forge/") || lower == ".forge-ci.yml" {
        return true;
    }
    let file = lower.rsplit('/').next().unwrap_or("");
    if file == "codeowners" {
        return true;
    }
    lower
        .split('/')
        .any(|segment| matches!(segment, "auth" | "authz" | "permissions" | "acl"))
}

/// The merge request's own `ci/pipeline` on its current merge result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    /// No run (no `.forge-ci.yml`, or not reported yet): the queue's train
    /// tests it.
    None,
    Running,
    Green,
    Red,
}

impl PipelineState {
    /// From the resolved run (fresh runs only; a stale run is `None`).
    pub fn of(run: Option<CheckRun>) -> Self {
        match run {
            None => Self::None,
            Some(r) if r.status != CheckStatus::Completed => Self::Running,
            Some(r) => match r.conclusion {
                Some(CheckConclusion::Success) => Self::Green,
                Some(
                    CheckConclusion::Failure | CheckConclusion::Error | CheckConclusion::Neutral,
                ) => Self::Red,
                Some(CheckConclusion::ActionRequired) | None => Self::Running,
            },
        }
    }
}

/// How long auto-merge waits for `forge/doc-lint` on a head (no run, or
/// a run that ended `error`) before it stops with `doc_lint_missing` /
/// `doc_lint_error` (ADR-0030 Amendment 2026-11-17).
pub const DOC_LINT_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(600);

/// The waiting reason while `forge/doc-lint` is asked for again.
pub const DOC_LINT_PENDING: &str = "doc_lint_pending";
/// The stop past [`DOC_LINT_WAIT_LIMIT`] without a run.
pub const DOC_LINT_MISSING: &str = "doc_lint_missing";
/// The stop past [`DOC_LINT_WAIT_LIMIT`] while the run ends `error`.
pub const DOC_LINT_ERROR: &str = "doc_lint_error";

/// `forge/doc-lint` on the head as auto-merge reads it (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocLintState {
    /// No completed run on the head yet: ask for it, wait.
    Pending,
    /// The run itself failed (`error`; doc-lint never writes
    /// `action_required`, so one is read the same): ask again, wait.
    Errored,
    /// `success` or `neutral` (a bound hit, no problem found).
    Clear,
    /// `failure`: a document or a type definition has a problem.
    Failed,
    /// Still [`Self::Pending`] / [`Self::Errored`] past the wait limit.
    Overdue { errored: bool },
}

impl DocLintState {
    /// From the head's run (only runs on the head count).
    pub fn of(run: Option<CheckRun>) -> Self {
        match run {
            Some(r) if r.status == CheckStatus::Completed => match r.conclusion {
                Some(CheckConclusion::Failure) => Self::Failed,
                Some(CheckConclusion::Success | CheckConclusion::Neutral) => Self::Clear,
                Some(CheckConclusion::Error | CheckConclusion::ActionRequired) => Self::Errored,
                None => Self::Pending,
            },
            Some(_) | None => Self::Pending,
        }
    }

    /// Whether the run is still to come (auto-merge asks for it).
    pub fn needs_run(self) -> bool {
        matches!(self, Self::Pending | Self::Errored)
    }

    /// `self` after auto-merge has waited `waited` for it (`None`: it has
    /// not waited on this head yet): [`Self::Overdue`] at `limit` or more.
    #[must_use]
    pub fn after_waiting(
        self,
        waited: Option<std::time::Duration>,
        limit: std::time::Duration,
    ) -> Self {
        if self.needs_run() && waited.is_some_and(|w| w >= limit) {
            Self::Overdue {
                errored: self == Self::Errored,
            }
        } else {
            self
        }
    }
}

/// One reason `evaluate_merge()` gave (its stable name), and whether it is
/// a required check that concluded red (`check`: its name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub reason: &'static str,
    pub red_check: Option<String>,
}

/// The merge request's place in the merge queue, as the auto-merge sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueFacts {
    /// No live entry, and no auto-merge entry of this head that ended.
    None,
    /// A live entry (`auto`: the auto-merge principal's).
    Live { auto: bool },
    /// The auto-merge principal's entry of this very head left the queue
    /// with `reason`.
    Removed { reason: String },
}

/// Everything [`decide`] reads about one open merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub mode: AutoMergeMode,
    /// The instance's `merge.allow_full_auto`.
    pub full_allowed: bool,
    /// The emergency stop in effect (the instance's first).
    pub stop: Option<StopScope>,
    pub draft: bool,
    pub queue: QueueFacts,
    pub risk: RiskVerdict,
    pub pipeline: PipelineState,
    /// `forge/doc-lint` on the head (ADR-0030 Amendment 2026-11-17).
    pub doc_lint: DocLintState,
    /// `evaluate_merge()`'s reasons, `ci/pipeline` left to the queue.
    pub blockers: Vec<Blocker>,
    /// `conditional` only: a changed path is protected (or the changed
    /// paths could not be read).
    pub protected_path: bool,
}

/// What auto-merge does with the merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The mode does not act (`off` / `reserve`): nothing, no line.
    Inactive,
    /// Add it to the merge queue (as the auto-merge principal).
    Enqueue { warned: bool },
    /// It is in the queue (`auto`: by the auto-merge principal).
    Queued { auto: bool },
    /// Not yet: the reason names what it waits for.
    Waiting { reason: &'static str },
    /// A person took it out of the queue: auto-merge leaves this head to
    /// people (a new head is decided again).
    Held { reason: &'static str },
    /// Stopped (the closed list of [`StopClass`]).
    Stopped(Stop),
}

/// A stop: its class, the stable reason, and the rule or check behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub class: StopClass,
    pub reason: &'static str,
    pub detail: Option<String>,
}

impl Verdict {
    /// The stored state (`forge.auto_merge_states.state`).
    pub fn state(&self) -> Option<&'static str> {
        match self {
            Self::Inactive => None,
            Self::Enqueue { .. } | Self::Queued { .. } => Some("queued"),
            Self::Waiting { .. } => Some("waiting"),
            Self::Held { .. } => Some("held"),
            Self::Stopped(_) => Some("stopped"),
        }
    }
}

/// Queue removals of an auto-merge entry that keep the same head away
/// from the queue (a person's decision, or the head proven red alone).
fn sticky_removal(reason: &str) -> Option<Verdict> {
    match reason {
        "checks_failed" => Some(Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "merge_queue_checks_failed",
            detail: None,
        })),
        "dequeued" => Some(Verdict::Held {
            reason: "dequeued_by_person",
        }),
        // The merge result could not be judged, again and again: the head
        // cannot be shown safe (review MEDIUM).
        MERGE_RESULT_UNAVAILABLE => Some(Verdict::Stopped(Stop {
            class: StopClass::NotAssessed,
            reason: MERGE_RESULT_UNAVAILABLE,
            detail: None,
        })),
        other => merge_result_class(other).map(|class| {
            Verdict::Stopped(Stop {
                class,
                reason: "merge_result_risk",
                detail: None,
            })
        }),
    }
}

/// The queue removal reason of an auto-merge entry whose batch's merge
/// result could not be judged after repeated attempts (`FG-REVIEW-0026`,
/// `not_assessed`).
pub const MERGE_RESULT_UNAVAILABLE: &str = "merge_result_unavailable";

/// The queue removal reason of an auto-merge entry stopped on the merge
/// result (`merge_result_<class>`, a plain word the entry row holds).
pub fn merge_result_reason(class: StopClass) -> String {
    format!("merge_result_{}", class.as_str())
}

/// The class of a [`merge_result_reason`] (`None`: another reason).
pub fn merge_result_class(reason: &str) -> Option<StopClass> {
    reason
        .strip_prefix("merge_result_")
        .and_then(StopClass::parse)
}

/// The verdict for one open merge request (module docs). Order: the mode,
/// the emergency stop, `full` not allowed by the instance, the queue, a
/// draft, the risk layers, red checks, `forge/doc-lint`, then what it
/// waits for;
/// `conditional` keeps protected paths for a person.
pub fn decide(facts: &Facts) -> Verdict {
    if !facts.mode.acts() {
        return Verdict::Inactive;
    }
    if let Some(scope) = facts.stop {
        return Verdict::Stopped(Stop {
            class: StopClass::EmergencyStop,
            reason: match scope {
                StopScope::Instance => "instance_stopped",
                StopScope::Project => "project_stopped",
            },
            detail: None,
        });
    }
    // M4 close: `full` without the instance's allowance is `reserve` in
    // effect; the line says why nothing merges by itself.
    if !facts.mode.effective(facts.full_allowed).acts() {
        return Verdict::Held {
            reason: "full_auto_not_allowed",
        };
    }
    match &facts.queue {
        QueueFacts::Live { auto } => return Verdict::Queued { auto: *auto },
        QueueFacts::Removed { reason } => {
            if let Some(verdict) = sticky_removal(reason) {
                return verdict;
            }
        }
        QueueFacts::None => {}
    }
    if facts.draft {
        return Verdict::Waiting { reason: "draft" };
    }
    let warned = match &facts.risk {
        RiskVerdict::Stop { class, rule } => {
            return Verdict::Stopped(Stop {
                class: *class,
                reason: "risk_stop",
                detail: rule.map(str::to_owned),
            })
        }
        RiskVerdict::Pending => {
            return Verdict::Waiting {
                reason: "risk_pending",
            }
        }
        RiskVerdict::Clear { warned } => *warned,
    };
    if facts.pipeline == PipelineState::Red {
        return Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "pipeline_failed",
            detail: Some(crate::checks::CI_PIPELINE_CHECK.to_owned()),
        });
    }
    if let Some(red) = facts.blockers.iter().find_map(|b| b.red_check.as_ref()) {
        return Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "required_check_failed",
            detail: Some(red.clone()), // clone: the stop owns its detail
        });
    }
    match facts.doc_lint {
        DocLintState::Failed => {
            return Verdict::Stopped(Stop {
                class: StopClass::ProgramBreakage,
                reason: "doc_lint_failed",
                detail: Some(crate::checks::DOC_LINT_CHECK.to_owned()),
            })
        }
        DocLintState::Pending | DocLintState::Errored => {
            return Verdict::Waiting {
                reason: DOC_LINT_PENDING,
            }
        }
        DocLintState::Overdue { errored } => {
            return Verdict::Stopped(Stop {
                class: StopClass::NotAssessed,
                reason: if errored {
                    DOC_LINT_ERROR
                } else {
                    DOC_LINT_MISSING
                },
                detail: Some(crate::checks::DOC_LINT_CHECK.to_owned()),
            })
        }
        DocLintState::Clear => {}
    }
    if let Some(first) = facts.blockers.first() {
        return Verdict::Waiting {
            reason: first.reason,
        };
    }
    if facts.mode == AutoMergeMode::Conditional && facts.protected_path {
        return Verdict::Waiting {
            reason: "protected_path",
        };
    }
    Verdict::Enqueue { warned }
}

/// What the merge result ([`MergeResultAssessor`]) is judged on: the
/// combined tree the queue will write to `target`, as the diff from `base`
/// (the target's tip the train was built on) to `result` (a step of the
/// train: every merge request up to one, merged on `base`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeResultRange<'a> {
    pub project: ProjectId,
    pub repo: RepoId,
    pub target: &'a RefName,
    pub base: &'a ObjectId,
    pub result: &'a ObjectId,
}

/// The answer of a [`MergeResultAssessor`].
pub type MergeResultFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RiskVerdict, ForgeError>> + Send + 'a>>;

/// `forge/risk`'s deterministic rules on a merge result (M4 close): the
/// same rules, the same run and the same [`deterministic_layer`] as on a
/// head, applied to a [`MergeResultRange`] (forge-checks implements it; the
/// merge queue in forge-review calls it — two feature crates meet here).
/// The implementation bounds its own work and fails with its code.
pub trait MergeResultAssessor: Send + Sync {
    fn assess<'a>(&'a self, range: MergeResultRange<'a>) -> MergeResultFuture<'a>;
}

/// One step of a train as the merge result's decision reads it: whether
/// the entry is the auto-merge principal's, and the verdict on the merge
/// result up to and including it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultStep {
    pub auto: bool,
    pub risk: RiskVerdict,
}

/// What the merge queue does with a green train before it lands (M4
/// close).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResultDecision {
    /// Nothing an auto-merge entry brings in stops: land.
    Land,
    /// The auto-merge entry at `index` turns a clear merge result into a
    /// stop (alone, or only combined with the entries before it): it
    /// leaves the queue stopped with `class`; the rest go back in order.
    Stop {
        index: usize,
        class: StopClass,
        rule: Option<&'static str>,
    },
    /// The merge result was already a stop before the auto-merge entry at
    /// `limit` (a person's entry brought it in; a person's merge is not
    /// stopped by auto-merge), or the combined result is only too large to
    /// assess ([`SIZE_LIMITED_RULES`]): the next batch ends before it, so
    /// the auto-merge entry is judged on a smaller merge result afterwards.
    Cut { limit: usize },
}

/// The `forge/risk` rules a merge result can hit only because the combined
/// change is large (the files counted or read are bounded): on a step after
/// the first they cut the batch instead of stopping the entry — the entry
/// is judged again on a smaller merge result, and stops there if it still
/// hits them.
pub const SIZE_LIMITED_RULES: &[&str] = &[
    "too_many_changes",
    "content_not_scanned",
    "critical_content_unread",
];

/// The decision on a train's steps in order (`steps[k]`: the merge result
/// of entries `0..=k`, every step of the train). An auto-merge entry stops
/// when its step is a stop and the step before was not; when the step
/// before was already a stop, or its own stop is only a size bound
/// ([`SIZE_LIMITED_RULES`]) on a step after the first, the batch is cut
/// before it. The final step (the tree the target would hold) is judged
/// too: a stop there that people's entries after the last auto-merge entry
/// bring in would carry the auto-merge content through, so the batch is cut
/// before the first auto-merge entry (at least one entry: that entry alone).
/// `Pending` cannot be shown safe and counts as a stop (`not_assessed`).
/// No auto-merge entry: [`MergeResultDecision::Land`].
pub fn merge_result_decision(steps: &[ResultStep]) -> MergeResultDecision {
    let mut before_stopped = false;
    for (index, step) in steps.iter().enumerate() {
        let stop = match step.risk {
            RiskVerdict::Clear { .. } => None,
            RiskVerdict::Stop { class, rule } => Some((class, rule)),
            RiskVerdict::Pending => Some((StopClass::NotAssessed, None)),
        };
        if step.auto {
            if before_stopped {
                return MergeResultDecision::Cut { limit: index };
            }
            if let Some((class, rule)) = stop {
                let size_only = rule.is_some_and(|r| SIZE_LIMITED_RULES.contains(&r));
                if index > 0 && size_only {
                    return MergeResultDecision::Cut { limit: index };
                }
                return MergeResultDecision::Stop { index, class, rule };
            }
        }
        before_stopped = stop.is_some();
    }
    if let Some(first_auto) = steps.iter().position(|s| s.auto) {
        if before_stopped {
            return MergeResultDecision::Cut {
                limit: first_auto.max(1),
            };
        }
    }
    MergeResultDecision::Land
}

/// A person's change of auto-merge (audited, no event; plan 04 M-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoMergeChange {
    /// `auto_merge.mode_changed` (resource the project).
    ModeChanged {
        project: crate::ids::ProjectId,
        from: AutoMergeMode,
        to: AutoMergeMode,
    },
    /// `auto_merge.stopped` (resource the project, or the instance when
    /// `project` is `None`).
    Stopped {
        project: Option<crate::ids::ProjectId>,
    },
    /// `auto_merge.resumed` (as `Stopped`).
    Resumed {
        project: Option<crate::ids::ProjectId>,
    },
    /// `auto_merge.allow_full_auto_changed` (resource the instance): an
    /// administrator allowed `full` (`allowed`) or took the allowance away.
    FullAutoAllowed { allowed: bool },
}

impl AutoMergeChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::ModeChanged { .. } => "auto_merge.mode_changed",
            Self::Stopped { .. } => "auto_merge.stopped",
            Self::Resumed { .. } => "auto_merge.resumed",
            Self::FullAutoAllowed { .. } => "auto_merge.allow_full_auto_changed",
        }
    }

    pub fn project(&self) -> Option<crate::ids::ProjectId> {
        match self {
            Self::ModeChanged { project, .. } => Some(*project),
            Self::Stopped { project } | Self::Resumed { project } => *project,
            Self::FullAutoAllowed { .. } => None,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::ModeChanged { project, from, to } => serde_json::json!({
                "project_id": project.get(),
                "from": from.as_str(),
                "to": to.as_str(),
            }),
            Self::Stopped { project } | Self::Resumed { project } => serde_json::json!({
                "scope": if project.is_some() { "project" } else { "instance" },
                "project_id": project.map(|p| p.get()),
            }),
            Self::FullAutoAllowed { allowed } => serde_json::json!({
                "scope": "instance",
                "allow_full_auto": allowed,
            }),
        }
    }
}

#[cfg(test)]
#[path = "auto_merge_tests.rs"]
mod tests;
