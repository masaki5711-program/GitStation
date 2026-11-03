//! The combination table of `evaluate_merge()` (M3 R6, plan 11 R6 test
//! column; ADR-0024 §7): every combination of the inputs gives exactly the
//! reasons each input implies, in the fixed order, and a clearance only
//! when there is none. Also: a draft, "no check reported" against success,
//! a `neutral` check, discussions only when required.

use std::collections::BTreeMap;

use forge_core::{CheckName, MergeMethod, MergeRequestState};
use proptest::prelude::*;

use super::*;
use crate::approvals::ApprovalInputs;

fn check(name: &str, run: Option<CheckRun>) -> RequiredCheck {
    RequiredCheck {
        name: CheckName::parse(name).expect("name"),
        run,
    }
}

fn completed(conclusion: CheckConclusion) -> Option<CheckRun> {
    Some(CheckRun {
        status: CheckStatus::Completed,
        conclusion: Some(conclusion),
    })
}

fn ready() -> MergeFacts {
    MergeFacts {
        state: MergeRequestState::Open,
        draft: false,
        merge_in_progress: false,
        source_present: true,
        mergeable: Some(Mergeable::Clean),
        method: MergeMethod::MergeCommit,
        ff_possible: None,
        approvals: ApprovalInputs::nothing_required(),
        checks: Vec::new(),
        discussions: None,
    }
}

fn names(facts: &MergeFacts) -> Vec<&'static str> {
    match evaluate_merge(facts) {
        Ok(_) => Vec::new(),
        Err(reasons) => reasons.iter().map(BlockReason::as_str).collect(),
    }
}

/// ADR-0024 §10: "no check reported" is never success; `neutral`, a
/// failure and a run still in progress block too; only `completed` /
/// `success` passes.
#[test]
fn no_check_reported_is_not_success() {
    let missing = MergeFacts {
        checks: vec![check("ci/gates", None)],
        ..ready()
    };
    let reasons = evaluate_merge(&missing).expect_err("missing");
    assert_eq!(
        reasons,
        [BlockReason::RequiredCheckMissing {
            name: CheckName::parse("ci/gates").expect("name")
        }]
    );
    assert_eq!(reasons[0].detail().as_deref(), Some("name=ci/gates"));
    let success = MergeFacts {
        checks: vec![check("ci/gates", completed(CheckConclusion::Success))],
        ..ready()
    };
    assert!(evaluate_merge(&success).is_ok());
    for conclusion in [
        CheckConclusion::Neutral,
        CheckConclusion::Failure,
        CheckConclusion::Error,
        CheckConclusion::ActionRequired,
    ] {
        let facts = MergeFacts {
            checks: vec![check("ci/gates", completed(conclusion))],
            ..ready()
        };
        assert_eq!(
            names(&facts),
            ["required_check_not_successful"],
            "{conclusion:?}"
        );
    }
    let running = MergeFacts {
        checks: vec![check(
            "kanflow/linked",
            Some(CheckRun {
                status: CheckStatus::InProgress,
                conclusion: None,
            }),
        )],
        ..ready()
    };
    let reasons = evaluate_merge(&running).expect_err("running");
    assert_eq!(
        reasons[0].detail().as_deref(),
        Some("name=kanflow/linked status=in_progress conclusion=none")
    );
}

/// Unresolved discussions block only when the merge rule asks for them.
#[test]
fn unresolved_discussions_block_only_when_required() {
    assert!(evaluate_merge(&MergeFacts {
        discussions: None,
        ..ready()
    })
    .is_ok());
    assert!(evaluate_merge(&MergeFacts {
        discussions: Some(0),
        ..ready()
    })
    .is_ok());
    let open = evaluate_merge(&MergeFacts {
        discussions: Some(3),
        ..ready()
    })
    .expect_err("blocked");
    assert_eq!(open, [BlockReason::UnresolvedDiscussions { count: 3 }]);
    assert_eq!(open[0].detail().as_deref(), Some("count=3"));
}

/// A draft is blocked with every other reason it has too (ADR-0024 §7:
/// never the first one only).
#[test]
fn a_draft_is_blocked_with_every_other_reason() {
    let facts = MergeFacts {
        draft: true,
        checks: vec![check("ci/gates", None)],
        discussions: Some(1),
        ..ready()
    };
    assert_eq!(
        names(&facts),
        ["draft", "required_check_missing", "unresolved_discussions"]
    );
}

