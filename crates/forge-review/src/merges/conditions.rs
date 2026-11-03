//! The M3 R6 inputs of `evaluate_merge()` beyond the approvals (ADR-0024
//! §7, §10, §12): the target's merge rule (the strictest of the matching
//! protection rules) turned into the required checks with their runs on
//! the head, and the unresolved discussions when the rule asks for them.
//! Shared by the merge request, the job's gates and the status read, so
//! every decision reads the conditions alike.
//!
//! The check runs (M3 C1) are one snapshot of the head's runs
//! ([`forge_core::CheckRunsOnHead`]): the request and the job read it in the
//! gate's transaction (`MergeGate::check_runs`), the status through
//! `forge_core::CheckResultsSource` — the same SQL. A required name without
//! a run is `required_check_missing`, never success (ADR-0024 §10).

use forge_core::{MergeMethod, ObjectId, ProtectionRules, RefName};
use forge_db::review::StoredMergeSettings;
use forge_errors::ForgeError;

use super::evaluate::{CheckRun, RequiredCheck};
use super::MergeSettings;

/// The runs of checks on a commit.
pub trait CheckRuns {
    /// The latest run of `name` on `head` (`None`: none reported).
    fn run(&self, head: &ObjectId, name: &forge_core::CheckName) -> Option<CheckRun>;
}

/// M3 C1: the stored runs of one head (a run on another head never counts).
impl CheckRuns for forge_core::CheckRunsOnHead {
    fn run(&self, head: &ObjectId, name: &forge_core::CheckName) -> Option<CheckRun> {
        forge_core::CheckRunsOnHead::run(self, head, name)
    }
}

/// The merge rule's conditions of a merge of `head` into `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conditions {
    pub checks: Vec<RequiredCheck>,
    /// The unresolved discussions when the rule requires them resolved.
    pub discussions: Option<u32>,
}

/// The conditions `protection` puts on a merge of `head` into `target`
/// (`unresolved`: the merge request's unresolved discussions now).
pub fn conditions(
    protection: &ProtectionRules,
    target: &RefName,
    head: &ObjectId,
    unresolved: u32,
    runs: &dyn CheckRuns,
) -> Conditions {
    let rule = protection.merge_rule(target);
    Conditions {
        checks: rule
            .required_checks
            .into_iter()
            .map(|name| {
                let run = runs.run(head, &name);
                RequiredCheck { name, run }
            })
            .collect(),
        discussions: rule.require_resolved_discussions.then_some(unresolved),
    }
}

/// The project's merge settings as the merge decides with them
/// (`FG-REVIEW-0010` for a method they do not allow).
pub fn settings_of(stored: &StoredMergeSettings) -> MergeSettings {
    MergeSettings::from_values(&stored.values)
}

/// `FG-REVIEW-0010` MergeMethodNotAllowed unless `stored` allows `method`
/// (the request, and every gate of the job: a setting changed while the
/// merge waited is decided again, ADR-0024 §4.2 steps 3 and 5).
pub fn method_allowed(stored: &StoredMergeSettings, method: MergeMethod) -> Result<(), ForgeError> {
    settings_of(stored).check(method)
}
