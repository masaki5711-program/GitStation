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

use crate::checks::{CheckConclusion, CheckRun, CheckStatus};
use crate::risk::{RiskLevel, RULES};

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
        "secret_file" | "secret_in_content" => StopClass::Leak,
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
    /// The emergency stop in effect (the instance's first).
    pub stop: Option<StopScope>,
    pub draft: bool,
    pub queue: QueueFacts,
    pub risk: RiskVerdict,
    pub pipeline: PipelineState,
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
        _ => None,
    }
}

/// The verdict for one open merge request (module docs). Order: the mode,
/// the emergency stop, the queue, a draft, the risk layers, red checks,
/// then what it waits for; `conditional` keeps protected paths for a
/// person.
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
}

impl AutoMergeChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::ModeChanged { .. } => "auto_merge.mode_changed",
            Self::Stopped { .. } => "auto_merge.stopped",
            Self::Resumed { .. } => "auto_merge.resumed",
        }
    }

    pub fn project(&self) -> Option<crate::ids::ProjectId> {
        match self {
            Self::ModeChanged { project, .. } => Some(*project),
            Self::Stopped { project } | Self::Resumed { project } => *project,
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
        }
    }
}

#[cfg(test)]
#[path = "auto_merge_tests.rs"]
mod tests;