/// The protected branch's minimum is an approval requirement with its own
/// rule name; reasons stay in ADR-0024 §7's order whatever the order of
/// discovery (approvals before checks before discussions).
#[test]
fn the_protected_minimum_blocks_as_approvals_missing_first() {
    let facts = MergeFacts {
        approvals: ApprovalInputs {
            minimum: 2,
            ..ApprovalInputs::nothing_required()
        },
        checks: vec![check("ci/gates", None)],
        discussions: Some(1),
        ..ready()
    };
    let reasons = evaluate_merge(&facts).expect_err("blocked");
    assert_eq!(
        reasons.iter().map(BlockReason::as_str).collect::<Vec<_>>(),
        [
            "approvals_missing",
            "required_check_missing",
            "unresolved_discussions"
        ]
    );
    assert_eq!(
        reasons[0].detail().as_deref(),
        Some("rule=@protected_branch required=2 counted=0")
    );
}

fn arb_state() -> impl Strategy<Value = MergeRequestState> {
    prop_oneof![
        Just(MergeRequestState::Open),
        Just(MergeRequestState::Merged),
        Just(MergeRequestState::Closed),
    ]
}

fn arb_mergeable() -> impl Strategy<Value = Option<Mergeable>> {
    prop_oneof![
        Just(None),
        Just(Some(Mergeable::Clean)),
        Just(Some(Mergeable::Conflict)),
        Just(Some(Mergeable::Unknown)),
    ]
}

fn arb_method() -> impl Strategy<Value = MergeMethod> {
    prop_oneof![
        Just(MergeMethod::MergeCommit),
        Just(MergeMethod::Squash),
        Just(MergeMethod::FastForward),
    ]
}

/// A check's run: none, or one of every status / conclusion pair.
fn arb_run() -> impl Strategy<Value = Option<CheckRun>> {
    prop_oneof![
        Just(None),
        Just(Some(CheckRun {
            status: CheckStatus::Queued,
            conclusion: None
        })),
        Just(Some(CheckRun {
            status: CheckStatus::InProgress,
            conclusion: None
        })),
        proptest::sample::select(CheckConclusion::ALL.to_vec()).prop_map(completed),
    ]
}

fn arb_checks() -> impl Strategy<Value = Vec<RequiredCheck>> {
    proptest::collection::vec(arb_run(), 0..4).prop_map(|runs| {
        runs.into_iter()
            .enumerate()
            .map(|(i, run)| check(&format!("ci/check-{i}"), run))
            .collect()
    })
}

/// The reasons each input implies on its own (the table's oracle).
fn expected(facts: &MergeFacts, minimum: u32) -> Vec<&'static str> {
    let mut out: BTreeMap<u8, Vec<&'static str>> = BTreeMap::new();
    let mut push = |rank: u8, name: &'static str| out.entry(rank).or_default().push(name);
    if facts.state != MergeRequestState::Open {
        push(0, "not_open");
    }
    if facts.draft {
        push(1, "draft");
    }
    if facts.merge_in_progress {
        push(2, "merge_in_progress");
    }
    if !facts.source_present {
        push(3, "source_branch_missing");
    }
    match facts.mergeable {
        Some(Mergeable::Unknown) => push(4, "mergeability_unknown"),
        Some(Mergeable::Conflict) => push(5, "conflict"),
        Some(Mergeable::Clean) | None => {}
    }
    if facts.method == MergeMethod::FastForward && facts.ff_possible == Some(false) {
        push(6, "ff_not_possible");
    }
    if minimum > 0 {
        push(8, "approvals_missing");
    }
    for c in &facts.checks {
        match c.run {
            None => push(11, "required_check_missing"),
            Some(run) if !run.succeeded() => push(12, "required_check_not_successful"),
            Some(_) => {}
        }
    }
    if facts.discussions.is_some_and(|n| n > 0) {
        push(13, "unresolved_discussions");
    }
    out.into_values().flatten().collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The combination table: whatever the inputs, `evaluate_merge()`
    /// returns exactly the implied reasons (all of them, ranked), and a
    /// clearance for the method asked exactly when there is none.
    #[test]
    fn every_combination_yields_exactly_its_reasons_in_order(
        state in arb_state(),
        draft in any::<bool>(),
        merge_in_progress in any::<bool>(),
        source_present in any::<bool>(),
        mergeable in arb_mergeable(),
        method in arb_method(),
        ff_possible in proptest::option::of(any::<bool>()),
        minimum in 0u32..3,
        checks in arb_checks(),
        discussions in proptest::option::of(0u32..3),
    ) {
        let facts = MergeFacts {
            state,
            draft,
            merge_in_progress,
            source_present,
            mergeable,
            method,
            ff_possible,
            approvals: ApprovalInputs { minimum, ..ApprovalInputs::nothing_required() },
            checks,
            discussions,
        };
        let want = expected(&facts, minimum);
        match evaluate_merge(&facts) {
            Ok(clearance) => {
                prop_assert!(want.is_empty(), "cleared but expected {want:?}");
                prop_assert_eq!(clearance.method(), method);
            }
            Err(reasons) => {
                let got: Vec<&str> = reasons.iter().map(BlockReason::as_str).collect();
                prop_assert_eq!(&got, &want);
                let ranks: Vec<u8> = reasons.iter().map(BlockReason::rank).collect();
                let mut sorted = ranks.clone();
                sorted.sort_unstable();
                prop_assert_eq!(ranks, sorted, "the fixed order");
                // FG-REVIEW-0007 names them all, first one as `reason`.
                let e = blocked(&reasons);
                let listed = e.context().iter().find(|(k, _)| *k == "reasons").map(|(_, v)| v.clone());
                prop_assert_eq!(listed, Some(got.join(",")));
            }
        }
    }
}

