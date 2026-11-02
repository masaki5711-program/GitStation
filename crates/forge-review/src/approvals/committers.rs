//! Who pushed a merge request's commits (ADR-0024 §8.3; the exact query is
//! fixed here in R5): the actors of the `repository.pushed` records whose
//! ref updates carried a commit of `merge_base..head`.
//!
//! 1. The range `R` is read with git (at most [`MAX_RANGE_COMMITS`]; more is
//!    `range_too_long`).
//! 2. A ref update **names** a range commit when its `new` is in `R` or it
//!    lists one as a new commit: its pusher is a committer (this covers the
//!    pushes before the merge request was opened, the pushes to other
//!    branches, and a reviewer pushing the branch under another name).
//! 3. A range commit no record names was introduced by a push whose commit
//!    list was cut (more than 20 new commits) — or by a push with no record
//!    at all. For each such commit, the cut ref updates whose `new` reaches
//!    it (git, at most [`MAX_PARTIAL_CHECKS`] updates) are its possible
//!    introducers: all their pushers are committers. A commit no update
//!    reaches, a record whose ref list was cut, a pusher that cannot be
//!    read, or a failed or capped read: **unknown** (no approval counts —
//!    never assumed "no committer"). Records past the audit retention are
//!    gone, so an old range ends here too (`committers_unknown`, runbook
//!    merge-blocked).
//!
//! The commit e-mail is never used (ADR-0021).

use std::collections::{BTreeMap, BTreeSet};

use forge_core::ObjectId;
use forge_db::review::PushCoverage;

use super::evaluate::Committers;

/// The longest range evaluated.
pub const MAX_RANGE_COMMITS: usize = 1_000;
/// The most cut ref updates checked with git for one evaluation.
pub const MAX_PARTIAL_CHECKS: usize = 16;

/// The committers of `range` from `coverage` (steps 2 and 3). `reach`
/// answers step 3: for the index of a cut ref update in
/// `coverage.entries`, the range commits its `new` reaches (`None`: could
/// not be read). It is asked only when step 2 leaves a commit unnamed.
pub fn committers_of(
    range: &[ObjectId],
    coverage: &PushCoverage,
    reach: &BTreeMap<usize, Option<BTreeSet<ObjectId>>>,
) -> Committers {
    if coverage.over_cap {
        return Committers::Unknown("push_records_over_cap");
    }
    let range_set: BTreeSet<&ObjectId> = range.iter().collect();
    let mut pushers: BTreeMap<forge_core::ids::UserId, i64> = BTreeMap::new();
    let mut named: BTreeSet<&ObjectId> = BTreeSet::new();
    for entry in &coverage.entries {
        let mut hit = false;
        if let Some(new) = entry.new.as_ref().filter(|n| range_set.contains(n)) {
            named.insert(new);
            hit = true;
        }
        for c in entry.commits.iter().filter(|c| range_set.contains(c)) {
            named.insert(c);
            hit = true;
        }
        if hit {
            let Some(pusher) = entry.pusher else {
                return Committers::Unknown("push_actor_unknown");
            };
            pushers.entry(pusher).or_insert(entry.audit_id);
        }
    }
    let unnamed: Vec<&ObjectId> = range.iter().filter(|c| !named.contains(c)).collect();
    if unnamed.is_empty() {
        return Committers::Known(pushers);
    }
    if coverage.refs_truncated {
        return Committers::Unknown("push_record_truncated");
    }
    for commit in unnamed {
        let mut reached = false;
        for (index, entry) in coverage.entries.iter().enumerate() {
            if !entry.commits_truncated || entry.new.is_none() {
                continue;
            }
            match reach.get(&index) {
                Some(Some(set)) if set.contains(commit) => {
                    let Some(pusher) = entry.pusher else {
                        return Committers::Unknown("push_actor_unknown");
                    };
                    pushers.entry(pusher).or_insert(entry.audit_id);
                    reached = true;
                }
                Some(Some(_)) => {}
                Some(None) | None => return Committers::Unknown("push_reach_unknown"),
            }
        }
        if !reached {
            return Committers::Unknown("push_record_missing");
        }
    }
    Committers::Known(pushers)
}

/// The indexes of the cut ref updates step 3 needs `reach` for (`None`:
/// more than [`MAX_PARTIAL_CHECKS`]), or an empty list when step 2 names
/// every commit.
pub fn partial_entries(range: &[ObjectId], coverage: &PushCoverage) -> Option<Vec<usize>> {
    let range_set: BTreeSet<&ObjectId> = range.iter().collect();
    let named: BTreeSet<&ObjectId> = coverage
        .entries
        .iter()
        .flat_map(|e| e.new.iter().chain(e.commits.iter()))
        .filter(|c| range_set.contains(c))
        .collect();
    if range.iter().all(|c| named.contains(c)) {
        return Some(Vec::new());
    }
    let partial: Vec<usize> = coverage
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.commits_truncated && e.new.is_some())
        .map(|(i, _)| i)
        .collect();
    (partial.len() <= MAX_PARTIAL_CHECKS).then_some(partial)
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
    /// count (ADR-0024 §8.3: `committers_include_pushes_before_mr_creation`,
    /// `committers_include_pushes_to_other_branches`): any record naming a
    /// range commit, whatever ref it updated and whenever.
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

    /// A cut list: the cut updates reaching the unnamed commit are its
    /// possible introducers.
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
        assert_eq!(partial_entries(&range, &coverage), Some(vec![0, 1]));
        let reach = BTreeMap::from([
            (0, Some(BTreeSet::from([oid('a'), oid('b'), oid('c')]))),
            (1, Some(BTreeSet::new())),
        ]);
        assert_eq!(known(&committers_of(&range, &coverage, &reach)), [1]);
        let unread = BTreeMap::from([(0, None), (1, Some(BTreeSet::new()))]);
        assert_eq!(
            committers_of(&range, &coverage, &unread),
            Committers::Unknown("push_reach_unknown")
        );
        let nobody = BTreeMap::from([(0, Some(BTreeSet::new())), (1, Some(BTreeSet::new()))]);
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
