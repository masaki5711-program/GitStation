//! `evaluate_merge()` (ADR-0024 §7): a pure function from the facts the
//! caller read to either a [`MergeClearance`] — the only proof that a merge
//! request is ready, with no public constructor — or **every** blocking
//! reason, in the fixed order of ADR-0024 §7. It reads neither the database
//! nor git.
//!
//! M3 R4 knows the reasons its inputs exist for: `not_open`, `draft`,
//! `merge_in_progress`, `source_branch_missing`, `mergeability_unknown`,
//! `conflict`, `ff_not_possible`. M3 R5 adds the approvals
//! (`approvals_missing`, `codeowner_approval_missing`,
//! `codeowners_invalid`, `config_change_needs_maintainer_approval`,
//! `changed_paths_unknown`; decided by [`crate::approvals::evaluate`] on
//! [`MergeFacts::approvals`]). M3 R6 adds the merge rule of the target's
//! protection: `required_check_missing` (no run reported is never
//! success), `required_check_not_successful` and `unresolved_discussions`
//! ([`MergeFacts::checks`], [`MergeFacts::discussions`]); the protected
//! branch's minimum approvals is an approval requirement
//! (`approvals_missing` with the rule `@protected_branch`). M3 I1d adds
//! `work_item_link_missing` ([`MergeFacts::work_item_link`]: the rule's
//! `require_work_item_link` and no open link of the project's binding). The matches
//! below are exhaustive; the order is [`BlockReason::rank`] — ADR-0024 §7's
//! list — whatever the order the reasons were found in.

use forge_core::{
    CheckName, CheckReason, MergeMethod, MergeRequestState, MergeabilityStatus, ObjectId,
};
use forge_db::review::StoredMergeability;
use forge_errors::{ErrorCode, ForgeError};

/// What blocks a merge (ADR-0024 §7). The names go into the logs and the
/// API unchanged; the details of the approval reasons go into `details`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    NotOpen,
    Draft,
    MergeInProgress,
    SourceBranchMissing,
    MergeabilityUnknown,
    Conflict,
    FfNotPossible,
    /// `fast_forward` and the target is at the head already (review round
    /// 1): nothing would move.
    NothingToMerge,
    /// An applicable approval rule has fewer counted approvals than it
    /// needs (M3 R5).
    ApprovalsMissing {
        rule: String,
        required: u32,
        counted: u32,
    },
    /// A changed path's CODEOWNERS rule has no counted approval by one of
    /// its owners (M3 R5).
    CodeownerApprovalMissing {
        pattern: String,
        owners: Vec<String>,
    },
    /// The target's CODEOWNERS file cannot be used (FG-REVIEW-0014).
    CodeownersInvalid {
        line: u32,
        reason: &'static str,
    },
    /// A configuration path changed and no Maintainer (or higher) gave a
    /// counted approval (ADR-0024 §8.7, ADR-0017 §2.7).
    ConfigChangeNeedsMaintainerApproval,
    /// The changed paths could not be read in full (ADR-0024 §9):
    /// CODEOWNERS and the configuration rule cannot be decided.
    ChangedPathsUnknown {
        reason: &'static str,
    },
    /// A required check has no run on the head (M3 R6, ADR-0024 §10:
    /// "no check reported" is never success).
    RequiredCheckMissing {
        name: CheckName,
    },
    /// A required check's run on the head is not `completed` / `success`
    /// (`neutral` included).
    RequiredCheckNotSuccessful {
        name: CheckName,
        status: CheckStatus,
        conclusion: Option<CheckConclusion>,
    },
    /// The merge rule asks for resolved discussions and `count` are not
    /// (M3 R6).
    UnresolvedDiscussions {
        count: u32,
    },
    /// The merge rule requires a work item link and the merge request has
    /// no open link to a work item of its project's current binding (M3
    /// I1d, ADR-0024 §7 / §11).
    WorkItemLinkMissing,
}

// M3 C1: the check run types moved to forge-core (forge-checks records
// them, forge-review decides with them; neither depends on the other).
pub use forge_core::checks::{CheckConclusion, CheckRun, CheckStatus};

/// A required check and its run on the head (`None`: no run reported).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredCheck {
    pub name: CheckName,
    pub run: Option<CheckRun>,
}

