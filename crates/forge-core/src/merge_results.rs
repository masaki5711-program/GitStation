//! Merge results and the freshness of required checks (M4 C7, plan 04 M-1 /
//! M-2, ADR-0024 Amendment 2026-11-11).
//!
//! The mergeability job writes the merge of a merge request's head into the
//! target tip as a commit (`refs/merge-requests/<iid>/merge`, one row of
//! `forge.merge_result_refs` per (head, target tip) pair), and the merge
//! request pipeline runs on that commit. A merge decision reads the runs of
//! the head and of every merge result of the head ([`MergeCheckRuns`]) and
//! resolves each required name against the LIVE target tip
//! ([`MergeCheckRuns::resolve`]):
//!
//! 1. a run on the current merge result (the row whose target is the live
//!    tip) counts — [`Freshness::Fresh`];
//! 2. for a name forge itself reports on merge results
//!    ([`reported_on_merge_results`]: [`crate::checks::CI_PIPELINE_CHECK`],
//!    the merge request pipeline's), a run on an earlier merge result of
//!    the head (the target advanced since) is [`Freshness::Stale`]: it never
//!    counts, whatever its conclusion (`required_check_stale`,
//!    FG-CI-0013); a run on the head never counts for it either (a push
//!    pipeline's run is not the merge result's);
//! 3. for any other name (external reporters: the Checks API takes only
//!    commits a branch or tag reaches) a run on the head counts, and a run
//!    only an earlier merge result holds is ignored — rerunning the merge
//!    request pipeline would never report it again (a removed or renamed
//!    job), so it never holds the merge as stale (review MEDIUM);
//! 4. otherwise no run (`required_check_missing`).

use crate::checks::{CheckRun, CheckRunsOnHead, CI_PIPELINE_CHECK};
use crate::merge_conditions::CheckName;
use crate::push::ObjectId;

/// The most merge results of one head a decision reads (the newest first);
/// older ones are left out (a run only they hold reads as missing, never
/// as success).
pub const MAX_MERGE_RESULTS_READ: usize = 20;

/// One merge result of the head with its runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResultRuns {
    /// The target tip the head was merged into.
    pub target: ObjectId,
    /// The merge commit.
    pub result: ObjectId,
    /// Every run on `result`.
    pub runs: CheckRunsOnHead,
}

/// Whether a run counts for the merge now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// On the current merge result, or (for a head-reported name) on the head.
    Fresh,
    /// On an earlier merge result of the head only.
    Stale,
}

/// The runs a merge decision reads for one head (module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeCheckRuns {
    /// The runs on the head.
    pub head: CheckRunsOnHead,
    /// The merge results of the head, newest first (at most
    /// [`MAX_MERGE_RESULTS_READ`]).
    pub results: Vec<MergeResultRuns>,
}

impl MergeCheckRuns {
    /// The current merge result for `target_tip` (`None`: none written for
    /// that tip yet, a conflict, or no target).
    pub fn current(&self, target_tip: Option<&ObjectId>) -> Option<&MergeResultRuns> {
        let tip = target_tip?;
        self.results.iter().find(|r| &r.target == tip)
    }

    /// The run of `name` for a merge of `head` into `target_tip` and its
    /// freshness (module docs, rules 1–4). `None`: no run.
    pub fn resolve(
        &self,
        head: &ObjectId,
        target_tip: Option<&ObjectId>,
        name: &CheckName,
    ) -> Option<(CheckRun, Freshness)> {
        let current = self.current(target_tip);
        if let Some(run) = current.and_then(|c| c.runs.run(&c.result, name)) {
            return Some((run, Freshness::Fresh));
        }
        if !reported_on_merge_results(name) {
            return self.head.run(head, name).map(|run| (run, Freshness::Fresh));
        }
        self.results
            .iter()
            .filter(|r| Some(&r.target) != target_tip)
            .find_map(|r| r.runs.run(&r.result, name))
            .map(|run| (run, Freshness::Stale))
    }
}

