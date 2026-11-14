//! The merge queue's vocabulary (M4 C8, ADR-0030): the states of an entry
//! and of a batch, why an entry runs alone, the batch bound and the
//! bisection rule, and the audit details of a person's change to a queue.
//! Pure: the rows are forge-db's (`forge_db::review::merge_queue`), the
//! git work and every decision forge-review's (`forge_review::merges::queue`).

use crate::{MergeRequestId, ProjectId};

/// The most entries one batch holds (plan 14 §9 decision 5).
pub const MAX_BATCH: usize = 4;

/// An entry reaped this many times (its batch passed the deadline) is
/// removed (`FG-CI-0024`, ADR-0030 §14). The table's CHECK
/// `merge_queue_entries_reaps` (`reaps BETWEEN 0 AND 3`, migration
/// 20261213100000) holds the same number: change both together.
pub const MAX_REAPS: i32 = 3;

/// The batch bound after a red batch of `size` entries (ADR-0030 §7): the
/// first half (rounded up) is tried next; a red batch of one is the
/// culprit's, the bound stays.
pub fn bisected_limit(size: usize) -> usize {
    if size <= 1 {
        1
    } else {
        size.div_ceil(2)
    }
}

macro_rules! words {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident => $word:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vdoc])* $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $word),+
                }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($word => Some($name::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

words! {
    /// `merge_queue_entries.state`.
    EntryState {
        /// Waiting for a batch.
        Queued => "queued",
        /// In the live batch.
        Testing => "testing",
        /// Landed: its merge attempt is `applied`.
        Merged => "merged",
        /// Out of the queue (a person, or the worker with a reason and code).
        Removed => "removed",
    }
}

impl EntryState {
    /// `queued` or `testing`: at most one per merge request.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Testing)
    }
}

words! {
    /// `merge_queue_batches.state`.
    BatchState {
        /// Its train pipeline runs (or is about to).
        Testing => "testing",
        /// Green: the merge attempts are being written and the target moved.
        Landing => "landing",
        /// The target is at the train; every entry `merged`.
        Landed => "landed",
        /// Red: bisected (2+) or its one entry removed.
        Failed => "failed",
        /// Thrown away without a verdict (target moved, an entry left,
        /// timeout, a refusal at landing): the entries went back.
        Abandoned => "abandoned",
    }
}

impl BatchState {
    /// `testing` or `landing`: at most one per queue.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Testing | Self::Landing)
    }
}

words! {
    /// Why an entry runs in a batch of its own (ADR-0030 §4).
    Isolation {
        /// Its diff touches a migration (`risk::is_migration_path`).
        Migration => "migration",
        /// The head's `forge/risk` run concluded `failure` (a `stop`).
        RiskStop => "risk_stop",
        /// Its method is `fast_forward` (a train is no fast-forward).
        FastForward => "fast_forward",
    }
}

/// A person's change to a merge queue (audited on the project, no event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeQueueChange {
    /// `merge_queue.enqueued`.
    Enqueued {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        entry: i64,
        target_branch: String,
        head_sha: String,
    },
    /// `merge_queue.dequeued`.
    Dequeued {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        entry: i64,
        target_branch: String,
    },
    /// `merge_queue.paused`.
    Paused {
        project: ProjectId,
        target_branch: String,
    },
    /// `merge_queue.resumed`.
    Resumed {
        project: ProjectId,
        target_branch: String,
    },
}

impl MergeQueueChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Enqueued { .. } => "merge_queue.enqueued",
            Self::Dequeued { .. } => "merge_queue.dequeued",
            Self::Paused { .. } => "merge_queue.paused",
            Self::Resumed { .. } => "merge_queue.resumed",
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::Enqueued { project, .. }
            | Self::Dequeued { project, .. }
            | Self::Paused { project, .. }
            | Self::Resumed { project, .. } => *project,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Enqueued {
                project,
                merge_request,
                iid,
                entry,
                target_branch,
                head_sha,
            } => serde_json::json!({
                "project_id": project.get(),
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "entry_id": entry,
                "target_branch": target_branch,
                "head_sha": head_sha,
            }),
            Self::Dequeued {
                project,
                merge_request,
                iid,
                entry,
                target_branch,
            } => serde_json::json!({
                "project_id": project.get(),
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "entry_id": entry,
                "target_branch": target_branch,
            }),
            Self::Paused {
                project,
                target_branch,
            }
            | Self::Resumed {
                project,
                target_branch,
            } => serde_json::json!({
                "project_id": project.get(),
                "target_branch": target_branch,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_red_batch_halves_rounding_up() {
        assert_eq!(bisected_limit(4), 2);
        assert_eq!(bisected_limit(3), 2);
        assert_eq!(bisected_limit(2), 1);
        assert_eq!(bisected_limit(1), 1);
        assert_eq!(bisected_limit(0), 1);
    }

    #[test]
    fn the_words_round_trip() {
        for s in EntryState::ALL {
            assert_eq!(EntryState::parse(s.as_str()), Some(*s));
        }
        for s in BatchState::ALL {
            assert_eq!(BatchState::parse(s.as_str()), Some(*s));
        }
        for s in Isolation::ALL {
            assert_eq!(Isolation::parse(s.as_str()), Some(*s));
        }
        assert!(EntryState::parse("other").is_none());
        assert!(EntryState::Testing.is_live() && !EntryState::Merged.is_live());
        assert!(BatchState::Landing.is_live() && !BatchState::Abandoned.is_live());
    }

    #[test]
    fn the_audit_actions_are_named_by_the_queue() {
        let p = ProjectId::new(1);
        let change = MergeQueueChange::Paused {
            project: p,
            target_branch: "refs/heads/main".to_owned(),
        };
        assert_eq!(change.action(), "merge_queue.paused");
        assert_eq!(change.project(), p);
        assert_eq!(change.to_value()["target_branch"], "refs/heads/main");
    }
}