impl BlockReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotOpen => "not_open",
            Self::Draft => "draft",
            Self::MergeInProgress => "merge_in_progress",
            Self::SourceBranchMissing => "source_branch_missing",
            Self::MergeabilityUnknown => "mergeability_unknown",
            Self::Conflict => "conflict",
            Self::FfNotPossible => "ff_not_possible",
            Self::NothingToMerge => "nothing_to_merge",
            Self::ApprovalsMissing { .. } => "approvals_missing",
            Self::CodeownerApprovalMissing { .. } => "codeowner_approval_missing",
            Self::CodeownersInvalid { .. } => "codeowners_invalid",
            Self::ConfigChangeNeedsMaintainerApproval => "config_change_needs_maintainer_approval",
            Self::ChangedPathsUnknown { .. } => "changed_paths_unknown",
            Self::RequiredCheckMissing { .. } => "required_check_missing",
            Self::RequiredCheckNotSuccessful { .. } => "required_check_not_successful",
            Self::UnresolvedDiscussions { .. } => "unresolved_discussions",
            Self::WorkItemLinkMissing => "work_item_link_missing",
        }
    }

    /// The fixed position of the reason in every list (ADR-0024 §7): the
    /// screen's "why it is stopped" line is the first one (plan 08).
    pub fn rank(&self) -> u8 {
        match self {
            Self::NotOpen => 0,
            Self::Draft => 1,
            Self::MergeInProgress => 2,
            Self::SourceBranchMissing => 3,
            Self::MergeabilityUnknown => 4,
            Self::Conflict => 5,
            Self::FfNotPossible => 6,
            Self::NothingToMerge => 7,
            Self::ApprovalsMissing { .. } => 8,
            Self::CodeownerApprovalMissing { .. } => 9,
            Self::CodeownersInvalid { .. } => 10,
            Self::RequiredCheckMissing { .. } => 11,
            Self::RequiredCheckNotSuccessful { .. } => 12,
            Self::UnresolvedDiscussions { .. } => 13,
            Self::WorkItemLinkMissing => 14,
            Self::ConfigChangeNeedsMaintainerApproval => 15,
            Self::ChangedPathsUnknown { .. } => 16,
        }
    }

    /// The reason's detail for the logs and the API (`None` for the plain
    /// ones): `rule=<name> required=<n> counted=<n>`, `pattern=<p>
    /// owners=<@a,@b>`, `line=<n> reason=<r>`, `reason=<r>`.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::ApprovalsMissing {
                rule,
                required,
                counted,
            } => Some(format!("rule={rule} required={required} counted={counted}")),
            Self::CodeownerApprovalMissing { pattern, owners } => {
                Some(format!("pattern={pattern} owners={}", owners.join(",")))
            }
            Self::CodeownersInvalid { line, reason } => {
                Some(format!("line={line} reason={reason}"))
            }
            Self::ChangedPathsUnknown { reason } => Some(format!("reason={reason}")),
            Self::RequiredCheckMissing { name } => Some(format!("name={name}")),
            Self::RequiredCheckNotSuccessful {
                name,
                status,
                conclusion,
            } => Some(format!(
                "name={name} status={} conclusion={}",
                status.as_str(),
                conclusion.map_or("none", CheckConclusion::as_str)
            )),
            Self::UnresolvedDiscussions { count } => Some(format!("count={count}")),
            Self::NotOpen
            | Self::Draft
            | Self::MergeInProgress
            | Self::SourceBranchMissing
            | Self::MergeabilityUnknown
            | Self::Conflict
            | Self::FfNotPossible
            | Self::NothingToMerge
            | Self::WorkItemLinkMissing
            | Self::ConfigChangeNeedsMaintainerApproval => None,
        }
    }
}

/// Whether the head merges cleanly into the target's live tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Clean,
    Conflict,
    /// Not decided for this (head, target tip) pair: never "mergeable".
    Unknown,
}

/// The facts of one decision (all read by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeFacts {
    pub state: MergeRequestState,
    pub draft: bool,
    /// Another attempt is live (ADR-0024 §4.2).
    pub merge_in_progress: bool,
    /// The source branch exists.
    pub source_present: bool,
    /// `None`: decided later under the merge lock (the job's first check,
    /// tx1a, before its own trial merge).
    pub mergeable: Option<Mergeable>,
    pub method: MergeMethod,
    /// For `fast_forward`: whether the target's tip is an ancestor of the
    /// head (`None`: not computed yet; the job computes it).
    pub ff_possible: Option<bool>,
    /// M3 R5: the approvals, the rules, CODEOWNERS, the changed paths and
    /// the committers, as read for this decision (M3 R6: with the project's
    /// settings and the protected branch's minimum).
    pub approvals: crate::approvals::ApprovalInputs,
    /// M3 R6: every check the target's merge rule requires, with its run
    /// on the head (none required: empty).
    pub checks: Vec<RequiredCheck>,
    /// M3 R6: the unresolved discussions when the target's merge rule asks
    /// for them resolved (`None`: not required).
    pub discussions: Option<u32>,
    /// M3 I1d: whether the merge request has an open link to a work item of
    /// its project's current binding, when the target's merge rule requires
    /// one (`None`: not required).
    pub work_item_link: Option<bool>,
}

