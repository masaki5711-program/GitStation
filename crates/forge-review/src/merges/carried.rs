//! Plan 18 S4: what a merge does about a carried-forward mergeability
//! result (forge-db `review::mergeability::paths`).
//!
//! A carried row answers "can be merged" for the live target tip without a
//! trial merge at that tip, so it has **no merge result** there. Two
//! things follow:
//!
//! - a merge that finds a conflict has disproved the carried answer
//!   ([`Recompute::Conflict`]);
//! - a **direct** merge that needs a required check reported on merge
//!   results (`ci/pipeline`) is refused — the run it needs is the live
//!   tip's merge result's, and a carried row has none
//!   ([`Recompute::MergeResult`]). The refusal is kept (the condition is
//!   not weakened); what changes is that the computed check is requested
//!   **at once**, by the refusal itself, so the merge result and its
//!   pipeline follow without waiting for the worker's sweep
//!   (`sweep_missing_merge_results`, which a steady stream of landings can
//!   hold off for as long as it lasts). The merge queue does not need
//!   this: it decides `ci/pipeline` on its own train.
//!
//! Both end in [`recompute`]: the carried row becomes `unchecked` with a
//! new check job (`review::recheck_if_carried`). A row that is not
//! carried is left alone.

use forge_core::ids::MergeRequestId;
use forge_core::{ObjectId, RequestId};
use forge_db::review::{self, StoredMergeability};
use forge_db::Db;
use forge_errors::ForgeError;

use super::evaluate::{blocked, BlockReason};

/// Why a carried result is replaced by a computed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Recompute {
    /// A merge (the job's trial merge, the queue's train) conflicted.
    Conflict,
    /// A direct merge is refused for a required check that reports on the
    /// live tip's merge result, which a carried row does not have.
    MergeResult,
}

/// The context field a refusal carries when it is [`Recompute::MergeResult`]'s
/// (logs only: it says the refusal was decided on a carried row).
const KEY: &str = "mergeability";
const CARRIED: &str = "carried";

/// Whether one of `reasons` is a required check that reports on merge
/// results and has no run that counts for the live tip.
fn needs_merge_result(reasons: &[BlockReason]) -> bool {
    reasons.iter().any(|reason| {
        if let BlockReason::RequiredCheckMissing { name }
        | BlockReason::RequiredCheckStale { name } = reason
        {
            forge_core::merge_results::reported_on_merge_results(name)
        } else {
            false
        }
    })
}

/// Whether `stored` answers for (`head`, `target_tip`) as a carried row.
fn carried_for(
    stored: Option<&StoredMergeability>,
    head: &ObjectId,
    target_tip: Option<&ObjectId>,
) -> bool {
    stored.is_some_and(|s| {
        s.status.is_decided()
            && s.carried_from().is_some()
            && s.checked_head_sha.as_ref() == Some(head)
            && target_tip.is_some()
            && s.checked_target_sha.as_ref() == target_tip
    })
}

/// [`blocked`] for a direct merge of `head` into `target_tip` decided with
/// `stored`: the same refusal, marked when what blocks is the merge result
/// a carried row does not have ([`wants_merge_result`]).
pub(super) fn blocked_at(
    reasons: &[BlockReason],
    stored: Option<&StoredMergeability>,
    head: &ObjectId,
    target_tip: Option<&ObjectId>,
) -> ForgeError {
    let refusal = blocked(reasons);
    if needs_merge_result(reasons) && carried_for(stored, head, target_tip) {
        refusal.with_context(KEY, CARRIED)
    } else {
        refusal
    }
}

/// Whether `refusal` is one [`blocked_at`] marked.
pub(super) fn wants_merge_result(refusal: &ForgeError) -> bool {
    refusal
        .context()
        .iter()
        .any(|(key, value)| *key == KEY && value == CARRIED)
}

