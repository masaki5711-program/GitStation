//! Merging a merge request (M3 R4, ADR-0024 §4): the methods and the
//! states of one merge attempt (`forge.merge_request_merges`).
//!
//! A merge attempt moves `queued` → `prepared` → `executing` → `applied`;
//! from any live state it may end `superseded` (the target ref did not
//! move: a refusal, a lost compare-and-swap, a stale request) or `failed`
//! (a person decides: the reconciliation could not tell whether the ref
//! moved). The DB guard `merge_request_merges_guard` holds the same
//! transitions ([`MergeState::may_become`]).

/// How a merge request is merged (ADR-0024 §4.1; the CHECK of
/// `forge.merge_request_merges.method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MergeMethod {
    /// A merge commit with parents `[target, head]` and the trial merge's
    /// tree.
    MergeCommit,
    /// One commit with parent `[target]` and the trial merge's tree.
    Squash,
    /// No commit: the target moves to the head, only when the target is an
    /// ancestor of the head.
    FastForward,
}

impl MergeMethod {
    pub const ALL: &'static [MergeMethod] = &[Self::MergeCommit, Self::Squash, Self::FastForward];

    /// The database / API name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MergeCommit => "merge_commit",
            Self::Squash => "squash",
            Self::FastForward => "fast_forward",
        }
    }

    /// The method named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.as_str() == raw)
    }
}

/// The state of one merge attempt (ADR-0024 §4.2; the CHECK of
/// `forge.merge_request_merges.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeState {
    /// Requested; the job is queued. Nothing was created yet.
    Queued,
    /// The objects exist and `expected_old` / `new_sha` are fixed; the ref
    /// has not moved.
    Prepared,
    /// The attempt holds the right to move the ref (the fence); it may have
    /// moved. Only git can tell (the reconciliation asks it).
    Executing,
    /// Final: the target ref moved and the merge is recorded.
    Applied,
    /// Final: the ref did not move (refused, lost the compare-and-swap,
    /// stale).
    Superseded,
    /// Final: a person decides (runbook `merge-execution`).
    Failed,
}

impl MergeState {
    pub const ALL: &'static [MergeState] = &[
        Self::Queued,
        Self::Prepared,
        Self::Executing,
        Self::Applied,
        Self::Superseded,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Prepared => "prepared",
            Self::Executing => "executing",
            Self::Applied => "applied",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }

    /// `queued`, `prepared` or `executing`: the merge is in progress (at
    /// most one per merge request, the partial unique index).
    pub fn is_live(self) -> bool {
        match self {
            Self::Queued | Self::Prepared | Self::Executing => true,
            Self::Applied | Self::Superseded | Self::Failed => false,
        }
    }

    /// The transitions the state machine (and the DB guard) allows.
    pub fn may_become(self, next: MergeState) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Prepared)
                | (Self::Prepared, Self::Executing)
                | (Self::Executing, Self::Applied)
                | (
                    Self::Queued | Self::Prepared | Self::Executing,
                    Self::Superseded | Self::Failed
                )
        )
    }
}

/// What happened to the source branch after a merge that asked to delete
/// it (ADR-0024 §4.2 step 10; `forge.merge_request_merges.source_outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceOutcome {
    /// Deleted (compare-and-swap on the merged head).
    Deleted,
    /// Kept: the requester may not delete it (protected branch, role,
    /// scope) — decided as a branch delete by `authorize()`.
    KeptRefused,
    /// Kept: the branch moved past the merged head (or is gone already).
    KeptMoved,
    /// Kept: git or the record failed (logged with its code).
    KeptFailed,
}

impl SourceOutcome {
    pub const ALL: &'static [SourceOutcome] = &[
        Self::Deleted,
        Self::KeptRefused,
        Self::KeptMoved,
        Self::KeptFailed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::KeptRefused => "kept_refused",
            Self::KeptMoved => "kept_moved",
            Self::KeptFailed => "kept_failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for m in MergeMethod::ALL {
            assert_eq!(MergeMethod::parse(m.as_str()), Some(*m));
        }
        for s in MergeState::ALL {
            assert_eq!(MergeState::parse(s.as_str()), Some(*s));
        }
        for o in SourceOutcome::ALL {
            assert_eq!(SourceOutcome::parse(o.as_str()), Some(*o));
        }
        assert_eq!(MergeMethod::parse("rebase"), None);
    }

    #[test]
    fn final_states_never_change_and_live_states_only_move_forward() {
        for from in MergeState::ALL {
            for to in MergeState::ALL {
                let allowed = from.may_become(*to);
                if !from.is_live() {
                    assert!(!allowed, "{from:?} -> {to:?}");
                }
                if from == to {
                    assert!(!allowed, "{from:?} -> itself");
                }
            }
        }
        assert!(MergeState::Executing.may_become(MergeState::Applied));
        assert!(!MergeState::Queued.may_become(MergeState::Applied));
        assert!(!MergeState::Prepared.may_become(MergeState::Queued));
    }
}