/// The proof that `evaluate_merge()` found nothing blocking. No public
/// constructor: only [`evaluate_merge`] makes one, and moving a ref takes
/// it (with the `Authorized` proof of `authorize()`, ADR-0024 §7).
#[derive(Debug)]
pub struct MergeClearance {
    method: MergeMethod,
}

impl MergeClearance {
    /// The method the decision was made for.
    pub fn method(&self) -> MergeMethod {
        self.method
    }
}

/// Every blocking reason of `facts`, in the fixed order; a clearance when
/// there is none.
pub fn evaluate_merge(facts: &MergeFacts) -> Result<MergeClearance, Vec<BlockReason>> {
    let mut reasons = Vec::new();
    if facts.state != MergeRequestState::Open {
        reasons.push(BlockReason::NotOpen);
    }
    if facts.draft {
        reasons.push(BlockReason::Draft);
    }
    if facts.merge_in_progress {
        reasons.push(BlockReason::MergeInProgress);
    }
    if !facts.source_present {
        reasons.push(BlockReason::SourceBranchMissing);
    }
    match facts.mergeable {
        Some(Mergeable::Unknown) => reasons.push(BlockReason::MergeabilityUnknown),
        Some(Mergeable::Conflict) => reasons.push(BlockReason::Conflict),
        Some(Mergeable::Clean) | None => {}
    }
    let ff = match facts.method {
        MergeMethod::FastForward => facts.ff_possible,
        MergeMethod::MergeCommit | MergeMethod::Squash => None,
    };
    if ff == Some(false) {
        reasons.push(BlockReason::FfNotPossible);
    }
    reasons.extend(crate::approvals::evaluate(&facts.approvals).blocks);
    for check in &facts.checks {
        match check.run {
            None => reasons.push(BlockReason::RequiredCheckMissing {
                name: check.name.clone(), // clone: the reason owns its name
            }),
            Some(run) if !run.succeeded() => {
                reasons.push(BlockReason::RequiredCheckNotSuccessful {
                    name: check.name.clone(), // clone: as above
                    status: run.status,
                    conclusion: run.conclusion,
                })
            }
            Some(_) => {}
        }
    }
    if let Some(count) = facts.discussions.filter(|n| *n > 0) {
        reasons.push(BlockReason::UnresolvedDiscussions { count });
    }
    if facts.work_item_link == Some(false) {
        reasons.push(BlockReason::WorkItemLinkMissing);
    }
    // The fixed order of ADR-0024 §7 (stable: within one reason, the
    // order found — rule names, CODEOWNERS lines, check names).
    reasons.sort_by_key(BlockReason::rank);
    if reasons.is_empty() {
        Ok(MergeClearance {
            method: facts.method,
        })
    } else {
        Err(reasons)
    }
}

/// `FG-REVIEW-0007` MergeBlocked with `reasons` (comma-separated names).
pub fn blocked(reasons: &[BlockReason]) -> ForgeError {
    let names: Vec<&str> = reasons.iter().map(|r| r.as_str()).collect();
    let details: Vec<String> = reasons
        .iter()
        .filter_map(|r| r.detail().map(|d| format!("{}:{d}", r.as_str())))
        .collect();
    let e = ForgeError::new(ErrorCode::MergeBlocked)
        .with_context("reason", names.first().copied().unwrap_or("blocked"))
        .with_context("reasons", names.join(","));
    if details.is_empty() {
        e
    } else {
        e.with_context("details", details.join(";"))
    }
}