/// Replace the carried result of `mr` by a computed check, and say so
/// once. A failure here is logged with its code and is not the merge's:
/// the change is refused either way, and the next landing or the worker's
/// sweep asks again.
pub(super) async fn recompute(db: &Db, mr: MergeRequestId, request_id: &RequestId, why: Recompute) {
    // clone: the requested job keeps the merge's request id
    let asked = review::recheck_if_carried(db, mr, request_id.clone()).await;
    match (asked, why) {
        (Ok(Some(check_seq)), Recompute::Conflict) => tracing::info!(
            target: "forge_review",
            event = "forge.review.mergeability_carried_disproved",
            request_id = %request_id,
            merge_request_id = mr.get(),
            check_seq,
            "a carried-forward mergeability result conflicted in the merge; a computed check was requested"
        ),
        (Ok(Some(check_seq)), Recompute::MergeResult) => tracing::info!(
            target: "forge_review",
            event = "forge.review.mergeability_carried_recompute_requested",
            request_id = %request_id,
            merge_request_id = mr.get(),
            check_seq,
            "a direct merge needs the merge result of the live target tip and the mergeability result was carried forward; a computed check was requested"
        ),
        (Ok(None), Recompute::Conflict | Recompute::MergeResult) => {}
        (Err(e), Recompute::Conflict | Recompute::MergeResult) => {
            forge_core::telemetry::log_failure(
                &e.with_context("operation", "mergeability_recheck_carried")
                    .with_context("merge_request_id", mr.get()),
                request_id,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::{CheckName, MergeabilityStatus};

    fn oid(c: char) -> ObjectId {
        ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
    }

    fn name(raw: &str) -> CheckName {
        CheckName::parse(raw).expect("name")
    }

    fn row(checked: char, computed: Option<char>) -> StoredMergeability {
        StoredMergeability {
            status: MergeabilityStatus::CanBeMerged,
            reason: None,
            check_seq: 3,
            checked_head_sha: Some(oid('a')),
            checked_target_sha: Some(oid(checked)),
            computed_target_sha: computed.map(oid),
            checked_at: None,
            conflicts: Vec::new(),
            conflicts_truncated: false,
        }
    }

    /// Only a required check that reports on merge results, on a row
    /// carried to exactly the pair decided, marks the refusal; the refusal
    /// itself (code, reasons) is the unmarked one.
    #[test]
    fn only_a_merge_result_check_on_a_carried_row_marks_the_refusal() {
        let stale = [BlockReason::RequiredCheckStale {
            name: name("ci/pipeline"),
        }];
        let missing = [
            BlockReason::Draft,
            BlockReason::RequiredCheckMissing {
                name: name("ci/pipeline"),
            },
        ];
        let carried = row('c', Some('b'));
        let (head, tip) = (oid('a'), oid('c'));
        for reasons in [&stale[..], &missing[..]] {
            let marked = blocked_at(reasons, Some(&carried), &head, Some(&tip));
            assert!(wants_merge_result(&marked), "{reasons:?}");
            let plain = blocked(reasons);
            assert_eq!(marked.code(), plain.code());
            assert_eq!(
                marked.context()[..plain.context().len()],
                *plain.context(),
                "the refusal says what it said, plus the mark"
            );
        }
        // Computed at the checked tip: not carried.
        let computed = row('c', Some('c'));
        assert!(!wants_merge_result(&blocked_at(
            &stale,
            Some(&computed),
            &head,
            Some(&tip)
        )));
        // Before the column, no row, another pair, not decided.
        assert!(!wants_merge_result(&blocked_at(
            &stale,
            Some(&row('c', None)),
            &head,
            Some(&tip)
        )));
        assert!(!wants_merge_result(&blocked_at(
            &stale,
            None,
            &head,
            Some(&tip)
        )));
        assert!(!wants_merge_result(&blocked_at(
            &stale,
            Some(&carried),
            &head,
            Some(&oid('d'))
        )));
        assert!(!wants_merge_result(&blocked_at(
            &stale,
            Some(&carried),
            &oid('e'),
            Some(&tip)
        )));
        assert!(!wants_merge_result(&blocked_at(
            &stale,
            Some(&carried),
            &head,
            None
        )));
        let waiting = StoredMergeability {
            status: MergeabilityStatus::Unchecked,
            ..row('c', Some('b'))
        };
        assert!(!wants_merge_result(&blocked_at(
            &stale,
            Some(&waiting),
            &head,
            Some(&tip)
        )));
        // A check reported on the head, or another reason: nothing a
        // merge result would change.
        let on_head = [BlockReason::RequiredCheckMissing {
            name: name("ci/gates"),
        }];
        assert!(!wants_merge_result(&blocked_at(
            &on_head,
            Some(&carried),
            &head,
            Some(&tip)
        )));
        assert!(!wants_merge_result(&blocked_at(
            &[BlockReason::Draft],
            Some(&carried),
            &head,
            Some(&tip)
        )));
    }
}
