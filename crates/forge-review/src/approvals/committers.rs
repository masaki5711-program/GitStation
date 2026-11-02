//! Who pushed a merge request's commits (ADR-0024 §8.3; the exact query is
//! fixed here in R5, tightened by the R5 review): the actors of the
//! `repository.pushed` records whose ref updates carried a commit of the
//! range — the commits the merge brings, `head ^<target tip>`.
//!
//! 1. The range `R` is read with git (at most [`MAX_RANGE_COMMITS`]; more is
//!    `range_too_long`), with its bottoms.
//! 2. A ref update **names** a range commit when its `new` is in `R` or it
//!    lists one as a new commit: its pusher is a committer (this covers the
//!    pushes before the merge request was opened, the pushes to other
//!    branches, and a reviewer pushing the branch under another name).
//! 3. A ref update whose commit list was **cut** (more than 20 new commits)
//!    may have introduced any range commit its `new` reaches. **Every** cut
//!    update whose `new` reaches the range makes its pusher a committer —
//!    even when the commits it reaches are named by another record (R5
//!    review HIGH: Bob pushes B1..B21 in one push, B1 drops off the list;
//!    Alice creating `feature` at B1 must not make Bob's approval count).
//!    Reaching is asked of git (`merge-base --is-ancestor <bottom> <new>`);
//!    more than [`MAX_PARTIAL_CHECKS`] reaching updates, or an answer git
//!    could not give, is unknown.
//! 4. A range commit no record names must be reached by a cut update (its
//!    possible introducer); otherwise — no record at all (retention, a
//!    failed record, a hand-moved ref), or a record whose ref list was cut —
//!    **unknown** (no approval counts — never assumed "no committer").
//!
//! The commit e-mail is never used (ADR-0021).

use std::collections::{BTreeMap, BTreeSet};

use forge_core::ObjectId;
use forge_db::review::PushCoverage;

use super::evaluate::Committers;

/// The longest range evaluated.
pub const MAX_RANGE_COMMITS: usize = 1_000;
/// The most cut ref updates that may reach the range.
pub const MAX_PARTIAL_CHECKS: usize = 16;

/// Whether a cut ref update's `new` reaches the range (git's answer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// It reaches no range commit.
    No,
    /// It reaches the range; the range commits it reaches when they were
    /// read (`None`: not read — only needed for step 4).
    Yes(Option<BTreeSet<ObjectId>>),
    /// git could not tell (failed, timed out, over the budget).
    Unknown,
}

/// The indexes of the cut ref updates (with a `new`) of `coverage`: the
/// updates step 3 must ask git about.
pub fn cut_entries(coverage: &PushCoverage) -> Vec<usize> {
    coverage
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.commits_truncated && e.new.is_some())
        .map(|(i, _)| i)
        .collect()
}

/// The range commits no record names (step 4 needs them).
pub fn unnamed(range: &[ObjectId], coverage: &PushCoverage) -> Vec<ObjectId> {
    let named: BTreeSet<&ObjectId> = coverage
        .entries
        .iter()
        .flat_map(|e| e.new.iter().chain(e.commits.iter()))
        .collect();
    range
        .iter()
        .filter(|c| !named.contains(c))
        .cloned() // clone: the caller owns the list
        .collect()
}

/// The committers of `range` from `coverage` and git's answers `reach` for
/// every index of [`cut_entries`] (module docs).
pub fn committers_of(
    range: &[ObjectId],
    coverage: &PushCoverage,
    reach: &BTreeMap<usize, Reach>,
) -> Committers {
    if coverage.over_cap {
        return Committers::Unknown("push_records_over_cap");
    }
    let range_set: BTreeSet<&ObjectId> = range.iter().collect();
    let mut pushers: BTreeMap<forge_core::ids::UserId, i64> = BTreeMap::new();
    // Step 2.
    for entry in &coverage.entries {
        let hit = entry.new.as_ref().is_some_and(|n| range_set.contains(n))
            || entry.commits.iter().any(|c| range_set.contains(c));
        if hit {
            let Some(pusher) = entry.pusher else {
                return Committers::Unknown("push_actor_unknown");
            };
            pushers.entry(pusher).or_insert(entry.audit_id);
        }
    }
    // Step 3.
    let mut reached: Vec<&Option<BTreeSet<ObjectId>>> = Vec::new();
    let mut reaching = 0;
    for index in cut_entries(coverage) {
        let Some(entry) = coverage.entries.get(index) else {
            continue;
        };
        match reach.get(&index) {
            Some(Reach::No) => {}
            Some(Reach::Yes(set)) => {
                reaching += 1;
                let Some(pusher) = entry.pusher else {
                    return Committers::Unknown("push_actor_unknown");
                };
                pushers.entry(pusher).or_insert(entry.audit_id);
                reached.push(set);
            }
            Some(Reach::Unknown) | None => return Committers::Unknown("push_reach_unknown"),
        }
    }
    if reaching > MAX_PARTIAL_CHECKS {
        return Committers::Unknown("too_many_partial_pushes");
    }
    // Step 4.
    let missing = unnamed(range, coverage);
    if missing.is_empty() {
        return Committers::Known(pushers);
    }
    if coverage.refs_truncated {
        return Committers::Unknown("push_record_truncated");
    }
    for commit in &missing {
        let mut found = false;
        for set in &reached {
            match set {
                Some(set) => found |= set.contains(commit),
                None => return Committers::Unknown("push_reach_unknown"),
            }
        }
        if !found {
            return Committers::Unknown("push_record_missing");
        }
    }
    Committers::Known(pushers)
}

#[cfg(test)]
mod tests {
    use forge_core::ids::UserId;
    use forge_db::review::PushedEntry;

    use super::*;