/// The stored mergeability as a decision for (`head`, `target_tip`): only a
/// decided status whose checked pair is exactly that pair counts; anything
/// else (none, unchecked, checking, failed, another pair) is unknown.
pub fn stored_mergeable(
    stored: Option<&StoredMergeability>,
    head: &ObjectId,
    target_tip: Option<&ObjectId>,
) -> Mergeable {
    let Some(stored) = stored else {
        return Mergeable::Unknown;
    };
    let same_pair = stored.checked_head_sha.as_ref() == Some(head)
        && target_tip.is_some()
        && stored.checked_target_sha.as_ref() == target_tip;
    match stored.status {
        MergeabilityStatus::CanBeMerged if same_pair => Mergeable::Clean,
        MergeabilityStatus::CannotBeMerged
            if same_pair && stored.reason == Some(CheckReason::Conflict) =>
        {
            Mergeable::Conflict
        }
        MergeabilityStatus::CanBeMerged
        | MergeabilityStatus::CannotBeMerged
        | MergeabilityStatus::Unchecked
        | MergeabilityStatus::Checking
        | MergeabilityStatus::CheckFailed => Mergeable::Unknown,
    }
}

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod conditions_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> MergeFacts {
        MergeFacts {
            state: MergeRequestState::Open,
            draft: false,
            merge_in_progress: false,
            source_present: true,
            mergeable: Some(Mergeable::Clean),
            method: MergeMethod::MergeCommit,
            ff_possible: None,
            approvals: crate::approvals::ApprovalInputs::nothing_required(),
            checks: Vec::new(),
            discussions: None,
            work_item_link: None,
        }
    }

    #[test]
    fn a_ready_merge_request_is_cleared_for_its_method() {
        let clearance = evaluate_merge(&ready()).expect("ready");
        assert_eq!(clearance.method(), MergeMethod::MergeCommit);
        // The job's first check decides the mergeability later.
        assert!(evaluate_merge(&MergeFacts {
            mergeable: None,
            ..ready()
        })
        .is_ok());
    }

    /// ADR-0024 §7: every reason at once, in the fixed order (never the
    /// first one only).
    #[test]
    fn every_reason_is_returned_in_the_fixed_order() {
        let facts = MergeFacts {
            state: MergeRequestState::Closed,
            draft: true,
            merge_in_progress: true,
            source_present: false,
            mergeable: Some(Mergeable::Unknown),
            method: MergeMethod::FastForward,
            ff_possible: Some(false),
            approvals: crate::approvals::ApprovalInputs::nothing_required(),
            checks: Vec::new(),
            discussions: None,
            work_item_link: None,
        };
        let reasons = evaluate_merge(&facts).expect_err("blocked");
        let names: Vec<&str> = reasons.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            [
                "not_open",
                "draft",
                "merge_in_progress",
                "source_branch_missing",
                "mergeability_unknown",
                "ff_not_possible"
            ]
        );
        let conflict = evaluate_merge(&MergeFacts {
            mergeable: Some(Mergeable::Conflict),
            ..ready()
        })
        .expect_err("conflict");
        assert_eq!(conflict, [BlockReason::Conflict]);
        let e = blocked(&reasons);
        assert_eq!(e.code(), ErrorCode::MergeBlocked);
        assert!(e
            .context()
            .iter()
            .any(|(k, v)| *k == "reasons" && v.starts_with("not_open,draft")));
    }

    /// `ff_possible` matters for fast-forward only; a merge commit or a
    /// squash never stops on it.
    #[test]
    fn ff_not_possible_blocks_only_a_fast_forward() {
        let not_ff = MergeFacts {
            ff_possible: Some(false),
            ..ready()
        };
        assert!(evaluate_merge(&not_ff).is_ok());
        let ff = MergeFacts {
            method: MergeMethod::FastForward,
            ..not_ff
        };
        assert_eq!(
            evaluate_merge(&ff).expect_err("blocked"),
            [BlockReason::FfNotPossible]
        );
    }

    /// Only a decision for exactly the live pair counts (R2 review: the
    /// stored status is about `checked_head_sha` / `checked_target_sha`).
    #[test]
    fn the_stored_status_counts_only_for_the_live_pair() {
        let oid = |c: char| ObjectId::parse(&c.to_string().repeat(40)).expect("oid");
        let stored = |status, reason| StoredMergeability {
            status,
            reason,
            check_seq: 1,
            checked_head_sha: Some(oid('a')),
            checked_target_sha: Some(oid('b')),
            checked_at: None,
            conflicts: Vec::new(),
            conflicts_truncated: false,
        };
        let can = stored(MergeabilityStatus::CanBeMerged, None);
        assert_eq!(
            stored_mergeable(Some(&can), &oid('a'), Some(&oid('b'))),
            Mergeable::Clean
        );
        assert_eq!(
            stored_mergeable(Some(&can), &oid('a'), Some(&oid('c'))),
            Mergeable::Unknown
        );
        assert_eq!(
            stored_mergeable(Some(&can), &oid('c'), Some(&oid('b'))),
            Mergeable::Unknown
        );
        assert_eq!(
            stored_mergeable(Some(&can), &oid('a'), None),
            Mergeable::Unknown
        );
        let cannot = stored(
            MergeabilityStatus::CannotBeMerged,
            Some(CheckReason::Conflict),
        );
        assert_eq!(
            stored_mergeable(Some(&cannot), &oid('a'), Some(&oid('b'))),
            Mergeable::Conflict
        );
        let failed = stored(
            MergeabilityStatus::CheckFailed,
            Some(CheckReason::GitTimeout),
        );
        assert_eq!(
            stored_mergeable(Some(&failed), &oid('a'), Some(&oid('b'))),
            Mergeable::Unknown
        );
        assert_eq!(
            stored_mergeable(None, &oid('a'), Some(&oid('b'))),
            Mergeable::Unknown
        );
    }
}