/// R6 review (LOW): the fixed order as literal names — the screen's first
/// line and the `gstn mr status` contract depend on it.
#[test]
fn the_reason_order_is_fixed() {
    let name = |n: &str| CheckName::parse(n).expect("name");
    let every = [
        BlockReason::ChangedPathsUnknown { reason: "timeout" },
        BlockReason::ConfigChangeNeedsMaintainerApproval,
        BlockReason::UnresolvedDiscussions { count: 1 },
        BlockReason::RequiredCheckNotSuccessful {
            name: name("ci/b"),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
        },
        BlockReason::RequiredCheckMissing { name: name("ci/a") },
        BlockReason::CodeownersInvalid {
            line: 1,
            reason: "syntax",
        },
        BlockReason::CodeownerApprovalMissing {
            pattern: "*".to_owned(),
            owners: Vec::new(),
        },
        BlockReason::ApprovalsMissing {
            rule: "r".to_owned(),
            required: 1,
            counted: 0,
        },
        BlockReason::NothingToMerge,
        BlockReason::FfNotPossible,
        BlockReason::Conflict,
        BlockReason::MergeabilityUnknown,
        BlockReason::SourceBranchMissing,
        BlockReason::MergeInProgress,
        BlockReason::Draft,
        BlockReason::NotOpen,
    ];
    let mut sorted = every.to_vec();
    sorted.sort_by_key(BlockReason::rank);
    let names: Vec<&str> = sorted.iter().map(BlockReason::as_str).collect();
    assert_eq!(
        names,
        [
            "not_open",
            "draft",
            "merge_in_progress",
            "source_branch_missing",
            "mergeability_unknown",
            "conflict",
            "ff_not_possible",
            "nothing_to_merge",
            "approvals_missing",
            "codeowner_approval_missing",
            "codeowners_invalid",
            "required_check_missing",
            "required_check_not_successful",
            "unresolved_discussions",
            "config_change_needs_maintainer_approval",
            "changed_paths_unknown",
        ]
    );
    let ranks: Vec<u8> = sorted.iter().map(BlockReason::rank).collect();
    assert_eq!(ranks, (0..16).collect::<Vec<u8>>(), "one rank each");
}

/// The order agrees with ADR-0024 §7's list (the names it lists appear in
/// the same order; `nothing_to_merge` is R4's addition and
/// `work_item_link_missing` waits for I1).
#[test]
fn the_reason_order_agrees_with_the_adr() {
    let adr =
        include_str!("../../../../docs/adr/0024-merge-requests-merge-execution-and-checks.md");
    let start = adr.find("止める理由（`BlockReason`").expect("§7 list");
    let section = &adr[start..];
    let ranked = [
        "not_open",
        "draft",
        "merge_in_progress",
        "source_branch_missing",
        "mergeability_unknown",
        "conflict",
        "ff_not_possible",
        "approvals_missing",
        "codeowner_approval_missing",
        "codeowners_invalid",
        "required_check_missing",
        "required_check_not_successful",
        "unresolved_discussions",
        "config_change_needs_maintainer_approval",
        "changed_paths_unknown",
    ];
    let positions: Vec<usize> = ranked
        .iter()
        .map(|n| {
            section
                .find(&format!("`{n}"))
                .unwrap_or_else(|| panic!("{n} in ADR §7"))
        })
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        positions, sorted,
        "ADR-0024 §7 lists the reasons in rank order"
    );
}