    fn oid(c: char) -> ObjectId {
        ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
    }

    fn entry(audit: i64, pusher: i64, new: char, commits: &[char], cut: bool) -> PushedEntry {
        PushedEntry {
            audit_id: audit,
            pusher: Some(UserId::new(pusher)),
            new: Some(oid(new)),
            commits: commits.iter().map(|c| oid(*c)).collect(),
            commits_truncated: cut,
        }
    }

    fn known(c: &Committers) -> Vec<i64> {
        match c {
            Committers::Known(m) => m.keys().map(|u| u.get()).collect(),
            Committers::Unknown(r) => panic!("unknown: {r}"),
        }
    }

    /// The pushes before the merge request existed and to other branches
    /// count (ADR-0024 §8.3): any record naming a range commit, whatever ref
    /// it updated and whenever.
    #[test]
    fn committers_include_pushes_before_mr_creation() {
        let range = [oid('b'), oid('a')];
        let coverage = PushCoverage {
            entries: vec![
                // Alice pushed `a` to `wip` long before opening the MR.
                entry(10, 1, 'a', &['a'], false),
                // Bob pushed `b` on top to the source.
                entry(11, 2, 'b', &['b'], false),
            ],
            ..PushCoverage::default()
        };
        assert_eq!(
            known(&committers_of(&range, &coverage, &BTreeMap::new())),
            [1, 2]
        );
    }

    #[test]
    fn committers_include_pushes_to_other_branches() {
        let range = [oid('a')];
        // Carol pushed the branch under another name (create: no new
        // commit listed, `new` is the range commit).
        let coverage = PushCoverage {
            entries: vec![
                entry(10, 1, 'a', &['a'], false),
                entry(12, 3, 'a', &[], false),
            ],
            ..PushCoverage::default()
        };
        assert_eq!(
            known(&committers_of(&range, &coverage, &BTreeMap::new())),
            [1, 3]
        );
    }

    /// R5 review HIGH: Bob pushes B1..B21 to `bob` in one push (B1 drops off
    /// the 20-entry list); Alice creates `feature` at B1. B1 is "named" by
    /// Alice's record, but Bob's cut push reaches it: Bob is a committer.
    #[test]
    fn a_cut_push_reaching_a_named_commit_still_makes_a_committer() {
        // `a` = B1 (the range is just B1: feature at B1).
        let range = [oid('a')];
        let coverage = PushCoverage {
            entries: vec![
                // Bob: new = B21 (`e`), the list holds B2..B21, not B1.
                entry(10, 2, 'e', &['e', 'd'], true),
                // Alice: create `feature` at B1.
                entry(11, 1, 'a', &[], false),
            ],
            ..PushCoverage::default()
        };
        assert_eq!(cut_entries(&coverage), vec![0]);
        let reach = BTreeMap::from([(0, Reach::Yes(None))]);
        assert_eq!(known(&committers_of(&range, &coverage, &reach)), [1, 2]);
        // Without git's answer: unknown, never "Alice only".
        assert_eq!(
            committers_of(&range, &coverage, &BTreeMap::new()),
            Committers::Unknown("push_reach_unknown")
        );
        // A cut push that does not reach the range adds nobody.
        let reach = BTreeMap::from([(0, Reach::No)]);
        assert_eq!(known(&committers_of(&range, &coverage, &reach)), [1]);
    }

    /// An unnamed commit: the cut updates reaching it are its possible
    /// introducers; git's answer decides, an unread set is unknown.
    #[test]
    fn a_cut_commit_list_is_resolved_with_git_or_fails_closed() {
        let range = [oid('c'), oid('b'), oid('a')];
        let coverage = PushCoverage {
            entries: vec![
                entry(10, 1, 'c', &['c'], true),
                entry(11, 2, 'f', &['f'], true),
            ],
            ..PushCoverage::default()
        };
        assert_eq!(cut_entries(&coverage), vec![0, 1]);
        assert_eq!(unnamed(&range, &coverage), vec![oid('b'), oid('a')]);
        let reach = BTreeMap::from([
            (
                0,
                Reach::Yes(Some(BTreeSet::from([oid('a'), oid('b'), oid('c')]))),
            ),
            (1, Reach::No),
        ]);
        assert_eq!(known(&committers_of(&range, &coverage, &reach)), [1]);
        let unread = BTreeMap::from([(0, Reach::Yes(None)), (1, Reach::No)]);
        assert_eq!(
            committers_of(&range, &coverage, &unread),
            Committers::Unknown("push_reach_unknown")
        );
        let failed = BTreeMap::from([(0, Reach::Unknown), (1, Reach::No)]);
        assert_eq!(
            committers_of(&range, &coverage, &failed),
            Committers::Unknown("push_reach_unknown")
        );
        let nobody = BTreeMap::from([(0, Reach::No), (1, Reach::No)]);
        assert_eq!(
            committers_of(&range, &coverage, &nobody),
            Committers::Unknown("push_record_missing")
        );
    }

    /// No record at all (retention, a failed record): unknown.
    #[test]
    fn a_range_commit_without_a_record_is_unknown() {
        let range = [oid('a')];
        assert_eq!(
            committers_of(&range, &PushCoverage::default(), &BTreeMap::new()),
            Committers::Unknown("push_record_missing")
        );
        let truncated = PushCoverage {
            refs_truncated: true,
            ..PushCoverage::default()
        };
        assert_eq!(
            committers_of(&range, &truncated, &BTreeMap::new()),
            Committers::Unknown("push_record_truncated")
        );
        let over = PushCoverage {
            over_cap: true,
            ..PushCoverage::default()
        };
        assert_eq!(
            committers_of(&range, &over, &BTreeMap::new()),
            Committers::Unknown("push_records_over_cap")
        );
    }
}
