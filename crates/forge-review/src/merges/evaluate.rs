//! `evaluate_merge()` (ADR-0024 §7): a pure function from the facts the
//! caller read to either a [`MergeClearance`] — the only proof that a merge
//! request is ready, with no public constructor — or **every** blocking
//! reason, in the fixed order of ADR-0024 §7. It reads neither the database
//! nor git.
//!
//! M3 R4 knows the reasons its inputs exist for: `not_open`, `draft`,
//! `merge_in_progress`, `source_branch_missing`, `mergeability_unknown`,
//! `conflict`, `ff_not_possible`. Approvals and CODEOWNERS (R5), required
//! checks and unresolved discussions (R6) join with their inputs (each a
//! new variant: the matches below are exhaustive).

use forge_core::{CheckReason, MergeMethod, MergeRequestState, MergeabilityStatus, ObjectId};
use forge_db::review::StoredMergeability;
use forge_errors::{ErrorCode, ForgeError};

/// What blocks a merge (ADR-0024 §7). The names go into the logs and the
/// API unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotOpen => "not_open",
            Self::Draft => "draft",
            Self::MergeInProgress => "merge_in_progress",
            Self::SourceBranchMissing => "source_branch_missing",
            Self::MergeabilityUnknown => "mergeability_unknown",
            Self::Conflict => "conflict",
            Self::FfNotPossible => "ff_not_possible",
            Self::NothingToMerge => "nothing_to_merge",
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
    ForgeError::new(ErrorCode::MergeBlocked)
        .with_context("reason", names.first().copied().unwrap_or("blocked"))
        .with_context("reasons", names.join(","))
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
