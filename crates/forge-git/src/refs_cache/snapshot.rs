//! One repository's branches at one moment, as `for-each-ref` printed
//! them: names and whether each names a commit — never a commit id (see
//! the parent module).

use std::collections::HashSet;

/// The branches of one repository at one moment.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BranchSnapshot {
    /// Full names (`refs/heads/…`) that name a commit.
    commits: HashSet<Box<str>>,
    /// Full names that name something else (a tree, a blob, a tag of one).
    others: HashSet<Box<str>>,
    /// `false`: git printed more than was kept (the output or the branch
    /// cap); a name that is not here may still exist.
    complete: bool,
}

/// What a snapshot says about one branch name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchAnswer {
    /// The branch exists and names a commit.
    Commit,
    /// The branch exists and names no commit.
    NotACommit,
    /// No such branch.
    Missing,
    /// Not in a snapshot that is not complete: ask git for that name.
    Unknown,
}

impl BranchSnapshot {
    /// `for-each-ref --format=%(objecttype)%00%(*objecttype)%00%(refname)`
    /// lines. `cut`: the output was truncated (the last line may be
    /// partial and is dropped). At most `max` names are kept. A line that
    /// is not UTF-8 or not three fields is left out: no valid ref name can
    /// equal it.
    pub fn parse(out: &[u8], cut: bool, max: usize) -> Self {
        let mut snapshot = Self {
            complete: !cut,
            ..Self::default()
        };
        let mut lines: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
        if cut {
            lines.pop();
        }
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Ok(text) = std::str::from_utf8(line) else {
                continue;
            };
            let fields: Vec<&str> = text.split('\0').collect();
            let [kind, peeled_kind, name] = fields.as_slice() else {
                continue;
            };
            if snapshot.len() >= max {
                snapshot.complete = false;
                break;
            }
            // The same rule as the browse ref list: a commit, or a tag of
            // one.
            let commit = matches!((*kind, *peeled_kind), ("commit", _) | ("tag", "commit"));
            if commit {
                snapshot.commits.insert((*name).into());
            } else {
                snapshot.others.insert((*name).into());
            }
        }
        snapshot
    }

    /// What this snapshot says about the full name `full`.
    pub fn answer(&self, full: &str) -> BranchAnswer {
        if self.commits.contains(full) {
            BranchAnswer::Commit
        } else if self.others.contains(full) {
            BranchAnswer::NotACommit
        } else if self.complete {
            BranchAnswer::Missing
        } else {
            BranchAnswer::Unknown
        }
    }

    /// Branch names kept.
    pub fn len(&self) -> usize {
        self.commits.len() + self.others.len()
    }

    /// No branch at all (an empty repository).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether every branch git printed is here.
    pub fn is_complete(&self) -> bool {
        self.complete
    }
}
