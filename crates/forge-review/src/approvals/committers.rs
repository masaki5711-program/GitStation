//! Who pushed a merge request's commits (ADR-0024 §8.3; M3 R5 round 4 — a
//! 改訂 that supersedes the reachability design of rounds 1–3): the
//! committer rule reads what the push path recorded at receive time.
//!
//! 1. Every push records the commits it introduced — made reachable when no
//!    ref reached them before the push — one `push_commits` row per
//!    (commit, pusher), in the push record's transaction. A ref update that
//!    brings no new commit (a tag or a branch on an existing commit, a
//!    delete) records nothing; a merge records its own merge or squash
//!    commit for the requester.
//! 2. The range is `head ^<target tip>` (at most [`MAX_RANGE_COMMITS`];
//!    more is `range_too_long`).
//! 3. The committers are the pushers of the range's commits (one indexed
//!    statement). A range commit with no row — history before the table, an
//!    `update-ref` import, a push past its per-push cap — makes them
//!    **unknown** (fail closed: no approval counts).
//!
//! No audit log scan, no reachability walk, no cap on unrelated history,
//! and nothing depends on objects still existing (a prune changes nothing).
//! The commit e-mail is never used (ADR-0021).

use std::collections::{BTreeMap, BTreeSet};

use forge_core::ObjectId;
use forge_db::review::CommitPusher;

use super::evaluate::Committers;

/// The longest range evaluated.
pub const MAX_RANGE_COMMITS: usize = 1_000;

/// The committers of `range` from its commits' pushers (module docs).
pub fn committers_of(range: &[ObjectId], pushers: &[CommitPusher]) -> Committers {
    let pushed: BTreeSet<&ObjectId> = pushers.iter().map(|p| &p.commit).collect();
    if range.iter().any(|c| !pushed.contains(c)) {
        return Committers::Unknown("push_record_missing");
    }
    let mut out: BTreeMap<forge_core::ids::UserId, i64> = BTreeMap::new();
    for p in pushers {
        let evidence = p.audit_id.unwrap_or(0);
        out.entry(p.pusher)
            .and_modify(|e| {
                if *e == 0 {
                    *e = evidence;
                }
            })
            .or_insert(evidence);
    }
    Committers::Known(out)
}

#[cfg(test)]
mod tests {
    use forge_core::ids::UserId;

    use super::*;

    fn oid(c: char) -> ObjectId {
        ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
    }

    fn row(commit: char, pusher: i64, audit: i64) -> CommitPusher {
        CommitPusher {
            commit: oid(commit),
            pusher: UserId::new(pusher),
            audit_id: Some(audit),
        }
    }

    fn known(c: &Committers) -> Vec<i64> {
        match c {
            Committers::Known(m) => m.keys().map(|u| u.get()).collect(),
            Committers::Unknown(r) => panic!("unknown: {r}"),
        }
    }

    /// Every pusher of a range commit is a committer — whenever and to
    /// whatever ref they pushed it (ADR-0024 §8.3:
    /// `committers_include_pushes_before_mr_creation`).
    #[test]
    fn committers_include_pushes_before_mr_creation() {
        let range = [oid('b'), oid('a')];
        let pushers = [row('a', 1, 10), row('b', 2, 11)];
        assert_eq!(known(&committers_of(&range, &pushers)), [1, 2]);
    }

    /// `committers_include_pushes_to_other_branches`: the row names who
    /// introduced the commit, on any branch; a later branch at the same
    /// commit adds no row (Alice at B1 leaves Bob, who pushed B1, a
    /// committer).
    #[test]
    fn committers_include_pushes_to_other_branches() {
        let range = [oid('a')];
        let pushers = [row('a', 2, 10)];
        assert_eq!(known(&committers_of(&range, &pushers)), [2]);
    }

    /// A range commit no push recorded (before the table, an update-ref
    /// import, a push past its cap): unknown.
    #[test]
    fn a_range_commit_without_a_row_is_unknown() {
        let range = [oid('b'), oid('a')];
        assert_eq!(
            committers_of(&range, &[row('b', 1, 10)]),
            Committers::Unknown("push_record_missing")
        );
        assert_eq!(known(&committers_of(&[], &[])), Vec::<i64>::new());
    }
}