/// Whether forge itself reports `name` on merge results (the merge request
/// pipeline's [`CI_PIPELINE_CHECK`]; forge reports no job-level names):
/// only such a name can be stale (module docs, rule 2).
pub fn reported_on_merge_results(name: &CheckName) -> bool {
    name.as_str() == CI_PIPELINE_CHECK
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;
    use crate::checks::{CheckConclusion, CheckStatus};

    fn oid(c: char) -> ObjectId {
        ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
    }

    fn name(raw: &str) -> CheckName {
        CheckName::parse(raw).expect("name")
    }

    fn success() -> CheckRun {
        CheckRun::new(CheckStatus::Completed, Some(CheckConclusion::Success)).expect("run")
    }

    fn on(sha: char, names: &[&str]) -> CheckRunsOnHead {
        CheckRunsOnHead::new(oid(sha), names.iter().map(|n| (name(n), success())))
    }

    fn result(target: char, result: char, names: &[&str]) -> MergeResultRuns {
        MergeResultRuns {
            target: oid(target),
            result: oid(result),
            runs: on(result, names),
        }
    }

    #[test]
    fn a_run_on_the_current_merge_result_is_fresh_and_an_earlier_one_stale() {
        let runs = MergeCheckRuns {
            head: on('e', &[]),
            results: vec![result('2', 'b', &[]), result('1', 'a', &["ci/pipeline"])],
        };
        let pipeline = name("ci/pipeline");
        // The target was at 1: the run on its merge result counts.
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('1')), &pipeline),
            Some((success(), Freshness::Fresh))
        );
        // The target advanced to 2: the run of the 1-result is stale.
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('2')), &pipeline),
            Some((success(), Freshness::Stale))
        );
        // A tip with no merge result yet (the check is pending): stale too.
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('3')), &pipeline),
            Some((success(), Freshness::Stale))
        );
        assert_eq!(
            runs.current(Some(&oid('2'))).map(|r| &r.result),
            Some(&oid('b'))
        );
        assert!(runs.current(None).is_none());
    }

    #[test]
    fn a_head_run_counts_for_external_names_but_never_for_ci_pipeline() {
        let runs = MergeCheckRuns {
            head: on('e', &["ci/gates", "ci/pipeline"]),
            results: vec![result('1', 'a', &[])],
        };
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('1')), &name("ci/gates")),
            Some((success(), Freshness::Fresh))
        );
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('1')), &name("ci/pipeline")),
            None,
            "a push pipeline on the head never satisfies ci/pipeline"
        );
        // A run on another head never counts.
        assert_eq!(
            runs.resolve(&oid('f'), Some(&oid('1')), &name("ci/gates")),
            None
        );
    }

    #[test]
    fn an_external_name_reported_on_the_head_counts_after_an_old_merge_result_report() {
        // Review MEDIUM: a name forge does not report on merge results (an
        // external reporter's) is never held stale by an older merge
        // result: its head report counts.
        let runs = MergeCheckRuns {
            head: on('e', &["ci/gates"]),
            results: vec![result('2', 'b', &[]), result('1', 'a', &["ci/gates"])],
        };
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('2')), &name("ci/gates")),
            Some((success(), Freshness::Fresh))
        );
    }

    #[test]
    fn an_old_merge_result_report_of_an_external_name_never_blocks_forever() {
        // A removed or renamed job name only an older merge result holds:
        // not stale (rerunning the merge request pipeline would never
        // report it), just missing until the reporter reports it again.
        let runs = MergeCheckRuns {
            head: on('e', &[]),
            results: vec![result('2', 'b', &[]), result('1', 'a', &["ci/build"])],
        };
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('2')), &name("ci/build")),
            None
        );
        // On the current merge result it still counts.
        let current = MergeCheckRuns {
            head: on('e', &[]),
            results: vec![result('2', 'b', &["ci/build"])],
        };
        assert_eq!(
            current.resolve(&oid('e'), Some(&oid('2')), &name("ci/build")),
            Some((success(), Freshness::Fresh))
        );
    }

    #[test]
    fn ci_pipeline_on_an_old_merge_result_stays_stale_even_with_a_head_run() {
        let runs = MergeCheckRuns {
            head: on('e', &["ci/pipeline"]),
            results: vec![result('2', 'b', &[]), result('1', 'a', &["ci/pipeline"])],
        };
        assert_eq!(
            runs.resolve(&oid('e'), Some(&oid('2')), &name("ci/pipeline")),
            Some((success(), Freshness::Stale))
        );
    }
}
